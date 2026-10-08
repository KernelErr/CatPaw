//! Hand-off: the `handoff` tool, waiting for the user to give the tab
//! back, and keeping the agent off a tab the user has (ADR 0006,
//! decision 15).

use std::time::Duration;

use super::*;

/// How long one `wait({for: "handoff"})` waits by default: under the
/// time hosts give a tool call; the agent waits again if the user needs
/// longer.
const WAIT: Duration = Duration::from_secs(50);
/// The longest one wait may be asked to take.
const WAIT_MAX: Duration = Duration::from_secs(1800);

/// A reason as the end of "ask the user to open … and …": its first
/// letter lowered, unless it is an acronym.
fn reason_phrase(reason: &str) -> String {
    let mut chars = reason.chars();
    match (chars.next(), chars.next()) {
        (Some(first), Some(second)) if first.is_uppercase() && !second.is_uppercase() => {
            first.to_lowercase().chain(reason.chars().skip(1)).collect()
        }
        _ => reason.to_string(),
    }
}

/// Whether a call works on the current tab's page (and so must wait while
/// the user has the tab).
fn uses_page(call: &Call) -> bool {
    match call {
        Call::Navigate(_)
        | Call::Snapshot(_)
        | Call::Click(_)
        | Call::Type(_)
        | Call::Press(_)
        | Call::Select(_)
        | Call::Act(_)
        | Call::Read(_)
        | Call::Screenshot(_)
        | Call::Evaluate(_)
        | Call::Logs(_) => true,
        Call::Wait(p) => p.until != params::WaitFor::Handoff,
        Call::Tabs(_) | Call::Handoff(_) | Call::Session(_) => false,
    }
}

impl Session {
    /// `handoff`: the user takes over the current tab on a page of their
    /// own (in place of a hand-off the tab had).
    pub(super) fn handoff(&mut self, p: params::Handoff) -> CallResult {
        let tab = self.current_tab()?;
        let group = self
            .routes
            .get(&tab)
            .and_then(|g| self.groups.get(g))
            .and_then(|g| g.caller())
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let reason = p
            .reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "Take over this tab".to_string());
        let unavailable = |e: std::io::Error| {
            Failure::new(
                ErrorCode::Unsupported,
                format!("the hand-off page could not start: {e}"),
            )
        };
        let (id, path) = self
            .handoffs
            .start(tab, &reason, group)
            .map_err(unavailable)?;
        let url = self.local_url(&path).map_err(unavailable)?;
        self.journal(
            "handoff",
            json!({"id": format!("h{id}"), "tab": format!("t{tab}"), "reason": reason}),
        );
        Ok(ToolOutput::ok(format!(
            "ok handoff h{id} t{tab}: ask the user to open {url} and {}; then wait({{\"for\":\"handoff\"}})",
            reason_phrase(&reason)
        )))
    }

    /// `wait({for: "handoff"})`: until the user gives the tab back, the
    /// tab goes, or the wait runs out (the agent then waits again).
    pub(super) fn wait_handoff(&mut self, p: params::Wait, host: &mut dyn Host) -> CallResult {
        let id = self
            .handoffs
            .open(self.current)
            .ok_or_else(|| Failure::bad_argument("no hand-off is open: call handoff first"))?;
        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(WAIT)
            .min(WAIT_MAX);
        let started = Instant::now();
        let mut checked = Instant::now();
        let tab = loop {
            match self.handoffs.state(id) {
                Some((tab, true)) => break tab,
                Some((tab, false)) => {
                    // A tab the page closed (a popup) takes its hand-off.
                    if checked.elapsed() >= Duration::from_secs(1) {
                        checked = Instant::now();
                        if !self.tab_alive(tab) {
                            self.handoffs.close(id);
                            return Err(Failure::new(
                                ErrorCode::NoTab,
                                format!("t{tab} closed during hand-off h{id}"),
                            ));
                        }
                    }
                }
                None => return Err(Failure::bad_argument(format!("h{id} is over"))),
            }
            if started.elapsed() >= timeout {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!("h{id} is not given back yet"),
                )
                .with(advice::HANDOFF));
            }
            if host.pause(Duration::from_millis(200)) {
                return Err(Failure::new(
                    ErrorCode::Timeout,
                    format!("cancelled while waiting for h{id}"),
                ));
            }
        };
        self.handoffs.close(id);
        self.journal("handoff-done", json!({"id": format!("h{id}")}));
        if self.routes.contains_key(&tab) {
            self.current = Some(tab);
        }
        let status = format!("ok wait handoff h{id}: given back");
        self.on_tab(tab, move |g, tab, view| g.after_handoff(tab, status, view))
    }

    /// Refuses a call on a tab the user has: the agent neither acts on it
    /// nor reads it until it is given back.
    pub(super) fn refuse_handed_over(&self, call: &Call) -> Result<(), Failure> {
        let busy = |tab: u32| self.handoffs.with_user(tab).map(|h| (tab, h));
        let held = match call {
            Call::Tabs(p) if p.op == params::TabsOp::Close => p
                .tab
                .as_deref()
                .and_then(|t| parse_tab(t).ok())
                .or(self.current)
                .and_then(busy),
            call if uses_page(call) => self.current.and_then(busy),
            _ => None,
        };
        match held {
            Some((tab, h)) => Err(Failure::new(
                ErrorCode::Busy,
                format!("t{tab} is with the user (hand-off h{h})"),
            )
            .with(advice::HANDED_OVER)),
            None => Ok(()),
        }
    }

    /// Whether a tab is still open in its group.
    fn tab_alive(&self, tab: u32) -> bool {
        self.routes
            .get(&tab)
            .and_then(|g| self.groups.get(g))
            .and_then(|g| g.call(move |g| g.tab_ids().contains(&tab)).ok())
            .unwrap_or(false)
    }
}
