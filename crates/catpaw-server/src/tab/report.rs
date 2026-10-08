//! What an action led to: the page's events taken in for the tab they
//! concern, and the report a result is made of (navigations, new and
//! closed tabs, downloads, dialogs, requests, console errors, holds, and
//! what kept the page busy).

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use catpaw_agent::snapshot::{quote, truncate};
use catpaw_engine::{ConsoleLevel, DialogAnswer, PageEvent, SettlePolicy};
use catpaw_protocol::params::SnapshotMode;
use catpaw_protocol::wording::{ErrorCode, consequence, outcome};
use url::Url;

use super::holds::describe_held;
use super::pending::{outcome_word, short_url};
use super::{Absorbed, CONSOLE_LINES, GroupState, INBOX, Tab, View};
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

/// Whether a console message is about a request to a host the settle
/// policy ignores (analytics): noise to the agent.
fn about_ignored_host(text: &str, policy: &SettlePolicy) -> bool {
    text.split_whitespace()
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .filter_map(|w| Url::parse(w.trim_end_matches([':', ',', ')', '.', '…'])).ok())
        .any(|url| policy.ignores_host(&url))
}

/// A failure with the consequences of the action that failed; one that
/// only repeats a navigation failure is left out.
pub(super) fn with_consequences(mut failure: Failure, lines: Vec<String>) -> Failure {
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

    /// Takes in what the page did since it was last asked: popups become
    /// tabs and closed ones go at once; the events wait in the inbox for the
    /// next result of the tab they concern.
    pub(super) fn absorb_events(&mut self) {
        for event in self.page.take_events() {
            let (tab, which) = match &event {
                PageEvent::PopupOpened { frame, opener, .. } => {
                    let id = self.next_tab.fetch_add(1, Ordering::SeqCst);
                    let opener = self.tab_of_frame(*opener);
                    let epoch = self.page.document_epoch(*frame).unwrap_or(0);
                    self.tabs.insert(id, Tab::new(id, *frame, opener, epoch));
                    (opener, Some(id))
                }
                PageEvent::PopupClosed { frame } => {
                    let closed = self.tabs.values().find(|t| t.root == *frame).map(|t| t.id);
                    if let Some(id) = closed {
                        self.tabs.remove(&id);
                    }
                    (None, closed)
                }
                PageEvent::Navigated { frame, .. }
                | PageEvent::NavigationFailed { frame, .. }
                | PageEvent::NavigationHeld { frame, .. }
                | PageEvent::NavigationBlocked { frame, .. } => (self.tab_of_frame(*frame), None),
                PageEvent::HoldDropped { .. }
                | PageEvent::RequestBlocked { .. }
                | PageEvent::Download { .. } => (None, None),
            };
            self.inbox.push(Absorbed { tab, which, event });
        }
        if self.inbox.len() > INBOX {
            self.inbox.drain(..self.inbox.len() - INBOX);
        }
    }

    /// The events of the inbox for a result of `tab`: its own, those that
    /// name no tab, and those of tabs that are gone.
    fn take_inbox(&mut self, tab: u32) -> Vec<Absorbed> {
        let (mine, others): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.inbox).into_iter().partition(|a| {
                a.tab
                    .is_none_or(|t| t == tab || !self.tabs.contains_key(&t))
            });
        self.inbox = others;
        mine
    }

    /// Collects what happened since `base`: navigations of the tab, tabs
    /// opened and closed, dialogs, requests, console errors, and what kept
    /// the page busy.
    pub(super) fn finish(&mut self, tab: u32, base: &Baseline) -> Report {
        self.absorb_events();
        let mut report = Report::default();
        let root = self.tabs.get(&tab).map(|t| t.root);
        let mut events = Vec::new();
        let mut held_ids = Vec::new();
        let mut held_what = Vec::new();
        let typed = self.user_values(tab);
        for Absorbed { which, event, .. } in self.take_inbox(tab) {
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
                        held_what.push(describe_held(held, &typed));
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
                PageEvent::NavigationBlocked { frame, url, reason } if Some(frame) == root => {
                    events.push(format!("blocked {url}: {reason}"));
                    report.blocked = Some((url, reason));
                }
                // A frame the policy kept from loading: the page goes on.
                PageEvent::NavigationBlocked { url, reason, .. } => {
                    events.push(format!("blocked frame {url}: {reason}"));
                    report.lines.push(format!(
                        "! {} frame {} ({reason})",
                        consequence::BLOCKED,
                        truncate(url.as_str(), 160)
                    ));
                }
                PageEvent::NavigationFailed { frame, url, error } if Some(frame) == root => {
                    events.push(format!("navigation failed {url}: {error}"));
                    report.lines.push(format!(
                        "! {} {url}: {error}",
                        consequence::NAVIGATION_FAILED
                    ));
                }
                PageEvent::PopupOpened { url, .. } => {
                    let Some(id) = which else { continue };
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
                PageEvent::PopupClosed { .. } => {
                    let Some(id) = which else { continue };
                    events.push(format!("tab closed t{id}"));
                    report
                        .lines
                        .push(format!("! {} t{id}", consequence::TAB_CLOSED));
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
                format!(
                    "{} {} {}",
                    r.method,
                    short_url(&r.url, page_url.as_ref()),
                    outcome_word(&r, false)
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
        // The requests the action left held in the tab (older ones belong to
        // earlier confirmations).
        let requests: Vec<_> = self
            .held_requests_of(tab)
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
        // In a fixed order, each kind in the order it happened.
        report.lines.sort_by_key(|line| {
            let word = line
                .strip_prefix("! ")
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or("");
            consequence::ORDER
                .iter()
                .position(|w| *w == word)
                .unwrap_or(consequence::ORDER.len())
        });
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
}
