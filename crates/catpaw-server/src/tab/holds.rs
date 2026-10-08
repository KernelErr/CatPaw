//! Holds: what a held navigation would do, as a confirmation or the
//! hand-off page names it, and letting holds go or dropping them.

use catpaw_agent::snapshot::truncate;
use catpaw_engine::HeldNavigation;
use catpaw_protocol::wording::ErrorCode;

use super::report::{in_order, with_consequences};
use super::{GroupState, View};
use crate::output::{Failure, Held, ToolOutput};

/// The fields of a submission's body, `name=value` (secrets and what the
/// user typed in a hand-off masked, values cut short), as many as fit.
pub(super) fn describe_fields(kind: &str, body: &[u8], typed: &[String]) -> String {
    let mut fields: Vec<(String, String)> = Vec::new();
    let lower = kind.to_ascii_lowercase();
    if lower.starts_with("application/x-www-form-urlencoded") {
        fields = url::form_urlencoded::parse(body).into_owned().collect();
    } else if let Some(boundary) = lower
        .find("boundary=")
        .map(|i| kind[i + 9..].trim_matches('"').to_string())
    {
        let text = String::from_utf8_lossy(body);
        for part in text.split(&format!("--{boundary}")) {
            let Some((head, value)) = part.split_once("\r\n\r\n") else {
                continue;
            };
            let attr = |key: &str| {
                head.split(';').find_map(|p| {
                    p.trim()
                        .strip_prefix(&format!("{key}=\""))
                        .and_then(|v| v.split('"').next())
                        .map(str::to_string)
                })
            };
            let Some(name) = attr("name") else { continue };
            let value = match attr("filename") {
                Some(file) => format!("(file {file})"),
                None => value.trim_end_matches("\r\n").to_string(),
            };
            fields.push((name, value));
        }
    }
    let empty = fields.iter().filter(|(_, v)| v.is_empty()).count();
    fields.retain(|(_, v)| !v.is_empty());
    let mut out: Vec<String> = fields
        .iter()
        .take(8)
        .map(|(name, value)| {
            // The same names a recording keeps out are masked here.
            let value = if catpaw_net::har::is_secret_field(name) || typed.contains(value) {
                "***".to_string()
            } else {
                truncate(value, 40)
            };
            format!("{name}={value}")
        })
        .collect();
    if fields.len() > 8 {
        out.push(format!("+{} more", fields.len() - 8));
    }
    if empty > 0 {
        out.push(format!("{empty} empty"));
    }
    out.join(", ")
}

/// What a held navigation would do, for the confirmation (`typed`: what
/// the user typed in a hand-off, which shows masked).
pub(super) fn describe_held(held: &HeldNavigation, typed: &[String]) -> String {
    let request = &held.request;
    let target = format!("{} {}", request.method, truncate(request.url.as_str(), 160));
    match &request.body {
        Some((kind, body)) => {
            let fields = describe_fields(kind, body, typed);
            if fields.is_empty() {
                format!("submit → {target}")
            } else {
                format!("submit → {target} (fields: {fields})")
            }
        }
        None => format!("load → {target}"),
    }
}

impl GroupState {
    /// Lets the holds `ids` go (navigations, requests), as though the
    /// action had not been stopped, and reports like that action; with
    /// `before` (what the action left held, when its own result was not
    /// shown), what it led to before it was held as well. `None` when none
    /// of them is held any more (the page replaced or dropped them).
    pub(crate) fn release_holds(
        &mut self,
        tab: u32,
        ids: &[u64],
        status: String,
        view: View,
        before: Option<Held>,
    ) -> Result<Option<ToolOutput>, Failure> {
        let mut base = self.baseline(tab);
        // The action's requests are told again, with how they went.
        if let Some(held) = &before {
            base.requests = held.requests;
        }
        let navigations: Vec<u64> = self
            .page
            .held_navigations()
            .iter()
            .filter(|h| ids.contains(&h.id))
            .map(|h| h.id)
            .collect();
        let mut released = self.page.release_held_requests(ids);
        let mut result = Ok(());
        for id in navigations {
            match self.page.release_held(id) {
                Ok(true) => released += 1,
                Ok(false) => {}
                Err(e) => {
                    released += 1;
                    result = Err(e);
                    break;
                }
            }
        }
        if released == 0 {
            return Ok(None);
        }
        // Let the page take in what came back.
        self.page.settle(&super::action_limits());
        let mut report = self.finish(tab, &base);
        if let Some(held) = before {
            let after = std::mem::replace(&mut report.lines, held.lines);
            report.lines.extend(after);
            in_order(&mut report.lines);
            // A script's value comes after what happened, as it does in
            // its own result.
            report.lines.extend(held.value);
        }
        if let Err(e) = result {
            let failure = Failure::new(ErrorCode::NavigationFailed, e.to_string());
            return Err(with_consequences(failure, report.lines));
        }
        self.page_result(tab, status, report, None, view).map(Some)
    }

    /// Drops the holds `ids`: a navigation does not happen, requests fail
    /// for the page.
    pub(crate) fn drop_holds(&mut self, ids: &[u64]) {
        for &id in ids {
            self.page.drop_held(id);
        }
        self.page.drop_held_requests(ids);
    }
}
