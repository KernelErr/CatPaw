//! CST (CatPaw Snapshot Text): the page as agents read it, one line per
//! element with a stable ref (ADR 0005).
//!
//! Compact format (the default):
//!
//! ```text
//! # s1 url=https://shop.example/cart title="Cart" filter=interesting nodes=8/41
//! e1 banner
//!   e2 searchbox "Search products" [value=""]
//!   e3 button "Search"
//! e4 main
//!   e5 heading "Your cart" [level=1]
//!   text: Free shipping on orders over $50
//! ```
//!
//! The aria format is Playwright's aria-snapshot syntax with refs:
//! `- searchbox "Search products" [ref=e2] [value=""]`.

use std::fmt::Write as _;

use catpaw_dom::{Dom, ElementData, NodeId, NodeKind};
use url::Url;

use crate::a11y::{
    LabelIndex, collapse_whitespace, heading_level, is_interactive, is_landmark, name_for,
    names_from_content, role_for, subtree_text,
};
use crate::refs::{RefKey, RefTable};
use crate::visibility::{StyleOracle, is_hidden};

/// How much of the tree to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Every node with a role, plus generic containers.
    All,
    /// Interactive elements, headings, landmarks, structure and text;
    /// generic wrappers are elided.
    #[default]
    Interesting,
    /// Interactive elements and the headings/landmarks that organise them.
    Interactive,
}

impl Filter {
    pub fn as_str(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Interesting => "interesting",
            Filter::Interactive => "interactive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Filter::All),
            "interesting" => Some(Filter::Interesting),
            "interactive" => Some(Filter::Interactive),
            _ => None,
        }
    }
}

/// How lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// `e12 link "upvote"`: the ref first, no list markers.
    #[default]
    Compact,
    /// `- link "upvote" [ref=e12]`: Playwright's aria-snapshot syntax.
    Aria,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Compact => "compact",
            Format::Aria => "aria",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "compact" => Some(Format::Compact),
            "aria" => Some(Format::Aria),
            _ => None,
        }
    }
}

/// Attributes shown only on request: they cost tokens and rarely change
/// what an agent does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtraAttrs {
    /// Link targets.
    pub href: bool,
    /// Image and frame sources.
    pub src: bool,
    /// Accessible descriptions (`aria-describedby`, `title`).
    pub description: bool,
}

impl ExtraAttrs {
    /// Reads a comma-separated list such as `href,src`; `none` is empty.
    pub fn parse_list(list: &str) -> Result<Self, String> {
        let mut out = Self::default();
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match item {
                "href" => out.href = true,
                "src" => out.src = true,
                "description" => out.description = true,
                "none" => {}
                other => return Err(format!("unknown attribute `{other}`")),
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub filter: Filter,
    pub format: Format,
    /// Subtree to snapshot; the document by default.
    pub root: Option<NodeId>,
    pub max_depth: Option<usize>,
    /// Soft character budget; the output is cut with a `[truncated ...]` line.
    pub max_chars: Option<usize>,
    /// Longest accessible name emitted before truncation with an ellipsis.
    pub max_name_len: usize,
    pub extra: ExtraAttrs,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            filter: Filter::Interesting,
            format: Format::Compact,
            root: None,
            max_depth: None,
            max_chars: None,
            max_name_len: 100,
            extra: ExtraAttrs::default(),
        }
    }
}

/// Longest text leaf shown; longer prose is for the read views.
const MAX_TEXT_LEN: usize = 200;
/// Longest control value shown.
const MAX_VALUE_LEN: usize = 80;

/// Canonical attribute order: the same node always prints the same bytes.
const ATTR_ORDER: &[&str] = &[
    "level",
    "value",
    "placeholder",
    "checked",
    "selected",
    "pressed",
    "expanded",
    "disabled",
    "readonly",
    "required",
    "invalid",
    "current",
    "min",
    "max",
    "options",
    "type",
    "clickable",
    "href",
    "src",
    "description",
    "frame",
    "origin",
    "shadow",
    "collapsed",
    "more",
];

pub(crate) fn attr_rank(key: &str) -> usize {
    ATTR_ORDER
        .iter()
        .position(|k| *k == key)
        .unwrap_or(ATTR_ORDER.len())
}

/// One line of a snapshot, as data: what diffs compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapLine {
    pub depth: u16,
    pub kind: LineKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Element {
        r: u32,
        role: &'static str,
        name: String,
        attrs: Vec<(&'static str, String)>,
        has_children: bool,
        /// The element's only child, a text, shown on its line
        /// (`e13 paragraph: Ready`).
        text: Option<String>,
    },
    Text(String),
    /// Nodes left out for the budget.
    Truncated(usize),
    /// The rest of a long list, left out for the budget: how many lines,
    /// and the ref of the last item shown.
    More {
        count: usize,
        after: Option<u32>,
    },
}

/// Writes one line, without the newline.
pub fn render_line(out: &mut String, line: &SnapLine, format: Format) {
    for _ in 0..line.depth {
        out.push_str("  ");
    }
    match &line.kind {
        LineKind::Element {
            r,
            role,
            name,
            attrs,
            has_children,
            text,
        } => {
            match format {
                Format::Compact => {
                    let _ = write!(out, "e{r} {role}");
                }
                Format::Aria => {
                    let _ = write!(out, "- {role}");
                }
            }
            if !name.is_empty() {
                out.push(' ');
                out.push_str(&quote(name));
            }
            if format == Format::Aria {
                let _ = write!(out, " [ref=e{r}]");
            }
            for (key, value) in attrs {
                write_attr(out, key, value);
            }
            if let Some(text) = text {
                out.push_str(": ");
                out.push_str(text);
            } else if format == Format::Aria && *has_children {
                out.push(':');
            }
        }
        LineKind::Text(text) => {
            if format == Format::Aria {
                out.push_str("- ");
            }
            out.push_str("text: ");
            out.push_str(text);
        }
        LineKind::Truncated(n) => {
            if format == Format::Aria {
                out.push_str("- ");
            }
            let _ = write!(out, "[truncated: {n} more nodes]");
        }
        LineKind::More { count, after } => {
            if format == Format::Aria {
                out.push_str("- ");
            }
            match after {
                Some(r) => {
                    let _ = write!(out, "[more={count} nodes after e{r}]");
                }
                None => {
                    let _ = write!(out, "[more={count} nodes]");
                }
            }
        }
    }
}

/// One `[key]` or `[key=value]` attribute.
pub fn write_attr(out: &mut String, key: &str, value: &str) {
    if value.is_empty() && key != "value" {
        let _ = write!(out, " [{key}]");
    } else {
        let _ = write!(out, " [{key}={}]", attr_value(value));
    }
}

/// All lines, each ending in a newline.
pub fn render_lines(lines: &[SnapLine], format: Format) -> String {
    let mut out = String::new();
    for line in lines {
        render_line(&mut out, line, format);
        out.push('\n');
    }
    out
}

/// The first line of a snapshot. Keys appear in a fixed order and only
/// when they carry something.
#[derive(Debug, Clone, Default)]
pub struct Header {
    pub id: u64,
    pub diff_from: Option<u64>,
    pub tab: Option<String>,
    pub doc: Option<u64>,
    pub navigated_from: Option<u64>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub viewport: Option<(u32, u32)>,
    pub scroll: Option<(i64, i64)>,
    pub focus: Option<u32>,
    pub filter: Option<Filter>,
    pub root: Option<u32>,
    pub nodes: Option<(usize, usize)>,
    pub settled: Option<bool>,
    pub pending: Option<String>,
    pub challenge: Option<String>,
    pub budget_hit: bool,
    /// Why a full snapshot was returned where a diff was asked for.
    pub full: Option<String>,
    /// The URL is the one of the snapshot diffed from (`url=(same)`).
    pub same_url: bool,
    /// A diff's counts (`changed=3 added=1 …`, or `no changes`).
    pub stats: Option<String>,
}

impl Header {
    pub fn render(&self) -> String {
        let mut out = format!("# s{}", self.id);
        if let Some(from) = self.diff_from {
            let _ = write!(out, " diff-from=s{from}");
        }
        if let Some(tab) = &self.tab {
            let _ = write!(out, " tab={tab}");
        }
        if let Some(doc) = self.doc {
            let _ = write!(out, " doc=d{doc}");
        }
        if let Some(from) = self.navigated_from {
            let _ = write!(out, " navigated-from=d{from}");
        }
        if self.same_url {
            out.push_str(" url=(same)");
        } else if let Some(url) = &self.url {
            let _ = write!(out, " url={}", truncate(url, 120));
        }
        if let Some(title) = &self.title {
            let _ = write!(out, " title={}", quote(&truncate(title, 80)));
        }
        if let Some((w, h)) = self.viewport {
            let _ = write!(out, " vp={w}x{h}");
        }
        if let Some((x, y)) = self.scroll {
            let _ = write!(out, " scroll={x},{y}");
        }
        if let Some(focus) = self.focus {
            let _ = write!(out, " focus=e{focus}");
        }
        if let Some(filter) = self.filter {
            let _ = write!(out, " filter={}", filter.as_str());
        }
        if let Some(root) = self.root {
            let _ = write!(out, " root=e{root}");
        }
        if let Some((emitted, total)) = self.nodes {
            let _ = write!(out, " nodes={emitted}/{total}");
        }
        if let Some(settled) = self.settled {
            out.push_str(if settled {
                " settled=yes"
            } else {
                " settled=no"
            });
        }
        if let Some(pending) = &self.pending {
            let _ = write!(out, " pending={}", attr_value(pending));
        }
        if let Some(challenge) = &self.challenge {
            let _ = write!(out, " challenge={challenge}");
        }
        if self.budget_hit {
            out.push_str(" budget=hit");
        }
        if let Some(full) = &self.full {
            let _ = write!(out, " full={full}");
        }
        if let Some(stats) = &self.stats {
            out.push(' ');
            out.push_str(stats);
        }
        out
    }
}

/// The body of a snapshot: its lines and how much of the page they cover.
#[derive(Debug, Clone, Default)]
pub struct SnapBody {
    pub lines: Vec<SnapLine>,
    /// The text of the lines, as rendered in the requested format.
    pub text: String,
    pub emitted: usize,
    pub total_elements: usize,
    pub truncated_nodes: usize,
}

/// A rendered snapshot: header and body.
#[derive(Debug)]
pub struct Snapshot {
    pub id: u64,
    pub text: String,
    pub lines: Vec<SnapLine>,
    pub emitted_nodes: usize,
    pub total_elements: usize,
    pub truncated: bool,
}

#[derive(Debug)]
struct AxNode {
    node: NodeId,
    /// `None` is generic; `Some("none")` is presentational.
    role: Option<&'static str>,
    name: String,
    attrs: Vec<(&'static str, String)>,
    interactive: bool,
    children: Vec<AxNode>,
    /// Set for text leaves.
    text: Option<String>,
    /// For a text leaf: a block starts before it, or ends after it, so it
    /// is not joined with the text on that side.
    breaks_before: bool,
    breaks_after: bool,
}

impl AxNode {
    fn text_leaf(text: String) -> Self {
        Self {
            node: NodeId::default(),
            role: None,
            name: String::new(),
            attrs: Vec::new(),
            interactive: false,
            children: Vec::new(),
            text: Some(text),
            breaks_before: false,
            breaks_after: false,
        }
    }

    fn count(&self) -> usize {
        1 + self.children.iter().map(AxNode::count).sum::<usize>()
    }

    fn has_interactive(&self) -> bool {
        self.children
            .iter()
            .any(|c| c.interactive || c.has_interactive())
    }
}

/// Builds snapshots of one document, allocating refs from a [`RefTable`].
pub struct Snapshotter<'a> {
    dom: &'a Dom,
    oracle: &'a dyn StyleOracle,
    refs: &'a mut RefTable,
    labels: LabelIndex,
    document_url: Option<Url>,
    frame: u32,
    epoch: u64,
    next_id: u64,
}

impl<'a> Snapshotter<'a> {
    pub fn new(dom: &'a Dom, oracle: &'a dyn StyleOracle, refs: &'a mut RefTable) -> Self {
        Self {
            dom,
            oracle,
            refs,
            labels: LabelIndex::build(dom),
            document_url: dom.url().cloned(),
            frame: 0,
            epoch: 0,
            next_id: 1,
        }
    }

    /// Refs from this snapshotter name nodes of `frame`'s document of
    /// `epoch` (see [`RefKey`]).
    pub fn in_frame(mut self, frame: u32, epoch: u64) -> Self {
        self.frame = frame;
        self.epoch = epoch;
        self
    }

    /// The document title.
    pub fn title(&self) -> String {
        self.dom
            .descendants(self.dom.document())
            .find(|&n| self.dom.is_html_element(n, "title"))
            .map(|n| collapse_whitespace(&self.dom.text_content(n)))
            .unwrap_or_default()
    }

    /// The lines of a snapshot, without a header.
    pub fn body(&mut self, options: &SnapshotOptions) -> SnapBody {
        let root = options.root.unwrap_or(self.dom.document());
        let total_elements = self
            .dom
            .descendants(root)
            .filter(|&n| self.dom.is_element(n))
            .count();

        let mut forest = self.build_children(root, options);
        if options.filter != Filter::All {
            forest = forest.into_iter().flat_map(elide_generic).collect();
        }
        tidy(&mut forest);
        if options.filter == Filter::Interactive {
            forest = forest.into_iter().flat_map(prune_non_interactive).collect();
        }

        let parent = options
            .root
            .filter(|&r| r != self.dom.document())
            .and_then(|r| self.refs.get(self.key(r)));
        let mut emitter = Emitter {
            refs: self.refs,
            frame: self.frame,
            epoch: self.epoch,
            options,
            lines: Vec::new(),
            text: String::new(),
            emitted: 0,
            truncated_nodes: 0,
        };
        for node in &forest {
            emitter.emit(node, 0, parent);
        }
        if emitter.truncated_nodes > 0 {
            let line = SnapLine {
                depth: 0,
                kind: LineKind::Truncated(emitter.truncated_nodes),
            };
            render_line(&mut emitter.text, &line, options.format);
            emitter.text.push('\n');
            emitter.lines.push(line);
        }
        SnapBody {
            lines: emitter.lines,
            text: emitter.text,
            emitted: emitter.emitted,
            total_elements,
            truncated_nodes: emitter.truncated_nodes,
        }
    }

    /// A whole snapshot with a short header (url, title, filter, counts):
    /// what the command line prints.
    pub fn snapshot(&mut self, options: &SnapshotOptions) -> Snapshot {
        let body = self.body(options);
        let header = Header {
            id: self.next_id,
            url: self.document_url.as_ref().map(Url::to_string),
            title: Some(self.title()),
            filter: Some(options.filter),
            nodes: Some((body.emitted, body.total_elements)),
            ..Header::default()
        };
        let id = self.next_id;
        self.next_id += 1;
        let mut text = header.render();
        text.push('\n');
        text.push_str(&body.text);
        Snapshot {
            id,
            text,
            lines: body.lines,
            emitted_nodes: body.emitted,
            total_elements: body.total_elements,
            truncated: body.truncated_nodes > 0,
        }
    }

    fn key(&self, node: NodeId) -> RefKey {
        RefKey {
            frame: self.frame,
            epoch: self.epoch,
            node,
        }
    }

    fn build_children(&self, parent: NodeId, options: &SnapshotOptions) -> Vec<AxNode> {
        let mut out = Vec::new();
        // Adjacent text nodes render as one text (`$<!-- -->29.99` is
        // `$29.99`): they are joined before whitespace is collapsed.
        let mut run = String::new();
        // A block (even an empty one) ends the text before it and starts
        // the text after it on a line of its own.
        let mut after_block = false;
        let flush = |run: &mut String, out: &mut Vec<AxNode>, after_block: &mut bool| {
            let t = collapse_whitespace(run);
            if !t.is_empty() {
                let mut leaf = AxNode::text_leaf(t);
                leaf.breaks_before = std::mem::take(after_block);
                out.push(leaf);
            }
            run.clear();
        };
        for child in self.dom.rendered_children(parent) {
            match self.dom.kind(child) {
                NodeKind::Text(t) => run.push_str(t),
                NodeKind::Element(el) => {
                    if is_hidden(self.dom, child, self.oracle) {
                        continue;
                    }
                    flush(&mut run, &mut out, &mut after_block);
                    let block = self.is_block(child, el);
                    if block {
                        if let Some(last) = out.last_mut().filter(|n| n.text.is_some()) {
                            last.breaks_after = true;
                        }
                        after_block = true;
                    } else {
                        after_block = false;
                    }
                    let mut node = self.build_element(child, el, options);
                    if block {
                        mark_block_edges(&mut node.children);
                    }
                    out.push(node);
                }
                _ => {}
            }
        }
        flush(&mut run, &mut out, &mut after_block);
        out
    }

    /// Whether an element is laid out as a block of its own.
    fn is_block(&self, id: NodeId, el: &ElementData) -> bool {
        self.oracle
            .is_block_level(self.dom, id)
            .unwrap_or_else(|| el.is_html() && is_block_element(&el.name.local))
    }

    fn build_element(&self, id: NodeId, el: &ElementData, options: &SnapshotOptions) -> AxNode {
        let role = role_for(self.dom, id);
        let name = name_for(self.dom, id, role, self.oracle, &self.labels);
        let mut attrs = self.attrs_for(id, el, role, &name, options);
        let role_str = role.unwrap_or("");
        let mut interactive = is_interactive(role_str)
            || el.has_attr("contenteditable")
            || el.has_attr("onclick")
            || el
                .attr("tabindex")
                .is_some_and(|t| t.trim().parse::<i32>().is_ok_and(|v| v >= 0));

        // Name-from-content roles whose subtree is plain text have no children
        // to show: the name carries it.
        let mut children = self.build_children(id, options);
        if role.is_some_and(names_from_content)
            && children.iter().all(|c| c.text.is_some())
            && !name.is_empty()
        {
            children.clear();
        }
        if el.is_html() && matches!(&*el.name.local, "select" | "textarea") {
            children.clear();
        }
        if role == Some("img") {
            children.clear();
        }

        // A generic element styled or scripted as something to click, with
        // nothing clickable inside it: the "clickable div" of single-page
        // apps. It gets a ref so that it can be acted on.
        let generic = matches!(role, None | Some("generic") | Some("none"));
        if generic {
            let pointer = self.oracle.is_pointer_cursor(self.dom, id)
                && !self
                    .dom
                    .parent_element(id)
                    .is_some_and(|p| self.oracle.is_pointer_cursor(self.dom, p));
            let listens =
                el.has_attr("onclick") || self.oracle.has_activation_listener(self.dom, id);
            let node_children_interactive = children
                .iter()
                .any(|c| c.interactive || c.has_interactive());
            // A label hands its clicks to its control, which is shown in
            // its own right: it is not one more thing to click (and its
            // text would read as a second target).
            let label_of_shown_control = el.is_html()
                && &*el.name.local == "label"
                && label_control(self.dom, id)
                    .is_some_and(|control| !is_hidden(self.dom, control, self.oracle));
            if (pointer || listens) && !node_children_interactive && !label_of_shown_control {
                interactive = true;
                attrs.push(("clickable", String::new()));
                attrs.sort_by_key(|(k, _)| attr_rank(k));
            }
        }

        AxNode {
            node: id,
            role,
            name,
            attrs,
            interactive,
            children,
            text: None,
            breaks_before: false,
            breaks_after: false,
        }
    }

    fn attrs_for(
        &self,
        id: NodeId,
        el: &ElementData,
        role: Option<&str>,
        name: &str,
        options: &SnapshotOptions,
    ) -> Vec<(&'static str, String)> {
        let mut attrs = Vec::new();
        let role = role.unwrap_or("");
        let local = &*el.name.local;
        let input_type = el
            .attr("type")
            .map(|t| t.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "text".to_string());

        if role == "heading"
            && let Some(level) = heading_level(el)
        {
            attrs.push(("level", level.to_string()));
        }
        if matches!(
            role,
            "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
        ) {
            let native = el.is_html() && local == "input";
            let checked = if native {
                self.oracle
                    .is_checked(self.dom, id)
                    .unwrap_or_else(|| el.has_attr("checked"))
            } else {
                el.attr("aria-checked")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
            };
            let mixed = el
                .attr("aria-checked")
                .is_some_and(|v| v.eq_ignore_ascii_case("mixed"))
                || el.has_attr("indeterminate");
            if mixed {
                attrs.push(("checked", "mixed".to_string()));
            } else if checked {
                attrs.push(("checked", String::new()));
            }
        }
        if el.has_attr("disabled")
            || el
                .attr("aria-disabled")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            attrs.push(("disabled", String::new()));
        }
        if let Some(expanded) = el.attr("aria-expanded") {
            if expanded.eq_ignore_ascii_case("true") {
                attrs.push(("expanded", String::new()));
            }
        } else if local == "details" && el.has_attr("open") {
            attrs.push(("expanded", String::new()));
        }
        if el
            .attr("aria-pressed")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            attrs.push(("pressed", String::new()));
        }
        let selected = if role == "option" && el.is_html() && local == "option" {
            self.oracle
                .is_option_selected(self.dom, id)
                .unwrap_or_else(|| el.has_attr("selected"))
        } else {
            el.attr("aria-selected")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        };
        if selected {
            attrs.push(("selected", String::new()));
        }
        if el.has_attr("required")
            || el
                .attr("aria-required")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            attrs.push(("required", String::new()));
        }
        if el.has_attr("readonly")
            || el
                .attr("aria-readonly")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
            attrs.push(("readonly", String::new()));
        }
        if el
            .attr("aria-invalid")
            .is_some_and(|v| !v.eq_ignore_ascii_case("false") && !v.is_empty())
        {
            attrs.push(("invalid", String::new()));
        }
        if let Some(current) = el.attr("aria-current")
            && !current.eq_ignore_ascii_case("false")
            && !current.is_empty()
        {
            attrs.push(("current", current.to_string()));
        }

        // Values.
        match role {
            "textbox" | "searchbox" | "spinbutton" | "slider" => {
                let value = self.oracle.control_value(self.dom, id).unwrap_or_else(|| {
                    if local == "textarea" {
                        self.dom.text_content(id)
                    } else {
                        el.attr("value").unwrap_or("").to_string()
                    }
                });
                let value = if input_type == "password" && !value.is_empty() {
                    "***".to_string()
                } else {
                    truncate(&collapse_whitespace(&value), MAX_VALUE_LEN)
                };
                let empty = value.is_empty();
                if !empty {
                    attrs.push(("value", value));
                }
                if empty
                    && let Some(p) = el
                        .attr("placeholder")
                        .or_else(|| el.attr("aria-placeholder"))
                {
                    let p = collapse_whitespace(p);
                    if !p.is_empty() && p != name {
                        attrs.push(("placeholder", p));
                    }
                }
                if role == "spinbutton" || role == "slider" {
                    for key in ["min", "max"] {
                        if let Some(v) = el.attr(key) {
                            attrs.push((if key == "min" { "min" } else { "max" }, v.to_string()));
                        }
                    }
                }
            }
            "combobox" | "listbox" if local == "select" => {
                let selected = self.selected_option_text(id);
                if !selected.is_empty() {
                    attrs.push(("value", truncate(&selected, MAX_VALUE_LEN)));
                }
                let count = self
                    .dom
                    .descendants(id)
                    .filter(|&n| self.dom.is_html_element(n, "option"))
                    .count();
                attrs.push(("options", count.to_string()));
            }
            "link" if options.extra.href => {
                if let Some(href) = el.attr("href") {
                    attrs.push(("href", self.display_href(href)));
                }
            }
            "img" if options.extra.src => {
                if let Some(src) = el.attr("src")
                    && !src.starts_with("data:")
                {
                    attrs.push(("src", truncate(src, 80)));
                }
            }
            "iframe" if options.extra.src => {
                if let Some(src) = el.attr("src") {
                    attrs.push(("src", truncate(src, 100)));
                }
            }
            _ => {}
        }
        if role == "button" && local == "input" && input_type == "file" {
            attrs.push(("type", "file".to_string()));
        }
        if options.extra.description
            && let Some(description) = self.description(el, name)
        {
            attrs.push(("description", truncate(&description, 100)));
        }
        attrs.sort_by_key(|(k, _)| attr_rank(k));
        attrs
    }

    /// `aria-description`, the text of `aria-describedby`, or a `title`
    /// that is not already the name.
    fn description(&self, el: &ElementData, name: &str) -> Option<String> {
        if let Some(d) = el.attr("aria-description") {
            let d = collapse_whitespace(d);
            if !d.is_empty() {
                return Some(d);
            }
        }
        if let Some(ids) = el.attr("aria-describedby") {
            let text: Vec<String> = ids
                .split_ascii_whitespace()
                .filter_map(|target| {
                    self.dom
                        .descendants(self.dom.document())
                        .find(|&n| self.dom.attr(n, "id") == Some(target))
                })
                .map(|n| subtree_text(self.dom, n, self.oracle))
                .filter(|s| !s.is_empty())
                .collect();
            if !text.is_empty() {
                return Some(text.join(" "));
            }
        }
        el.attr("title")
            .map(collapse_whitespace)
            .filter(|t| !t.is_empty() && t != name)
    }

    fn selected_option_text(&self, select: NodeId) -> String {
        let chosen = match self.oracle.displayed_options(self.dom, select) {
            Some(shown) => shown.first().copied(),
            None => {
                let options: Vec<NodeId> = self
                    .dom
                    .descendants(select)
                    .filter(|&n| self.dom.is_html_element(n, "option"))
                    .collect();
                options
                    .iter()
                    .copied()
                    .find(|&o| self.dom.attr(o, "selected").is_some())
                    .or_else(|| options.first().copied())
            }
        };
        chosen
            .map(|o| {
                self.dom
                    .attr(o, "label")
                    .map(collapse_whitespace)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| subtree_text(self.dom, o, self.oracle))
            })
            .unwrap_or_default()
    }

    /// Links show their path on the same origin and the full URL elsewhere.
    fn display_href(&self, href: &str) -> String {
        let href = href.trim();
        let Some(base) = &self.document_url else {
            return truncate(href, 100);
        };
        match base.join(href) {
            Ok(resolved) => {
                if resolved.origin() == base.origin() {
                    let mut s = resolved.path().to_string();
                    if let Some(q) = resolved.query() {
                        s.push('?');
                        s.push_str(q);
                    }
                    if let Some(f) = resolved.fragment() {
                        s.push('#');
                        s.push_str(f);
                    }
                    truncate(&s, 100)
                } else {
                    truncate(resolved.as_str(), 100)
                }
            }
            Err(_) => truncate(href, 100),
        }
    }
}

/// Cuts `s` to `max` characters, ending in an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// A double-quoted string with `"`, `\` and newlines escaped.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// An attribute value, quoted only when it has to be.
pub fn attr_value(v: &str) -> String {
    let needs_quotes = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == ']' || c == '[');
    if needs_quotes {
        quote(v)
    } else {
        v.to_string()
    }
}

/// Characters that separate items rather than say anything.
const SEPARATORS: &[char] = &['|', '·', '•', '›', '»', '—', '–'];

/// A text leaf as worth showing: `None` when it has no letter or digit;
/// otherwise without leading or trailing separators and unbalanced
/// brackets (the `)` that closes a link's parenthesis, the `(` that opens
/// one), cut to the text budget.
pub fn tidy_text(text: &str) -> Option<String> {
    if !text.chars().any(char::is_alphanumeric) {
        return None;
    }
    let count = |s: &str, c: char| s.chars().filter(|&x| x == c).count();
    let mut s = text.trim();
    loop {
        let before = s;
        if let Some(c) = s.chars().next() {
            let unbalanced = match c {
                ')' => count(s, ')') > count(s, '('),
                ']' => count(s, ']') > count(s, '['),
                _ => false,
            };
            if SEPARATORS.contains(&c) || unbalanced || c == ',' || c == ';' {
                s = s[c.len_utf8()..].trim_start();
            }
        }
        if let Some(c) = s.chars().last() {
            let unbalanced = match c {
                '(' => count(s, '(') > count(s, ')'),
                '[' => count(s, '[') > count(s, ']'),
                _ => false,
            };
            if SEPARATORS.contains(&c) || unbalanced {
                s = s[..s.len() - c.len_utf8()].trim_end();
            }
        }
        if s == before {
            break;
        }
    }
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// A long text as a snapshot over its budget shows it: its start, and how
/// much more there is (`… [+1830 chars]`). `None` when it is short.
pub fn cap_text(text: &str) -> Option<String> {
    let count = text.chars().count();
    (count > MAX_TEXT_LEN).then(|| {
        let shown = MAX_TEXT_LEN - 1;
        let mut out: String = text.chars().take(shown).collect();
        out.push_str(&format!("… [+{} chars]", count - shown));
        out
    })
}

/// The control a `<label>` is for: the element its `for` names, else the
/// first form control inside it.
fn label_control(dom: &Dom, label: NodeId) -> Option<NodeId> {
    let labelable = |n: NodeId| match dom.kind(n) {
        NodeKind::Element(el) if el.is_html() => match &*el.name.local {
            "input" => !el
                .attr("type")
                .is_some_and(|t| t.trim().eq_ignore_ascii_case("hidden")),
            "select" | "textarea" | "button" | "meter" | "output" | "progress" => true,
            _ => false,
        },
        _ => false,
    };
    match dom.attr(label, "for") {
        Some(target) => dom
            .descendants(dom.document())
            .find(|&n| dom.attr(n, "id") == Some(target) && labelable(n)),
        None => dom.descendants(label).find(|&n| labelable(n)),
    }
}

/// HTML elements laid out as blocks by default (for pages whose styles
/// are not known).
fn is_block_element(local: &str) -> bool {
    matches!(
        local,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "caption"
            | "center"
            | "dd"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "legend"
            | "li"
            | "listing"
            | "main"
            | "menu"
            | "nav"
            | "ol"
            | "p"
            | "plaintext"
            | "pre"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "xmp"
    )
}

/// Marks the first and last texts of a block's content, so that they are
/// not joined with the texts around the block once wrappers are gone.
fn mark_block_edges(children: &mut [AxNode]) {
    if let Some(first) = edge_text(children, false) {
        first.breaks_before = true;
    }
    if let Some(last) = edge_text(children, true) {
        last.breaks_after = true;
    }
}

/// The first (or last) text leaf among `nodes` and their descendants.
fn edge_text(nodes: &mut [AxNode], last: bool) -> Option<&mut AxNode> {
    let order: Vec<usize> = if last {
        (0..nodes.len()).rev().collect()
    } else {
        (0..nodes.len()).collect()
    };
    let at = order
        .into_iter()
        .find(|&i| nodes[i].text.is_some() || has_text(&nodes[i].children))?;
    let node = &mut nodes[at];
    if node.text.is_some() {
        return Some(node);
    }
    edge_text(&mut node.children, last)
}

fn has_text(nodes: &[AxNode]) -> bool {
    nodes
        .iter()
        .any(|n| n.text.is_some() || has_text(&n.children))
}

/// Merges adjacent text leaves and tidies them, at every level.
fn tidy(nodes: &mut Vec<AxNode>) {
    merge_text(nodes);
    nodes.retain_mut(|node| match &node.text {
        Some(t) => match tidy_text(t) {
            Some(t) => {
                node.text = Some(t);
                true
            }
            None => false,
        },
        None => true,
    });
    drop_label_text(nodes);
    for node in nodes.iter_mut() {
        tidy(&mut node.children);
    }
}

/// Roles whose name usually comes from a `<label>` beside them.
fn is_labelled_control(role: Option<&str>) -> bool {
    matches!(
        role,
        Some(
            "textbox"
                | "searchbox"
                | "checkbox"
                | "radio"
                | "switch"
                | "combobox"
                | "listbox"
                | "spinbutton"
                | "slider"
        )
    )
}

/// Drops text that only repeats the name of the control next to it: the
/// label of `<label>Search <input></label>` is already the textbox's name.
fn drop_label_text(nodes: &mut Vec<AxNode>) {
    let names: Vec<Option<String>> = nodes
        .iter()
        .map(|n| {
            (n.text.is_none() && is_labelled_control(n.role) && !n.name.is_empty())
                .then(|| n.name.clone())
        })
        .collect();
    let mut index: usize = 0;
    nodes.retain(|node| {
        let i = index;
        index += 1;
        let Some(text) = &node.text else {
            return true;
        };
        let before = i.checked_sub(1).and_then(|j| names[j].as_ref());
        let after = names.get(i + 1).and_then(Option::as_ref);
        before != Some(text) && after != Some(text)
    });
}

/// Replaces generic and presentational nodes by their children, unless
/// they are interactive. A name does not keep a generic node: ARIA forbids
/// naming generics, and the names they carry are `title` tooltips.
fn elide_generic(node: AxNode) -> Vec<AxNode> {
    // Text-level roles (`<strong>`, `<em>`, `<code>`, `<time>`) are read as
    // part of the sentence around them.
    let generic = matches!(
        node.role,
        None | Some(
            "none"
                | "generic"
                | "rowgroup"
                | "strong"
                | "emphasis"
                | "mark"
                | "code"
                | "time"
                | "subscript"
                | "superscript"
        )
    ) && !node.interactive
        && node.text.is_none();
    let mut node = node;
    let mut kids: Vec<AxNode> = std::mem::take(&mut node.children)
        .into_iter()
        .flat_map(elide_generic)
        .collect();
    merge_text(&mut kids);
    // After wrappers are gone, a name-from-content node whose children only
    // show its name again needs one of the two: an element to act on keeps
    // its name (a link around an image), a container keeps its children (a
    // cell around a button).
    if node.role.is_some_and(names_from_content) && !node.name.is_empty() && !kids.is_empty() {
        let shown = kids
            .iter()
            .map(|c| c.text.clone().unwrap_or_else(|| c.name.clone()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if shown == node.name {
            let texts_only = kids.iter().all(|c| c.text.is_some());
            let inner_actions = kids.iter().any(|c| c.interactive || c.has_interactive());
            if texts_only || (node.interactive && !inner_actions) {
                kids.clear();
            } else if !node.interactive {
                node.name.clear();
            }
        }
    }
    // Inside something to act on, an image or a text that only repeats
    // part of its name says nothing more ("View details for Backpack"
    // around an image of the backpack).
    if node.interactive
        && !node.name.is_empty()
        && !kids.iter().any(|c| c.interactive || c.has_interactive())
    {
        let name = node.name.to_lowercase();
        kids.retain(|c| {
            let shown = c.text.as_deref().unwrap_or(&c.name);
            !(c.children.is_empty() && !shown.is_empty() && name.contains(&shown.to_lowercase()))
        });
    }
    node.children = kids;
    if generic {
        return node.children;
    }
    if !node.interactive && node.text.is_none() && node.name.is_empty() {
        // An empty element says nothing (a star rating drawn by CSS); a
        // frame is kept, its document is elsewhere.
        if node.children.is_empty() && !matches!(node.role, Some("iframe" | "document")) {
            return Vec::new();
        }
        // A list item around one element is that element.
        if node.role == Some("listitem")
            && node.children.len() == 1
            && node.children[0].text.is_none()
        {
            return node.children;
        }
    }
    vec![node]
}

/// Joins adjacent text leaves with a space, within a block.
fn merge_text(nodes: &mut Vec<AxNode>) {
    let mut merged: Vec<AxNode> = Vec::with_capacity(nodes.len());
    for node in nodes.drain(..) {
        if let Some(t) = &node.text
            && !node.breaks_before
            && let Some(last) = merged.last_mut()
            && !last.breaks_after
            && let Some(prev) = &mut last.text
        {
            // Texts split by inline markup: no space before closing
            // punctuation or after an opening bracket or currency sign.
            let tight = t.starts_with(['.', ',', ';', ':', '!', '?', ')', ']', '}', '%'])
                || prev.ends_with(['(', '[', '{', '$', '£', '€', '¥']);
            if !tight {
                prev.push(' ');
            }
            prev.push_str(t);
            last.breaks_after = node.breaks_after;
            continue;
        }
        merged.push(node);
    }
    *nodes = merged;
}

/// Keeps interactive nodes, headings and landmarks; other containers are
/// flattened so their kept descendants move up. Text leaves are dropped.
fn prune_non_interactive(node: AxNode) -> Vec<AxNode> {
    if node.text.is_some() {
        return Vec::new();
    }
    let children: Vec<AxNode> = node
        .children
        .into_iter()
        .flat_map(prune_non_interactive)
        .collect();
    let keep = node.interactive
        || matches!(node.role, Some("heading"))
        || node.role.is_some_and(is_landmark);
    if keep {
        vec![AxNode { children, ..node }]
    } else {
        children
    }
}

struct Emitter<'a> {
    refs: &'a mut RefTable,
    frame: u32,
    epoch: u64,
    options: &'a SnapshotOptions,
    lines: Vec<SnapLine>,
    text: String,
    emitted: usize,
    truncated_nodes: usize,
}

impl Emitter<'_> {
    fn over_budget(&self) -> bool {
        self.options
            .max_chars
            .is_some_and(|max| self.text.len() >= max)
    }

    fn push(&mut self, line: SnapLine) {
        render_line(&mut self.text, &line, self.options.format);
        self.text.push('\n');
        self.lines.push(line);
        self.emitted += 1;
    }

    fn emit(&mut self, node: &AxNode, depth: usize, parent: Option<u32>) {
        if self.over_budget() || self.options.max_depth.is_some_and(|d| depth > d) {
            self.truncated_nodes += node.count();
            return;
        }
        let depth16 = depth.min(u16::MAX as usize) as u16;
        if let Some(text) = &node.text {
            self.push(SnapLine {
                depth: depth16,
                kind: LineKind::Text(text.clone()),
            });
            return;
        }
        let role = node.role.unwrap_or("generic");
        let role = if role == "none" { "generic" } else { role };
        let name = truncate(&node.name, self.options.max_name_len);
        let key = RefKey {
            frame: self.frame,
            epoch: self.epoch,
            node: node.node,
        };
        let inline = match node.children.as_slice() {
            [only] if name.is_empty() => only.text.clone(),
            _ => None,
        };
        // A nameless element is known by its text in errors and results.
        let known_as = match &inline {
            Some(text) => truncate(text, self.options.max_name_len),
            None => name.clone(),
        };
        let r = self.refs.get_or_assign(key, role, &known_as, parent);
        let children: &[AxNode] = if inline.is_some() {
            &[]
        } else {
            &node.children
        };
        self.push(SnapLine {
            depth: depth16,
            kind: LineKind::Element {
                r,
                role,
                name,
                attrs: node.attrs.clone(),
                has_children: !children.is_empty(),
                text: inline,
            },
        });
        for child in children {
            self.emit(child, depth + 1, Some(r));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::{HtmlParseOptions, parse_html};

    fn snap_with(html: &str, options: SnapshotOptions) -> String {
        let opts = HtmlParseOptions {
            url: Some(Url::parse("https://shop.example/cart").unwrap()),
            ..Default::default()
        };
        let r = parse_html(html, &opts);
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        s.snapshot(&options).text
    }

    fn snap(html: &str, filter: Filter) -> String {
        snap_with(
            html,
            SnapshotOptions {
                filter,
                ..Default::default()
            },
        )
    }

    const CART: &str = r#"<!doctype html><title>Cart (2)</title>
<header><a href="/">Example</a><input type=search aria-label="Search products"><button>Search</button></header>
<main><h1>Your cart</h1>
<table><tr><th>Item</th><th>Qty</th><th>Action</th></tr><tr><td>Wool socks</td><td><input type=number aria-label=Quantity value=2 min=1></td><td><button>Remove</button></td></tr></table>
<div><span>Free shipping over $50</span></div>
<a href="https://pay.example/go">Pay</a>
<input type=password aria-label=PIN value=1234>
<script>ignored()</script><div hidden>gone</div></main>"#;

    #[test]
    fn renders_compact_cst_for_a_small_page() {
        let text = snap(CART, Filter::Interesting);
        let expected = r#"# s1 url=https://shop.example/cart title="Cart (2)" filter=interesting nodes=20/28
e1 banner
  e2 link "Example"
  e3 searchbox "Search products"
  e4 button "Search"
e5 main
  e6 heading "Your cart" [level=1]
  e7 table
    e8 row
      e9 columnheader "Item"
      e10 columnheader "Qty"
      e11 columnheader "Action"
    e12 row
      e13 cell "Wool socks"
      e14 cell
        e15 spinbutton "Quantity" [value=2] [min=1]
      e16 cell
        e17 button "Remove"
  text: Free shipping over $50
  e18 link "Pay"
  e19 textbox "PIN" [value=***]
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn renders_the_aria_format_with_hrefs_on_request() {
        let text = snap_with(
            CART,
            SnapshotOptions {
                format: Format::Aria,
                extra: ExtraAttrs::parse_list("href").unwrap(),
                ..Default::default()
            },
        );
        assert!(
            text.contains("- banner [ref=e1]:\n  - link \"Example\" [ref=e2] [href=/]\n"),
            "{text}"
        );
        assert!(
            text.contains("  - link \"Pay\" [ref=e18] [href=https://pay.example/go]\n"),
            "{text}"
        );
        assert!(
            text.contains("  - text: Free shipping over $50\n"),
            "{text}"
        );
    }

    #[test]
    fn interactive_filter_keeps_only_actionable_nodes_and_their_context() {
        let html = "<h1>Hi</h1><p>para</p><nav><ul><li><a href=/a>A</a></li></ul></nav><p>more</p>";
        let text = snap(html, Filter::Interactive);
        assert!(text.contains("heading \"Hi\""));
        assert!(!text.contains("para"));
        assert!(text.contains(" navigation\n"), "{text}");
        assert!(text.contains("link \"A\""));
        assert!(!text.contains("listitem"));
    }

    #[test]
    fn wrapped_link_text_is_not_repeated() {
        let text = snap(
            "<a href=/x><span>Main page</span></a><a href=/y><img alt=Logo src=l.png> Home</a>",
            Filter::Interesting,
        );
        assert!(text.contains("e1 link \"Main page\"\n"), "{text}");
        // The image's alt is already in the link's name.
        assert!(text.contains("e2 link \"Logo Home\"\n"), "{text}");
        assert!(!text.contains("img"), "{text}");
    }

    #[test]
    fn separators_and_tooltip_generics_cost_nothing() {
        // A Hacker News story line: a nameless vote link around a titled
        // arrow, punctuation between links, a timestamp in a titled span.
        let html = r#"<span class=rank>1.</span>
<a id=up href="vote?id=1"><div class=votearrow title=upvote></div></a>
<a href="https://example.com/x">A story</a> (<a href="from?site=example.com">example.com</a>)
<br>205 points by <a href="user?id=pg">pg</a> <span title="2026-10-07T11:25:02"><a href="item?id=1">2 hours ago</a></span> | <a href="hide?id=1">hide</a>"#;
        let text = snap(html, Filter::Interesting);
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            body,
            [
                "text: 1.",
                "e1 link \"upvote\"",
                "e2 link \"A story\"",
                "e3 link \"example.com\"",
                "text: 205 points by",
                "e4 link \"pg\"",
                "e5 link \"2 hours ago\"",
                "e6 link \"hide\"",
            ],
            "{text}"
        );
    }

    #[test]
    fn tidy_text_keeps_meaning_and_drops_noise() {
        assert_eq!(tidy_text("|"), None);
        assert_eq!(tidy_text(" ( "), None);
        assert_eq!(
            tidy_text(") 205 points by").as_deref(),
            Some("205 points by")
        );
        assert_eq!(tidy_text("(optional)").as_deref(), Some("(optional)"));
        assert_eq!(tidy_text("Price: $5 |").as_deref(), Some("Price: $5"));
        assert_eq!(tidy_text("· Home ›").as_deref(), Some("Home"));
        assert_eq!(tidy_text("3 (").as_deref(), Some("3"));
    }

    struct LiveOracle {
        value: NodeId,
        pointer: NodeId,
    }

    impl StyleOracle for LiveOracle {
        fn is_display_none(&self, _dom: &Dom, _id: NodeId) -> bool {
            false
        }
        fn is_visibility_hidden(&self, _dom: &Dom, _id: NodeId) -> bool {
            false
        }
        fn control_value(&self, _dom: &Dom, id: NodeId) -> Option<String> {
            (id == self.value).then(|| "typed by the user".to_string())
        }
        fn is_pointer_cursor(&self, _dom: &Dom, id: NodeId) -> bool {
            id == self.pointer
        }
    }

    #[test]
    fn live_values_and_clickable_generics_come_from_the_oracle() {
        let r = parse_html(
            r#"<input aria-label=Name value=default placeholder=Name>
<div id=card><span>Open settings</span></div><div id=wrap><button>Inner</button></div>"#,
            &Default::default(),
        );
        let find = |id: &str| {
            r.dom
                .descendants(r.dom.document())
                .find(|&n| r.dom.attr(n, "id") == Some(id))
                .unwrap()
        };
        let input = r
            .dom
            .descendants(r.dom.document())
            .find(|&n| r.dom.is_html_element(n, "input"))
            .unwrap();
        let oracle = LiveOracle {
            value: input,
            pointer: find("card"),
        };
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        let text = s.snapshot(&SnapshotOptions::default()).text;
        assert!(
            text.contains("e1 textbox \"Name\" [value=\"typed by the user\"]\n"),
            "{text}"
        );
        assert!(
            text.contains("e2 generic [clickable]: Open settings\n"),
            "{text}"
        );
        assert!(text.contains("e3 button \"Inner\""), "{text}");
        assert!(!text.contains("wrap"), "{text}");
    }

    #[test]
    fn empty_fields_show_a_placeholder_unless_it_is_the_name() {
        let text = snap(
            "<input aria-label=Search placeholder=Search><input aria-label=Email placeholder=you@example.com>",
            Filter::Interesting,
        );
        assert!(text.contains("e1 textbox \"Search\"\n"), "{text}");
        assert!(
            text.contains("e2 textbox \"Email\" [placeholder=you@example.com]\n"),
            "{text}"
        );
    }

    #[test]
    fn texts_join_within_a_block_and_not_across_blocks() {
        let text = snap(
            "<div>First block</div><div>Second <b>part</b></div>text after<div></div>text below              <span>$</span><span>29.99</span>",
            Filter::Interesting,
        );
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            body,
            [
                "text: First block",
                "text: Second part",
                "text: text after",
                "text: text below $29.99",
            ],
            "{text}"
        );
    }

    #[test]
    fn a_label_is_not_one_more_thing_to_click() {
        let r = parse_html(
            "<label><input type=checkbox> Remember me</label>",
            &Default::default(),
        );
        let label = r
            .dom
            .descendants(r.dom.document())
            .find(|&n| r.dom.is_html_element(n, "label"))
            .unwrap();
        let oracle = LiveOracle {
            value: NodeId::default(),
            pointer: label,
        };
        let mut refs = RefTable::new();
        let text = Snapshotter::new(&r.dom, &oracle, &mut refs)
            .snapshot(&SnapshotOptions::default())
            .text;
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(body, ["e1 checkbox \"Remember me\""], "{text}");
    }

    #[test]
    fn long_texts_stay_whole_in_the_model() {
        let long = "word ".repeat(100);
        let r = parse_html(&format!("<p>{long}</p>"), &Default::default());
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let snapshot =
            Snapshotter::new(&r.dom, &oracle, &mut refs).snapshot(&SnapshotOptions::default());
        assert!(snapshot.text.contains(long.trim()), "{}", snapshot.text);
        assert_eq!(cap_text("short"), None);
        let capped = cap_text(long.trim()).unwrap();
        assert!(capped.ends_with("… [+300 chars]"), "{capped}");
        assert_eq!(
            capped.chars().count(),
            199 + "… [+300 chars]".chars().count()
        );
    }

    #[test]
    fn labels_and_lone_texts_fold_into_their_lines() {
        let text = snap(
            "<label>Search <input name=q></label><label><input type=checkbox> In stock</label>\
             <p>Ready</p><span onclick='buy()'>Buy now</span>",
            Filter::Interesting,
        );
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            body,
            [
                "e1 textbox \"Search\"",
                "e2 checkbox \"In stock\"",
                "e3 paragraph: Ready",
                "e4 generic [clickable]: Buy now",
            ],
            "{text}"
        );
    }

    #[test]
    fn wrappers_and_empty_elements_cost_nothing() {
        let text = snap(
            "<p><strong>1000</strong> results - showing <em>1</em> to <strong>20</strong>.</p>\
             <ul><li><a href=/a>Travel</a></li><li>Page 1 of 50</li></ul>\
             <article><p class='star-rating Three'><i></i></p><h3><a href=/b>A Light</a></h3></article>",
            Filter::Interesting,
        );
        let body: Vec<&str> = text.lines().skip(1).collect();
        assert_eq!(
            body,
            [
                "e1 paragraph: 1000 results - showing 1 to 20.",
                "e2 list",
                "  e3 link \"Travel\"",
                "  e4 listitem: Page 1 of 50",
                "e5 article",
                "  e6 heading [level=3]",
                "    e7 link \"A Light\"",
            ],
            "{text}"
        );
    }

    #[test]
    fn refs_are_stable_across_snapshots() {
        let r = parse_html("<button>A</button><button>B</button>", &Default::default());
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        let first = s.snapshot(&SnapshotOptions::default()).text;
        let second = s.snapshot(&SnapshotOptions::default()).text;
        assert!(first.contains("e1 button") && first.contains("e2 button"));
        assert_eq!(
            first.lines().skip(1).collect::<Vec<_>>(),
            second.lines().skip(1).collect::<Vec<_>>()
        );
        assert_eq!(refs.len(), 2);
        assert!(refs.resolve("e2").is_some());
    }

    #[test]
    fn budget_truncates() {
        let html = (0..50)
            .map(|i| format!("<button>B{i}</button>"))
            .collect::<String>();
        let r = parse_html(&html, &Default::default());
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        let snap = s.snapshot(&SnapshotOptions {
            max_chars: Some(300),
            ..Default::default()
        });
        assert!(snap.truncated);
        assert!(snap.text.contains("[truncated: "));
        assert!(snap.emitted_nodes < 50);
    }

    #[test]
    fn headers_omit_what_is_unset() {
        let header = Header {
            id: 14,
            tab: Some("t1".into()),
            doc: Some(3),
            url: Some("https://shop.example/cart".into()),
            title: Some("Cart (2)".into()),
            viewport: Some((1280, 720)),
            scroll: Some((0, 0)),
            filter: Some(Filter::Interesting),
            nodes: Some((38, 412)),
            settled: Some(true),
            ..Header::default()
        };
        assert_eq!(
            header.render(),
            "# s14 tab=t1 doc=d3 url=https://shop.example/cart title=\"Cart (2)\" vp=1280x720 scroll=0,0 filter=interesting nodes=38/412 settled=yes"
        );
    }
}
