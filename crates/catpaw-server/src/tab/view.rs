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
    GroupState, HISTORY, MARKS, Mark, SNAPSHOT_TOKENS, SnapRequest, Stored, View, resolve_ref,
    token_bytes,
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
    pub doc: u64,
    /// The ref of the subtree shown, when not the whole page.
    pub root: Option<u32>,
}

/// The lines after the item `after` of a subtree shown with `root`: the
/// rest of a long list.
fn after_item(lines: &[SnapLine], after: &str) -> Result<Vec<SnapLine>, Failure> {
    let wanted = catpaw_agent::RefTable::parse(after)
        .ok_or_else(|| Failure::bad_argument(format!("{after:?} is not a ref")))?;
    let at = lines
        .iter()
        .position(|l| l.depth == 0 && matches!(l.kind, LineKind::Element { r, .. } if r == wanted));
    let Some(at) = at else {
        return Err(Failure::bad_argument(format!(
            "e{wanted} is not an item of that root"
        )));
    };
    let next = lines[at + 1..]
        .iter()
        .position(|l| l.depth == 0)
        .map_or(lines.len(), |k| at + 1 + k);
    Ok(lines[next..].to_vec())
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
        let epoch = entry.doc_epoch;
        let doc = entry.doc;
        let root_node: Option<(FrameId, NodeId, u32)> = match root {
            Some(text) => Some(resolve_ref(page, &mut entry.refs, text, &allowed)?),
            None => None,
        };
        let url = state.url.borrow().to_string();
        let viewport = agent::viewport(&state);
        let (sx, sy) = agent::window_scroll(&state);
        let start = root_node.map(|(frame, _, _)| frame).unwrap_or(top);
        let (lines, total) = self.frame_lines(
            tab,
            start,
            root_node.map(|(_, node, _)| node),
            filter,
            extra,
            &frames,
            0,
        )?;
        let title = super::title_of(&state);
        let focus = self.focus_ref(tab, top, &frames);
        Ok(Model {
            lines,
            total,
            title,
            url,
            focus,
            scroll: (sx.round() as i64, sy.round() as i64),
            viewport,
            epoch,
            doc,
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
    fn remember(&mut self, tab: u32, id: u64, filter: Filter, model: Model) {
        let Some(entry) = self.tabs.get_mut(&tab) else {
            return;
        };
        entry.history.push_back(Stored {
            id,
            filter,
            epoch: model.epoch,
            doc: model.doc,
            url: model.url,
            title: model.title,
            lines: model.lines,
        });
        while entry.history.len() > HISTORY {
            entry.history.pop_front();
        }
    }

    /// The `settled=` and `pending=` of a header.
    fn settledness(&self, tab: u32) -> (bool, Option<String>) {
        let settled = self.page.is_settled();
        let pending = if settled {
            None
        } else {
            self.tabs
                .get(&tab)
                .and_then(|t| self.page.pending_of(t.root))
                .and_then(|p| super::pending::summary(&p))
        };
        (settled, pending)
    }

    fn challenge(&self, tab: u32) -> Option<String> {
        let root = self.tabs.get(&tab)?.root;
        (root == FrameId(0) && self.page.document().cloudflare_challenge)
            .then(|| "cloudflare".to_string())
    }

    /// A whole snapshot, rendered within the request's budget.
    fn full_text(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
        full: Option<&str>,
        navigated_from: Option<u64>,
    ) -> Result<String, Failure> {
        let model = self.model(tab, request.filter, request.extra, request.root.as_deref())?;
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
            tab: Some(format!("t{}", entry.id)),
            doc: Some(model.doc),
            navigated_from,
            url: Some(model.url.clone()),
            title: Some(model.title.clone()),
            viewport: Some(model.viewport),
            scroll: Some(model.scroll),
            focus: model.focus,
            filter: Some(request.filter),
            root: model.root,
            nodes: Some((shown.len() - truncated, model.total)),
            settled: Some(settled),
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
            self.remember(tab, id, request.filter, model);
        }
        Ok(text.trim_end().to_string())
    }

    /// The tab's snapshot as text: header line, then the lines.
    pub(crate) fn snapshot_text(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<String, Failure> {
        self.full_text(tab, request, view, None, None)
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
        };
        let text = if p.diff && request.root.is_none() {
            self.diff_or_full(tab, &request, view)?
        } else {
            self.snapshot_text(tab, &request, view)?
        };
        Ok(ToolOutput::ok(format!("ok snapshot\n{text}")))
    }

    /// What the page looks like after an action, as `mode` asks: what
    /// changed (the default), all of it, or nothing.
    pub(super) fn page_view(
        &mut self,
        tab: u32,
        mode: Option<SnapshotMode>,
        view: View,
    ) -> Result<String, Failure> {
        if !self.tabs.contains_key(&tab) {
            return Ok(String::new());
        }
        let request = SnapRequest::default();
        match mode.unwrap_or(SnapshotMode::Diff) {
            SnapshotMode::None => Ok(String::new()),
            SnapshotMode::Full => self.snapshot_text(tab, &request, view),
            SnapshotMode::Diff => self.diff_or_full(tab, &request, view),
        }
    }

    /// What changed since the last snapshot with the same filter; the whole
    /// snapshot when there is none to compare with, the document changed,
    /// or the changes are most of the page.
    fn diff_or_full(
        &mut self,
        tab: u32,
        request: &SnapRequest,
        view: View,
    ) -> Result<String, Failure> {
        let model = self.model(tab, request.filter, request.extra, None)?;
        let entry = self.tabs.get(&tab).expect("model found the tab");
        let baseline = entry
            .history
            .iter()
            .rev()
            .find(|s| s.filter == request.filter);
        let Some(baseline) = baseline else {
            return self.full_text(tab, request, view, Some("no-baseline"), None);
        };
        if baseline.epoch != model.epoch {
            let from = baseline.doc;
            return self.full_text(tab, request, view, Some("navigated"), Some(from));
        }
        let diff = catpaw_agent::diff(&baseline.lines, &model.lines);
        let budget = token_bytes(request.max_tokens);
        let diff_len: usize = diff.lines.iter().map(|l| l.len() + 1).sum();
        if diff_len * 10 > rendered_len(&model.lines, view.format) * 6 || diff_len > budget {
            return self.full_text(tab, request, view, Some("large"), None);
        }
        let (baseline_id, same_url, title_changed) = (
            baseline.id,
            baseline.url == model.url,
            baseline.title != model.title,
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
            diff_from: Some(baseline_id),
            tab: Some(format!("t{}", entry.id)),
            doc: Some(model.doc),
            url: Some(model.url.clone()),
            same_url,
            title: title_changed.then(|| model.title.clone()),
            scroll: Some(model.scroll),
            focus: model.focus,
            settled: Some(settled),
            pending,
            challenge,
            stats: Some(diff.stats()),
            ..Header::default()
        };
        let mut text = header.render();
        if !diff.is_empty() {
            text.push('\n');
            text.push_str(&diff.text());
        }
        self.mark(tab, id);
        self.remember(tab, id, request.filter, model);
        Ok(text.trim_end().to_string())
    }
}
