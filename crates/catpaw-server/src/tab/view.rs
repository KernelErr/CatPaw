//! Snapshots: the page as lines, kept per tab to diff against, and shown
//! whole or as what changed.

use catpaw_agent::snapshot::{LineKind, render_line};
use catpaw_agent::{
    ExtraAttrs, Filter, Format, Header, RefKey, SnapLine, SnapshotOptions, Snapshotter,
};
use catpaw_dom::NodeId;
use catpaw_engine::FrameId;
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

/// Lines rendered within a byte budget: the text, and how many lines were
/// left out.
fn render_budgeted(lines: &[SnapLine], format: Format, max: usize) -> (String, usize) {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if out.len() >= max {
            let rest = lines.len() - i;
            let marker = SnapLine {
                depth: 0,
                kind: LineKind::Truncated(rest),
            };
            render_line(&mut out, &marker, format);
            out.push('\n');
            return (out, rest);
        }
        render_line(&mut out, line, format);
        out.push('\n');
    }
    (out, 0)
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
        let (frame, state) = self.root_state(tab)?;
        let page = &self.page;
        let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
        entry.sync(page);
        let epoch = entry.doc_epoch;
        let root_node: Option<(NodeId, u32)> = match root {
            Some(text) => Some(resolve_ref(page, &mut entry.refs, text, frame)?),
            None => None,
        };
        let url = state.url.borrow().to_string();
        let viewport = agent::viewport(&state);
        let (sx, sy) = agent::window_scroll(&state);
        let (lines, total, title, focus) = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let options = SnapshotOptions {
                filter,
                format: Format::Compact,
                root: root_node.map(|(node, _)| node),
                max_depth: None,
                max_chars: None,
                extra,
                ..SnapshotOptions::default()
            };
            let mut snapshotter =
                Snapshotter::new(dom, &oracle, &mut entry.refs).in_frame(frame.0, epoch);
            let body = snapshotter.body(&options);
            let title = snapshotter.title();
            let focus = agent::focused(&state).and_then(|node| {
                entry.refs.get(RefKey {
                    frame: frame.0,
                    epoch,
                    node,
                })
            });
            (body.lines, body.total_elements, title, focus)
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
            doc: entry.doc,
            root: root_node.map(|(_, r)| r),
        })
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
        let (body, truncated) =
            render_budgeted(&model.lines, view.format, token_bytes(request.max_tokens));
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
            nodes: Some((model.lines.len() - truncated, model.total)),
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
