//! Readable views of a document: markdown, plain text, links and forms.

use std::fmt::Write as _;

use catpaw_dom::{Dom, ElementData, NodeId, NodeKind};
use url::Url;

use crate::a11y::{
    LabelIndex, collapse_whitespace, is_layout_table, name_for, role_for, subtree_text,
};
use crate::refs::RefScope;
use crate::visibility::{StyleOracle, is_hidden};

/// How links are rendered in markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkStyle {
    /// `[text](https://absolute/url)`
    #[default]
    Url,
    /// `[text](ref:e12)`, resolvable through the snapshot's [`RefTable`](crate::RefTable).
    Ref,
}

#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    pub link_style: LinkStyle,
    /// Render only the main content (`<main>`, `[role=main]`, or the first
    /// `<article>`) when the page has one.
    pub main_only: bool,
    /// Render only this element and what is inside it.
    pub root: Option<NodeId>,
}

fn content_root(dom: &Dom, options: &ReadOptions) -> NodeId {
    if let Some(root) = options.root {
        return root;
    }
    let main_only = options.main_only;
    let doc = dom.document();
    if main_only {
        let candidates = ["main", "article"];
        for local in candidates {
            if let Some(n) = dom
                .descendants(doc)
                .find(|&n| dom.is_html_element(n, local))
            {
                return n;
            }
        }
        if let Some(n) = dom.descendants(doc).find(|&n| {
            dom.attr(n, "role")
                .is_some_and(|r| r.eq_ignore_ascii_case("main"))
        }) {
            return n;
        }
    }
    dom.descendants(doc)
        .find(|&n| dom.is_html_element(n, "body"))
        .unwrap_or(doc)
}

/// Plain text with block boundaries as line breaks.
pub fn text(dom: &Dom, oracle: &dyn StyleOracle) -> String {
    text_with(dom, oracle, &ReadOptions::default())
}

/// Plain text, with options (`main_only`).
pub fn text_with(dom: &Dom, oracle: &dyn StyleOracle, options: &ReadOptions) -> String {
    crate::a11y::in_one_pass(|| markdown_with(dom, oracle, None, options, true))
}

/// Markdown rendering of the (main) content.
pub fn markdown(
    dom: &Dom,
    oracle: &dyn StyleOracle,
    refs: Option<RefScope<'_>>,
    options: &ReadOptions,
) -> String {
    crate::a11y::in_one_pass(|| markdown_with(dom, oracle, refs, options, false))
}

fn markdown_with(
    dom: &Dom,
    oracle: &dyn StyleOracle,
    refs: Option<RefScope<'_>>,
    options: &ReadOptions,
    plain: bool,
) -> String {
    let root = content_root(dom, options);
    let labels = LabelIndex::build(dom);
    let mut w = MdWriter {
        dom,
        oracle,
        refs,
        labels: &labels,
        base: dom.url().cloned(),
        link_style: options.link_style,
        plain,
        out: String::new(),
        inline: String::new(),
        list_stack: Vec::new(),
        pre_depth: 0,
    };
    w.block_children(root);
    w.flush_inline();
    let mut out = w.out;
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// `(ref:e12)` for ` ref:e12`; nothing for nothing.
fn bracketed(r: &str) -> String {
    match r.trim() {
        "" => String::new(),
        r => format!("({r})"),
    }
}

struct MdWriter<'a> {
    dom: &'a Dom,
    oracle: &'a dyn StyleOracle,
    refs: Option<RefScope<'a>>,
    /// Made once: fields are named from it.
    labels: &'a LabelIndex,
    base: Option<Url>,
    link_style: LinkStyle,
    plain: bool,
    out: String,
    /// Pending inline text of the current paragraph.
    inline: String,
    /// Marker stack for nested lists: `None` for bullets, `Some(n)` for the next number.
    list_stack: Vec<Option<u32>>,
    pre_depth: usize,
}

impl MdWriter<'_> {
    fn flush_inline(&mut self) {
        let text = if self.pre_depth > 0 {
            std::mem::take(&mut self.inline)
        } else {
            collapse_whitespace(&std::mem::take(&mut self.inline))
        };
        if text.is_empty() {
            return;
        }
        self.ensure_blank_line();
        self.out.push_str(&text);
        self.out.push('\n');
    }

    fn ensure_blank_line(&mut self) {
        if self.out.is_empty() {
            return;
        }
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn push_block_line(&mut self, line: &str) {
        self.flush_inline();
        self.ensure_blank_line();
        self.out.push_str(line);
        self.out.push('\n');
    }

    fn block_children(&mut self, parent: NodeId) {
        for child in self.dom.rendered_children(parent) {
            self.node(child);
        }
    }

    fn node(&mut self, id: NodeId) {
        match self.dom.kind(id) {
            NodeKind::Text(t) => {
                if self.pre_depth > 0 || !t.trim().is_empty() || !self.inline.is_empty() {
                    self.inline.push_str(t);
                }
            }
            NodeKind::Element(el) => {
                if is_hidden(self.dom, id, self.oracle) {
                    return;
                }
                let el = el.clone();
                self.element(id, &el);
            }
            _ => {}
        }
    }

    fn resolve(&self, href: &str) -> String {
        match &self.base {
            Some(base) => base
                .join(href.trim())
                .map(|u| u.to_string())
                .unwrap_or_else(|_| href.trim().to_string()),
            None => href.trim().to_string(),
        }
    }

    /// ` ref:e12` for a control, when the view names refs.
    fn control_ref(&mut self, id: NodeId) -> String {
        if self.plain || self.link_style != LinkStyle::Ref {
            return String::new();
        }
        match self.refs.as_mut() {
            Some(refs) => format!(" ref:e{}", refs.assign(self.dom, id, self.oracle)),
            None => String::new(),
        }
    }

    fn link_target(&mut self, id: NodeId, href: &str) -> String {
        match self.link_style {
            LinkStyle::Url => self.resolve(href),
            LinkStyle::Ref => match self.refs.as_mut() {
                Some(refs) => format!("ref:e{}", refs.assign(self.dom, id, self.oracle)),
                None => self.resolve(href),
            },
        }
    }

    fn element(&mut self, id: NodeId, el: &ElementData) {
        let local = &*el.name.local;
        if !el.is_html() {
            if local == "svg" {
                let title = subtree_text(self.dom, id, self.oracle);
                if !title.is_empty() && !self.plain {
                    let _ = write!(self.inline, "![{title}]()");
                } else if !title.is_empty() {
                    self.inline.push_str(&title);
                }
                return;
            }
            self.block_children(id);
            return;
        }
        match local {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = local[1..].parse::<usize>().unwrap_or(1);
                let text = subtree_text(self.dom, id, self.oracle);
                if text.is_empty() {
                    return;
                }
                let line = if self.plain {
                    text
                } else {
                    format!("{} {}", "#".repeat(level), text)
                };
                self.push_block_line(&line);
            }
            "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "nav"
            | "aside" | "address" | "figure" | "figcaption" | "details" | "summary" | "dialog"
            | "fieldset" | "form" | "body" | "html" | "dl" | "dt" | "dd" | "legend" | "label"
            | "center" | "tfoot" | "thead" | "tbody" | "caption" => {
                self.flush_inline();
                self.block_children(id);
                self.flush_inline();
            }
            "br" => self.inline.push('\n'),
            "hr" => self.push_block_line(if self.plain { "" } else { "---" }),
            "ul" | "ol" | "menu" => {
                self.flush_inline();
                let start = if local == "ol" {
                    Some(
                        el.attr("start")
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(1),
                    )
                } else {
                    None
                };
                self.list_stack.push(start);
                self.ensure_blank_line();
                for child in self.dom.rendered_children(id) {
                    if self.dom.is_html_element(child, "li") {
                        self.list_item(child);
                    } else {
                        self.node(child);
                    }
                }
                self.list_stack.pop();
                self.flush_inline();
            }
            "li" => self.list_item(id),
            "pre" => {
                self.flush_inline();
                self.pre_depth += 1;
                self.block_children(id);
                let code = std::mem::take(&mut self.inline);
                self.pre_depth -= 1;
                let code = code.trim_end_matches('\n');
                self.ensure_blank_line();
                if self.plain {
                    self.out.push_str(code);
                    self.out.push('\n');
                } else {
                    let _ = writeln!(self.out, "```\n{code}\n```");
                }
            }
            "blockquote" => {
                self.flush_inline();
                let mut inner = MdWriter {
                    dom: self.dom,
                    oracle: self.oracle,
                    refs: None,
                    labels: self.labels,
                    base: self.base.clone(),
                    link_style: match self.link_style {
                        LinkStyle::Ref => LinkStyle::Url,
                        other => other,
                    },
                    plain: self.plain,
                    out: String::new(),
                    inline: String::new(),
                    list_stack: Vec::new(),
                    pre_depth: 0,
                };
                inner.block_children(id);
                inner.flush_inline();
                let quoted: Vec<String> = inner
                    .out
                    .trim_end()
                    .lines()
                    .map(|l| {
                        if self.plain {
                            l.to_string()
                        } else {
                            format!("> {l}")
                        }
                    })
                    .collect();
                if !quoted.is_empty() {
                    self.ensure_blank_line();
                    self.out.push_str(&quoted.join("\n"));
                    self.out.push('\n');
                }
            }
            "table" => self.table(id),
            "a" => {
                let text_before = self.inline.len();
                self.block_children(id);
                // Block content inside the link has been flushed already;
                // what is left of the inline text is the link's.
                let start = text_before.min(self.inline.len());
                let text = collapse_whitespace(&self.inline[start..]);
                self.inline.truncate(start);
                if let Some(href) = el.attr("href").filter(|_| !self.plain) {
                    let target = self.link_target(id, href);
                    // Image-only or icon links: fall back to the accessible
                    // name, then to a labelled descendant, then to the URL.
                    let text = if text.is_empty() {
                        name_for(
                            self.dom,
                            id,
                            Some("link"),
                            self.oracle,
                            &LabelIndex::default(),
                        )
                    } else {
                        text
                    };
                    let text = if text.is_empty() {
                        descendant_label(self.dom, id).unwrap_or_else(|| target.clone())
                    } else {
                        text
                    };
                    let _ = write!(self.inline, "[{text}]({target})");
                } else {
                    self.inline.push_str(&text);
                }
            }
            "img" => {
                let alt = el.attr("alt").map(collapse_whitespace).unwrap_or_default();
                if self.plain {
                    if !alt.is_empty() {
                        let _ = write!(self.inline, " {alt} ");
                    }
                } else if let Some(src) = el.attr("src").filter(|s| !s.starts_with("data:")) {
                    let _ = write!(self.inline, "![{alt}]({})", self.resolve(src));
                } else if !alt.is_empty() {
                    let _ = write!(self.inline, "![{alt}]()");
                }
            }
            "strong" | "b" => self.wrap_inline(id, "**"),
            "em" | "i" => self.wrap_inline(id, "*"),
            "code" | "kbd" | "samp" => {
                if self.pre_depth > 0 {
                    self.block_children(id);
                } else {
                    self.wrap_inline(id, "`");
                }
            }
            "del" | "s" => self.wrap_inline(id, "~~"),
            "input" => {
                let ty = el
                    .attr("type")
                    .map(|t| t.to_ascii_lowercase())
                    .unwrap_or_else(|| "text".into());
                match ty.as_str() {
                    "submit" | "button" | "reset" => {
                        if let Some(v) = el.attr("value") {
                            let r = self.control_ref(id);
                            let _ = write!(self.inline, " [{v}{r}] ");
                        }
                    }
                    "hidden" => {}
                    "checkbox" | "radio" => {
                        if !self.plain {
                            let name =
                                name_for(self.dom, id, Some(ty.as_str()), self.oracle, self.labels);
                            let mark = if self.oracle.is_checked(self.dom, id).unwrap_or(false) {
                                "x"
                            } else {
                                " "
                            };
                            let r = self.control_ref(id);
                            let _ = write!(self.inline, " [{mark}] {name}{} ", bracketed(&r));
                        }
                    }
                    _ => {
                        let name =
                            name_for(self.dom, id, Some("textbox"), self.oracle, self.labels);
                        if !name.is_empty() && !self.plain {
                            let r = self.control_ref(id);
                            let _ = write!(self.inline, " [{name}: ___{r}] ");
                        }
                    }
                }
            }
            "button" => {
                let text = subtree_text(self.dom, id, self.oracle);
                if !text.is_empty() {
                    let r = self.control_ref(id);
                    let _ = write!(self.inline, " [{text}{r}] ");
                }
            }
            "select" => {
                if !self.plain {
                    let name = name_for(self.dom, id, Some("combobox"), self.oracle, self.labels);
                    let chosen = match self.oracle.displayed_options(self.dom, id) {
                        Some(options) => options,
                        None => self
                            .dom
                            .descendants(id)
                            .filter(|&o| self.dom.is_html_element(o, "option"))
                            .find(|&o| self.dom.attr(o, "selected").is_some())
                            .into_iter()
                            .collect(),
                    };
                    let shown: Vec<String> = chosen
                        .iter()
                        .map(|&o| subtree_text(self.dom, o, self.oracle))
                        .collect();
                    let r = self.control_ref(id);
                    let label = if name.is_empty() {
                        String::new()
                    } else {
                        format!("{name}: ")
                    };
                    let _ = write!(self.inline, " [{label}{} ▾{r}] ", shown.join(", "));
                }
            }
            "textarea" => {
                let name = name_for(self.dom, id, Some("textbox"), self.oracle, self.labels);
                if !name.is_empty() && !self.plain {
                    let r = self.control_ref(id);
                    let _ = write!(self.inline, " [{name}: ___{r}] ");
                }
            }
            "script" | "style" | "template" | "noscript" | "iframe" | "object" | "embed"
            | "canvas" | "video" | "audio" | "map" | "head" | "title" => {}
            _ => self.block_children(id),
        }
    }

    fn wrap_inline(&mut self, id: NodeId, marker: &str) {
        if self.plain {
            self.block_children(id);
            return;
        }
        let start = self.inline.len();
        self.block_children(id);
        let text = collapse_whitespace(&self.inline[start..]);
        self.inline.truncate(start);
        if !text.is_empty() {
            let _ = write!(self.inline, "{marker}{text}{marker}");
        }
    }

    fn list_item(&mut self, id: NodeId) {
        self.flush_inline();
        let depth = self.list_stack.len().saturating_sub(1);
        let marker = match self.list_stack.last_mut() {
            Some(Some(n)) => {
                let m = format!("{n}. ");
                *n += 1;
                m
            }
            _ => "- ".to_string(),
        };
        // Render the item's content in a nested writer so block children
        // (nested lists, paragraphs) indent under the marker. The nested
        // writer starts a fresh list stack: its own nesting is expressed by
        // the continuation-line indent added below.
        let mut inner = MdWriter {
            dom: self.dom,
            oracle: self.oracle,
            refs: self.refs.as_mut().map(RefScope::reborrow),
            labels: self.labels,
            base: self.base.clone(),
            link_style: self.link_style,
            plain: self.plain,
            out: String::new(),
            inline: String::new(),
            list_stack: Vec::new(),
            pre_depth: 0,
        };
        inner.block_children(id);
        inner.flush_inline();
        let body = inner.out.trim_end().to_string();
        let indent = "  ".repeat(depth);
        if !self.out.ends_with('\n') && !self.out.is_empty() {
            self.out.push('\n');
        }
        let mut lines = body.lines().filter(|l| !l.trim().is_empty());
        let first = lines.next().unwrap_or("");
        let _ = writeln!(self.out, "{indent}{marker}{first}");
        for line in lines {
            let _ = writeln!(self.out, "{indent}  {line}");
        }
    }

    fn table(&mut self, id: NodeId) {
        self.flush_inline();
        if is_layout_table(self.dom, id) {
            // Layout tables read as a sequence of blocks: each cell's content
            // in order, with row boundaries as block breaks.
            for row in self
                .dom
                .descendants(id)
                .filter(|&n| self.dom.is_html_element(n, "tr"))
                .collect::<Vec<_>>()
            {
                if is_hidden(self.dom, row, self.oracle) {
                    continue;
                }
                let nested_table = self
                    .dom
                    .ancestors(row)
                    .take_while(|&a| a != id)
                    .any(|a| self.dom.is_html_element(a, "table"));
                if nested_table {
                    continue; // rendered by the nested table's own pass
                }
                for cell in self.dom.child_elements(row).collect::<Vec<_>>() {
                    if self.dom.is_html_element(cell, "td") || self.dom.is_html_element(cell, "th")
                    {
                        self.block_children(cell);
                        self.flush_inline();
                    }
                }
            }
            return;
        }
        let mut rows: Vec<(bool, Vec<String>)> = Vec::new();
        for row in self
            .dom
            .descendants(id)
            .filter(|&n| self.dom.is_html_element(n, "tr"))
        {
            if is_hidden(self.dom, row, self.oracle) {
                continue;
            }
            let mut cells = Vec::new();
            let mut header = false;
            for cell in self.dom.child_elements(row) {
                if self.dom.is_html_element(cell, "th") || self.dom.is_html_element(cell, "td") {
                    header |= self.dom.is_html_element(cell, "th");
                    cells.push(subtree_text(self.dom, cell, self.oracle).replace('|', "\\|"));
                }
            }
            if !cells.is_empty() {
                rows.push((header, cells));
            }
        }
        if rows.is_empty() {
            return;
        }
        if let Some(caption) = self
            .dom
            .child_elements(id)
            .find(|&c| self.dom.is_html_element(c, "caption"))
        {
            let text = subtree_text(self.dom, caption, self.oracle);
            if !text.is_empty() {
                self.push_block_line(&if self.plain {
                    text
                } else {
                    format!("**{text}**")
                });
            }
        }
        self.ensure_blank_line();
        let width = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
        if self.plain {
            for (_, cells) in &rows {
                let _ = writeln!(self.out, "{}", cells.join(" | "));
            }
            return;
        }
        let pad = |cells: &Vec<String>| {
            let mut c = cells.clone();
            c.resize(width, String::new());
            format!("| {} |", c.join(" | "))
        };
        let (first_is_header, _) = rows[0];
        let header_cells = if first_is_header {
            rows[0].1.clone()
        } else {
            vec![String::new(); width]
        };
        let _ = writeln!(self.out, "{}", pad(&header_cells));
        let _ = writeln!(self.out, "|{}", " --- |".repeat(width));
        for (_, cells) in rows.iter().skip(if first_is_header { 1 } else { 0 }) {
            let _ = writeln!(self.out, "{}", pad(cells));
        }
    }
}

/// The first `aria-label` or `title` found on a descendant element.
fn descendant_label(dom: &Dom, id: NodeId) -> Option<String> {
    dom.descendants(id).find_map(|n| {
        let el = dom.element(n)?;
        el.attr("aria-label")
            .or_else(|| el.attr("title"))
            .map(collapse_whitespace)
            .filter(|s| !s.is_empty())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    /// The link's ref, when a ref table was given.
    pub r#ref: Option<String>,
    pub text: String,
    pub href: String,
}

/// Every visible link with an `href`, resolved against the document URL.
pub fn links(dom: &Dom, oracle: &dyn StyleOracle, mut refs: Option<RefScope<'_>>) -> Vec<LinkInfo> {
    let base = dom.url().cloned();
    let mut out = Vec::new();
    for n in dom.descendants(dom.document()) {
        let Some(el) = dom.element(n) else { continue };
        if !(el.is_html() && (&*el.name.local == "a" || &*el.name.local == "area")) {
            continue;
        }
        let Some(href) = el.attr("href") else {
            continue;
        };
        if is_hidden(dom, n, oracle) || dom.ancestors(n).any(|a| is_hidden(dom, a, oracle)) {
            continue;
        }
        let resolved = base
            .as_ref()
            .and_then(|b| b.join(href.trim()).ok())
            .map(|u| u.to_string())
            .unwrap_or_else(|| href.trim().to_string());
        let mut text = subtree_text(dom, n, oracle);
        if text.is_empty() {
            text = el
                .attr("aria-label")
                .or_else(|| el.attr("title"))
                .map(collapse_whitespace)
                .unwrap_or_default();
        }
        out.push(LinkInfo {
            r#ref: refs
                .as_mut()
                .map(|r| format!("e{}", r.assign(dom, n, oracle))),
            text,
            href: resolved,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    pub r#ref: Option<String>,
    /// `text`, `password`, `checkbox`, `select`, `textarea`, `submit`, ...
    pub kind: String,
    pub name: Option<String>,
    pub label: String,
    pub value: String,
    /// For checkboxes and radio buttons: whether they are checked now.
    pub checked: Option<bool>,
    pub required: bool,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormInfo {
    pub r#ref: Option<String>,
    pub action: Option<String>,
    pub method: String,
    pub fields: Vec<FieldInfo>,
}

/// Forms and their controls (plus controls outside any form, grouped as a
/// final form with no action).
pub fn forms(dom: &Dom, oracle: &dyn StyleOracle, mut refs: Option<RefScope<'_>>) -> Vec<FormInfo> {
    let labels = LabelIndex::build(dom);
    let base = dom.url().cloned();
    let mut forms: Vec<(Option<NodeId>, FormInfo)> = Vec::new();
    let doc = dom.document();
    for n in dom.descendants(doc) {
        if dom.is_html_element(n, "form") && !is_hidden(dom, n, oracle) {
            let el = dom.element(n).unwrap();
            let action = el.attr("action").map(|a| {
                base.as_ref()
                    .and_then(|b| b.join(a.trim()).ok())
                    .map(|u| u.to_string())
                    .unwrap_or_else(|| a.trim().to_string())
            });
            forms.push((
                Some(n),
                FormInfo {
                    r#ref: refs
                        .as_mut()
                        .map(|r| format!("e{}", r.assign(dom, n, oracle))),
                    action,
                    method: el
                        .attr("method")
                        .map(|m| m.trim().to_ascii_uppercase())
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| "GET".to_string()),
                    fields: Vec::new(),
                },
            ));
        }
    }
    let mut loose = FormInfo {
        r#ref: None,
        action: None,
        method: String::new(),
        fields: Vec::new(),
    };
    for n in dom.descendants(doc) {
        let Some(el) = dom.element(n) else { continue };
        if !el.is_html() || !matches!(&*el.name.local, "input" | "select" | "textarea" | "button") {
            continue;
        }
        if is_hidden(dom, n, oracle) {
            continue;
        }
        let local = &*el.name.local;
        let kind = match local {
            "input" => el
                .attr("type")
                .map(|t| t.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "text".into()),
            "button" => el
                .attr("type")
                .map(|t| t.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "submit".into()),
            other => other.to_string(),
        };
        if kind == "hidden" {
            continue;
        }
        let role = role_for(dom, n);
        let label = name_for(dom, n, role, oracle, &labels);
        let (value, options) = match local {
            "select" => {
                let opts: Vec<String> = dom
                    .descendants(n)
                    .filter(|&o| dom.is_html_element(o, "option"))
                    .map(|o| subtree_text(dom, o, oracle))
                    .collect();
                let selected = match oracle.displayed_options(dom, n) {
                    Some(shown) => shown
                        .iter()
                        .map(|&o| subtree_text(dom, o, oracle))
                        .collect::<Vec<_>>()
                        .join(", "),
                    None => dom
                        .descendants(n)
                        .filter(|&o| dom.is_html_element(o, "option"))
                        .find(|&o| dom.attr(o, "selected").is_some())
                        .map(|o| subtree_text(dom, o, oracle))
                        .or_else(|| opts.first().cloned())
                        .unwrap_or_default(),
                };
                (selected, opts)
            }
            "textarea" => (
                oracle
                    .control_value(dom, n)
                    .unwrap_or_else(|| dom.text_content(n)),
                Vec::new(),
            ),
            "button" => (subtree_text(dom, n, oracle), Vec::new()),
            _ => {
                let v = oracle
                    .control_value(dom, n)
                    .unwrap_or_else(|| el.attr("value").unwrap_or("").to_string());
                let v = if kind == "password" && !v.is_empty() {
                    "***".to_string()
                } else {
                    v
                };
                (v, Vec::new())
            }
        };
        let checked = (kind == "checkbox" || kind == "radio").then(|| {
            oracle
                .is_checked(dom, n)
                .unwrap_or_else(|| el.has_attr("checked"))
        });
        let field = FieldInfo {
            r#ref: refs
                .as_mut()
                .map(|r| format!("e{}", r.assign(dom, n, oracle))),
            kind,
            checked,
            name: el.attr("name").map(str::to_string),
            label,
            value,
            required: el.has_attr("required"),
            options,
        };
        let owner = el
            .form_owner
            .or_else(|| dom.ancestors(n).find(|&a| dom.is_html_element(a, "form")));
        match owner.and_then(|o| forms.iter_mut().find(|(id, _)| *id == Some(o))) {
            Some((_, form)) => form.fields.push(field),
            None => loose.fields.push(field),
        }
    }
    let mut out: Vec<FormInfo> = forms.into_iter().map(|(_, f)| f).collect();
    // Buttons outside any form belong to the page, not to a form, unless
    // there are fields beside them (a form without a `<form>`).
    let is_button = |f: &FieldInfo| matches!(f.kind.as_str(), "submit" | "button" | "reset");
    if loose.fields.iter().all(is_button) {
        loose.fields.clear();
    }
    if !loose.fields.is_empty() {
        out.push(loose);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::{HtmlParseOptions, parse_html};

    fn parse(html: &str) -> Dom {
        let opts = HtmlParseOptions {
            url: Some(Url::parse("https://example.com/base/").unwrap()),
            ..Default::default()
        };
        parse_html(html, &opts).dom
    }

    #[test]
    fn markdown_rendering() {
        let dom = parse(
            r#"<h1>Title</h1><p>Hello <b>bold</b> and <a href="x">link</a>.</p>
<ul><li>one</li><li>two<ul><li>nested</li></ul></li></ul>
<ol start=3><li>three</li></ol>
<pre><code>let x = 1;
let y = 2;</code></pre>
<blockquote><p>quoted</p></blockquote>
<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
<p style="display:none">hidden</p><script>no()</script>"#,
        );
        let md = markdown(&dom, &AttributeOracle, None, &ReadOptions::default());
        let expected = "# Title\n\nHello **bold** and [link](https://example.com/base/x).\n\n- one\n- two\n  - nested\n\n3. three\n\n```\nlet x = 1;\nlet y = 2;\n```\n\n> quoted\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        assert_eq!(md, expected);
    }

    #[test]
    fn plain_text_and_links() {
        let dom = parse("<p>Hi <a href=/a>there</a></p><a href='https://o.example/' hidden>x</a>");
        assert_eq!(text(&dom, &AttributeOracle), "Hi there\n");
        let l = links(&dom, &AttributeOracle, None);
        assert_eq!(
            l,
            vec![LinkInfo {
                r#ref: None,
                text: "there".into(),
                href: "https://example.com/a".into()
            }]
        );
    }

    #[test]
    fn forms_are_extracted() {
        let dom = parse(
            r#"<form action=/login method=post><label>User <input name=u value=bob></label>
<input type=password name=p><select name=s><option>A</option><option selected>B</option></select>
<button>Go</button></form><input name=loose>"#,
        );
        let f = forms(&dom, &AttributeOracle, None);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].action.as_deref(), Some("https://example.com/login"));
        assert_eq!(f[0].method, "POST");
        assert_eq!(f[0].fields.len(), 4);
        assert_eq!(f[0].fields[0].label, "User");
        assert_eq!(f[0].fields[0].value, "bob");
        assert_eq!(f[0].fields[2].value, "B");
        assert_eq!(f[0].fields[2].options, vec!["A", "B"]);
        assert_eq!(f[0].fields[3].kind, "submit");
        assert_eq!(f[1].fields[0].name.as_deref(), Some("loose"));
    }
}

#[cfg(test)]
mod block_link_tests {
    use super::*;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::{HtmlParseOptions, parse_html};

    #[test]
    fn links_around_block_content_do_not_lose_their_place() {
        let opts = HtmlParseOptions {
            url: Some(Url::parse("https://example.com/").unwrap()),
            ..Default::default()
        };
        let dom = parse_html(
            r#"<p>before <a href="/x">lead <div><p>inner block</p></div> tail</a> after</p>"#,
            &opts,
        )
        .dom;
        let md = markdown(&dom, &AttributeOracle, None, &ReadOptions::default());
        assert!(md.contains("inner block"), "{md}");
        assert!(md.contains("https://example.com/x"), "{md}");
        assert!(md.contains("after"), "{md}");
    }
}
