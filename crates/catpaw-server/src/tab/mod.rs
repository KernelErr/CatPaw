//! A browsing-context group as the agent sees it: its tabs (the top page
//! and the popups it opened), their refs, snapshot history and logs, and
//! the tools that act on them. Everything here runs on the group's thread.

mod act;
mod locate;
mod pending;
mod read;
mod view;
mod wait;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use catpaw_agent::snapshot::{LineKind, quote, truncate};
use catpaw_agent::{ExtraAttrs, Filter, Format, RefError, RefKey, RefScope, RefTable, SnapLine};
use catpaw_dom::NodeId;
use catpaw_engine::{
    EngineError, FrameId, FrameInfo, Gate, GateRequest, LoopLimits, Page, PageOptions,
    SettlePolicy, SharedNet,
};
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::net::NetRequest;
use catpaw_web::{PageState, agent};
use url::Url;

use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure};
use crate::policy::{Policy, Preset, Verdict};
use crate::target::{self, Target};

/// The budget of a snapshot when the call gives none, in tokens.
pub(crate) const SNAPSHOT_TOKENS: u32 = 4000;
/// The budget of a read view when the call gives none, in tokens.
const READ_TOKENS: u32 = 6000;
/// The most a script result may take, in bytes.
const EVAL_BYTES: usize = 4000;
/// Console errors listed after an action; the rest are counted.
const CONSOLE_LINES: usize = 3;
/// Snapshot ids a tab remembers log positions for (`logs({since})`).
const MARKS: usize = 32;
/// Events a tab keeps for `logs({kind: "events"})`.
const EVENTS: usize = 500;
/// Page events kept for results not given yet (those of tabs nobody acts
/// on are dropped oldest first).
const INBOX: usize = 1000;

/// Bytes in `tokens` by the fixed estimate (3.5 bytes a token): the same
/// input always gets the same budget.
fn token_bytes(tokens: u32) -> usize {
    tokens as usize * 7 / 2
}

/// Session-wide display choices.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View {
    pub format: Format,
    /// The tabs open in the session: headers name the tab when there is
    /// more than one.
    pub tabs: usize,
}

/// What a snapshot should show.
#[derive(Debug, Clone)]
pub(crate) struct SnapRequest {
    pub filter: Filter,
    pub root: Option<String>,
    /// With `root`: show the items after this one.
    pub after: Option<String>,
    pub max_tokens: u32,
    pub extra: ExtraAttrs,
    /// The result's status line already gives the URL.
    pub url_shown: bool,
    /// The element the action was on: focus going to it is no news.
    pub acted: Option<u32>,
}

impl Default for SnapRequest {
    fn default() -> Self {
        Self {
            filter: Filter::Interesting,
            root: None,
            after: None,
            max_tokens: SNAPSHOT_TOKENS,
            extra: ExtraAttrs::default(),
            url_shown: false,
            acted: None,
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
    filter: Filter,
    epoch: u64,
    url: String,
    title: String,
    scroll: (i64, i64),
    focus: Option<u32>,
    lines: Vec<SnapLine>,
    /// What the tab showed then (see `shown_versions`).
    version: Vec<u64>,
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
    /// The epoch of the document shown.
    doc_epoch: u64,
    history: VecDeque<Stored>,
    marks: VecDeque<Mark>,
    /// Navigations, tabs and dialogs, for `logs({kind: "events"})`.
    events: Vec<String>,
}

impl Tab {
    fn new(id: u32, root: FrameId, opener: Option<u32>, epoch: u64) -> Self {
        Self {
            id,
            root,
            opener,
            refs: RefTable::new(),
            next_snapshot: 1,
            doc_epoch: epoch,
            history: VecDeque::new(),
            marks: VecDeque::new(),
            events: Vec::new(),
        }
    }

    /// Notices a new document in the tab: its refs into the old one go
    /// stale.
    fn sync(&mut self, page: &Page) {
        let Some(epoch) = page.document_epoch(self.root) else {
            return;
        };
        if epoch != self.doc_epoch {
            self.refs.document_replaced(self.root.0, epoch);
            self.doc_epoch = epoch;
        }
    }

    /// What comes before a ref in the latest snapshot, at its depth, to
    /// place it: the nearest named element (`e14 button "View details"`),
    /// else the nearest text.
    fn line_before(&self, r: u32) -> Option<String> {
        let lines = &self.history.back()?.lines;
        let at = lines
            .iter()
            .position(|l| matches!(l.kind, LineKind::Element { r: lr, .. } if lr == r))?;
        let depth = lines[at].depth;
        let siblings = || {
            lines[..at]
                .iter()
                .rev()
                .take_while(move |l| l.depth >= depth)
                .filter(move |l| l.depth == depth)
        };
        siblings()
            .find_map(|l| match &l.kind {
                LineKind::Element { r, role, name, .. } if !name.is_empty() => {
                    Some(format!("e{r} {role} {}", quote(&truncate(name, 60))))
                }
                _ => None,
            })
            .or_else(|| {
                siblings().find_map(|l| match &l.kind {
                    LineKind::Text(t) => Some(format!("text {}", quote(&truncate(t, 40)))),
                    _ => None,
                })
            })
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

/// What an action can use, to tell apart elements a target matches and to
/// take a label for its control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Use {
    Any,
    /// Something to type into.
    Text,
    /// Something to check or uncheck.
    Check,
    /// Something to choose options of.
    Choose,
}

impl Use {
    /// Whether an element of `role` (`editable`: taking text as a rich
    /// text editor does) is what the action uses.
    fn fits(self, role: &str, editable: bool) -> bool {
        match self {
            Use::Any => true,
            Use::Text => {
                editable || matches!(role, "textbox" | "searchbox" | "combobox" | "spinbutton")
            }
            Use::Check => matches!(
                role,
                "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
            ),
            Use::Choose => matches!(role, "combobox" | "listbox"),
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
    /// The stale ref the page re-rendered as this one.
    retargeted: Option<u32>,
}

/// A handed-over tab as the hand-off page shows it.
#[derive(Debug, Clone)]
pub(crate) struct HandState {
    pub url: String,
    pub title: String,
    /// What the page holds for the user to decide: hold numbers and what
    /// each would do.
    pub held: Vec<(u64, String)>,
}

/// A page event taken in, kept for the next result of the tab it
/// concerns.
struct Absorbed {
    /// That tab; `None` when the page does not say (a request it refused,
    /// a download, a hold it dropped, a popup that closed): the next
    /// result of any tab gives it.
    tab: Option<u32>,
    /// The tab a popup became, or the tab that closed.
    which: Option<u32>,
    event: catpaw_engine::PageEvent,
}

/// The tabs of one browsing-context group and the page they live in.
pub(crate) struct GroupState {
    page: Page,
    tabs: BTreeMap<u32, Tab>,
    /// Page events not reported yet.
    inbox: Vec<Absorbed>,
    /// Tabs with the user, and the hold number their hand-off began at:
    /// what the page holds in them from then on is the user's to decide.
    handoff_marks: BTreeMap<u32, u64>,
    next_tab: Arc<AtomicU32>,
    /// Where relative paths of uploaded files start.
    files_root: Option<PathBuf>,
}

/// What a group needs from its session beyond the page options.
#[derive(Clone, Debug, Default)]
pub(crate) struct GroupSetup {
    pub policy: Policy,
    pub files_root: Option<PathBuf>,
}

/// Puts `policy` in front of a page's navigations and (under the strict
/// preset) the requests its scripts make.
fn install_gates(page: &mut Page, policy: &Policy) {
    let navigations = policy.clone();
    page.set_navigation_gate(Some(Box::new(
        move |request: &GateRequest<'_>| match navigations.navigation(request.method, request.url) {
            Verdict::Allow => Gate::Allow,
            Verdict::Confirm => Gate::Hold,
            Verdict::Block(reason) => Gate::Deny(reason),
        },
    )));
    if policy.preset == Preset::Strict {
        let requests = policy.clone();
        page.set_request_gate(Some(Rc::new(move |request: &NetRequest| {
            let origin = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("origin"))
                .map(|(_, value)| value.as_str());
            match requests.request(&request.method, &request.url, origin) {
                Verdict::Allow => Gate::Allow,
                Verdict::Confirm => Gate::Hold,
                Verdict::Block(reason) => Gate::Deny(reason),
            }
        })));
    }
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
        setup: GroupSetup,
    ) -> Result<Self, EngineError> {
        let mut page = Page::blank(options, net)?;
        install_gates(&mut page, &setup.policy);
        let epoch = page.document_epoch(FrameId(0)).unwrap_or(0);
        let mut tabs = BTreeMap::new();
        tabs.insert(first, Tab::new(first, FrameId(0), None, epoch));
        Ok(Self {
            page,
            tabs,
            inbox: Vec::new(),
            handoff_marks: BTreeMap::new(),
            next_tab,
            files_root: setup.files_root,
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

    /// The URL of a tab's document and how far its window is scrolled.
    pub(crate) fn place(&self, tab: u32) -> Option<(Url, (f32, f32))> {
        let root = self.tabs.get(&tab)?.root;
        let state = self.page.frame_state(root)?;
        Some((state.url.borrow().clone(), agent::window_scroll(state)))
    }

    /// Scrolls a tab's window to `(x, y)`.
    pub(crate) fn scroll_to(&mut self, tab: u32, x: f32, y: f32) {
        let Some(root) = self.tabs.get(&tab).map(|t| t.root) else {
            return;
        };
        let _ = self.page.input_in(root, |cx| {
            let (from_x, from_y) = agent::window_scroll(cx.page);
            agent::scroll_by(cx, x - from_x, y - from_y);
            Ok(())
        });
    }

    /// A PNG of a tab's viewport.
    pub(crate) fn screen(&self, tab: u32) -> Option<Vec<u8>> {
        let root = self.tabs.get(&tab)?.root;
        self.page.screenshot_of(root, false)
    }

    /// A hand-off of `tab` begins: what its page holds from now on is
    /// shown to the user, who lets it go or not on the hand-off page.
    pub(crate) fn begin_handoff(&mut self, tab: u32) {
        let mark = self.page.hold_watermark();
        self.handoff_marks.insert(tab, mark);
    }

    /// What the page holds in a handed-over tab since the hand-off began:
    /// hold numbers and what each would do.
    fn hand_held(&self, tab: u32) -> Vec<(u64, String)> {
        let Some(&mark) = self.handoff_marks.get(&tab) else {
            return Vec::new();
        };
        let frames: Vec<FrameId> = self.frames_of(tab).iter().map(|f| f.id).collect();
        let typed = self.user_values(tab);
        let mut held: Vec<(u64, String)> = self
            .page
            .held_navigations()
            .iter()
            .filter(|h| h.id >= mark && frames.contains(&h.frame))
            .map(|h| (h.id, act::describe_held(h, &typed)))
            .collect();
        held.extend(
            self.page
                .held_requests_in(&frames)
                .into_iter()
                .filter(|r| r.id >= mark)
                .map(|r| {
                    let what = format!("send → {} {}", r.method, truncate(r.url.as_str(), 160));
                    (r.id, what)
                }),
        );
        held.sort_by_key(|(id, _)| *id);
        held
    }

    /// What the user typed into the tab's fields during a hand-off (the
    /// values shown masked).
    fn user_values(&self, tab: u32) -> Vec<String> {
        self.frames_of(tab)
            .iter()
            .filter_map(|f| self.page.frame_state(f.id))
            .flat_map(|state| agent::user_values(state))
            .collect()
    }

    /// Whether a tab's fields hold what the user typed during a hand-off.
    pub(crate) fn holds_user_input(&self, tab: u32) -> bool {
        !self.user_values(tab).is_empty()
    }

    /// Where a handed-over tab is: its URL, title, and what it holds for
    /// the user to decide.
    fn hand_state(&self, tab: u32) -> Result<HandState, String> {
        let root = self
            .tabs
            .get(&tab)
            .map(|t| t.root)
            .ok_or("the tab is closed")?;
        let state = self.page.frame_state(root).ok_or("the tab is closed")?;
        Ok(HandState {
            url: state.url.borrow().to_string(),
            title: title_of(state),
            held: self.hand_held(tab),
        })
    }

    /// Where a handed-over tab is now (for the hand-off page).
    pub(crate) fn hand_view(&mut self, tab: u32) -> Result<HandState, String> {
        self.absorb_events();
        self.hand_state(tab)
    }

    /// Input from the user during a hand-off; where the tab is after it.
    /// What the input makes the page hold waits for the user's decision
    /// ([`GroupState::hand_decide`]).
    pub(crate) fn hand_input(
        &mut self,
        tab: u32,
        input: crate::handoff::Input,
    ) -> Result<HandState, String> {
        use crate::handoff::Input;
        use catpaw_web::input;
        let root = self
            .tabs
            .get(&tab)
            .map(|t| t.root)
            .ok_or("the tab is closed")?;
        let typing = matches!(input, Input::Text(_) | Input::Key(_));
        // The field typed into, before the input moves focus on (a code
        // box that passes it to the next).
        let typed_into = self
            .page
            .frame_state(root)
            .and_then(|state| agent::focused(state));
        self.page
            .input_in(root, move |cx| match input {
                Input::Click { x, y } => {
                    input::click_at(cx, x, y);
                    Ok(())
                }
                Input::Text(text) => input::type_text(cx, &text),
                Input::Key(key) => input::press(cx, &key),
                Input::Scroll(dy) => {
                    agent::scroll_by(cx, 0.0, dy);
                    Ok(())
                }
            })
            .map_err(|e| e.to_string())?;
        // What the user types stays theirs: the field's value is masked in
        // what the agent reads until the agent sets it itself.
        if typing && let Some(state) = self.page.frame_state(root) {
            for node in typed_into.into_iter().chain(agent::focused(state)) {
                agent::mask_value(state, node);
            }
        }
        self.absorb_events();
        self.hand_state(tab)
    }

    /// The user's decision on the hand-off page about what the tab holds:
    /// let it go, or drop it. Only holds of this hand-off are touched.
    pub(crate) fn hand_decide(
        &mut self,
        tab: u32,
        ids: &[u64],
        allow: bool,
    ) -> Result<HandState, String> {
        let ids: Vec<u64> = self
            .hand_held(tab)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| ids.contains(id))
            .collect();
        if allow {
            let navigations: Vec<u64> = self
                .page
                .held_navigations()
                .iter()
                .filter(|h| ids.contains(&h.id))
                .map(|h| h.id)
                .collect();
            self.page.release_held_requests(&ids);
            for id in navigations {
                self.page.release_held(id).map_err(|e| e.to_string())?;
            }
            self.page.settle(&action_limits());
        } else {
            self.drop_holds(&ids);
        }
        self.absorb_events();
        self.hand_state(tab)
    }

    /// The result of a hand-off given back: what happened meanwhile
    /// (navigations, new tabs) and the whole page as it is now.
    pub(crate) fn after_handoff(&mut self, tab: u32, status: String, view: View) -> CallResult {
        let mut base = self.baseline(tab);
        // What the page still holds from the hand-off is asked about now.
        if let Some(mark) = self.handoff_marks.remove(&tab) {
            base.watermark = mark;
        }
        let report = self.finish(tab, &base);
        self.page_result(
            tab,
            status,
            report,
            Some(catpaw_protocol::params::SnapshotMode::Full),
            view,
        )
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

    /// The frames of a tab: its top frame and the frames inside it (not
    /// the popups it opened), parents before children.
    fn frames_of(&self, tab: u32) -> Vec<FrameInfo> {
        let Some(root) = self.tabs.get(&tab).map(|t| t.root) else {
            return Vec::new();
        };
        let all = self.page.frames();
        let mut out: Vec<FrameInfo> = all.iter().filter(|f| f.id == root).cloned().collect();
        let mut i = 0;
        while i < out.len() {
            let parent = out[i].id;
            out.extend(
                all.iter()
                    .filter(|f| f.parent == Some(parent) && !f.popup)
                    .cloned(),
            );
            i += 1;
        }
        out
    }

    /// Resolves a target to a node of the tab, with a ref for it.
    fn aim(&mut self, tab: u32, text: &str) -> Result<Aim, Failure> {
        self.aim_for(tab, text, Use::Any)
    }

    /// [`GroupState::aim`] for an action that uses what `wants` says: of
    /// elements a text matches, the one it can use, and for a label, the
    /// control the label stands for.
    fn aim_for(&mut self, tab: u32, text: &str, wants: Use) -> Result<Aim, Failure> {
        let aim = self.aim_at(tab, text, wants)?;
        Ok(match wants {
            Use::Any => aim,
            _ => self.through_label(tab, aim),
        })
    }

    /// A label as the control it stands for.
    fn through_label(&mut self, tab: u32, aim: Aim) -> Aim {
        let Some(state) = self.page.frame_state(aim.frame).cloned() else {
            return aim;
        };
        match agent::labeled_control(&state, aim.node) {
            Some(control) => Aim {
                node: control,
                r: self.ref_for(tab, aim.frame, control).unwrap_or(aim.r),
                ..aim
            },
            None => aim,
        }
    }

    fn aim_at(&mut self, tab: u32, text: &str, wants: Use) -> Result<Aim, Failure> {
        let target = target::parse(text)?;
        let (root, state) = self.root_state(tab)?;
        let frames = self.frames_of(tab);
        let allowed: Vec<FrameId> = frames.iter().map(|f| f.id).collect();
        {
            let page = &self.page;
            let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
            entry.sync(page);
        }
        match target {
            Target::Ref(text) => {
                let page = &self.page;
                let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
                match resolve_ref(page, &mut entry.refs, &text, &allowed) {
                    Ok((frame, node, r)) => Ok(Aim {
                        frame,
                        node,
                        r,
                        point: None,
                        retargeted: None,
                    }),
                    // A node the page rendered again is acted on under its
                    // new ref, when there is no doubt which one it is.
                    Err(failure) if failure.code == ErrorCode::StaleRef => {
                        let old = RefTable::parse(&text).unwrap_or(0);
                        self.retarget(tab, old, &allowed).ok_or(failure)
                    }
                    Err(failure) => Err(failure),
                }
            }
            Target::Css(selector) => {
                // Like a strict locator: one element, the shown ones
                // counting first (a hidden template or closed menu is not
                // the one meant); more than that is for the agent to tell
                // apart.
                let mut all: Vec<(FrameId, NodeId, bool)> = Vec::new();
                for frame in &allowed {
                    let Some(state) = self.page.frame_state(*frame).cloned() else {
                        continue;
                    };
                    for node in query_all(&state, &selector)? {
                        all.push((*frame, node, is_shown(&state, node)));
                    }
                }
                let shown: Vec<(FrameId, NodeId)> = all
                    .iter()
                    .filter(|m| m.2)
                    .map(|&(f, n, _)| (f, n))
                    .collect();
                let candidates: Vec<(FrameId, NodeId)> = if shown.is_empty() {
                    all.iter().map(|&(f, n, _)| (f, n)).collect()
                } else {
                    shown
                };
                match candidates.as_slice() {
                    [] => Err(Failure::new(
                        ErrorCode::NotFound,
                        format!("css:{selector} matches nothing"),
                    )),
                    [(frame, node)] => {
                        let r = self.ref_for(tab, *frame, *node).unwrap_or(0);
                        Ok(Aim {
                            frame: *frame,
                            node: *node,
                            r,
                            point: None,
                            retargeted: None,
                        })
                    }
                    many => {
                        let listed: Vec<String> = many
                            .iter()
                            .take(5)
                            .map(|&(frame, node)| {
                                let r = self.ref_for(tab, frame, node).unwrap_or(0);
                                self.describe_in_context(tab, r)
                            })
                            .collect();
                        let more = if many.len() > 5 { ", …" } else { "" };
                        Err(Failure::new(
                            ErrorCode::AmbiguousTarget,
                            format!(
                                "css:{selector} matches {} elements: {}{more}",
                                many.len(),
                                listed.join(", ")
                            ),
                        )
                        .with(advice::AMBIGUOUS))
                    }
                }
            }
            Target::Named(role, name) => self.locate(tab, Some(&role), &name, wants),
            Target::Text(text) => self.locate(tab, None, &text, wants),
            Target::Point(x, y) => {
                // Into the frame under the point, as a click goes.
                let (mut frame, mut state, mut point) = (root, state, (x, y));
                let node = loop {
                    let node = agent::element_at(&state, point.0, point.1).ok_or_else(|| {
                        Failure::new(ErrorCode::NotFound, format!("nothing is at {x},{y}"))
                    })?;
                    let child = frames
                        .iter()
                        .find(|f| f.parent == Some(frame) && f.element == Some(node));
                    let (Some(child), Some(rect)) = (child, agent::element_rect(&state, node))
                    else {
                        break node;
                    };
                    let Some(inner) = self.page.frame_state(child.id).cloned() else {
                        break node;
                    };
                    point = (point.0 - rect.0, point.1 - rect.1);
                    frame = child.id;
                    state = inner;
                };
                let r = self.ref_for(tab, frame, node).unwrap_or(0);
                Ok(Aim {
                    frame,
                    node,
                    r,
                    point: Some(point),
                    retargeted: None,
                })
            }
        }
    }

    /// The live node a stale ref was re-rendered as, if it is certain.
    fn retarget(&mut self, tab: u32, old: u32, frames: &[FrameId]) -> Option<Aim> {
        let find = |g: &Self| -> Option<u32> {
            g.tabs.get(&tab)?.refs.replacement(old, |key| {
                frames.contains(&FrameId(key.frame)) && is_live(&g.page, key)
            })
        };
        let new = match find(self) {
            Some(new) => Some(new),
            None => {
                // The new node may not have a ref yet: show the page to the
                // ref table, then look again.
                self.model(
                    tab,
                    catpaw_agent::Filter::Interesting,
                    ExtraAttrs::default(),
                    None,
                )
                .ok()?;
                find(self)
            }
        }?;
        let key = self.tabs.get(&tab)?.refs.entry(new)?.key;
        Some(Aim {
            frame: FrameId(key.frame),
            node: key.node,
            r: new,
            point: None,
            retargeted: Some(old),
        })
    }

    /// The element a target names (`e5 button "Choose file"`), for a
    /// confirmation to show.
    pub(crate) fn describe_target(&mut self, tab: u32, target: &str) -> Result<String, Failure> {
        let aim = self.aim(tab, target)?;
        Ok(self.aimed(tab, &aim))
    }

    /// An aimed-at element for a status line, with a re-render noted.
    fn aimed(&self, tab: u32, aim: &Aim) -> String {
        let mut text = self.describe_in_context(tab, aim.r);
        if let Some(old) = aim.retargeted {
            text.push_str(&format!(" (e{old} re-rendered → e{})", aim.r));
        }
        text
    }

    fn describe(&self, tab: u32, r: u32) -> String {
        self.tabs
            .get(&tab)
            .map(|t| describe(&t.refs, r))
            .unwrap_or_else(|| format!("e{r}"))
    }

    /// An element as [`GroupState::describe`] names it, with where it is
    /// when another element the page shows has the same role and name
    /// (`e14 button "Add to cart" (in e12 listitem "Socks")`).
    pub(super) fn describe_in_context(&self, tab: u32, r: u32) -> String {
        let mut text = self.describe(tab, r);
        let Some(entry) = self.tabs.get(&tab) else {
            return text;
        };
        let refs = &entry.refs;
        let alike: Vec<u32> = refs
            .namesakes(r)
            .filter(|other| is_live(&self.page, &other.key))
            .filter_map(|other| refs.get(other.key))
            .collect();
        if alike.is_empty() {
            return text;
        }
        // Where it is, when that tells it from the others: the element
        // around it, else what comes before it on the page.
        let around = refs.context_of(r);
        if let Some(around) = around
            && alike
                .iter()
                .all(|&other| refs.context_of(other) != Some(around))
        {
            text.push_str(&format!(" (in {})", describe(refs, around)));
        } else if let Some(before) = entry.line_before(r) {
            text.push_str(&format!(" (after {before})"));
        }
        text
    }

    /// The versions of what each of the tab's documents shows: equal, the
    /// tab shows what it did.
    pub(super) fn shown_versions(&self, tab: u32) -> Vec<u64> {
        self.frames_of(tab)
            .iter()
            .filter_map(|f| self.page.frame_state(f.id))
            .map(|state| state.epoch ^ agent::shown_version(state).rotate_left(1))
            .collect()
    }

    /// The lines of the tab's latest snapshot (default filter), when the
    /// page has not changed since it was taken.
    pub(super) fn fresh_lines(&self, tab: u32) -> Option<Vec<SnapLine>> {
        let stored = self
            .tabs
            .get(&tab)?
            .history
            .iter()
            .rev()
            .find(|s| s.filter == catpaw_agent::Filter::Interesting)?;
        (stored.version == self.shown_versions(tab)).then(|| stored.lines.clone())
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
        // What the page did before stays to be reported; what the closed
        // tab did goes with it, but for the popups it opened (and its own
        // closing is no news to the agent that asked for it).
        self.absorb_events();
        self.tabs.remove(&tab);
        self.inbox.retain_mut(|a| {
            if matches!(a.event, catpaw_engine::PageEvent::PopupClosed { .. }) {
                return a.which != Some(tab);
            }
            if a.tab != Some(tab) {
                return true;
            }
            a.tab = None;
            matches!(a.event, catpaw_engine::PageEvent::PopupOpened { .. })
        });
        Ok(())
    }

    /// The frame an `iframe` element shows, when `node` of `frame` is one
    /// with a document: a root there means that document.
    fn hosted_frame(&self, frame: FrameId, node: NodeId) -> Option<FrameId> {
        self.page
            .frames()
            .iter()
            .find(|f| f.parent == Some(frame) && f.element == Some(node) && !f.popup)
            .map(|f| f.id)
    }

    /// Whether `frame` is `ancestor` or inside it.
    fn frame_within(&self, frame: FrameId, ancestor: FrameId) -> bool {
        let frames = self.page.frames();
        let mut at = Some(frame);
        for _ in 0..64 {
            match at {
                Some(f) if f == ancestor => return true,
                Some(f) => at = frames.iter().find(|i| i.id == f).and_then(|i| i.parent),
                None => return false,
            }
        }
        false
    }

    /// The requests held in a tab's frames (and their workers).
    fn held_requests_of(&self, tab: u32) -> Vec<catpaw_engine::HeldRequestInfo> {
        let frames: Vec<FrameId> = self.frames_of(tab).iter().map(|f| f.id).collect();
        self.page.held_requests_in(&frames)
    }
}

/// The elements matching a CSS selector in a page, shadow trees included,
/// in document order.
fn query_all(state: &PageState, selector: &str) -> Result<Vec<NodeId>, Failure> {
    let selectors = catpaw_style::Selectors::parse(selector)
        .ok_or_else(|| Failure::bad_argument(format!("{selector:?} is not a valid selector")))?;
    let dom = state.dom.borrow();
    Ok(dom
        .shadow_including_descendants(dom.document())
        .into_iter()
        .filter(|&n| dom.is_element(n) && catpaw_style::query::matches(&dom, n, &selectors))
        .collect())
}

/// Whether an element is rendered and visible (not `display: none`, in
/// itself or an ancestor, nor `visibility: hidden`).
fn is_shown(state: &PageState, node: NodeId) -> bool {
    agent::with_styles(state, |engine, dom| {
        !engine.is_display_none(dom, node) && !engine.is_visibility_hidden(node)
    })
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

/// Resolves a ref of one of the tab's frames, wording failures.
fn resolve_ref(
    page: &Page,
    refs: &mut RefTable,
    text: &str,
    frames: &[FrameId],
) -> Result<(FrameId, NodeId, u32), Failure> {
    let r = RefTable::parse(text).unwrap_or(0);
    match refs.lookup(text, |key| is_live(page, key)) {
        Ok(key) if frames.contains(&FrameId(key.frame)) => Ok((FrameId(key.frame), key.node, r)),
        Ok(_) => Err(Failure::new(
            ErrorCode::StaleRef,
            format!("e{r} is in another tab's frame"),
        )
        .with(advice::STALE_GONE)),
        Err(RefError::BadSyntax(text)) => {
            Err(Failure::bad_argument(format!("{text:?} is not a ref")).with(advice::TARGET_SYNTAX))
        }
        Err(RefError::Unknown(r)) => Err(Failure::new(
            ErrorCode::NotFound,
            format!("e{r} was never shown in this tab"),
        )
        .with(advice::UNKNOWN_REF)),
        Err(RefError::Forgotten(r)) => Err(Failure::new(
            ErrorCode::StaleRef,
            format!("e{r} (from a page this tab left long ago)"),
        )
        .with(advice::STALE_GONE)),
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
