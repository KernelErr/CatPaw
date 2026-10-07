//! Actions on a tab, and what they led to.

use std::fmt::Write as _;
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
use crate::output::{CallResult, Failure, ToolOutput};

/// What a tab's root document looked like before an action.
pub(super) struct Baseline {
    pub epoch: u64,
    pub console: usize,
    pub dialogs: usize,
    pub url: Option<Url>,
    pub requests: usize,
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
    /// What the policy holds for the user's approval.
    pub held: Option<String>,
    /// A navigation the policy refused: where to, and why.
    pub blocked: Option<(Url, String)>,
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
        match state {
            Some(state) => Baseline {
                epoch: state.epoch,
                console: state.console_len(),
                dialogs: state.dialogs.borrow().len(),
                url: Some(state.url.borrow().clone()),
                requests,
            },
            None => Baseline {
                epoch: 0,
                console: 0,
                dialogs: 0,
                url: None,
                requests,
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
                PageEvent::NavigationHeld { method, url, .. } => {
                    events.push(format!("held {method} {url}"));
                    report.held = self.page.held().map(describe_held);
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
                    self.tabs.insert(id, Tab::new(id, frame, opener, epoch, 1));
                    events.push(format!("popup t{id} {url}"));
                    report.lines.push(format!(
                        "! {} t{id} {} (switch with tabs)",
                        consequence::POPUP,
                        truncate(url.as_str(), 120)
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
        // Requests the action made (script's own, not analytics).
        let policy = SettlePolicy::default();
        let page_url = state.as_ref().map(|s| s.url.borrow().clone());
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
        let requests = self.page.held_requests();
        if !requests.is_empty() {
            let mut sent: Vec<String> = requests
                .iter()
                .take(3)
                .map(|(method, url)| format!("{method} {}", truncate(url.as_str(), 120)))
                .collect();
            if requests.len() > 3 {
                sent.push(format!("+{} more", requests.len() - 3));
            }
            let what = format!("send → {}", sent.join(", "));
            report.held = Some(match report.held.take() {
                Some(navigation) => format!("{navigation}, and {what}"),
                None => what,
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
        let mut text = status;
        text.push_str(&report.suffix());
        for line in &report.lines {
            text.push('\n');
            text.push_str(line);
        }
        let page = self.page_view(tab, mode, view)?;
        if !page.is_empty() {
            text.push('\n');
            text.push_str(&page);
        }
        Ok(ToolOutput {
            held,
            ..ToolOutput::ok(text)
        })
    }

    /// Lets what the policy held go (a navigation, requests), as though
    /// the action had not been stopped, and reports like that action.
    pub(crate) fn release_held(&mut self, tab: u32, status: String, view: View) -> CallResult {
        let base = self.baseline(tab);
        let navigation = self.page.held().is_some();
        let requests = !self.page.held_requests().is_empty();
        if !navigation && !requests {
            return Err(Failure::new(
                ErrorCode::BadArgument,
                "nothing is held any more: the page moved on",
            ));
        }
        self.page.release_held_requests();
        let result = self.page.release_held();
        // Let the page take in what came back.
        self.page.settle(&super::action_limits());
        let report = self.finish(tab, &base);
        if let Err(e) = result {
            return Err(Failure::new(ErrorCode::NavigationFailed, e.to_string()));
        }
        self.page_result(tab, status, report, None, view)
    }

    /// Drops what the policy held: the navigation does not happen, and the
    /// requests fail for the page.
    pub(crate) fn drop_held(&mut self) {
        self.page.drop_held();
        self.page.drop_held_requests();
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
        let report = self.finish(tab, &base);
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
        for line in report.lines {
            failure = failure.with(line);
        }
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
                    let url = self.page.url_of(root).expect("the tab's frame is open");
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
        let status = format!("ok click {}", self.aimed(tab, &aim));
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            match aim.point {
                Some((x, y)) => {
                    input::click_at(cx, x, y);
                    Ok(())
                }
                // Forced: the element gets the click, whatever covers it.
                None if force => match input::click_element(cx, aim.node) {
                    Err(InputError::Occluded { .. } | InputError::NotVisible) => {
                        catpaw_web::activation::click(cx, aim.node, true);
                        Ok(())
                    }
                    other => other.map(drop),
                },
                None => input::click_element(cx, aim.node).map(drop),
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

    pub(crate) fn press(&mut self, tab: u32, p: params::Press, view: View) -> CallResult {
        let options = p.options();
        let key = crate::target::normalize_key(&p.key);
        if key.is_empty() {
            return Err(Failure::bad_argument("key is empty"));
        }
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
        let all: Vec<(catpaw_dom::NodeId, String, String)> = agent::options_of(&state, aim.node)
            .into_iter()
            .map(|o| {
                let (label, value) = agent::option_label_and_value(&state, o);
                (o, label, value)
            })
            .collect();
        let mut chosen = Vec::new();
        for wanted in p.option.to_vec() {
            let w = wanted.split_whitespace().collect::<Vec<_>>().join(" ");
            let found = all
                .iter()
                .find(|(_, label, _)| *label == w)
                .or_else(|| all.iter().find(|(_, _, value)| *value == wanted))
                .or_else(|| {
                    all.iter()
                        .find(|(_, label, _)| label.to_lowercase() == w.to_lowercase())
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
        if kind == params::ActKind::Scroll && p.target.is_none() {
            let (_, state) = self.root_state(tab)?;
            let dy =
                p.dy.unwrap_or_else(|| f64::from(agent::viewport(&state).1) * 0.9) as f32;
            let status = format!("ok scroll {dy:.0}px");
            return self.act_on_focus(tab, status, &options, view, move |cx| {
                agent::scroll_by(cx, 0.0, dy);
                Ok(())
            });
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
                params::ActKind::Upload => unreachable!("uploads are handled above"),
            },
        )
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
        let args = vec![el.clone(), Value::Record(table.clone())];
        let mut result = self
            .page
            .call_in(frame, &params, &as_expression, args, limits);
        if let Err(e) = &result
            && e.starts_with("SyntaxError")
        {
            result = self.page.call_in(
                frame,
                &params,
                &as_body,
                vec![el, Value::Record(table)],
                limits,
            );
        }
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
