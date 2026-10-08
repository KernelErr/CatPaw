//! Actions on a tab, and what they led to.

use std::cell::Cell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use catpaw_agent::snapshot::{quote, truncate};
use catpaw_engine::{
    ActionError, ConsoleLevel, DialogAnswer, DialogPolicy, EngineError, FrameId, HeldNavigation,
    InputError, LoopLimits, PageEvent, SettlePolicy, Value,
};
use catpaw_protocol::params::{self, ActionOptions, DialogChoice, SnapshotMode};
use catpaw_protocol::wording::{ErrorCode, advice, consequence, outcome};
use catpaw_web::page::Cx;
use catpaw_web::{agent, input};
use url::Url;

use super::pending::short_url;
use super::{
    Aim, CONSOLE_LINES, EVAL_CHARS, GroupState, Tab, View, floor_char_boundary, is_live, parse_url,
    png_size,
};
use crate::output::{CallResult, Failure, Held, ToolOutput};

/// What a tab's root document looked like before an action.
pub(super) struct Baseline {
    pub epoch: u64,
    pub console: usize,
    pub dialogs: usize,
    pub url: Option<Url>,
    pub requests: usize,
    /// Holds numbered from here on are the action's.
    pub watermark: u64,
}

/// What an action led to.
#[derive(Default)]
pub(super) struct Report {
    /// The last document the tab's root loaded: method, URL, status.
    pub navigated: Option<(String, Url, u16)>,
    /// The URL the document moved to without loading another
    /// (`pushState`, a fragment).
    pub same_document: Option<Url>,
    pub lines: Vec<String>,
    /// What the policy holds for the user's approval: the hold numbers
    /// and what they would do.
    pub held: Option<Held>,
    /// Holds that went unsent during the action.
    pub dropped: Vec<u64>,
    /// A navigation the policy refused: where to, and why.
    pub blocked: Option<(Url, String)>,
    /// The element acted on: focus that went to it is no news.
    pub acted: Option<u32>,
}

impl Report {
    /// ` → https://… (200)` and its variants, for a status line.
    pub fn suffix(&self) -> String {
        if let Some((method, url, status)) = &self.navigated {
            let url = truncate(url.as_str(), 160);
            if method == "GET" {
                format!(" → {url} ({status})")
            } else {
                format!(" → {url} ({method}, {status})")
            }
        } else if let Some(url) = &self.same_document {
            format!(" → {} (same document)", truncate(url.as_str(), 160))
        } else {
            String::new()
        }
    }
}

/// Whether a form field's name says it holds a secret.
fn secret_field(name: &str) -> bool {
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| {
            [
                "pass", "pwd", "secret", "token", "card", "cvv", "cvc", "ssn", "otp",
            ]
            .iter()
            .any(|s| word.starts_with(s))
                || word == "pin"
        })
}

/// The fields of a submission's body, `name=value` (secrets masked,
/// values cut short), as many as fit.
fn describe_fields(kind: &str, body: &[u8]) -> String {
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
            let value = if secret_field(name) {
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

/// Whether a console message is about a request to a host the settle
/// policy ignores (analytics): noise to the agent.
fn about_ignored_host(text: &str, policy: &SettlePolicy) -> bool {
    text.split_whitespace()
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .filter_map(|w| Url::parse(w.trim_end_matches([':', ',', ')', '.', '…'])).ok())
        .any(|url| policy.ignores_host(&url))
}

/// What a held navigation would do, for the confirmation.
fn describe_held(held: &HeldNavigation) -> String {
    let request = &held.request;
    let target = format!("{} {}", request.method, truncate(request.url.as_str(), 160));
    match &request.body {
        Some((kind, body)) => {
            let fields = describe_fields(kind, body);
            if fields.is_empty() {
                format!("submit → {target}")
            } else {
                format!("submit → {target} (fields: {fields})")
            }
        }
        None => format!("load → {target}"),
    }
}

/// The options of a select that the words name (label, value, then
/// loosely, when only one fits), or an error listing the options.
fn choose_options(
    state: &catpaw_web::PageState,
    select: catpaw_dom::NodeId,
    wanted: &[String],
    what: &str,
) -> Result<Vec<(catpaw_dom::NodeId, String)>, Failure> {
    let all: Vec<(catpaw_dom::NodeId, String, String)> = agent::options_of(state, select)
        .into_iter()
        .map(|o| {
            let (label, value) = agent::option_label_and_value(state, o);
            (o, label, value)
        })
        .collect();
    let mut chosen = Vec::new();
    for wanted in wanted.iter().cloned() {
        let w = wanted.split_whitespace().collect::<Vec<_>>().join(" ");
        let found = all
            .iter()
            .find(|(_, label, _)| *label == w)
            .or_else(|| all.iter().find(|(_, _, value)| *value == wanted))
            .or_else(|| {
                all.iter()
                    .find(|(_, label, _)| label.to_lowercase() == w.to_lowercase())
            })
            .or_else(|| {
                // Loosely, when only one option fits: the same words,
                // punctuation aside ("Price low to high" for "Price (low
                // to high)"), or words all in the label.
                let key = loose(&wanted);
                let words: Vec<&str> = key.split(' ').filter(|w| !w.is_empty()).collect();
                let same: Vec<_> = all.iter().filter(|(_, l, _)| loose(l) == key).collect();
                let within: Vec<_> = all
                    .iter()
                    .filter(|(_, l, _)| {
                        let label = loose(l);
                        let label: Vec<&str> = label.split(' ').collect();
                        !words.is_empty() && words.iter().all(|w| label.contains(w))
                    })
                    .collect();
                match (same.as_slice(), within.as_slice()) {
                    ([one], _) => Some(*one),
                    ([], [one]) => Some(*one),
                    _ => None,
                }
            });
        match found {
            Some((node, label, _)) => chosen.push((*node, label.clone())),
            None => {
                let listed: Vec<String> = all
                    .iter()
                    .take(30)
                    .map(|(_, label, _)| quote(&truncate(label, 60)))
                    .collect();
                let mut message = format!("no option {} in {what}", quote(&wanted));
                let _ = write!(message, "; options: {}", listed.join(", "));
                if all.len() > 30 {
                    let _ = write!(message, " (+{} more)", all.len() - 30);
                }
                return Err(Failure::new(ErrorCode::NotFound, message));
            }
        }
    }
    Ok(chosen)
}

/// `true`/`false` as a field's value may say it.
fn flag(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "checked" | "1" => Some(true),
        "false" | "no" | "off" | "unchecked" | "0" => Some(false),
        _ => None,
    }
}

/// How a click call asks to click.
fn click_options(p: &params::Click) -> Result<input::ClickOptions, Failure> {
    let mut how = input::ClickOptions {
        button: match p.button {
            None | Some(params::MouseButton::Left) => 0,
            Some(params::MouseButton::Middle) => 1,
            Some(params::MouseButton::Right) => 2,
        },
        count: p.count.unwrap_or(1).clamp(1, 3),
        ..input::ClickOptions::default()
    };
    for key in p.modifiers.iter().flatten() {
        match key.trim().to_ascii_lowercase().as_str() {
            "control" | "ctrl" => how.ctrl = true,
            "shift" => how.shift = true,
            "alt" | "option" => how.alt = true,
            "meta" | "cmd" | "command" => how.meta = true,
            _ => {
                return Err(Failure::bad_argument(format!(
                    "{key:?} is not a modifier (Control, Shift, Alt, Meta)"
                )));
            }
        }
    }
    Ok(how)
}

/// Text as option matching compares it loosely: lowercase words, with
/// punctuation gone.
fn loose(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A failure with the consequences of the action that failed; one that
/// only repeats a navigation failure is left out.
fn with_consequences(mut failure: Failure, lines: Vec<String>) -> Failure {
    let repeats = format!("! {} ", consequence::NAVIGATION_FAILED);
    for line in lines {
        if failure.code == ErrorCode::NavigationFailed && line.starts_with(&repeats) {
            continue;
        }
        failure = failure.with(line);
    }
    failure
}

/// The element part of a diff line (`e2 textbox "Name"` of `e2 textbox
/// "Name" [value=- → Ada]`): its ref, role and quoted name.
fn element_part(line: &str) -> Option<&str> {
    let (r, rest) = line.split_once(' ')?;
    if !r.starts_with('e') || !r[1..].bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let role_end = rest.find(' ').unwrap_or(rest.len());
    let role = &rest[..role_end];
    if role.contains('[') {
        // A text of the element (`text[2]`), not the element.
        return None;
    }
    let mut end = r.len() + 1 + role_end;
    if let Some(quoted) = rest[role_end..].strip_prefix(" \"") {
        let mut escaped = false;
        let close = quoted.char_indices().find(|&(_, c)| {
            let close = c == '"' && !escaped;
            escaped = c == '\\' && !escaped;
            close
        })?;
        end += 2 + close.0 + 1;
    }
    Some(&line[..end])
}

/// What a lone change to the element a status line names adds to that
/// line: `[value=- → Ada]` for `~ e2 textbox "Name" [value=- → Ada]`.
fn lone_change<'a>(status: &str, lines: &'a [String]) -> Option<&'a str> {
    let [line] = lines else {
        return None;
    };
    let change = line.strip_prefix("~ ")?;
    let element = element_part(change)?;
    let echoed = status
        .split_once(' ')
        .and_then(|(_, rest)| rest.split_once(' '))
        .is_some_and(|(_, rest)| rest.starts_with(element) || rest == element);
    echoed.then(|| &change[element.len()..])
}

fn dialog_line(kind: &str, message: &str, answer: &DialogAnswer) -> String {
    let message = quote(&truncate(message, 120));
    let answer = match (kind, answer) {
        ("alert", _) => return format!("{kind} {message}"),
        (_, DialogAnswer::Dismissed) => "dismissed".to_string(),
        (_, DialogAnswer::Accepted) => "accepted".to_string(),
        (_, DialogAnswer::Text(text)) => format!("accepted {}", quote(&truncate(text, 60))),
    };
    format!("{kind} {message} → {answer}")
}

impl GroupState {
    pub(super) fn baseline(&self, tab: u32) -> Baseline {
        let state = self
            .tabs
            .get(&tab)
            .and_then(|t| self.page.frame_state(t.root));
        let requests = self.page.net().requests_len();
        let watermark = self.page.hold_watermark();
        match state {
            Some(state) => Baseline {
                epoch: state.epoch,
                console: state.console_len(),
                dialogs: state.dialogs.borrow().len(),
                url: Some(state.url.borrow().clone()),
                requests,
                watermark,
            },
            None => Baseline {
                epoch: 0,
                console: 0,
                dialogs: 0,
                url: None,
                requests,
                watermark,
            },
        }
    }

    /// Collects what happened since `base`: navigations of the tab, tabs
    /// opened and closed, dialogs, requests, console errors, and what kept
    /// the page busy.
    pub(super) fn finish(&mut self, tab: u32, base: &Baseline) -> Report {
        let mut report = Report::default();
        let root = self.tabs.get(&tab).map(|t| t.root);
        let mut events = Vec::new();
        let mut held_ids = Vec::new();
        let mut held_what = Vec::new();
        for event in self.page.take_events() {
            match event {
                PageEvent::Navigated {
                    frame,
                    method,
                    url,
                    status,
                } if Some(frame) == root => {
                    events.push(format!("navigated {method} {url} {status}"));
                    report.navigated = Some((method, url, status));
                }
                PageEvent::NavigationHeld {
                    id, method, url, ..
                } => {
                    events.push(format!("held {method} {url}"));
                    // One the page replaced before the action ended is gone.
                    if let Some(held) = self.page.held_navigations().iter().find(|h| h.id == id) {
                        held_ids.push(id);
                        held_what.push(describe_held(held));
                    }
                }
                PageEvent::HoldDropped { id } => report.dropped.push(id),
                PageEvent::RequestBlocked {
                    method,
                    url,
                    reason,
                } => {
                    events.push(format!("blocked {method} {url}: {reason}"));
                    report.lines.push(format!(
                        "! {} {method} {} ({reason})",
                        consequence::BLOCKED,
                        truncate(url.as_str(), 160)
                    ));
                }
                PageEvent::NavigationBlocked { url, reason, .. } => {
                    events.push(format!("blocked {url}: {reason}"));
                    report.blocked = Some((url, reason));
                }
                PageEvent::NavigationFailed { frame, url, error } if Some(frame) == root => {
                    events.push(format!("navigation failed {url}: {error}"));
                    report.lines.push(format!(
                        "! {} {url}: {error}",
                        consequence::NAVIGATION_FAILED
                    ));
                }
                PageEvent::PopupOpened { frame, opener, url } => {
                    let id = self.next_tab.fetch_add(1, Ordering::SeqCst);
                    let opener = self.tab_of_frame(opener);
                    let epoch = self.page.document_epoch(frame).unwrap_or(0);
                    self.tabs.insert(id, Tab::new(id, frame, opener, epoch));
                    events.push(format!("popup t{id} {url}"));
                    report.lines.push(format!(
                        "! {} t{id} {} (switch with tabs)",
                        consequence::POPUP,
                        truncate(url.as_str(), 120)
                    ));
                }
                PageEvent::Download {
                    url,
                    name,
                    mime,
                    size,
                } => {
                    events.push(format!("download {name} {url}"));
                    let kind = if mime.is_empty() {
                        String::new()
                    } else {
                        format!("{mime}, ")
                    };
                    report.lines.push(format!(
                        "! {} {} ({kind}{})",
                        consequence::DOWNLOAD,
                        quote(&truncate(&name, 80)),
                        crate::files::size(size as u64)
                    ));
                }
                PageEvent::PopupClosed { frame } => {
                    let closed = self.tabs.values().find(|t| t.root == frame).map(|t| t.id);
                    if let Some(id) = closed {
                        self.tabs.remove(&id);
                        events.push(format!("tab closed t{id}"));
                        report
                            .lines
                            .push(format!("! {} t{id}", consequence::TAB_CLOSED));
                    }
                }
                _ => {}
            }
        }
        let state = root.and_then(|root| self.page.frame_state(root)).cloned();
        if let Some(state) = &state {
            let fresh = state.epoch != base.epoch;
            if report.navigated.is_none() && !fresh {
                let now = state.url.borrow().clone();
                if base.url.as_ref() != Some(&now) {
                    events.push(format!("url {now}"));
                    report.same_document = Some(now);
                }
            }
            let dialogs = state.dialogs.borrow();
            for dialog in dialogs.iter().skip(if fresh { 0 } else { base.dialogs }) {
                let line = dialog_line(dialog.kind, &dialog.message, &dialog.answer);
                events.push(format!("dialog {line}"));
                report
                    .lines
                    .push(format!("! {} {line}", consequence::DIALOG));
            }
        }
        // Requests the action made (script's own, not analytics); those of
        // a polling timer only when they write or fail.
        let policy = SettlePolicy::default();
        let page_url = state.as_ref().map(|s| s.url.borrow().clone());
        let mut polled = 0;
        let made: Vec<String> = self
            .page
            .net()
            .requests_since(base.requests)
            .into_iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    catpaw_web::net::RequestKind::Fetch | catpaw_web::net::RequestKind::Xhr
                ) && !policy.ignores_host(&r.url)
            })
            .filter(|r| {
                let routine = r.polling
                    && matches!(r.method.as_str(), "GET" | "HEAD")
                    && r.status.is_some_and(|s| s < 400);
                if routine {
                    polled += 1;
                }
                !routine
            })
            .map(|r| {
                let outcome = match (r.status, &r.error, r.finished) {
                    (Some(status), _, _) => status.to_string(),
                    (None, Some(_), _) => "failed".to_string(),
                    (None, None, true) => "failed".to_string(),
                    (None, None, false) => "pending".to_string(),
                };
                format!(
                    "{} {} {outcome}",
                    r.method,
                    short_url(&r.url, page_url.as_ref())
                )
            })
            .collect();
        if !made.is_empty() {
            let mut line = format!(
                "! {} {}",
                consequence::NETWORK,
                made[..made.len().min(3)].join(", ")
            );
            if made.len() > 3 {
                let _ = write!(line, " (+{} more)", made.len() - 3);
            }
            if polled > 0 {
                let _ = write!(line, " (+{polled} polling)");
            }
            report.lines.push(line);
        }
        if let Some(state) = &state {
            let fresh = state.epoch != base.epoch;
            let errors: Vec<String> = state
                .console_since(if fresh { 0 } else { base.console })
                .into_iter()
                .filter(|m| m.level == ConsoleLevel::Error && !about_ignored_host(&m.text, &policy))
                .map(|m| m.text)
                .collect();
            for text in errors.iter().take(CONSOLE_LINES) {
                let first = text.lines().next().unwrap_or("");
                report.lines.push(format!(
                    "! {} error: {}",
                    consequence::CONSOLE,
                    truncate(first, 160)
                ));
            }
            if errors.len() > CONSOLE_LINES {
                report.lines.push(format!(
                    "! {} +{} more errors",
                    consequence::CONSOLE,
                    errors.len() - CONSOLE_LINES
                ));
            }
        }
        // The requests the action left held (older ones belong to earlier
        // confirmations).
        let requests: Vec<_> = self
            .page
            .held_requests()
            .into_iter()
            .filter(|r| r.id >= base.watermark)
            .collect();
        if !requests.is_empty() {
            let mut sent: Vec<String> = requests
                .iter()
                .take(3)
                .map(|r| format!("{} {}", r.method, truncate(r.url.as_str(), 120)))
                .collect();
            if requests.len() > 3 {
                sent.push(format!("+{} more", requests.len() - 3));
            }
            held_ids.extend(requests.iter().map(|r| r.id));
            held_what.push(format!("send → {}", sent.join(", ")));
        }
        if !held_ids.is_empty() {
            report.held = Some(Held {
                ids: held_ids,
                what: held_what.join(", and "),
            });
        }
        if let Some(root) = root {
            report.lines.extend(self.not_settled(root));
        }
        if let Some(entry) = self.tabs.get_mut(&tab) {
            for event in events {
                entry.event(event);
            }
        }
        report
    }

    /// The text of a result that changed the page: the status line, what
    /// happened, and the page as `mode` asks (what changed, by default).
    pub(super) fn page_result(
        &mut self,
        tab: u32,
        status: String,
        report: Report,
        mode: Option<SnapshotMode>,
        view: View,
    ) -> CallResult {
        if let Some((url, reason)) = &report.blocked {
            let action = status.strip_prefix("ok ").unwrap_or(&status);
            let mut text = format!(
                "{} {}: {action} → {} ({reason})",
                outcome::BLOCKED,
                outcome::POLICY,
                truncate(url.as_str(), 160)
            );
            for line in &report.lines {
                text.push('\n');
                text.push_str(line);
            }
            return Ok(ToolOutput::ok(text));
        }
        let held = report.held.clone();
        let dropped = report.dropped.clone();
        let suffix = report.suffix();
        let url_shown = report.navigated.is_some() || report.same_document.is_some();
        let page = self.page_view(tab, mode, view, url_shown, report.acted)?;
        // A lone change to the element acted on, with nothing else to say,
        // goes on the status line (`ok type e2 textbox "Name" [value=- →
        // Ada]`).
        if suffix.is_empty()
            && report.lines.is_empty()
            && let Some(rest) = page
                .quiet
                .as_deref()
                .and_then(|lines| lone_change(&status, lines))
        {
            return Ok(ToolOutput {
                held,
                dropped_holds: dropped,
                ..ToolOutput::ok(format!("{status}{rest}"))
            });
        }
        let mut text = status;
        text.push_str(&suffix);
        for line in &report.lines {
            text.push('\n');
            text.push_str(line);
        }
        if !page.text.is_empty() {
            text.push('\n');
            text.push_str(&page.text);
        }
        Ok(ToolOutput {
            held,
            dropped_holds: dropped,
            ..ToolOutput::ok(text)
        })
    }

    /// Lets the holds `ids` go (navigations, requests), as though the
    /// action had not been stopped, and reports like that action. `None`
    /// when none of them is held any more (the page replaced or dropped
    /// them).
    pub(crate) fn release_holds(
        &mut self,
        tab: u32,
        ids: &[u64],
        status: String,
        view: View,
    ) -> Result<Option<ToolOutput>, Failure> {
        let base = self.baseline(tab);
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
        let report = self.finish(tab, &base);
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

    /// Runs `f` with dialogs answered as `options` say, then back to
    /// dismissing them.
    fn with_dialogs<R>(&mut self, options: &ActionOptions, f: impl FnOnce(&mut Self) -> R) -> R {
        let accept = options.dialog == Some(DialogChoice::Accept)
            || (options.prompt_text.is_some() && options.dialog.is_none());
        self.page.set_dialog_policy(DialogPolicy {
            accept,
            prompt_text: options.prompt_text.clone(),
        });
        let result = f(self);
        self.page.set_dialog_policy(DialogPolicy::default());
        result
    }

    /// Runs an input action on the aimed-at element's page, then reports.
    fn act_on<R>(
        &mut self,
        tab: u32,
        aim: Aim,
        status: String,
        options: &ActionOptions,
        view: View,
        action: impl FnOnce(&mut Cx<'_>, &Aim) -> Result<R, InputError>,
    ) -> CallResult {
        let base = self.baseline(tab);
        let result = self.with_dialogs(options, |g| {
            g.page.input_in(aim.frame, |cx| action(cx, &aim))
        });
        let mut report = self.finish(tab, &base);
        report.acted = Some(aim.r);
        match result {
            Ok(_) => self.page_result(tab, status, report, options.snapshot, view),
            Err(e) => Err(self.action_failure(tab, &aim, e, report)),
        }
    }

    /// Runs an input action on the tab's focused element (or its document).
    fn act_on_focus(
        &mut self,
        tab: u32,
        status: String,
        options: &ActionOptions,
        view: View,
        action: impl FnOnce(&mut Cx<'_>) -> Result<(), InputError>,
    ) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let base = self.baseline(tab);
        let result = self.with_dialogs(options, |g| g.page.input_in(root, action));
        let report = self.finish(tab, &base);
        match result {
            Ok(()) => self.page_result(tab, status, report, options.snapshot, view),
            Err(ActionError::Input(InputError::Detached)) => {
                Err(Failure::bad_argument("nothing has focus").with(advice::NOTHING_FOCUSED))
            }
            Err(e) => Err(Failure::new(ErrorCode::NavigationFailed, e.to_string())),
        }
    }

    /// The checks before acting on an element: enabled (when the action
    /// needs it), and holding still while the page animates.
    fn actionable(&mut self, tab: u32, aim: &Aim, enabled: bool) -> Result<(), Failure> {
        let Some(state) = self.page.frame_state(aim.frame).cloned() else {
            return Ok(());
        };
        let what = self.aimed(tab, aim);
        if enabled && agent::is_disabled(&state, aim.node) {
            return Err(
                Failure::new(ErrorCode::NotActionable, format!("{what} is disabled"))
                    .with(advice::DISABLED),
            );
        }
        let animating = self.page.pending_of(aim.frame).is_some_and(|p| p.animating);
        if !animating {
            return Ok(());
        }
        let mut last = agent::element_rect(&state, aim.node);
        for _ in 0..10 {
            self.page.settle(&LoopLimits {
                wall: std::time::Duration::from_millis(500),
                virtual_ms: 17.0,
                settle: None,
                ..LoopLimits::default()
            });
            let Some(state) = self.page.frame_state(aim.frame).cloned() else {
                return Ok(());
            };
            let now = agent::element_rect(&state, aim.node);
            if now == last {
                return Ok(());
            }
            last = now;
        }
        Err(
            Failure::new(ErrorCode::NotActionable, format!("{what} keeps moving"))
                .with(advice::MOVING),
        )
    }

    /// A control in or around what covers an element that would dismiss
    /// it: a close, accept or "no thanks" button.
    fn dismiss_hint(&mut self, tab: u32, frame: FrameId, cover: catpaw_dom::NodeId) -> Option<u32> {
        const WORDS: &[&str] = &[
            "close",
            "dismiss",
            "accept",
            "agree",
            "got it",
            "ok",
            "no thanks",
            "reject",
            "decline",
            "continue",
            "allow",
            "×",
            "✕",
            "✖",
        ];
        let state = self.page.frame_state(frame)?.clone();
        let found = agent::with_styles(&state, |engine, dom| {
            let oracle = crate::oracle::EngineOracle {
                engine,
                page: &state,
            };
            for around in std::iter::once(cover).chain(dom.ancestors(cover)).take(6) {
                for n in std::iter::once(around).chain(dom.descendants(around)) {
                    let role = catpaw_agent::a11y::role_for(dom, n);
                    if !matches!(role, Some("button" | "link")) {
                        continue;
                    }
                    let label = dom
                        .attr(n, "aria-label")
                        .map(str::to_string)
                        .unwrap_or_else(|| catpaw_agent::a11y::subtree_text(dom, n, &oracle))
                        .trim()
                        .to_lowercase();
                    let fits = label == "x"
                        || WORDS
                            .iter()
                            .any(|w| label == *w || (label.len() <= 30 && label.contains(w)));
                    if fits && !catpaw_agent::visibility::is_hidden(dom, n, &oracle) {
                        return Some(n);
                    }
                }
            }
            None
        })?;
        self.ref_for(tab, frame, found)
    }

    /// Words an input error about the aimed-at element.
    fn action_failure(
        &mut self,
        tab: u32,
        aim: &Aim,
        error: ActionError,
        report: Report,
    ) -> Failure {
        let what = self.aimed(tab, aim);
        let mut failure = match error {
            ActionError::Input(InputError::Detached) => {
                Failure::new(ErrorCode::StaleRef, format!("{what} (removed)")).with(advice::STALE)
            }
            ActionError::Input(InputError::NotVisible) => {
                Failure::new(ErrorCode::NotActionable, format!("{what} is not visible"))
                    .with(advice::NOT_VISIBLE)
            }
            ActionError::Input(InputError::NotEditable) => Failure::new(
                ErrorCode::NotActionable,
                format!("{what} does not take this input"),
            )
            .with(advice::NOT_EDITABLE),
            ActionError::Input(InputError::Disabled) => {
                Failure::new(ErrorCode::NotActionable, format!("{what} is disabled"))
                    .with(advice::DISABLED)
            }
            ActionError::Input(InputError::Occluded { by }) => {
                let cover = self.ref_for(tab, aim.frame, by);
                let cover = cover
                    .map(|r| self.describe(tab, r))
                    .unwrap_or_else(|| "another element".to_string());
                let failure =
                    Failure::new(ErrorCode::Occluded, format!("{what} is covered by {cover}"));
                match self.dismiss_hint(tab, aim.frame, by) {
                    Some(r) => failure
                        .with(format!("maybe dismiss it with {}", self.describe(tab, r)))
                        .with(advice::OCCLUDED),
                    None => failure.with(advice::OCCLUDED),
                }
            }
            ActionError::NoFrame(_) => {
                Failure::new(ErrorCode::StaleRef, format!("{what} (frame closed)"))
                    .with(advice::STALE)
            }
            other => Failure::new(ErrorCode::NavigationFailed, other.to_string()),
        };
        failure = with_consequences(failure, report.lines);
        failure
    }

    pub(crate) fn navigate(&mut self, tab: u32, p: params::Navigate, view: View) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let base = self.baseline(tab);
        let (status, result) = match (p.url, p.go) {
            (Some(url), None) => {
                let url = parse_url(&url)?;
                ("ok navigate".to_string(), self.page.goto_in(root, url))
            }
            (None, Some(go)) => {
                let word = match go {
                    params::Go::Back => "back",
                    params::Go::Forward => "forward",
                    params::Go::Reload => "reload",
                };
                let result = if root == FrameId(0) {
                    match go {
                        params::Go::Back => self.page.back(),
                        params::Go::Forward => self.page.forward(),
                        params::Go::Reload => self.page.reload(),
                    }
                    .map_err(ActionError::from)
                } else if go == params::Go::Reload {
                    let url = self.page.url_of(root).ok_or_else(|| {
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is closed"))
                    })?;
                    self.page.goto_in(root, url)
                } else {
                    return Err(Failure::new(
                        ErrorCode::Unsupported,
                        format!("{word} in a tab a page opened"),
                    ));
                };
                (format!("ok {word}"), result)
            }
            (Some(_), Some(_)) => {
                return Err(Failure::bad_argument("pass url or go, not both"));
            }
            (None, None) => {
                return Err(Failure::bad_argument(
                    "pass url, or go: back, forward or reload",
                ));
            }
        };
        let report = self.finish(tab, &base);
        if let Err(e) = result {
            let message = match &e {
                ActionError::Engine(EngineError::Net(net)) => net.to_string(),
                other => other.to_string(),
            };
            return Err(Failure::new(ErrorCode::NavigationFailed, message));
        }
        let mut status = status;
        if report.navigated.is_none() && report.same_document.is_none() && p.go.is_some() {
            status.push_str(" (no page to go to)");
        }
        let mode = p.snapshot.or(Some(SnapshotMode::Full));
        self.page_result(tab, status, report, mode, view)
    }

    pub(crate) fn click(&mut self, tab: u32, p: params::Click, view: View) -> CallResult {
        let options = p.options();
        let aim = self.aim(tab, &p.target)?;
        let force = p.force;
        if !force && aim.point.is_none() {
            self.actionable(tab, &aim, true)?;
        }
        let how = click_options(&p)?;
        let verb = match (how.button, how.count) {
            (0, 1) => "click".to_string(),
            (0, 2) => "double-click".to_string(),
            (0, _) => "triple-click".to_string(),
            (2, 1) => "right-click".to_string(),
            (1, 1) => "middle-click".to_string(),
            (button, count) => {
                let name = if button == 2 { "right" } else { "middle" };
                format!("{name}-click x{count}")
            }
        };
        let mut status = format!("ok {verb} {}", self.aimed(tab, &aim));
        let held: Vec<&str> = [
            (how.ctrl, "Control"),
            (how.shift, "Shift"),
            (how.alt, "Alt"),
            (how.meta, "Meta"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
        if !held.is_empty() {
            let _ = write!(status, " with {}", held.join("+"));
        }
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            match aim.point {
                Some((x, y)) => {
                    input::click_at_with(cx, x, y, how);
                    Ok(())
                }
                // Forced: the element gets the click, whatever covers it.
                None if force => match input::click_element_with(cx, aim.node, how) {
                    Err(InputError::Occluded { .. } | InputError::NotVisible) => {
                        catpaw_web::activation::click(cx, aim.node, true);
                        Ok(())
                    }
                    other => other.map(drop),
                },
                None => input::click_element_with(cx, aim.node, how).map(drop),
            }
        })
    }

    pub(crate) fn type_text(&mut self, tab: u32, p: params::Type, view: View) -> CallResult {
        let options = p.options();
        let aim = match &p.target {
            Some(target) => self.aim(tab, target)?,
            None => {
                let (root, state) = self.root_state(tab)?;
                let node = agent::focused(&state).ok_or_else(|| {
                    Failure::bad_argument("nothing has focus").with(advice::NOTHING_FOCUSED)
                })?;
                let r = self.ref_for(tab, root, node).unwrap_or(0);
                Aim {
                    frame: root,
                    node,
                    r,
                    point: None,
                    retargeted: None,
                }
            }
        };
        self.actionable(tab, &aim, true)?;
        let mut status = format!("ok type {}", self.aimed(tab, &aim));
        if p.submit {
            status.push_str(" + Enter");
        }
        let secret = self.page.frame_state(aim.frame).is_some_and(|state| {
            let dom = state.dom.borrow();
            dom.is_html_element(aim.node, "input")
                && dom
                    .attr(aim.node, "type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"))
        });
        let text = p.text;
        let (append, submit) = (p.append, p.submit);
        if let Some(state) = self.page.frame_state(aim.frame) {
            agent::unmask_value(state, aim.node);
        }
        let mut output = self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            if append {
                input::focus(cx, aim.node)?;
                input::type_text(cx, &text)?;
            } else {
                input::fill(cx, aim.node, &text)?;
            }
            if submit {
                input::press(cx, "Enter")?;
            }
            Ok(())
        });
        if let Ok(output) = &mut output {
            output.secret_input = secret;
        }
        output
    }

    /// Sets several fields as one action, each as what it is: text, checked
    /// or not, or options; then Enter in the last one when asked. Every
    /// target is found before anything is set.
    pub(crate) fn fill(&mut self, tab: u32, p: params::Fill, view: View) -> CallResult {
        enum Step {
            Text(String),
            /// Checked or not, and an ARIA checkbox's state now.
            Check(bool, Option<bool>),
            Choose(Vec<catpaw_dom::NodeId>),
        }
        let options = p.options();
        if p.fields.is_empty() {
            return Err(Failure::bad_argument(
                "fill needs fields: [{\"target\":…, \"value\":…}]",
            ));
        }
        let mut plan: Vec<(Aim, Step)> = Vec::new();
        let mut shown = Vec::new();
        let mut secret = false;
        for field in &p.fields {
            let aim = self.aim(tab, &field.target)?;
            let what = self.aimed(tab, &aim);
            let state = self
                .page
                .frame_state(aim.frame)
                .cloned()
                .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
            let role = self
                .tabs
                .get(&tab)
                .and_then(|t| t.refs.entry(aim.r))
                .map(|e| e.role)
                .unwrap_or("");
            let (is_select, password, aria_checked) = {
                let dom = state.dom.borrow();
                let is_input = dom.is_html_element(aim.node, "input");
                let password = is_input
                    && dom
                        .attr(aim.node, "type")
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"));
                let aria = (!is_input).then(|| {
                    dom.attr(aim.node, "aria-checked")
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
                });
                (dom.is_html_element(aim.node, "select"), password, aria)
            };
            let checkable = matches!(
                role,
                "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
            );
            let step = if is_select {
                let wanted = match &field.value {
                    params::FillValue::Text(text) => vec![text.clone()],
                    params::FillValue::Options(list) => list.clone(),
                    params::FillValue::Checked(_) => {
                        return Err(Failure::bad_argument(format!(
                            "{what} takes an option's label, not true or false"
                        )));
                    }
                };
                let chosen = choose_options(&state, aim.node, &wanted, &what)?;
                let labels: Vec<String> = chosen.iter().map(|(_, l)| quote(l)).collect();
                shown.push(format!("{what} ← {}", labels.join(", ")));
                Step::Choose(chosen.into_iter().map(|(n, _)| n).collect())
            } else if checkable {
                let want = match &field.value {
                    params::FillValue::Checked(on) => Some(*on),
                    params::FillValue::Text(text) => flag(text),
                    params::FillValue::Options(_) => None,
                }
                .ok_or_else(|| Failure::bad_argument(format!("{what} takes true or false")))?;
                shown.push(format!(
                    "{what} ← {}",
                    if want { "checked" } else { "unchecked" }
                ));
                Step::Check(
                    want,
                    if role == "radio" && !want {
                        None
                    } else {
                        aria_checked
                    },
                )
            } else {
                let params::FillValue::Text(text) = &field.value else {
                    return Err(Failure::bad_argument(format!("{what} takes text")));
                };
                secret |= password;
                let value = if password {
                    "***".to_string()
                } else {
                    quote(&truncate(text, 60))
                };
                shown.push(format!("{what} ← {value}"));
                Step::Text(text.clone())
            };
            self.actionable(tab, &aim, true)?;
            plan.push((aim, step));
        }
        let mut status = format!("ok fill {}", shown.join(", "));
        if p.submit {
            status.push_str(" + Enter");
        }
        for (aim, step) in &plan {
            if matches!(step, Step::Text(_))
                && let Some(state) = self.page.frame_state(aim.frame)
            {
                agent::unmask_value(state, aim.node);
            }
        }
        let base = self.baseline(tab);
        let submit = p.submit;
        let mut failed: Option<(usize, ActionError)> = None;
        self.with_dialogs(&options, |g| {
            for (i, (aim, step)) in plan.iter().enumerate() {
                let node = aim.node;
                let result = g.page.input_in(aim.frame, |cx| match step {
                    Step::Text(text) => input::fill(cx, node, text),
                    Step::Check(want, aria) => match aria {
                        // An ARIA checkbox flips when clicked.
                        Some(now) if now == want => Ok(()),
                        Some(_) => input::click_element(cx, node).map(drop),
                        None => input::set_checked(cx, node, *want),
                    },
                    Step::Choose(nodes) => input::select_options(cx, node, nodes),
                });
                if let Err(e) = result {
                    failed = Some((i, e));
                    return;
                }
            }
            if submit && let Some((aim, _)) = plan.last() {
                let node = aim.node;
                let pressed = g.page.input_in(aim.frame, |cx| {
                    input::focus(cx, node)?;
                    input::press(cx, "Enter")
                });
                if let Err(e) = pressed {
                    failed = Some((plan.len() - 1, e));
                }
            }
        });
        let mut report = self.finish(tab, &base);
        report.acted = plan.last().map(|(aim, _)| aim.r);
        if let Some((i, e)) = failed {
            return Err(self.action_failure(tab, &plan[i].0, e, report));
        }
        let mut output = self.page_result(tab, status, report, options.snapshot, view)?;
        output.secret_input = secret;
        Ok(output)
    }

    pub(crate) fn press(&mut self, tab: u32, p: params::Press, view: View) -> CallResult {
        let options = p.options();
        let key = crate::target::normalize_key(&p.key).map_err(Failure::bad_argument)?;
        let repeat = p.repeat.unwrap_or(1).clamp(1, 50);
        let times = if repeat > 1 {
            format!(" x{repeat}")
        } else {
            String::new()
        };
        match &p.target {
            Some(target) => {
                let aim = self.aim(tab, target)?;
                let status = format!("ok press {key}{times} on {}", self.aimed(tab, &aim));
                self.act_on(tab, aim, status, &options, view, move |cx, aim| {
                    input::focus(cx, aim.node)?;
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                })
            }
            None => {
                let status = format!("ok press {key}{times}");
                self.act_on_focus(tab, status, &options, view, move |cx| {
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                })
            }
        }
    }

    pub(crate) fn select(&mut self, tab: u32, p: params::Select, view: View) -> CallResult {
        let options = p.options();
        let aim = self.aim(tab, &p.target)?;
        let state = self
            .page
            .frame_state(aim.frame)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let what = self.aimed(tab, &aim);
        if !state.dom.borrow().is_html_element(aim.node, "select") {
            return Err(Failure::new(
                ErrorCode::NotActionable,
                format!("{what} is not a <select>"),
            )
            .with(advice::NOT_A_SELECT));
        }
        let chosen = choose_options(&state, aim.node, &p.option.to_vec(), &what)?;
        self.actionable(tab, &aim, true)?;
        let labels: Vec<String> = chosen.iter().map(|(_, l)| quote(l)).collect();
        let status = format!("ok select {what} ← {}", labels.join(", "));
        let nodes: Vec<catpaw_dom::NodeId> = chosen.iter().map(|(n, _)| *n).collect();
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::select_options(cx, aim.node, &nodes)
        })
    }

    pub(crate) fn act(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let kind = p.kind;
        if kind == params::ActKind::Upload {
            return self.upload(tab, p, view);
        }
        if kind == params::ActKind::Drag {
            return self.drag(tab, p, view);
        }
        if kind == params::ActKind::Scroll && (p.target.is_none() || p.dy.is_some()) {
            return self.scroll(tab, p, view);
        }
        let target = p
            .target
            .ok_or_else(|| Failure::bad_argument(format!("{} needs a target", kind.as_str())))?;
        let aim = self.aim(tab, &target)?;
        let what = self.aimed(tab, &aim);
        let mut aria_checked = None;
        if matches!(kind, params::ActKind::Check | params::ActKind::Uncheck) {
            let role = self
                .tabs
                .get(&tab)
                .and_then(|t| t.refs.entry(aim.r))
                .map(|e| e.role)
                .unwrap_or("");
            if !matches!(
                role,
                "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
            ) {
                return Err(Failure::new(
                    ErrorCode::NotActionable,
                    format!("{what} is not a checkbox or radio button"),
                )
                .with(advice::NOT_CHECKABLE));
            }
            let state = self
                .page
                .frame_state(aim.frame)
                .cloned()
                .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
            let dom = state.dom.borrow();
            if !dom.is_html_element(aim.node, "input") {
                aria_checked = Some(
                    dom.attr(aim.node, "aria-checked")
                        .is_some_and(|v| v.eq_ignore_ascii_case("true")),
                );
            }
        }
        let enabled = matches!(
            kind,
            params::ActKind::Check | params::ActKind::Uncheck | params::ActKind::Clear
        );
        self.actionable(tab, &aim, enabled)?;
        let status = format!("ok {} {what}", kind.as_str());
        self.act_on(
            tab,
            aim,
            status,
            &options,
            view,
            move |cx, aim| match kind {
                params::ActKind::Hover => input::hover_element(cx, aim.node),
                params::ActKind::Check | params::ActKind::Uncheck => {
                    let want = kind == params::ActKind::Check;
                    match aria_checked {
                        // An ARIA checkbox flips when clicked.
                        Some(now) if now == want => Ok(()),
                        Some(_) => input::click_element(cx, aim.node).map(drop),
                        None => input::set_checked(cx, aim.node, want),
                    }
                }
                params::ActKind::Focus => input::focus(cx, aim.node),
                params::ActKind::Clear => input::fill(cx, aim.node, ""),
                params::ActKind::Scroll => {
                    agent::scroll_into_view(cx, aim.node);
                    Ok(())
                }
                // Uploads and drags went their own ways above.
                params::ActKind::Upload | params::ActKind::Drag => Ok(()),
            },
        )
    }

    /// Scrolls the window, or with a target the content of that element
    /// (or of the nearest element around it that scrolls), by `dy` (90% of
    /// the viewport by default); says how far it really moved.
    fn scroll(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let (_, state) = self.root_state(tab)?;
        let dy =
            p.dy.unwrap_or_else(|| f64::from(agent::viewport(&state).1) * 0.9) as f32;
        let moved = Rc::new(Cell::new(0.0_f32));
        let out = moved.clone();
        let (what, mut result) = match p.target.as_deref() {
            Some(target) => {
                let aim = self.aim(tab, target)?;
                let what = format!(" {}", self.aimed(tab, &aim));
                let result = self.act_on(
                    tab,
                    aim,
                    "ok scroll".to_string(),
                    &options,
                    view,
                    move |cx, aim| {
                        out.set(agent::scroll_within(cx, aim.node, dy));
                        Ok(())
                    },
                )?;
                (what, result)
            }
            None => {
                let result =
                    self.act_on_focus(tab, "ok scroll".to_string(), &options, view, move |cx| {
                        let before = agent::window_scroll(cx.page).1;
                        agent::scroll_by(cx, 0.0, dy);
                        out.set(agent::window_scroll(cx.page).1 - before);
                        Ok(())
                    })?;
                (String::new(), result)
            }
        };
        let moved = moved.get().round();
        let mut status = format!("ok scroll{what} {moved:.0}px");
        if (moved - dy.round()).abs() >= 1.0 {
            let edge = if dy > 0.0 { "the end" } else { "the start" };
            let _ = write!(status, " (asked {:.0}px; at {edge})", dy.round());
        }
        if let Some(rest) = result.text.strip_prefix("ok scroll") {
            result.text = format!("{status}{rest}");
        }
        Ok(result)
    }

    /// Drags an element onto another, both in the same frame.
    fn drag(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let target = p
            .target
            .as_deref()
            .ok_or_else(|| Failure::bad_argument("drag needs target: what to drag"))?;
        let to =
            p.to.as_deref()
                .ok_or_else(|| Failure::bad_argument("drag needs to: where to drop it"))?;
        let aim = self.aim(tab, target)?;
        let onto = self.aim(tab, to)?;
        if onto.frame != aim.frame {
            return Err(Failure::new(
                ErrorCode::Unsupported,
                "dragging from one frame into another",
            ));
        }
        let status = format!(
            "ok drag {} → {}",
            self.aimed(tab, &aim),
            self.aimed(tab, &onto)
        );
        let onto = onto.node;
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::drag_element(cx, aim.node, onto)
        })
    }

    /// Chooses local files in a file input. The input may be hidden (sites
    /// often hide it behind a styled button): only a disabled one refuses.
    fn upload(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let target = p
            .target
            .as_deref()
            .ok_or_else(|| Failure::bad_argument("upload needs target: the file input"))?;
        let paths = p
            .files
            .filter(|f| !f.is_empty())
            .ok_or_else(|| Failure::bad_argument("upload needs files: paths of local files"))?;
        let names = crate::files::describe(self.files_root.as_deref(), &paths)?;
        let files = crate::files::read(self.files_root.as_deref(), &paths)?;
        let aim = self.aim(tab, target)?;
        let what = self.aimed(tab, &aim);
        let one_only = self
            .page
            .frame_state(aim.frame)
            .is_some_and(|state| state.dom.borrow().attr(aim.node, "multiple").is_none());
        if files.len() > 1 && one_only {
            return Err(Failure::bad_argument(format!(
                "{what} takes one file (it has no multiple attribute)"
            )));
        }
        let status = format!("ok upload {what} ← {names}");
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::choose_files(cx, aim.node, files)
        })
    }

    pub(crate) fn screenshot(&mut self, tab: u32, p: params::Screenshot) -> CallResult {
        let (root, state) = self.root_state(tab)?;
        let png = self
            .page
            .screenshot_of(root, p.full_page)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let (w, h) = png_size(&png).unwrap_or(agent::viewport(&state));
        let what = if p.full_page { "full page" } else { "viewport" };
        Ok(ToolOutput {
            text: format!("ok screenshot t{tab} {what} {w}x{h}"),
            image: Some(png),
            ..ToolOutput::default()
        })
    }

    pub(crate) fn evaluate(
        &mut self,
        tab: u32,
        p: params::Evaluate,
        limits: &LoopLimits,
    ) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let aim = match &p.target {
            Some(target) => Some(self.aim(tab, target)?),
            None => None,
        };
        let frame = aim.map(|a| a.frame).unwrap_or(root);
        // Refs the script names, for `$ref("e12")`.
        let mut table = Vec::new();
        {
            let page = &self.page;
            let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
            entry.sync(page);
            for word in p
                .script
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| w.starts_with('e'))
            {
                if table.iter().any(|(name, _): &(String, Value)| name == word) {
                    continue;
                }
                if let Ok(key) = entry.refs.lookup(word, |key| is_live(page, key))
                    && FrameId(key.frame) == frame
                {
                    table.push((word.to_string(), Value::Node(key.node)));
                }
            }
        }
        let el = aim.map(|a| Value::Node(a.node)).unwrap_or(Value::Undefined);
        let prelude = "const $ref = (r) => __catpaw_refs[String(r).replace(/^\\[?(ref=)?/, \"\").replace(/\\]$/, \"\")] ?? null;\n";
        // `document.title;` is an expression too.
        let expression = p.script.trim().trim_end_matches(';');
        let as_expression = format!(
            "{prelude}const __catpaw_value = (\n{expression}\n);\nreturn typeof __catpaw_value === \"function\" ? __catpaw_value(el) : __catpaw_value;"
        );
        let as_body = format!("{prelude}{}", p.script);
        let base = self.baseline(tab);
        let params = ["el", "__catpaw_refs"];
        let args = vec![el, Value::Record(table)];
        // An expression is run as one (its value is the result), anything
        // else as a function body; which, is settled before anything runs,
        // so that the script runs once.
        let source = if self.page.compiles_in(frame, &params, &as_expression) {
            &as_expression
        } else {
            &as_body
        };
        let result = self.page.call_in(frame, &params, source, args, limits);
        let _ = self.page.follow_navigations();
        let report = self.finish(tab, &base);
        match result {
            Ok(value) => {
                let mut text = format!("ok evaluate{}", report.suffix());
                for line in report.lines.iter().filter(|l| !l.starts_with("  ")) {
                    if line.starts_with(&format!("! {}", consequence::NOT_SETTLED)) {
                        continue;
                    }
                    text.push('\n');
                    text.push_str(line);
                }
                text.push('\n');
                if value.len() > EVAL_CHARS {
                    let cut = floor_char_boundary(&value, EVAL_CHARS);
                    let _ = write!(
                        text,
                        "{}\n[truncated at {cut} of {} chars]",
                        &value[..cut],
                        value.len()
                    );
                } else {
                    text.push_str(&value);
                }
                Ok(ToolOutput::ok(text))
            }
            Err(e) => {
                let mut failure = Failure::new(
                    ErrorCode::ScriptError,
                    truncate(e.lines().next().unwrap_or(""), 300),
                );
                for line in report.lines {
                    failure = failure.with(line);
                }
                Err(failure)
            }
        }
    }
}
