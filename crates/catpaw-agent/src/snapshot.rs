//! CST (CatPaw Snapshot Text) emission and the per-tab reference table.
//!
//! ```text
//! # s1 url=https://shop.example/cart title="Cart" nodes=38/412 filter=interesting
//! - banner [ref=e1]:
//!   - searchbox "Search products" [ref=e3]
//!   - button "Search" [ref=e4]
//! - main [ref=e6]:
//!   - heading "Your cart" [ref=e7] [level=1]
//!   - text: Free shipping on orders over $50
//! ```

use std::collections::HashMap;
use std::fmt::Write as _;

use catpaw_dom::{Dom, ElementData, NodeId, NodeKind};
use url::Url;

use crate::a11y::{
    LabelIndex, collapse_whitespace, heading_level, is_interactive, is_landmark, name_for,
    names_from_content, role_for, subtree_text,
};
use crate::visibility::{StyleOracle, is_hidden};

/// How much of the tree to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    /// Every node with a role, plus generic containers.
    All,
    /// Interactive elements, headings, landmarks, structure and text;
    /// nameless generic wrappers are elided.
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

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub filter: Filter,
    /// Subtree to snapshot; the document by default.
    pub root: Option<NodeId>,
    pub max_depth: Option<usize>,
    /// Soft character budget; the output is cut with a `[truncated ...]` line.
    pub max_chars: Option<usize>,
    /// Longest accessible name emitted before truncation with an ellipsis.
    pub max_name_len: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            filter: Filter::Interesting,
            root: None,
            max_depth: None,
            max_chars: None,
            max_name_len: 160,
        }
    }
}

/// Element references: allocated the first time a node is emitted, monotonic,
/// never reused (ADR 0005).
#[derive(Debug, Default)]
pub struct RefTable {
    next: u32,
    by_node: HashMap<NodeId, u32>,
    by_ref: HashMap<u32, NodeId>,
}

impl RefTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_assign(&mut self, node: NodeId) -> u32 {
        if let Some(&r) = self.by_node.get(&node) {
            return r;
        }
        self.next += 1;
        self.by_node.insert(node, self.next);
        self.by_ref.insert(self.next, node);
        self.next
    }

    pub fn get(&self, node: NodeId) -> Option<u32> {
        self.by_node.get(&node).copied()
    }

    /// Resolves `e12` or `12` to a node.
    pub fn resolve(&self, text: &str) -> Option<NodeId> {
        let n: u32 = text.trim().trim_start_matches('e').parse().ok()?;
        self.by_ref.get(&n).copied()
    }

    pub fn len(&self) -> usize {
        self.by_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }
}

/// A rendered snapshot.
#[derive(Debug)]
pub struct Snapshot {
    pub id: u64,
    pub text: String,
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
        }
    }

    fn count(&self) -> usize {
        1 + self.children.iter().map(AxNode::count).sum::<usize>()
    }
}

/// Builds snapshots of one document, allocating refs from a [`RefTable`].
pub struct Snapshotter<'a> {
    dom: &'a Dom,
    oracle: &'a dyn StyleOracle,
    refs: &'a mut RefTable,
    labels: LabelIndex,
    document_url: Option<Url>,
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
            next_id: 1,
        }
    }

    pub fn snapshot(&mut self, options: &SnapshotOptions) -> Snapshot {
        let root = options.root.unwrap_or(self.dom.document());
        let total_elements = self
            .dom
            .descendants(root)
            .filter(|&n| self.dom.is_element(n))
            .count();

        let mut forest = self.build_children(root, options);
        if options.filter != Filter::All {
            forest = forest.into_iter().flat_map(elide_generic).collect();
            merge_text(&mut forest);
        }
        if options.filter == Filter::Interactive {
            forest = forest.into_iter().flat_map(prune_non_interactive).collect();
        }

        let mut out = String::new();
        let title = self.title();
        let _ = write!(out, "# s{}", self.next_id);
        if let Some(url) = &self.document_url {
            let _ = write!(out, " url={url}");
        }
        let _ = write!(out, " title={}", quote(&title));
        let header_end = out.len();

        let mut emitter = Emitter {
            refs: self.refs,
            out: &mut out,
            options,
            emitted: 0,
            truncated_nodes: 0,
        };
        for node in &forest {
            emitter.emit(node, 0);
        }
        let emitted = emitter.emitted;
        let truncated_nodes = emitter.truncated_nodes;
        if truncated_nodes > 0 {
            let _ = writeln!(out, "- [truncated: {truncated_nodes} more nodes]");
        }
        let header_tail = format!(
            " nodes={emitted}/{total_elements} filter={}\n",
            options.filter.as_str()
        );
        out.insert_str(header_end, &header_tail);

        let id = self.next_id;
        self.next_id += 1;
        Snapshot {
            id,
            text: out,
            emitted_nodes: emitted,
            total_elements,
            truncated: truncated_nodes > 0,
        }
    }

    fn title(&self) -> String {
        self.dom
            .descendants(self.dom.document())
            .find(|&n| self.dom.is_html_element(n, "title"))
            .map(|n| collapse_whitespace(&self.dom.text_content(n)))
            .unwrap_or_default()
    }

    fn build_children(&self, parent: NodeId, options: &SnapshotOptions) -> Vec<AxNode> {
        let mut out = Vec::new();
        for child in self.dom.children(parent) {
            match self.dom.kind(child) {
                NodeKind::Text(t) => {
                    let t = collapse_whitespace(t);
                    if !t.is_empty() {
                        out.push(AxNode::text_leaf(t));
                    }
                }
                NodeKind::Element(el) => {
                    if is_hidden(self.dom, child, self.oracle) {
                        continue;
                    }
                    out.push(self.build_element(child, el, options));
                }
                _ => {}
            }
        }
        out
    }

    fn build_element(&self, id: NodeId, el: &ElementData, options: &SnapshotOptions) -> AxNode {
        let role = role_for(self.dom, id);
        let name = name_for(self.dom, id, role, self.oracle, &self.labels);
        let attrs = self.attrs_for(id, el, role);
        let role_str = role.unwrap_or("");
        let interactive = is_interactive(role_str)
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

        AxNode {
            node: id,
            role,
            name,
            attrs,
            interactive,
            children,
            text: None,
        }
    }

    fn attrs_for(
        &self,
        id: NodeId,
        el: &ElementData,
        role: Option<&str>,
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
            let checked = el.has_attr("checked")
                || el
                    .attr("aria-checked")
                    .is_some_and(|v| v.eq_ignore_ascii_case("true"));
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
        if role == "option" && el.has_attr("selected")
            || el
                .attr("aria-selected")
                .is_some_and(|v| v.eq_ignore_ascii_case("true"))
        {
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
                let value = if local == "textarea" {
                    self.dom.text_content(id)
                } else {
                    el.attr("value").unwrap_or("").to_string()
                };
                let value = if input_type == "password" && !value.is_empty() {
                    "***".to_string()
                } else {
                    value
                };
                attrs.push(("value", value));
                if let Some(p) = el.attr("placeholder")
                    && !p.is_empty()
                {
                    attrs.push(("placeholder", collapse_whitespace(p)));
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
                attrs.push(("value", selected));
                let count = self
                    .dom
                    .descendants(id)
                    .filter(|&n| self.dom.is_html_element(n, "option"))
                    .count();
                attrs.push(("options", count.to_string()));
            }
            "link" => {
                if let Some(href) = el.attr("href") {
                    attrs.push(("href", self.display_href(href)));
                }
            }
            "img" => {
                if role == "img"
                    && let Some(src) = el.attr("src")
                    && !src.starts_with("data:")
                {
                    attrs.push(("src", truncate(src, 80)));
                }
            }
            "iframe" => {
                if let Some(src) = el.attr("src") {
                    attrs.push(("src", truncate(src, 100)));
                }
            }
            _ => {}
        }
        if role == "button" && local == "input" && input_type == "file" {
            attrs.push(("type", "file".to_string()));
        }
        attrs
    }

    fn selected_option_text(&self, select: NodeId) -> String {
        let options: Vec<NodeId> = self
            .dom
            .descendants(select)
            .filter(|&n| self.dom.is_html_element(n, "option"))
            .collect();
        let chosen = options
            .iter()
            .copied()
            .find(|&o| self.dom.attr(o, "selected").is_some())
            .or_else(|| options.first().copied());
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn quote(s: &str) -> String {
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

fn attr_value(v: &str) -> String {
    let needs_quotes = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == ']' || c == '[');
    if needs_quotes {
        quote(v)
    } else {
        v.to_string()
    }
}

/// Replaces nameless generic/presentational nodes by their children.
fn elide_generic(node: AxNode) -> Vec<AxNode> {
    let generic = matches!(
        node.role,
        None | Some("none") | Some("generic") | Some("rowgroup")
    ) && node.name.is_empty()
        && !node.interactive
        && node.text.is_none();
    let mut kids: Vec<AxNode> = node.children.into_iter().flat_map(elide_generic).collect();
    merge_text(&mut kids);
    // After wrappers are gone, a name-from-content node whose remaining
    // children are just the text its name already carries shows no children.
    if node.role.is_some_and(names_from_content)
        && !node.name.is_empty()
        && !kids.is_empty()
        && kids.iter().all(|c| c.text.is_some())
    {
        let merged = kids
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join(" ");
        if merged == node.name {
            kids.clear();
        }
    }
    let node = AxNode {
        children: kids,
        ..node
    };
    if generic { node.children } else { vec![node] }
}

/// Joins adjacent text leaves with a space.
fn merge_text(nodes: &mut Vec<AxNode>) {
    let mut merged: Vec<AxNode> = Vec::with_capacity(nodes.len());
    for node in nodes.drain(..) {
        if let Some(t) = &node.text
            && let Some(last) = merged.last_mut()
            && let Some(prev) = &mut last.text
        {
            prev.push(' ');
            prev.push_str(t);
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
    out: &'a mut String,
    options: &'a SnapshotOptions,
    emitted: usize,
    truncated_nodes: usize,
}

impl Emitter<'_> {
    fn over_budget(&self) -> bool {
        self.options
            .max_chars
            .is_some_and(|max| self.out.len() >= max)
    }

    fn emit(&mut self, node: &AxNode, depth: usize) {
        if self.over_budget() || self.options.max_depth.is_some_and(|d| depth > d) {
            self.truncated_nodes += node.count();
            return;
        }
        let indent = "  ".repeat(depth);
        if let Some(text) = &node.text {
            let _ = writeln!(self.out, "{indent}- text: {}", truncate(text, 400));
            self.emitted += 1;
            return;
        }
        let role = node.role.unwrap_or("generic");
        let _ = write!(self.out, "{indent}- {role}");
        if !node.name.is_empty() {
            let _ = write!(
                self.out,
                " {}",
                quote(&truncate(&node.name, self.options.max_name_len))
            );
        }
        let r = self.refs.get_or_assign(node.node);
        let _ = write!(self.out, " [ref=e{r}]");
        for (key, value) in &node.attrs {
            if value.is_empty() && *key != "value" {
                let _ = write!(self.out, " [{key}]");
            } else {
                let _ = write!(self.out, " [{key}={}]", attr_value(value));
            }
        }
        if node.children.is_empty() {
            self.out.push('\n');
        } else {
            self.out.push_str(":\n");
        }
        self.emitted += 1;
        for child in &node.children {
            self.emit(child, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::{HtmlParseOptions, parse_html};

    fn snap(html: &str, filter: Filter) -> String {
        let opts = HtmlParseOptions {
            url: Some(Url::parse("https://shop.example/cart").unwrap()),
            ..Default::default()
        };
        let r = parse_html(html, &opts);
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        s.snapshot(&SnapshotOptions {
            filter,
            ..Default::default()
        })
        .text
    }

    #[test]
    fn renders_cst_for_a_small_page() {
        let html = r#"<!doctype html><title>Cart (2)</title>
<header><a href="/">Example</a><input type=search aria-label="Search products"><button>Search</button></header>
<main><h1>Your cart</h1>
<table><tr><th>Item</th><th>Qty</th><th>Action</th></tr><tr><td>Wool socks</td><td><input type=number aria-label=Quantity value=2 min=1></td><td><button>Remove</button></td></tr></table>
<div><span>Free shipping over $50</span></div>
<a href="https://pay.example/go">Pay</a>
<input type=password aria-label=PIN value=1234>
<script>ignored()</script><div hidden>gone</div></main>"#;
        let text = snap(html, Filter::Interesting);
        let expected = r#"# s1 url=https://shop.example/cart title="Cart (2)" nodes=20/28 filter=interesting
- banner [ref=e1]:
  - link "Example" [ref=e2] [href=/]
  - searchbox "Search products" [ref=e3] [value=""]
  - button "Search" [ref=e4]
- main [ref=e5]:
  - heading "Your cart" [ref=e6] [level=1]
  - table [ref=e7]:
    - row [ref=e8]:
      - columnheader "Item" [ref=e9]
      - columnheader "Qty" [ref=e10]
      - columnheader "Action" [ref=e11]
    - row [ref=e12]:
      - cell "Wool socks" [ref=e13]
      - cell [ref=e14]:
        - spinbutton "Quantity" [ref=e15] [value=2] [min=1]
      - cell "Remove" [ref=e16]:
        - button "Remove" [ref=e17]
  - text: Free shipping over $50
  - link "Pay" [ref=e18] [href=https://pay.example/go]
  - textbox "PIN" [ref=e19] [value=***]
"#;
        assert_eq!(text, expected);
    }

    #[test]
    fn interactive_filter_keeps_only_actionable_nodes_and_their_context() {
        let html = "<h1>Hi</h1><p>para</p><nav><ul><li><a href=/a>A</a></li></ul></nav><p>more</p>";
        let text = snap(html, Filter::Interactive);
        assert!(text.contains("- heading \"Hi\""));
        assert!(!text.contains("para"));
        assert!(text.contains("- navigation [ref="));
        assert!(text.contains("- link \"A\""));
        assert!(!text.contains("listitem"));
    }

    #[test]
    fn wrapped_link_text_is_not_repeated() {
        let text = snap(
            "<a href=/x><span>Main page</span></a><a href=/y><img alt=Logo src=l.png> Home</a>",
            Filter::Interesting,
        );
        assert!(
            text.contains("- link \"Main page\" [ref=e1] [href=/x]\n"),
            "{text}"
        );
        assert!(
            text.contains("- link \"Logo Home\" [ref=e2] [href=/y]:\n"),
            "{text}"
        );
        assert!(text.contains("  - img \"Logo\""), "{text}");
    }

    #[test]
    fn refs_are_stable_across_snapshots() {
        let r = parse_html("<button>A</button><button>B</button>", &Default::default());
        let oracle = AttributeOracle;
        let mut refs = RefTable::new();
        let mut s = Snapshotter::new(&r.dom, &oracle, &mut refs);
        let first = s.snapshot(&SnapshotOptions::default()).text;
        let second = s.snapshot(&SnapshotOptions::default()).text;
        assert!(first.contains("[ref=e1]") && first.contains("[ref=e2]"));
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
}
