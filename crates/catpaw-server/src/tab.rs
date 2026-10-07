//! A browsing-context group as the agent sees it: its tabs (the top page
//! and the popups it opened), their refs and snapshot counters, and the
//! actions on them. Everything here runs on the group's own thread.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use catpaw_agent::snapshot::{quote, truncate};
use catpaw_agent::{
    ExtraAttrs, Filter, Format, Header, LinkStyle, ReadOptions, RefError, RefKey, RefScope,
    RefTable, SnapshotOptions, Snapshotter,
};
use catpaw_dom::NodeId;
use catpaw_engine::{
    ActionError, ConsoleLevel, EngineError, FrameId, InputError, LoopLimits, Page, PageEvent,
    PageOptions, SharedNet, Value,
};
use catpaw_protocol::params;
use catpaw_protocol::wording::{ErrorCode, advice, consequence};
use catpaw_web::page::Cx;
use catpaw_web::{PageState, agent, input};
use url::Url;

use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure, ToolOutput};
use crate::target::{self, Target};

/// The budget of a snapshot when the call gives none, in tokens.
pub(crate) const SNAPSHOT_TOKENS: u32 = 4000;
/// The budget of a read view when the call gives none, in tokens.
const READ_TOKENS: u32 = 6000;
/// The most a script result may take, in characters.
const EVAL_CHARS: usize = 4000;
/// Console errors listed after an action; the rest are counted.
const CONSOLE_LINES: usize = 3;

/// Bytes in `tokens` by the fixed estimate (3.5 bytes a token): the same
/// input always gets the same budget.
fn token_bytes(tokens: u32) -> usize {
    tokens as usize * 7 / 2
}

/// Session-wide display choices.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    pub format: Format,
}

/// What a snapshot should show.
#[derive(Debug, Clone)]
pub(crate) struct SnapRequest {
    pub filter: Filter,
    pub root: Option<String>,
    pub max_tokens: u32,
    pub extra: ExtraAttrs,
}

impl Default for SnapRequest {
    fn default() -> Self {
        Self {
            filter: Filter::Interesting,
            root: None,
            max_tokens: SNAPSHOT_TOKENS,
            extra: ExtraAttrs::default(),
        }
    }
}

/// A tab in the tab list.
#[derive(Debug, Clone)]
pub(crate) struct TabSummary {
    pub id: u32,
    pub url: String,
    pub title: String,
    pub opener: Option<u32>,
}

/// A tab: a top-level frame of the group (the top page or a popup).
struct Tab {
    id: u32,
    root: FrameId,
    opener: Option<u32>,
    refs: RefTable,
    next_snapshot: u64,
    /// The number of the document shown (`dN`), and its epoch.
    doc: u64,
    doc_epoch: u64,
}

impl Tab {
    fn new(id: u32, root: FrameId, opener: Option<u32>, epoch: u64, doc: u64) -> Self {
        Self {
            id,
            root,
            opener,
            refs: RefTable::new(),
            next_snapshot: 1,
            doc,
            doc_epoch: epoch,
        }
    }

    /// Notices a new document in the tab: its refs into the old one go
    /// stale, and the document number moves on.
    fn sync(&mut self, page: &Page) {
        let Some(epoch) = page.document_epoch(self.root) else {
            return;
        };
        if epoch != self.doc_epoch {
            self.refs.document_replaced(self.root.0, epoch);
            self.doc += 1;
            self.doc_epoch = epoch;
        }
    }
}

/// A node an action is aimed at.
#[derive(Debug, Clone, Copy)]
struct Aim {
    frame: FrameId,
    node: NodeId,
    r: u32,
    /// Where to click, for `xy:` targets.
    point: Option<(f32, f32)>,
}

/// What a tab's root document looked like before an action.
struct Baseline {
    epoch: u64,
    console: usize,
    dialogs: usize,
    url: Option<Url>,
}

/// What an action led to.
#[derive(Default)]
struct Report {
    /// The last document the tab's root loaded: method, URL, status.
    navigated: Option<(String, Url, u16)>,
    /// The URL the document moved to without loading another
    /// (`pushState`, a fragment).
    same_document: Option<Url>,
    lines: Vec<String>,
}

/// The tabs of one browsing-context group and the page they live in.
pub(crate) struct GroupState {
    page: Page,
    tabs: BTreeMap<u32, Tab>,
    next_tab: Arc<AtomicU32>,
}

/// Whether a ref's node is still in the document it was shown in.
fn is_live(page: &Page, key: &RefKey) -> bool {
    let frame = FrameId(key.frame);
    if page.document_epoch(frame) != Some(key.epoch) {
        return false;
    }
    let Some(state) = page.frame_state(frame) else {
        return false;
    };
    let dom = state.dom.borrow();
    dom.contains(key.node) && dom.is_connected(key.node)
}

fn describe(refs: &RefTable, r: u32) -> String {
    match refs.entry(r) {
        Some(entry) if entry.name.is_empty() => format!("e{r} {}", entry.role),
        Some(entry) => format!("e{r} {} {}", entry.role, quote(&entry.name)),
        None => format!("e{r}"),
    }
}

/// What an address bar makes of text: a URL, or a host to which
/// `https://` (`http://` for local hosts) is added.
fn parse_url(text: &str) -> Result<Url, Failure> {
    let text = text.trim();
    if let Ok(url) = Url::parse(text) {
        match url.scheme() {
            "http" | "https" | "about" | "data" => return Ok(url),
            "file" | "javascript" | "blob" | "ftp" | "ws" | "wss" => {
                return Err(Failure::bad_argument(format!(
                    "{} URLs do not open here; use http or https",
                    url.scheme()
                )));
            }
            // `localhost:8080` parses with a scheme of `localhost`.
            _ => {}
        }
    }
    let local =
        text.starts_with("localhost") || text.starts_with("127.") || text.starts_with("[::1]");
    let scheme = if local { "http" } else { "https" };
    Url::parse(&format!("{scheme}://{text}"))
        .ok()
        .filter(|u| u.has_host())
        .ok_or_else(|| Failure::bad_argument(format!("{text:?} is not a URL")))
}

impl GroupState {
    pub(crate) fn new(
        options: &PageOptions,
        net: &SharedNet,
        first: u32,
        next_tab: Arc<AtomicU32>,
    ) -> Result<Self, EngineError> {
        let page = Page::blank(options, net)?;
        let epoch = page.document_epoch(FrameId(0)).unwrap_or(0);
        let mut tabs = BTreeMap::new();
        tabs.insert(first, Tab::new(first, FrameId(0), None, epoch, 0));
        Ok(Self {
            page,
            tabs,
            next_tab,
        })
    }

    pub(crate) fn tab_ids(&self) -> Vec<u32> {
        self.tabs.keys().copied().collect()
    }

    pub(crate) fn summaries(&self) -> Vec<TabSummary> {
        self.tabs
            .values()
            .map(|tab| {
                let state = self.page.frame_state(tab.root);
                TabSummary {
                    id: tab.id,
                    url: self
                        .page
                        .url_of(tab.root)
                        .map(|u| u.to_string())
                        .unwrap_or_default(),
                    title: state.map(|s| title_of(s)).unwrap_or_default(),
                    opener: tab.opener,
                }
            })
            .collect()
    }

    /// `localStorage` by origin, as the group's documents hold it now.
    pub(crate) fn storage_snapshot(
        &self,
    ) -> std::collections::HashMap<String, Vec<(String, String)>> {
        self.page.storage_snapshot()
    }

    /// The opener of a tab, if it is still open.
    pub(crate) fn opener_of(&self, tab: u32) -> Option<u32> {
        self.tabs.get(&tab).and_then(|t| t.opener)
    }

    /// Whether the tab is the group's top page (closing it closes the
    /// group).
    pub(crate) fn is_top(&self, tab: u32) -> bool {
        self.tabs.get(&tab).is_some_and(|t| t.root == FrameId(0))
    }

    fn tab_mut(&mut self, tab: u32) -> Result<&mut Tab, Failure> {
        self.tabs
            .get_mut(&tab)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))
    }

    fn root_state(&self, tab: u32) -> Result<(FrameId, Rc<PageState>), Failure> {
        let root = self
            .tabs
            .get(&tab)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?
            .root;
        let state = self
            .page
            .frame_state(root)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        Ok((root, state))
    }

    /// The tab that holds a frame: the frame's own, or its parent's.
    fn tab_of_frame(&self, frame: FrameId) -> Option<u32> {
        let frames = self.page.frames();
        let mut current = frame;
        for _ in 0..64 {
            if let Some(tab) = self.tabs.values().find(|t| t.root == current) {
                return Some(tab.id);
            }
            current = frames.iter().find(|f| f.id == current)?.parent?;
        }
        None
    }

    // ------------------------------------------------------------ snapshots

    /// The tab's snapshot as text: header line, then the lines.
    pub(crate) fn snapshot_text(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<String, Failure> {
        let (root, state) = self.root_state(tab)?;
        let settled = self.page.is_settled();
        let challenge = (root == FrameId(0) && self.page.document().cloudflare_challenge)
            .then(|| "cloudflare".to_string());
        let page = &self.page;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        entry.sync(page);
        let epoch = entry.doc_epoch;
        let root_node = match &request.root {
            Some(text) => Some(resolve_ref(page, &mut entry.refs, text, root)?),
            None => None,
        };
        let url = state.url.borrow().to_string();
        let (vw, vh) = agent::viewport(&state);
        let (sx, sy) = agent::window_scroll(&state);
        let text = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let options = SnapshotOptions {
                filter: request.filter,
                format: view.format,
                root: root_node.map(|(node, _)| node),
                max_depth: None,
                max_chars: Some(token_bytes(request.max_tokens)),
                extra: request.extra,
                ..SnapshotOptions::default()
            };
            let mut snapshotter =
                Snapshotter::new(dom, &oracle, &mut entry.refs).in_frame(root.0, epoch);
            let body = snapshotter.body(&options);
            let title = snapshotter.title();
            let focus = agent::focused(&state).and_then(|node| {
                entry.refs.get(RefKey {
                    frame: root.0,
                    epoch,
                    node,
                })
            });
            let id = entry.next_snapshot;
            entry.next_snapshot += 1;
            let header = Header {
                id,
                tab: Some(format!("t{}", entry.id)),
                doc: Some(entry.doc),
                url: Some(url),
                title: Some(title),
                viewport: Some((vw, vh)),
                scroll: Some((sx.round() as i64, sy.round() as i64)),
                focus,
                filter: Some(request.filter),
                root: root_node.map(|(_, r)| r),
                nodes: Some((body.emitted, body.total_elements)),
                settled: Some(settled),
                challenge,
                budget_hit: body.truncated_nodes > 0,
                ..Header::default()
            };
            let mut text = header.render();
            text.push('\n');
            text.push_str(&body.text);
            if body.truncated_nodes > 0 {
                text.push_str(advice::SNAPSHOT_TRUNCATED);
                text.push('\n');
            }
            text
        });
        Ok(text.trim_end().to_string())
    }

    pub(crate) fn snapshot(&mut self, tab: u32, p: params::Snapshot, view: View) -> CallResult {
        let mut extra = ExtraAttrs::default();
        for attr in p.attrs.unwrap_or_default() {
            match attr {
                params::Attr::Href => extra.href = true,
                params::Attr::Src => extra.src = true,
                params::Attr::Description => extra.description = true,
            }
        }
        let request = SnapRequest {
            filter: match p.filter.unwrap_or(params::Filter::Interesting) {
                params::Filter::Interesting => Filter::Interesting,
                params::Filter::Interactive => Filter::Interactive,
                params::Filter::All => Filter::All,
            },
            root: p.root,
            max_tokens: p.max_tokens.unwrap_or(SNAPSHOT_TOKENS).max(200),
            extra,
        };
        Ok(ToolOutput::ok(self.snapshot_text(tab, &request, view)?))
    }

    // ------------------------------------------------------------- targets

    /// Resolves a target to a node of the tab, with a ref for it.
    fn aim(&mut self, tab: u32, text: &str) -> Result<Aim, Failure> {
        let target = target::parse(text)?;
        let (root, state) = self.root_state(tab)?;
        let page = &self.page;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        entry.sync(page);
        let epoch = entry.doc_epoch;
        match target {
            Target::Ref(text) => {
                let (node, r) = resolve_ref(page, &mut entry.refs, &text, root)?;
                Ok(Aim {
                    frame: root,
                    node,
                    r,
                    point: None,
                })
            }
            Target::Css(selector) => {
                let selectors = catpaw_style::Selectors::parse(&selector).ok_or_else(|| {
                    Failure::bad_argument(format!("{selector:?} is not a valid selector"))
                })?;
                let node = {
                    let dom = state.dom.borrow();
                    catpaw_style::query::query_first(&dom, dom.document(), &selectors)
                }
                .ok_or_else(|| {
                    Failure::new(
                        ErrorCode::NotFound,
                        format!("css:{selector} matches nothing"),
                    )
                })?;
                let r = assign(&state, &mut entry.refs, root.0, epoch, node);
                Ok(Aim {
                    frame: root,
                    node,
                    r,
                    point: None,
                })
            }
            Target::Point(x, y) => {
                let node = agent::element_at(&state, x, y).ok_or_else(|| {
                    Failure::new(ErrorCode::NotFound, format!("nothing is at {x},{y}"))
                })?;
                let r = assign(&state, &mut entry.refs, root.0, epoch, node);
                Ok(Aim {
                    frame: root,
                    node,
                    r,
                    point: Some((x, y)),
                })
            }
        }
    }

    fn describe(&self, tab: u32, r: u32) -> String {
        self.tabs
            .get(&tab)
            .map(|t| describe(&t.refs, r))
            .unwrap_or_else(|| format!("e{r}"))
    }

    // ------------------------------------------------------------- actions

    fn baseline(&self, tab: u32) -> Baseline {
        let state = self
            .tabs
            .get(&tab)
            .and_then(|t| self.page.frame_state(t.root));
        match state {
            Some(state) => Baseline {
                epoch: state.epoch,
                console: state.console_len(),
                dialogs: state.dialogs.borrow().len(),
                url: Some(state.url.borrow().clone()),
            },
            None => Baseline {
                epoch: 0,
                console: 0,
                dialogs: 0,
                url: None,
            },
        }
    }

    /// Collects what happened since `base`: navigations of the tab, tabs
    /// opened and closed, dialogs and console errors.
    fn finish(&mut self, tab: u32, base: &Baseline) -> Report {
        let mut report = Report::default();
        let root = self.tabs.get(&tab).map(|t| t.root);
        for event in self.page.take_events() {
            match event {
                PageEvent::Navigated {
                    frame,
                    method,
                    url,
                    status,
                } if Some(frame) == root => report.navigated = Some((method, url, status)),
                PageEvent::NavigationFailed { frame, url, error } if Some(frame) == root => {
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
                        report
                            .lines
                            .push(format!("! {} t{id}", consequence::TAB_CLOSED));
                    }
                }
                _ => {}
            }
        }
        let state = root.and_then(|root| self.page.frame_state(root));
        if let Some(state) = state {
            let fresh = state.epoch != base.epoch;
            if report.navigated.is_none() && !fresh {
                let now = state.url.borrow().clone();
                if base.url.as_ref() != Some(&now) {
                    report.same_document = Some(now);
                }
            }
            let dialogs = state.dialogs.borrow();
            for dialog in dialogs.iter().skip(if fresh { 0 } else { base.dialogs }) {
                report.lines.push(format!(
                    "! {} {} {} → dismissed",
                    consequence::DIALOG,
                    dialog.kind,
                    quote(&truncate(&dialog.message, 120))
                ));
            }
            let errors: Vec<String> = state
                .console_since(if fresh { 0 } else { base.console })
                .into_iter()
                .filter(|m| m.level == ConsoleLevel::Error)
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
        report
    }

    /// The text of a result that changed the page: the status line, what
    /// happened, and a fresh snapshot of the tab (when it is still open).
    fn page_result(&mut self, tab: u32, status: String, report: Report, view: View) -> CallResult {
        let mut text = status;
        if let Some((method, url, status)) = &report.navigated {
            let url = truncate(url.as_str(), 160);
            if method == "GET" {
                let _ = write!(text, " → {url} ({status})");
            } else {
                let _ = write!(text, " → {url} ({method}, {status})");
            }
        } else if let Some(url) = &report.same_document {
            let _ = write!(text, " → {} (same document)", truncate(url.as_str(), 160));
        }
        for line in &report.lines {
            text.push('\n');
            text.push_str(line);
        }
        if self.tabs.contains_key(&tab) {
            text.push('\n');
            text.push_str(&self.snapshot_text(tab, &SnapRequest::default(), view)?);
        }
        Ok(ToolOutput::ok(text))
    }

    /// Runs an input action on the aimed-at element's page, then reports.
    fn act_on<R>(
        &mut self,
        tab: u32,
        aim: Aim,
        status: String,
        view: View,
        action: impl FnOnce(&mut Cx<'_>, &Aim) -> Result<R, InputError>,
    ) -> CallResult {
        let base = self.baseline(tab);
        let result = self.page.input_in(aim.frame, |cx| action(cx, &aim));
        let report = self.finish(tab, &base);
        match result {
            Ok(_) => self.page_result(tab, status, report, view),
            Err(e) => Err(self.action_failure(tab, &aim, e)),
        }
    }

    /// Words an input error about the aimed-at element.
    fn action_failure(&mut self, tab: u32, aim: &Aim, error: ActionError) -> Failure {
        let what = self.describe(tab, aim.r);
        match error {
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
                Failure::new(ErrorCode::Occluded, format!("{what} is covered by {cover}"))
                    .with(advice::OCCLUDED)
            }
            ActionError::NoFrame(_) => {
                Failure::new(ErrorCode::StaleRef, format!("{what} (frame closed)"))
                    .with(advice::STALE)
            }
            other => Failure::new(ErrorCode::NavigationFailed, other.to_string()),
        }
    }

    /// The ref of a node of a frame of the tab, assigning one if needed.
    fn ref_for(&mut self, tab: u32, frame: FrameId, node: NodeId) -> Option<u32> {
        let state = self.page.frame_state(frame)?.clone();
        let epoch = state.epoch;
        let entry = self.tabs.get_mut(&tab)?;
        Some(assign(&state, &mut entry.refs, frame.0, epoch, node))
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
        self.page_result(tab, status, report, view)
    }

    pub(crate) fn click(&mut self, tab: u32, p: params::Click, view: View) -> CallResult {
        let aim = self.aim(tab, &p.target)?;
        let status = format!("ok click {}", self.describe(tab, aim.r));
        self.act_on(tab, aim, status, view, |cx, aim| match aim.point {
            Some((x, y)) => {
                input::click_at(cx, x, y);
                Ok(())
            }
            None => input::click_element(cx, aim.node).map(drop),
        })
    }

    pub(crate) fn type_text(&mut self, tab: u32, p: params::Type, view: View) -> CallResult {
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
                }
            }
        };
        let mut status = format!("ok type {}", self.describe(tab, aim.r));
        if p.submit {
            status.push_str(" + Enter");
        }
        let text = p.text;
        let (append, submit) = (p.append, p.submit);
        self.act_on(tab, aim, status, view, move |cx, aim| {
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
        })
    }

    pub(crate) fn press(&mut self, tab: u32, p: params::Press, view: View) -> CallResult {
        let key = target::normalize_key(&p.key);
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
                let status = format!("ok press {key}{times} on {}", self.describe(tab, aim.r));
                self.act_on(tab, aim, status, view, move |cx, aim| {
                    input::focus(cx, aim.node)?;
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                })
            }
            None => {
                let (root, _) = self.root_state(tab)?;
                let base = self.baseline(tab);
                let status = format!("ok press {key}{times}");
                let result = self.page.input_in(root, move |cx| {
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                });
                let report = self.finish(tab, &base);
                match result {
                    Ok(()) => self.page_result(tab, status, report, view),
                    Err(ActionError::Input(InputError::Detached)) => {
                        Err(Failure::bad_argument("nothing has focus")
                            .with(advice::NOTHING_FOCUSED))
                    }
                    Err(e) => Err(Failure::new(ErrorCode::NavigationFailed, e.to_string())),
                }
            }
        }
    }

    pub(crate) fn select(&mut self, tab: u32, p: params::Select, view: View) -> CallResult {
        let aim = self.aim(tab, &p.target)?;
        let state = self
            .page
            .frame_state(aim.frame)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let what = self.describe(tab, aim.r);
        if !state.dom.borrow().is_html_element(aim.node, "select") {
            return Err(Failure::new(
                ErrorCode::NotActionable,
                format!("{what} is not a <select>"),
            )
            .with(advice::NOT_A_SELECT));
        }
        let options: Vec<(NodeId, String, String)> = agent::options_of(&state, aim.node)
            .into_iter()
            .map(|o| {
                let (label, value) = agent::option_label_and_value(&state, o);
                (o, label, value)
            })
            .collect();
        let mut chosen = Vec::new();
        for wanted in p.option.to_vec() {
            let w = wanted.split_whitespace().collect::<Vec<_>>().join(" ");
            let found = options
                .iter()
                .find(|(_, label, _)| *label == w)
                .or_else(|| options.iter().find(|(_, _, value)| *value == wanted))
                .or_else(|| {
                    options
                        .iter()
                        .find(|(_, label, _)| label.to_lowercase() == w.to_lowercase())
                });
            match found {
                Some((node, label, _)) => chosen.push((*node, label.clone())),
                None => {
                    let listed: Vec<String> = options
                        .iter()
                        .take(30)
                        .map(|(_, label, _)| quote(&truncate(label, 60)))
                        .collect();
                    let mut message = format!("no option {} in {what}", quote(&wanted));
                    let _ = write!(message, "; options: {}", listed.join(", "));
                    if options.len() > 30 {
                        let _ = write!(message, " (+{} more)", options.len() - 30);
                    }
                    return Err(Failure::new(ErrorCode::NotFound, message));
                }
            }
        }
        let labels: Vec<String> = chosen.iter().map(|(_, l)| quote(l)).collect();
        let status = format!("ok select {what} ← {}", labels.join(", "));
        let nodes: Vec<NodeId> = chosen.iter().map(|(n, _)| *n).collect();
        self.act_on(tab, aim, status, view, move |cx, aim| {
            input::select_options(cx, aim.node, &nodes)
        })
    }

    pub(crate) fn act(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let kind = p.kind;
        if kind == params::ActKind::Scroll && p.target.is_none() {
            let (root, state) = self.root_state(tab)?;
            let dy =
                p.dy.unwrap_or_else(|| f64::from(agent::viewport(&state).1) * 0.9) as f32;
            let base = self.baseline(tab);
            let result = self.page.input_in(root, move |cx| {
                agent::scroll_by(cx, 0.0, dy);
                Ok(())
            });
            let report = self.finish(tab, &base);
            return match result {
                Ok(()) => self.page_result(tab, format!("ok scroll {dy:.0}px"), report, view),
                Err(e) => Err(Failure::new(ErrorCode::NavigationFailed, e.to_string())),
            };
        }
        let target = p
            .target
            .ok_or_else(|| Failure::bad_argument(format!("{} needs a target", kind.as_str())))?;
        let aim = self.aim(tab, &target)?;
        let what = self.describe(tab, aim.r);
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
        let status = format!("ok {} {what}", kind.as_str());
        self.act_on(tab, aim, status, view, move |cx, aim| match kind {
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
        })
    }

    // ---------------------------------------------------------------- reads

    pub(crate) fn read(&mut self, tab: u32, p: params::Read) -> CallResult {
        let (root, state) = self.root_state(tab)?;
        let page = &self.page;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        entry.sync(page);
        let epoch = entry.doc_epoch;
        let view = p.view;
        let main = p.main;
        let full = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let scope = RefScope::new(&mut entry.refs, root.0, epoch);
            match view {
                params::ReadView::Markdown => catpaw_agent::markdown(
                    dom,
                    &oracle,
                    Some(scope),
                    &ReadOptions {
                        link_style: LinkStyle::Ref,
                        main_only: main,
                    },
                ),
                params::ReadView::Text => catpaw_agent::text_with(
                    dom,
                    &oracle,
                    &ReadOptions {
                        main_only: main,
                        ..ReadOptions::default()
                    },
                ),
                params::ReadView::Links => {
                    let mut out = String::new();
                    for link in catpaw_agent::links(dom, &oracle, Some(scope)) {
                        let _ = writeln!(
                            out,
                            "{} {} {}",
                            link.r#ref.unwrap_or_default(),
                            quote(&truncate(&link.text, 100)),
                            link.href
                        );
                    }
                    out
                }
                params::ReadView::Forms => {
                    render_forms(&catpaw_agent::forms(dom, &oracle, Some(scope)))
                }
            }
        });
        let offset = p.offset.unwrap_or(0).min(full.len());
        let offset = floor_char_boundary(&full, offset);
        let budget = token_bytes(p.max_tokens.unwrap_or(READ_TOKENS).max(200));
        let rest = &full[offset..];
        let mut end = rest.len();
        if rest.len() > budget {
            end = floor_char_boundary(rest, budget);
            // End at a line break when one is near.
            if let Some(nl) = rest[..end].rfind('\n')
                && nl > end * 4 / 5
            {
                end = nl + 1;
            }
        }
        let mut text = format!("ok read {}", view.as_str());
        if offset > 0 || end < rest.len() {
            let _ = write!(
                text,
                " (chars {}-{} of {})",
                offset,
                offset + end,
                full.len()
            );
        }
        text.push('\n');
        let body = rest[..end].trim_end();
        if body.is_empty() {
            text.push_str("(nothing to read)");
        } else {
            text.push_str(body);
        }
        if end < rest.len() {
            let mut args = format!("{{\"view\":\"{}\"", view.as_str());
            if main {
                args.push_str(",\"main\":true");
            }
            if let Some(tokens) = p.max_tokens {
                let _ = write!(args, ",\"maxTokens\":{tokens}");
            }
            let _ = write!(args, ",\"offset\":{}}}", offset + end);
            let _ = write!(
                text,
                "\n[truncated at {} of {}; read({args}) {}]",
                offset + end,
                full.len(),
                advice::READ_CONTINUES
            );
        }
        Ok(ToolOutput::ok(text))
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
            is_error: false,
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
                let mut text = "ok evaluate".to_string();
                if let Some((method, url, status)) = &report.navigated {
                    let _ = write!(
                        text,
                        " → {} ({method}, {status})",
                        truncate(url.as_str(), 160)
                    );
                }
                for line in &report.lines {
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

    // ------------------------------------------------------------------ tabs

    /// Closes a popup tab (the top page closes with its group).
    pub(crate) fn close_popup(&mut self, tab: u32) -> Result<(), Failure> {
        let root = self.tab_mut(tab)?.root;
        self.page
            .close_popup(root)
            .map_err(|e| Failure::new(ErrorCode::NoTab, e.to_string()))?;
        self.page.take_events();
        self.tabs.remove(&tab);
        Ok(())
    }
}

/// The ref of `node`, named from what it shows.
fn assign(state: &PageState, refs: &mut RefTable, frame: u32, epoch: u64, node: NodeId) -> u32 {
    agent::with_styles(state, |engine, dom| {
        let oracle = EngineOracle {
            engine,
            page: state,
        };
        RefScope::new(refs, frame, epoch).assign(dom, node, &oracle)
    })
}

/// Resolves a ref of the tab's root document, wording failures.
fn resolve_ref(
    page: &Page,
    refs: &mut RefTable,
    text: &str,
    root: FrameId,
) -> Result<(NodeId, u32), Failure> {
    let r = RefTable::parse(text).unwrap_or(0);
    match refs.lookup(text, |key| is_live(page, key)) {
        Ok(key) if FrameId(key.frame) == root => Ok((key.node, r)),
        Ok(_) => Err(Failure::new(
            ErrorCode::Unsupported,
            format!("e{r} is inside a frame; frame refs come in a later version"),
        )),
        Err(RefError::BadSyntax(text)) => {
            Err(Failure::bad_argument(format!("{text:?} is not a ref")).with(advice::TARGET_SYNTAX))
        }
        Err(RefError::Unknown(r)) => Err(Failure::new(
            ErrorCode::NotFound,
            format!("e{r} was never shown in this tab"),
        )
        .with(advice::UNKNOWN_REF)),
        Err(RefError::Stale {
            r,
            reason,
            role,
            name,
            suggestion,
        }) => {
            let failure = Failure::new(
                ErrorCode::StaleRef,
                format!("e{r} {role} {} ({})", quote(&name), reason.as_str()),
            );
            Err(match suggestion {
                Some(s) => failure
                    .with(format!("maybe {}", describe(refs, s)))
                    .with(advice::STALE),
                None => failure.with(advice::STALE_GONE),
            })
        }
    }
}

fn title_of(state: &PageState) -> String {
    let dom = state.dom.borrow();
    dom.descendants(dom.document())
        .find(|&n| dom.is_html_element(n, "title"))
        .map(|n| {
            dom.text_content(n)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn render_forms(forms: &[catpaw_agent::FormInfo]) -> String {
    let mut out = String::new();
    for form in forms {
        let _ = write!(out, "form");
        if let Some(r) = &form.r#ref {
            let _ = write!(out, " {r}");
        }
        match &form.action {
            Some(action) => {
                let _ = writeln!(out, " {} {}", form.method, action);
            }
            None => {
                let _ = writeln!(out, " (fields outside a form)");
            }
        }
        for field in &form.fields {
            out.push_str("  ");
            if let Some(r) = &field.r#ref {
                let _ = write!(out, "{r} ");
            }
            let _ = write!(out, "{}", field.kind);
            if !field.label.is_empty() {
                let _ = write!(out, " {}", quote(&truncate(&field.label, 80)));
            }
            if let Some(name) = &field.name {
                let _ = write!(out, " name={name}");
            }
            let is_button = matches!(field.kind.as_str(), "submit" | "button" | "reset");
            if !field.value.is_empty()
                && field.checked.is_none()
                && !(is_button && field.value == field.label)
            {
                let _ = write!(out, " value={}", quote(&truncate(&field.value, 80)));
            }
            if field.checked == Some(true) {
                out.push_str(" checked");
            }
            if field.required {
                out.push_str(" required");
            }
            if !field.options.is_empty() {
                let shown: Vec<String> = field
                    .options
                    .iter()
                    .take(20)
                    .map(|o| quote(&truncate(o, 40)))
                    .collect();
                let _ = write!(out, " options={}", shown.join("|"));
                if field.options.len() > 20 {
                    let _ = write!(out, "|+{}", field.options.len() - 20);
                }
            }
            out.push('\n');
        }
    }
    out
}

/// The largest char boundary at or below `index`.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Width and height from a PNG's header.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// How long actions wait for the page, as page options.
pub(crate) fn action_limits() -> LoopLimits {
    LoopLimits {
        wall: Duration::from_secs(10),
        virtual_ms: 5_000.0,
        ..LoopLimits::default()
    }
}
