//! A browsing-context group as the agent sees it: its tabs (the top page
//! and the popups it opened), their refs, snapshot history and logs, and
//! the tools that act on them. Everything here runs on the group's thread.

mod act;
mod pending;
mod read;
mod view;
mod wait;

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use catpaw_agent::snapshot::quote;
use catpaw_agent::{ExtraAttrs, Filter, Format, RefError, RefKey, RefScope, RefTable, SnapLine};
use catpaw_dom::NodeId;
use catpaw_engine::{EngineError, FrameId, LoopLimits, Page, PageOptions, SettlePolicy, SharedNet};
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::{PageState, agent};
use url::Url;

use crate::oracle::EngineOracle;
use crate::output::Failure;
use crate::target::{self, Target};

/// The budget of a snapshot when the call gives none, in tokens.
pub(crate) const SNAPSHOT_TOKENS: u32 = 4000;
/// The budget of a read view when the call gives none, in tokens.
const READ_TOKENS: u32 = 6000;
/// The most a script result may take, in characters.
const EVAL_CHARS: usize = 4000;
/// Console errors listed after an action; the rest are counted.
const CONSOLE_LINES: usize = 3;
/// Snapshots a tab keeps to diff against.
const HISTORY: usize = 8;
/// Snapshot ids a tab remembers log positions for (`logs({since})`).
const MARKS: usize = 32;
/// Events a tab keeps for `logs({kind: "events"})`.
const EVENTS: usize = 500;

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

/// A snapshot a tab keeps to diff against: the whole page, as lines.
struct Stored {
    id: u64,
    filter: Filter,
    epoch: u64,
    doc: u64,
    url: String,
    title: String,
    lines: Vec<SnapLine>,
}

/// Where the logs stood when a snapshot was taken.
#[derive(Clone, Copy)]
struct Mark {
    id: u64,
    epoch: u64,
    console: usize,
    requests: usize,
    events: usize,
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
    history: VecDeque<Stored>,
    marks: VecDeque<Mark>,
    /// Navigations, tabs and dialogs, for `logs({kind: "events"})`.
    events: Vec<String>,
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
            history: VecDeque::new(),
            marks: VecDeque::new(),
            events: Vec::new(),
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

    fn event(&mut self, line: String) {
        self.events.push(line);
        if self.events.len() > EVENTS {
            self.events.drain(..self.events.len() - EVENTS);
        }
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_snapshot;
        self.next_snapshot += 1;
        id
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
                let node = query(&state, &selector)?.ok_or_else(|| {
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

    /// The ref of a node of a frame of the tab, assigning one if needed.
    fn ref_for(&mut self, tab: u32, frame: FrameId, node: NodeId) -> Option<u32> {
        let state = self.page.frame_state(frame)?.clone();
        let epoch = state.epoch;
        let entry = self.tabs.get_mut(&tab)?;
        Some(assign(&state, &mut entry.refs, frame.0, epoch, node))
    }

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

/// The first element matching a CSS selector in a page.
fn query(state: &PageState, selector: &str) -> Result<Option<NodeId>, Failure> {
    let selectors = catpaw_style::Selectors::parse(selector)
        .ok_or_else(|| Failure::bad_argument(format!("{selector:?} is not a valid selector")))?;
    let dom = state.dom.borrow();
    Ok(catpaw_style::query::query_first(
        &dom,
        dom.document(),
        &selectors,
    ))
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

/// How long actions wait for the page, and what they wait for.
pub(crate) fn action_limits() -> LoopLimits {
    LoopLimits {
        wall: Duration::from_secs(10),
        virtual_ms: 5_000.0,
        settle: Some(SettlePolicy::default()),
        ..LoopLimits::default()
    }
}

/// `1.4s` for a duration of a second or more, else nothing.
fn seconds(ms: f64) -> Option<String> {
    (ms >= 1000.0).then(|| format!("{:.1}s", ms / 1000.0))
}
