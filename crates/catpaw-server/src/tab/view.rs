//! Snapshots: the page as lines, kept per tab to diff against, and shown
//! whole or as what changed.

use catpaw_agent::snapshot::{LineKind, render_line};
use catpaw_agent::{
    ExtraAttrs, Filter, Format, Header, RefKey, SnapLine, SnapshotOptions, Snapshotter,
};
use catpaw_dom::NodeId;
use catpaw_engine::{FrameId, FrameInfo};
use catpaw_protocol::params::{self, SnapshotMode};
use catpaw_protocol::wording::advice;
use catpaw_web::agent;

use super::{
    GroupState, MARKS, Mark, SNAPSHOT_TOKENS, SnapRequest, Stored, View, resolve_ref, token_bytes,
};
use crate::oracle::EngineOracle;
use crate::output::{CallResult, Failure, ToolOutput};

/// A snapshot before it is shown: every line, and the facts the header
/// gives.
pub(super) struct Model {
    pub lines: Vec<SnapLine>,
    pub total: usize,
    pub title: String,
    pub url: String,
    pub focus: Option<u32>,
    pub scroll: (i64, i64),
    pub viewport: (u32, u32),
    pub epoch: u64,
    /// The ref of the subtree shown, when not the whole page.
    pub root: Option<u32>,
}

/// The items after `after` in the list (or other container) it is an item
/// of, as the top of what is shown: the rest of a long list.
fn after_item(lines: &[SnapLine], after: &str) -> Result<Vec<SnapLine>, Failure> {
    let wanted = catpaw_agent::RefTable::parse(after)
        .ok_or_else(|| Failure::bad_argument(format!("{after:?} is not a ref")))?;
    let at = lines
        .iter()
        .position(|l| matches!(l.kind, LineKind::Element { r, .. } if r == wanted));
    let Some(at) = at else {
        return Err(Failure::bad_argument(format!(
            "e{wanted} is not on the page shown (or in that root)"
        )));
    };
    let depth = lines[at].depth;
    // The item's own lines end at the next line no deeper than it; its
    // list's, at the next shallower one.
    let next = lines[at + 1..]
        .iter()
        .position(|l| l.depth <= depth)
        .map_or(lines.len(), |k| at + 1 + k);
    let stop = lines[next..]
        .iter()
        .position(|l| l.depth < depth)
        .map_or(lines.len(), |k| next + k);
    Ok(lines[next..stop]
        .iter()
        .map(|l| SnapLine {
            depth: l.depth - depth,
            ..l.clone()
        })
        .collect())
}

/// The viewport pages get unless told otherwise: not worth a header key.
fn default_viewport() -> (u32, u32) {
    let config = catpaw_web::PageConfig::default();
    (config.viewport_width, config.viewport_height)
}

/// What a result shows of the page.
pub(super) struct PageView {
    pub text: String,
    /// For a diff whose header says nothing but its counts: its lines (a
    /// lone change can then go on the status line).
    pub quiet: Option<Vec<String>>,
    /// The number of the snapshot shown.
    pub id: Option<u64>,
}

impl PageView {
    fn whole((text, id): (String, u64)) -> Self {
        Self {
            text,
            quiet: None,
            id: Some(id),
        }
    }

    fn nothing() -> Self {
        Self {
            text: String::new(),
            quiet: None,
            id: None,
        }
    }
}

/// The size of all the lines rendered.
fn rendered_len(lines: &[SnapLine], format: Format) -> usize {
    let mut out = String::new();
    for line in lines {
        render_line(&mut out, line, format);
        out.push('\n');
    }
    out.len()
}

impl GroupState {
    /// The tab's page (or a subtree of it) as lines, refs assigned.
    pub(super) fn model(
        &mut self,
        tab: u32,
        filter: Filter,
        extra: ExtraAttrs,
        root: Option<&str>,
    ) -> Result<Model, Failure> {
        let (top, state) = self.root_state(tab)?;
        let frames = self.frames_of(tab);
        let allowed: Vec<FrameId> = frames.iter().map(|f| f.id).collect();
        let page = &self.page;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        entry.sync(page);
        entry.refs.begin_pass();
        let epoch = entry.doc_epoch;
        let root_node: Option<(FrameId, NodeId, u32)> = match root {
            Some(text) => Some(resolve_ref(page, &mut entry.refs, text, &allowed)?),
            None => None,
        };
        let url = state.url.borrow().to_string();
        let viewport = agent::viewport(&state);
        let (sx, sy) = agent::window_scroll(&state);
        // An `iframe` as root shows the document inside it.
        let (start, start_node) = match root_node {
            Some((frame, node, _)) => match self.hosted_frame(frame, node) {
                Some(inner) => (inner, None),
                None => (frame, Some(node)),
            },
            None => (top, None),
        };
        let (lines, total) = self.frame_lines(tab, start, start_node, filter, extra, &frames, 0)?;
        let title = super::title_of(&state);
        // Focus on an element the page no longer shows says nothing.
        let focus = self.focus_ref(tab, top, &frames).filter(|&focused| {
            lines
                .iter()
                .any(|l| matches!(l.kind, LineKind::Element { r, .. } if r == focused))
        });
        Ok(Model {
            lines,
            total,
            title,
            url,
            focus,
            scroll: (sx.round() as i64, sy.round() as i64),
            viewport,
            epoch,
            root: root_node.map(|(_, _, r)| r),
        })
    }

    /// The lines of one frame's document (or a subtree of it), with the
    /// documents of the frames inside it under their `iframe` lines.
    #[allow(clippy::too_many_arguments)]
    fn frame_lines(
        &mut self,
        tab: u32,
        frame: FrameId,
        root: Option<NodeId>,
        filter: Filter,
        extra: ExtraAttrs,
        frames: &[FrameInfo],
        depth: u16,
    ) -> Result<(Vec<SnapLine>, usize), Failure> {
        let Some(state) = self.page.frame_state(frame).cloned() else {
            return Ok((Vec::new(), 0));
        };
        let epoch = state.epoch;
        let entry = self.tabs.get_mut(&tab).expect("the tab is open");
        let (mut lines, mut total) = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let options = SnapshotOptions {
                filter,
                format: Format::Compact,
                root,
                max_depth: None,
                max_chars: None,
                extra,
                ..SnapshotOptions::default()
            };
            let mut snapshotter =
                Snapshotter::new(dom, &oracle, &mut entry.refs).in_frame(frame.0, epoch);
            let body = snapshotter.body(&options);
            (body.lines, body.total_elements)
        });
        for line in &mut lines {
            line.depth += depth;
        }
        let children: Vec<&FrameInfo> = frames
            .iter()
            .filter(|f| f.parent == Some(frame) && !f.popup && f.element.is_some())
            .collect();
        if children.is_empty() || depth > 64 {
            return Ok((lines, total));
        }
        // The frame each line hosts, if it is an `iframe` with a document.
        let hosts: Vec<Option<&FrameInfo>> = {
            let entry = self.tabs.get(&tab).expect("the tab is open");
            lines
                .iter()
                .map(|line| match &line.kind {
                    LineKind::Element { r, .. } => entry
                        .refs
                        .entry(*r)
                        .filter(|e| e.key.frame == frame.0)
                        .and_then(|e| children.iter().find(|c| c.element == Some(e.key.node)))
                        .copied(),
                    _ => None,
                })
                .collect()
        };
        let parent_url = state.url.borrow().clone();
        let mut out = Vec::with_capacity(lines.len());
        for (mut line, host) in lines.into_iter().zip(hosts) {
            let Some(child) = host else {
                out.push(line);
                continue;
            };
            let (child_lines, child_total) =
                self.frame_lines(tab, child.id, None, filter, extra, frames, line.depth + 1)?;
            total += child_total;
            if let LineKind::Element {
                attrs,
                has_children,
                ..
            } = &mut line.kind
            {
                attrs.push(("frame", format!("f{}", child.id.0)));
                if child.url.origin() != parent_url.origin()
                    && let Some(host) = child.url.host_str()
                {
                    attrs.push(("origin", host.to_string()));
                }
                *has_children |= !child_lines.is_empty();
            }
            out.push(line);
            out.extend(child_lines);
        }
        Ok((out, total))
    }

    /// The ref of the focused element, following focus into frames.
    fn focus_ref(&self, tab: u32, top: FrameId, frames: &[FrameInfo]) -> Option<u32> {
        let entry = self.tabs.get(&tab)?;
        let mut frame = top;
        for _ in 0..16 {
            let state = self.page.frame_state(frame)?;
            let node = agent::focused(state)?;
            if let Some(child) = frames
                .iter()
                .find(|f| f.parent == Some(frame) && f.element == Some(node))
            {
                frame = child.id;
                continue;
            }
            return entry.refs.get(RefKey {
                frame: frame.0,
                epoch: state.epoch,
                node,
            });
        }
        None
    }

    /// Notes where the logs stand at snapshot `id`.
    fn mark(&mut self, tab: u32, id: u64) {
        let Some(entry) = self.tabs.get(&tab) else {
            return;
        };
        let console = self
            .page
            .frame_state(entry.root)
            .map(|s| s.console_len())
            .unwrap_or(0);
        let mark = Mark {
            id,
            epoch: entry.doc_epoch,
            console,
            requests: self.page.net().requests_len(),
            events: entry.events.len(),
        };
        let entry = self.tabs.get_mut(&tab).expect("checked above");
        entry.marks.push_back(mark);
        while entry.marks.len() > MARKS {
            entry.marks.pop_front();
        }
    }

    /// Keeps a whole-page model to diff later snapshots against.
    fn remember(&mut self, tab: u32, filter: Filter, model: Model) {
        let version = self.shown_versions(tab);
        let Some(entry) = self.tabs.get_mut(&tab) else {
            return;
        };
        // The latest of each filter is all a diff compares with, and those
        // of a document the tab left are of no use. The ones replaced are
        // kept until the next snapshot, should this one be taken back.
        let mut replaced = Vec::new();
        for (at, stored) in std::mem::take(&mut entry.history).into_iter().enumerate() {
            if stored.filter != filter && stored.epoch == model.epoch {
                entry.history.push_back(stored);
            } else {
                replaced.push((at, stored));
            }
        }
        entry.history.push_back(Stored {
            filter,
            epoch: model.epoch,
            url: model.url,
            title: model.title,
            scroll: model.scroll,
            focus: model.focus,
            lines: model.lines,
            version,
        });
        if let Some(taken) = &mut entry.taken {
            taken.replaced = Some(replaced);
        }
    }

    /// Takes back snapshot `id` of `tab`, which no result showed: the next
    /// snapshot gets its number, and diffs start again from the one shown
    /// before it. Only the latest snapshot can be taken back.
    pub(crate) fn take_back(&mut self, tab: u32, id: u64) {
        let Some(entry) = self.tabs.get_mut(&tab) else {
            return;
        };
        if entry.next_snapshot != id + 1 {
            return;
        }
        let Some(taken) = entry.taken.take_if(|t| t.id == id) else {
            return;
        };
        entry.next_snapshot = id;
        entry.marks.retain(|m| m.id != id);
        if let Some(replaced) = taken.replaced {
            entry.history.pop_back();
            // In the order they stood, each goes back to its place.
            for (at, stored) in replaced {
                entry.history.insert(at, stored);
            }
        }
    }

    /// Gives back the number of snapshot `id` of `tab`, whose one change a
    /// status line showed: the page it saw stays the one to diff against.
    pub(super) fn unnumber(&mut self, tab: u32, id: u64) {
        let Some(entry) = self.tabs.get_mut(&tab) else {
            return;
        };
        if entry.next_snapshot == id + 1 {
            entry.next_snapshot = id;
            entry.marks.retain(|m| m.id != id);
            entry.taken = None;
        }
    }

    /// The `settled=` and `pending=` of a header.
    fn settledness(&self, tab: u32) -> (bool, Option<String>) {
        let settled = match self.tabs.get(&tab) {
            Some(t) => self.page.is_settled_in(t.root),
            None => self.page.is_settled(),
        };
        // A page not settled always says why, if only by how its run
        // stopped.
        let pending = match self.tabs.get(&tab).map(|t| t.root) {
            Some(_) if settled => None,
            Some(root) => self
                .page
                .pending_of(root)
                .and_then(|p| super::pending::summary(&p))
                .or_else(|| {
                    let stop = self.page.frame_report(root).map(|r| r.stop);
                    Some(super::pending::stop_word(stop).to_string())
                }),
            None => None,
        };
        (settled, pending)
    }

    fn challenge(&self, tab: u32) -> Option<String> {
        let root = self.tabs.get(&tab)?.root;
        (root == FrameId(0) && self.page.document().cloudflare_challenge)
            .then(|| "cloudflare".to_string())
    }

    /// A whole snapshot, rendered within the request's budget. Its header
    /// gives what is not the usual: a URL the status line did not, an
    /// unusual viewport, a scroll, focus, a filter other than the default,
    /// counts when the budget left some out, a page not settled.
    fn full_text(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
        full: Option<&str>,
    ) -> Result<(String, u64), Failure> {
        let model = self.model(tab, request.filter, request.extra, request.root.as_deref())?;
        self.full_text_of(tab, request, view, full, model)
    }

    /// [`GroupState::full_text`] of a model built already (for the request),
    /// with the snapshot's number.
    fn full_text_of(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
        full: Option<&str>,
        model: Model,
    ) -> Result<(String, u64), Failure> {
        let (settled, pending) = self.settledness(tab);
        let challenge = self.challenge(tab);
        let entry = self.tabs.get_mut(&tab).expect("model found the tab");
        let id = entry.take_id();
        let mut shown = model.lines.clone();
        if let Some(after) = &request.after {
            shown = after_item(&shown, after)?;
        }
        let fitted =
            catpaw_agent::budget::fit(&shown, view.format, token_bytes(request.max_tokens));
        let truncated = fitted.hidden;
        let body = catpaw_agent::snapshot::render_lines(&fitted.lines, view.format);
        let header = Header {
            id,
            tab: (view.tabs > 1).then(|| format!("t{}", entry.id)),
            url: (!request.url_shown).then(|| model.url.clone()),
            title: (!model.title.is_empty()).then(|| model.title.clone()),
            viewport: (model.viewport != default_viewport()).then_some(model.viewport),
            scroll: (model.scroll != (0, 0)).then_some(model.scroll),
            focus: model.focus,
            filter: (request.filter != Filter::Interesting).then_some(request.filter),
            root: model.root,
            nodes: (truncated > 0).then_some((shown.len() - truncated, model.total)),
            settled: (!settled).then_some(false),
            pending,
            challenge,
            budget_hit: truncated > 0,
            full: full.map(str::to_string),
            ..Header::default()
        };
        let mut text = header.render();
        text.push('\n');
        text.push_str(&body);
        if truncated > 0 {
            text.push_str(advice::SNAPSHOT_TRUNCATED);
            text.push('\n');
        }
        self.mark(tab, id);
        if request.root.is_none() && request.extra == ExtraAttrs::default() {
            self.remember(tab, request.filter, model);
        }
        Ok((text.trim_end().to_string(), id))
    }

    /// The tab's snapshot as text: header line, then the lines.
    pub(crate) fn snapshot_text(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<String, Failure> {
        self.full_text(tab, request, view, None)
            .map(|(text, _)| text)
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
            after: p.after,
            max_tokens: p.max_tokens.unwrap_or(SNAPSHOT_TOKENS).max(200),
            extra,
            url_shown: false,
            acted: None,
        };
        let text = if p.diff && request.root.is_none() {
            self.diff_or_full(tab, &request, view)?
        } else {
            self.snapshot_text(tab, &request, view)?
        };
        Ok(ToolOutput::ok(format!("ok snapshot\n{text}")))
    }

    /// What the page looks like after an action, as `mode` asks: what
    /// changed (the default), all of it, or nothing. `url_shown`: the
    /// status line gives the URL already.
    pub(super) fn page_view(
        &mut self,
        tab: u32,
        mode: Option<SnapshotMode>,
        view: View,
        url_shown: bool,
        acted: Option<u32>,
    ) -> Result<PageView, Failure> {
        if !self.tabs.contains_key(&tab) {
            return Ok(PageView::nothing());
        }
        let request = SnapRequest {
            url_shown,
            acted,
            ..SnapRequest::default()
        };
        match mode.unwrap_or(SnapshotMode::Diff) {
            SnapshotMode::None => Ok(PageView::nothing()),
            SnapshotMode::Full => self
                .full_text(tab, &request, view, None)
                .map(PageView::whole),
            SnapshotMode::Diff => self.diff_view(tab, &request, view),
        }
    }

    /// A diff of a page that shows what its last snapshot (with the same
    /// filter) showed: no changes, and no model built to find that out.
    fn unchanged_view(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<Option<PageView>, Failure> {
        if request.extra != ExtraAttrs::default() {
            return Ok(None);
        }
        let version = self.shown_versions(tab);
        let unchanged = self.tabs.get(&tab).is_some_and(|entry| {
            entry
                .history
                .iter()
                .rev()
                .find(|s| s.filter == request.filter)
                .is_some_and(|s| s.version == version)
        });
        if !unchanged {
            return Ok(None);
        }
        let (settled, pending) = self.settledness(tab);
        let challenge = self.challenge(tab);
        let entry = self.tabs.get_mut(&tab).expect("checked above");
        let id = entry.take_id();
        let header = Header {
            id,
            tab: (view.tabs > 1).then(|| format!("t{}", entry.id)),
            settled: (!settled).then_some(false),
            pending,
            challenge,
            stats: Some(catpaw_agent::diff(&[], &[]).stats()),
            ..Header::default()
        };
        let quiet = header.tab.is_none() && header.settled.is_none() && header.challenge.is_none();
        let text = header.render();
        self.mark(tab, id);
        Ok(Some(PageView {
            text: text.trim_end().to_string(),
            quiet: quiet.then(Vec::new),
            id: Some(id),
        }))
    }

    /// [`GroupState::diff_view`] as text.
    fn diff_or_full(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<String, Failure> {
        self.diff_view(tab, request, view).map(|v| v.text)
    }

    /// What changed since the last snapshot with the same filter; the whole
    /// snapshot when there is none to compare with, the document changed,
    /// or the changes are most of the page. The header gives what changed
    /// besides the lines (the URL, the title, scroll, focus) and a page not
    /// settled.
    fn diff_view(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<PageView, Failure> {
        if let Some(unchanged) = self.unchanged_view(tab, request, view)? {
            return Ok(unchanged);
        }
        // The whole page goes out in place of a diff built from the same
        // model: it is built once.
        let model = self.model(tab, request.filter, request.extra, None)?;
        let entry = self.tabs.get(&tab).expect("model found the tab");
        let baseline = entry
            .history
            .iter()
            .rev()
            .find(|s| s.filter == request.filter);
        let Some(baseline) = baseline else {
            return self
                .full_text_of(tab, request, view, Some("no-baseline"), model)
                .map(PageView::whole);
        };
        if baseline.epoch != model.epoch {
            return self
                .full_text_of(tab, request, view, Some("navigated"), model)
                .map(PageView::whole);
        }
        // Long names and texts are cut before a diff is given up for the
        // whole page.
        let budget = token_bytes(request.max_tokens);
        let whole = rendered_len(&model.lines, view.format);
        let diff =
            catpaw_agent::diff_within(&baseline.lines, &model.lines, budget.min(whole * 6 / 10));
        let diff_len: usize = diff.lines.iter().map(|l| l.len() + 1).sum();
        if diff_len * 10 > whole * 6 || diff_len > budget {
            return self
                .full_text_of(tab, request, view, Some("large"), model)
                .map(PageView::whole);
        }
        let (same_url, title_changed, scroll_from, focus_from) = (
            baseline.url == model.url,
            baseline.title != model.title,
            baseline.scroll,
            baseline.focus,
        );
        let (settled, pending) = self.settledness(tab);
        let challenge = self.challenge(tab);
        let entry = self.tabs.get_mut(&tab).expect("model found the tab");
        for &(old, new) in &diff.replaced {
            entry.refs.set_replaced(old, new);
        }
        let id = entry.take_id();
        let header = Header {
            id,
            tab: (view.tabs > 1).then(|| format!("t{}", entry.id)),
            url: (!same_url && !request.url_shown).then(|| model.url.clone()),
            title: title_changed.then(|| model.title.clone()),
            scroll: (model.scroll != scroll_from).then_some(model.scroll),
            focus: model
                .focus
                .filter(|&f| model.focus != focus_from && Some(f) != request.acted),
            settled: (!settled).then_some(false),
            pending,
            challenge,
            stats: Some(diff.stats()),
            ..Header::default()
        };
        let quiet = header.tab.is_none()
            && header.url.is_none()
            && header.title.is_none()
            && header.scroll.is_none()
            && header.focus.is_none()
            && header.settled.is_none()
            && header.challenge.is_none();
        let mut text = header.render();
        if !diff.is_empty() {
            text.push('\n');
            text.push_str(&diff.text());
        }
        self.mark(tab, id);
        self.remember(tab, request.filter, model);
        Ok(PageView {
            text: text.trim_end().to_string(),
            quiet: quiet.then(|| diff.lines.clone()),
            id: Some(id),
        })
    }
}
