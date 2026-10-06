//! Roles and accessible names: a practical subset of HTML-AAM and the
//! Accessible Name and Description Computation, enough for the snapshot to
//! match what Chromium-based agent tools show.

use std::collections::HashMap;

use catpaw_dom::{Dom, ElementData, NodeId, NodeKind};

use crate::visibility::{StyleOracle, is_hidden};

/// Roles whose accessible name may come from their content.
pub const NAME_FROM_CONTENT: &[&str] = &[
    "button",
    "cell",
    "checkbox",
    "columnheader",
    "gridcell",
    "heading",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "radio",
    "rowheader",
    "switch",
    "tab",
    "tooltip",
    "treeitem",
];

/// Roles an agent can act on directly.
pub const INTERACTIVE: &[&str] = &[
    "link",
    "button",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "option",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "treeitem",
    "iframe",
];

pub const LANDMARKS: &[&str] = &[
    "banner",
    "complementary",
    "contentinfo",
    "form",
    "main",
    "navigation",
    "region",
    "search",
];

/// Roles accepted from the `role` attribute.
const KNOWN_ROLES: &[&str] = &[
    "alert",
    "alertdialog",
    "application",
    "article",
    "banner",
    "blockquote",
    "button",
    "caption",
    "cell",
    "checkbox",
    "code",
    "columnheader",
    "combobox",
    "complementary",
    "contentinfo",
    "definition",
    "deletion",
    "dialog",
    "directory",
    "document",
    "emphasis",
    "feed",
    "figure",
    "form",
    "generic",
    "grid",
    "gridcell",
    "group",
    "heading",
    "img",
    "image",
    "insertion",
    "link",
    "list",
    "listbox",
    "listitem",
    "log",
    "main",
    "marquee",
    "math",
    "menu",
    "menubar",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "meter",
    "navigation",
    "none",
    "note",
    "option",
    "paragraph",
    "presentation",
    "progressbar",
    "radio",
    "radiogroup",
    "region",
    "row",
    "rowgroup",
    "rowheader",
    "scrollbar",
    "search",
    "searchbox",
    "separator",
    "slider",
    "spinbutton",
    "status",
    "strong",
    "subscript",
    "superscript",
    "switch",
    "tab",
    "table",
    "tablist",
    "tabpanel",
    "term",
    "textbox",
    "time",
    "timer",
    "toolbar",
    "tooltip",
    "tree",
    "treegrid",
    "treeitem",
];

pub fn is_interactive(role: &str) -> bool {
    INTERACTIVE.contains(&role)
}

pub fn is_landmark(role: &str) -> bool {
    LANDMARKS.contains(&role)
}

pub fn names_from_content(role: &str) -> bool {
    NAME_FROM_CONTENT.contains(&role)
}

fn aria_token(value: &str) -> Option<&'static str> {
    for token in value.split_ascii_whitespace() {
        if let Some(role) = KNOWN_ROLES.iter().find(|r| r.eq_ignore_ascii_case(token)) {
            return Some(match *role {
                "presentation" => "none",
                "image" => "img",
                other => other,
            });
        }
    }
    None
}

fn input_type(el: &ElementData) -> String {
    el.attr("type")
        .map(|t| t.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "text".to_string())
}

fn has_ancestor(dom: &Dom, id: NodeId, locals: &[&str]) -> bool {
    dom.ancestors(id).any(|a| {
        dom.element(a)
            .is_some_and(|e| e.is_html() && locals.contains(&&*e.name.local))
    })
}

fn has_aria_name(el: &ElementData) -> bool {
    el.attr("aria-label").is_some_and(|v| !v.trim().is_empty())
        || el
            .attr("aria-labelledby")
            .is_some_and(|v| !v.trim().is_empty())
}

/// Chromium-style "data table" heuristic: tables used for layout carry no
/// table semantics, so they and their rows and cells read as generic.
pub fn is_layout_table(dom: &Dom, table: NodeId) -> bool {
    let Some(el) = dom.element(table) else {
        return false;
    };
    if let Some(role) = el.attr("role") {
        let role = role.trim().to_ascii_lowercase();
        if role == "presentation" || role == "none" {
            return true;
        }
        if matches!(role.as_str(), "table" | "grid" | "treegrid") {
            return false;
        }
    }
    if el.has_attr("summary")
        || dom
            .child_elements(table)
            .any(|c| dom.is_html_element(c, "caption"))
    {
        return false;
    }
    // Classic layout-table markup: zero border plus cell spacing/padding
    // attributes, with no header cells anywhere.
    let layout_attrs = el.attr("border").is_some_and(|b| b.trim() == "0")
        || el.has_attr("cellpadding")
        || el.has_attr("cellspacing");
    let mut has_header = false;
    let mut has_nested_table = false;
    let mut rows = 0usize;
    let mut cell_has_blocks = false;
    let mut has_colspan = false;
    for n in dom.descendants(table) {
        let Some(e) = dom.element(n) else { continue };
        if !e.is_html() {
            continue;
        }
        match &*e.name.local {
            "th" => has_header = true,
            "table" => has_nested_table = true,
            "tr" => rows += 1,
            "td" => {
                has_colspan |= e.has_attr("colspan");
                if dom.child_elements(n).any(|c| {
                    dom.element(c).is_some_and(|ce| {
                        ce.is_html()
                            && matches!(
                                &*ce.name.local,
                                "div"
                                    | "p"
                                    | "ul"
                                    | "ol"
                                    | "form"
                                    | "h1"
                                    | "h2"
                                    | "h3"
                                    | "h4"
                                    | "h5"
                                    | "h6"
                                    | "section"
                                    | "article"
                                    | "nav"
                                    | "header"
                                    | "footer"
                                    | "table"
                                    | "pre"
                                    | "blockquote"
                            )
                    })
                }) {
                    cell_has_blocks = true;
                }
            }
            _ => {}
        }
    }
    if has_header {
        return false;
    }
    if has_nested_table || cell_has_blocks || rows <= 1 || layout_attrs || has_colspan {
        return true;
    }
    let max_cols = dom
        .descendants(table)
        .filter(|&n| dom.is_html_element(n, "tr"))
        .map(|r| {
            dom.child_elements(r)
                .filter(|&c| dom.is_html_element(c, "td"))
                .count()
        })
        .max()
        .unwrap_or(0);
    max_cols <= 1
}

/// Whether `id` (a table part) belongs to a layout table.
fn in_layout_table(dom: &Dom, id: NodeId) -> bool {
    dom.ancestors(id)
        .find(|&a| dom.is_html_element(a, "table"))
        .is_some_and(|t| is_layout_table(dom, t))
}

/// The implicit role of an HTML element, or `None` for generic / no role.
fn implicit_role(dom: &Dom, id: NodeId, el: &ElementData) -> Option<&'static str> {
    if !el.is_html() {
        return match &*el.name.local {
            "svg" => Some("img"),
            "math" => Some("math"),
            _ => None,
        };
    }
    Some(match &*el.name.local {
        "a" | "area" => {
            if el.has_attr("href") {
                "link"
            } else {
                return None;
            }
        }
        "article" => "article",
        "aside" => "complementary",
        "nav" => "navigation",
        "main" => "main",
        "header" => {
            if has_ancestor(dom, id, &["article", "aside", "main", "nav", "section"]) {
                return None;
            }
            "banner"
        }
        "footer" => {
            if has_ancestor(dom, id, &["article", "aside", "main", "nav", "section"]) {
                return None;
            }
            "contentinfo"
        }
        "section" => {
            if has_aria_name(el) {
                "region"
            } else {
                return None;
            }
        }
        "form" => {
            if has_aria_name(el) || el.has_attr("title") {
                "form"
            } else {
                return None;
            }
        }
        "search" => "search",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "p" => "paragraph",
        "ul" | "ol" | "menu" => "list",
        "li" => "listitem",
        "dt" => "term",
        "dd" => "definition",
        "table" => {
            if is_layout_table(dom, id) {
                return None;
            }
            "table"
        }
        "thead" | "tbody" | "tfoot" => {
            if in_layout_table(dom, id) {
                return None;
            }
            "rowgroup"
        }
        "tr" => {
            if in_layout_table(dom, id) {
                return None;
            }
            "row"
        }
        "th" => {
            if el
                .attr("scope")
                .is_some_and(|s| s.eq_ignore_ascii_case("row"))
            {
                "rowheader"
            } else {
                "columnheader"
            }
        }
        "td" => {
            if in_layout_table(dom, id) {
                return None;
            }
            "cell"
        }
        "caption" => "caption",
        "img" => {
            if el.attr("alt").is_some_and(|a| a.is_empty()) {
                "none"
            } else {
                "img"
            }
        }
        "button" => "button",
        "summary" => "button",
        "input" => match input_type(el).as_str() {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "submit" | "button" | "reset" | "image" | "file" => "button",
            "number" => "spinbutton",
            "range" => "slider",
            "search" => "searchbox",
            "hidden" => return None,
            _ => "textbox",
        },
        "select" => {
            let size = el
                .attr("size")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(1);
            if el.has_attr("multiple") || size > 1 {
                "listbox"
            } else {
                "combobox"
            }
        }
        "option" => "option",
        "optgroup" => "group",
        "textarea" => "textbox",
        "fieldset" => "group",
        "details" => "group",
        "dialog" => "dialog",
        "hr" => "separator",
        "blockquote" => "blockquote",
        "code" => "code",
        "em" => "emphasis",
        "strong" => "strong",
        "del" | "s" => "deletion",
        "ins" => "insertion",
        "sub" => "subscript",
        "sup" => "superscript",
        "mark" => "mark",
        "time" => "time",
        "figure" => "figure",
        "address" => "group",
        "progress" => "progressbar",
        "meter" => "meter",
        "output" => "status",
        "iframe" | "frame" => "iframe",
        _ => return None,
    })
}

/// The role of an element: the first known `role` token, else the implicit
/// HTML role. `Some("none")` marks presentational elements; `None` is generic.
pub fn role_for(dom: &Dom, id: NodeId) -> Option<&'static str> {
    let el = dom.element(id)?;
    if let Some(explicit) = el.attr("role").and_then(aria_token) {
        return Some(explicit);
    }
    implicit_role(dom, id, el)
}

/// Collapses ASCII and Unicode whitespace runs to single spaces and trims.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// `<label for=...>` elements indexed by target id, built once per document.
#[derive(Debug, Default)]
pub struct LabelIndex {
    by_target: HashMap<String, Vec<NodeId>>,
}

impl LabelIndex {
    pub fn build(dom: &Dom) -> Self {
        let mut by_target: HashMap<String, Vec<NodeId>> = HashMap::new();
        for n in dom.descendants(dom.document()) {
            if dom.is_html_element(n, "label")
                && let Some(target) = dom.attr(n, "for")
            {
                by_target.entry(target.to_string()).or_default().push(n);
            }
        }
        Self { by_target }
    }

    fn labels_for(&self, id: &str) -> &[NodeId] {
        self.by_target.get(id).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// The rendered text of a subtree: text nodes plus image alt text, with
/// whitespace collapsed; hidden subtrees are skipped.
pub fn subtree_text(dom: &Dom, id: NodeId, oracle: &dyn StyleOracle) -> String {
    let mut out = String::new();
    collect_text(dom, id, oracle, &mut out);
    collapse_whitespace(&out)
}

fn collect_text(dom: &Dom, id: NodeId, oracle: &dyn StyleOracle, out: &mut String) {
    for child in dom.children(id) {
        match dom.kind(child) {
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Element(el) => {
                if is_hidden(dom, child, oracle) {
                    continue;
                }
                match &*el.name.local {
                    "img" | "area" => {
                        if let Some(alt) = el.attr("alt") {
                            out.push(' ');
                            out.push_str(alt);
                            out.push(' ');
                        }
                    }
                    "br" => out.push(' '),
                    "input" if el.is_html() => {
                        let ty = input_type(el);
                        if matches!(ty.as_str(), "submit" | "button" | "reset")
                            && let Some(v) = el.attr("value")
                        {
                            out.push_str(v);
                        }
                    }
                    "select" | "textarea" => {}
                    _ => {
                        if is_block_level(&el.name.local) {
                            out.push(' ');
                        }
                        collect_text(dom, child, oracle, out);
                        if is_block_level(&el.name.local) {
                            out.push(' ');
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn is_block_level(local: &str) -> bool {
    matches!(
        local,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "dd"
            | "details"
            | "dialog"
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
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "caption"
            | "tbody"
            | "td"
            | "tfoot"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "option"
    )
}

fn by_id(dom: &Dom, id: &str) -> Option<NodeId> {
    dom.descendants(dom.document())
        .find(|&n| dom.attr(n, "id") == Some(id))
}

fn nearest_ancestor_label(dom: &Dom, id: NodeId) -> Option<NodeId> {
    dom.ancestors(id).find(|&a| dom.is_html_element(a, "label"))
}

fn text_of_first_child_named(
    dom: &Dom,
    id: NodeId,
    local: &str,
    oracle: &dyn StyleOracle,
) -> Option<String> {
    dom.child_elements(id)
        .find(|&c| dom.is_html_element(c, local))
        .map(|c| subtree_text(dom, c, oracle))
        .filter(|s| !s.is_empty())
}

/// The accessible name of an element for a given role.
pub fn name_for(
    dom: &Dom,
    id: NodeId,
    role: Option<&str>,
    oracle: &dyn StyleOracle,
    labels: &LabelIndex,
) -> String {
    let Some(el) = dom.element(id) else {
        return String::new();
    };

    // 1. aria-labelledby
    if let Some(ids) = el.attr("aria-labelledby") {
        let parts: Vec<String> = ids
            .split_ascii_whitespace()
            .filter_map(|target| by_id(dom, target))
            .map(|n| {
                dom.attr(n, "aria-label")
                    .map(collapse_whitespace)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| subtree_text(dom, n, oracle))
            })
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    // 2. aria-label
    if let Some(label) = el.attr("aria-label") {
        let label = collapse_whitespace(label);
        if !label.is_empty() {
            return label;
        }
    }

    // 3. Native sources.
    let local = &*el.name.local;
    let native = if !el.is_html() {
        match local {
            "svg" => text_of_first_child_named(dom, id, "title", oracle),
            _ => None,
        }
    } else {
        match local {
            "img" | "area" => el.attr("alt").map(collapse_whitespace),
            "input"
                if matches!(
                    input_type(el).as_str(),
                    "submit" | "reset" | "button" | "image"
                ) =>
            {
                let ty = input_type(el);
                el.attr("alt")
                    .or_else(|| el.attr("value"))
                    .map(collapse_whitespace)
                    .filter(|s| !s.is_empty())
                    .or_else(|| match ty.as_str() {
                        "submit" => Some("Submit".to_string()),
                        "reset" => Some("Reset".to_string()),
                        _ => None,
                    })
            }
            "input" | "textarea" | "select" | "meter" | "progress" | "output" => {
                label_text(dom, id, el, oracle, labels).or_else(|| {
                    el.attr("placeholder")
                        .or_else(|| el.attr("aria-placeholder"))
                        .map(collapse_whitespace)
                        .filter(|s| !s.is_empty())
                })
            }
            "fieldset" => text_of_first_child_named(dom, id, "legend", oracle),
            "figure" => text_of_first_child_named(dom, id, "figcaption", oracle),
            "table" => text_of_first_child_named(dom, id, "caption", oracle),
            "iframe" | "frame" => el
                .attr("title")
                .or_else(|| el.attr("name"))
                .map(collapse_whitespace),
            _ => None,
        }
    };
    if let Some(name) = native.filter(|s| !s.is_empty()) {
        return name;
    }

    // 4. Name from content.
    if role.is_some_and(names_from_content) {
        let text = subtree_text(dom, id, oracle);
        if !text.is_empty() {
            return text;
        }
    }

    // 5. title attribute.
    el.attr("title")
        .map(collapse_whitespace)
        .unwrap_or_default()
}

fn label_text(
    dom: &Dom,
    id: NodeId,
    el: &ElementData,
    oracle: &dyn StyleOracle,
    labels: &LabelIndex,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(target) = el.id() {
        for &label in labels.labels_for(target) {
            let t = subtree_text(dom, label, oracle);
            if !t.is_empty() {
                parts.push(t);
            }
        }
    }
    if parts.is_empty()
        && let Some(label) = nearest_ancestor_label(dom, id)
    {
        let t = subtree_text(dom, label, oracle);
        if !t.is_empty() {
            parts.push(t);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Heading level for `h1`..`h6` or `aria-level`.
pub fn heading_level(el: &ElementData) -> Option<u32> {
    if let Some(level) = el.attr("aria-level").and_then(|v| v.trim().parse().ok()) {
        return Some(level);
    }
    if el.is_html() {
        return match &*el.name.local {
            "h1" => Some(1),
            "h2" => Some(2),
            "h3" => Some(3),
            "h4" => Some(4),
            "h5" => Some(5),
            "h6" => Some(6),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::parse_html;

    fn find(dom: &Dom, id: &str) -> NodeId {
        by_id(dom, id).unwrap()
    }

    #[test]
    fn roles_and_names() {
        let r = parse_html(
            r#"<nav id=nav><a id=l href=/x>Go <b>now</b></a></nav>
               <label for=q>Search</label><input id=q placeholder=Find>
               <input id=s type=submit>
               <img id=i alt="A cat"><img id=j alt="">
               <select id=sel><option>One</option></select>
               <section id=sec aria-label="Intro"><h2 id=h>Title</h2></section>
               <label>Email <input id=e type=email></label>"#,
            &Default::default(),
        );
        let dom = &r.dom;
        let oracle = AttributeOracle;
        let labels = LabelIndex::build(dom);
        let check = |id: &str, role: Option<&str>, name: &str| {
            let n = find(dom, id);
            let r = role_for(dom, n);
            assert_eq!(r, role, "role of #{id}");
            assert_eq!(name_for(dom, n, r, &oracle, &labels), name, "name of #{id}");
        };
        check("nav", Some("navigation"), "");
        check("l", Some("link"), "Go now");
        check("q", Some("textbox"), "Search");
        check("s", Some("button"), "Submit");
        check("i", Some("img"), "A cat");
        check("j", Some("none"), "");
        check("sel", Some("combobox"), "");
        check("sec", Some("region"), "Intro");
        check("h", Some("heading"), "Title");
        check("e", Some("textbox"), "Email");
        assert_eq!(heading_level(dom.element(find(dom, "h")).unwrap()), Some(2));
    }

    #[test]
    fn layout_tables_lose_table_semantics() {
        let r = parse_html(
            r#"<table id=layout><tr><td><div>nav</div></td><td><table><tr><td>x</td></tr></table></td></tr></table>
               <table id=data><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>
               <table id=grid><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>"#,
            &Default::default(),
        );
        let dom = &r.dom;
        assert!(is_layout_table(dom, find(dom, "layout")));
        assert_eq!(role_for(dom, find(dom, "layout")), None);
        assert!(!is_layout_table(dom, find(dom, "data")));
        assert_eq!(role_for(dom, find(dom, "data")), Some("table"));
        assert!(!is_layout_table(dom, find(dom, "grid")));
    }

    #[test]
    fn explicit_role_wins_and_unknown_roles_are_ignored() {
        let r = parse_html(
            "<div id=a role='bogus button'>x</div><div id=b role=nothing>y</div>",
            &Default::default(),
        );
        assert_eq!(role_for(&r.dom, find(&r.dom, "a")), Some("button"));
        assert_eq!(role_for(&r.dom, find(&r.dom, "b")), None);
    }
}
