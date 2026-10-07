//! Wording what kept a page busy: the `pending=` of a header and the
//! `! not-settled` lines after an action.

use std::fmt::Write as _;

use catpaw_agent::snapshot::truncate;
use catpaw_engine::{
    FrameId, Initiator, PendingReport, PendingRequest, RequestClass, StopReason, TimerClass,
};
use catpaw_protocol::wording::{advice, consequence};
use url::Url;

use super::GroupState;

/// Requests, timers and animation that hold the page up, in a few words:
/// `requests:2,timers:1,frames`.
pub(super) fn summary(report: &PendingReport) -> Option<String> {
    let requests = report
        .requests
        .iter()
        .filter(|r| r.class == RequestClass::Relevant)
        .count();
    let timers = report
        .timers
        .iter()
        .filter(|t| t.class == TimerClass::Near)
        .count();
    let mut parts = Vec::new();
    if requests > 0 {
        parts.push(format!("requests:{requests}"));
    }
    if timers > 0 {
        parts.push(format!("timers:{timers}"));
    }
    if report.animating {
        parts.push("frames".to_string());
    }
    if report.navigation_pending {
        parts.push("navigation".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(","))
}

/// A URL as short as it can be told: the path when it is on the page's
/// origin, else host and path.
pub(super) fn short_url(url: &Url, page: Option<&Url>) -> String {
    let same_origin = page.is_some_and(|p| p.origin() == url.origin());
    let mut out = if same_origin {
        String::new()
    } else {
        url.host_str().unwrap_or("").to_string()
    };
    out.push_str(url.path());
    if let Some(query) = url.query() {
        out.push('?');
        out.push_str(query);
    }
    truncate(&out, 100)
}

fn kind_word(request: &PendingRequest) -> &'static str {
    use catpaw_web::net::RequestKind;
    match request.kind {
        RequestKind::Fetch => "fetch",
        RequestKind::Xhr => "xhr",
        RequestKind::Script => "script",
        RequestKind::Style => "stylesheet",
        RequestKind::Document => "document",
        RequestKind::Beacon => "beacon",
        RequestKind::Other => "request",
    }
}

fn initiator_word(initiator: Initiator) -> Option<&'static str> {
    match initiator {
        Initiator::Timer { .. } => Some("a timer"),
        Initiator::Input => Some("the action"),
        Initiator::Frame => Some("an animation frame"),
        _ => None,
    }
}

impl GroupState {
    /// Why the page did not settle, and what it was still doing, when it
    /// did not: lines for the end of an action's consequences.
    pub(super) fn not_settled(&self, root: FrameId) -> Vec<String> {
        if self.page.is_settled() {
            return Vec::new();
        }
        let stop = self.page.frame_report(root).map(|r| r.stop);
        let why = match stop {
            Some(StopReason::VirtualBudget) => "timers kept it busy for 5s of page time",
            Some(StopReason::WallBudget) => "still busy at the time limit",
            Some(StopReason::StepBudget) => "stopped after too many tasks",
            Some(StopReason::Navigation) => "a navigation is on its way",
            _ => "a frame or worker is still busy",
        };
        let mut lines = vec![format!("! {} {why}", consequence::NOT_SETTLED)];
        let page_url = self.page.url_of(root);
        if let Some(report) = self.page.pending_of(root) {
            // What it waits on; when nothing, what is open at all.
            let relevant = report
                .requests
                .iter()
                .any(|r| r.class == RequestClass::Relevant);
            for request in report
                .requests
                .iter()
                .filter(|r| !relevant || r.class == RequestClass::Relevant)
                .take(3)
            {
                let mut line = format!(
                    "  {} {} {} {} ({:.1}s",
                    request.class.as_str(),
                    kind_word(request),
                    request.method,
                    short_url(&request.url, page_url.as_ref()),
                    request.age.as_secs_f64()
                );
                if let Some(site) = &request.site {
                    let _ = write!(line, ", from {site}");
                } else if let Some(word) = initiator_word(request.initiator) {
                    let _ = write!(line, ", started by {word}");
                }
                line.push(')');
                lines.push(line);
            }
            for timer in report
                .timers
                .iter()
                .filter(|t| t.class == TimerClass::Near)
                .take(3)
            {
                let mut line = format!(
                    "  timer {} {}ms (due in {}ms",
                    if timer.repeat {
                        "setInterval"
                    } else {
                        "setTimeout"
                    },
                    timer.delay_ms.round(),
                    timer.due_in_ms.round()
                );
                if let Some(site) = &timer.site {
                    let _ = write!(line, ", from {site}");
                }
                if timer.arms > 1 {
                    let _ = write!(line, ", set {}x", timer.arms);
                }
                line.push(')');
                lines.push(line);
            }
            if report.animating {
                lines.push("  animating: frames keep changing the document".to_string());
            }
        }
        lines.push(format!("  {}", advice::WAIT));
        lines
    }
}
