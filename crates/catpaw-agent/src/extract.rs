//! More read views: tables as GFM tables, text search, and HTML.

use std::collections::HashSet;
use std::fmt::Write as _;

use catpaw_dom::{Dom, NodeId, NodeKind};

use crate::a11y::{collapse_whitespace, is_interactive, role_for, subtree_text};
use crate::refs::RefScope;
use crate::snapshot::{quote, truncate};
use crate::visibility::{StyleOracle, is_hidden};

/// Whether `id` or an ancestor is hidden.
fn hidden(dom: &Dom, id: NodeId, oracle: &dyn StyleOracle) -> bool {
    std::iter::once(id)
        .chain(dom.ancestors(id))
        .any(|n| is_hidden(dom, n, oracle))
}

fn is_html(dom: &Dom, id: NodeId, local: &str) -> bool {
    dom.is_html_element(id, local)
}

/// The rows of a table, in order (those of `thead`, `tbody` and `tfoot`
/// too), leaving out nested tables'.
fn rows(dom: &Dom, table: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    for child in dom.children(table) {
        if is_html(dom, child, "tr") {
            out.push(child);
        } else if ["thead", "tbody", "tfoot"]
            .iter()
            .any(|t| is_html(dom, child, t))
        {
            out.extend(dom.children(child).filter(|&r| is_html(dom, r, "tr")));
        }
    }
    out
}

fn cells(dom: &Dom, row: NodeId) -> Vec<NodeId> {
    dom.children(row)
        .filter(|&c| is_html(dom, c, "td") || is_html(dom, c, "th"))
        .collect()
}

/// GFM-safe cell text.
fn cell_text(text: &str) -> String {
    truncate(&collapse_whitespace(text), 80).replace('|', "\\|")
}

/// Every visible table as a GFM table whose first column holds the rows'
/// refs; the controls in a cell follow its text as `[eN role "name"]`.
pub fn tables(
    dom: &Dom,
    oracle: &dyn StyleOracle,
    mut refs: Option<RefScope<'_>>,
    root: Option<NodeId>,
) -> String {
    let root = root.unwrap_or_else(|| dom.document());
    let mut out = String::new();
    let all: Vec<NodeId> = std::iter::once(root)
        .chain(dom.descendants(root))
        .filter(|&n| is_html(dom, n, "table"))
        .filter(|&t| !dom.ancestors(t).any(|a| is_html(dom, a, "table")))
        .filter(|&t| !hidden(dom, t, oracle))
        .collect();
    for table in all {
        let rows: Vec<NodeId> = rows(dom, table)
            .into_iter()
            .filter(|&r| !is_hidden(dom, r, oracle))
            .collect();
        if rows.is_empty() {
            continue;
        }
        let width = rows.iter().map(|&r| cells(dom, r).len()).max().unwrap_or(0);
        if width == 0 {
            continue;
        }
        let mut assign = |n: NodeId| {
            refs.as_mut()
                .map(|s| format!("e{}", s.assign(dom, n, oracle)))
                .unwrap_or_default()
        };
        let caption = dom
            .children(table)
            .find(|&c| is_html(dom, c, "caption"))
            .map(|c| collapse_whitespace(&subtree_text(dom, c, oracle)))
            .or_else(|| dom.attr(table, "aria-label").map(collapse_whitespace))
            .unwrap_or_default();
        let table_ref = assign(table);
        let _ = write!(out, "table {table_ref}");
        if !caption.is_empty() {
            let _ = write!(out, " {}", quote(&truncate(&caption, 80)));
        }
        let _ = writeln!(out, " ({} rows)", rows.len());
        // The first row is the header when it is all `th`.
        let first = cells(dom, rows[0]);
        let header_row = first.iter().all(|&c| is_html(dom, c, "th"));
        let mut header: Vec<String> = if header_row {
            first
                .iter()
                .map(|&c| cell_text(&subtree_text(dom, c, oracle)))
                .collect()
        } else {
            (1..=width).map(|i| format!("{i}")).collect()
        };
        header.resize(width, String::new());
        let _ = writeln!(out, "| ref | {} |", header.join(" | "));
        let _ = writeln!(out, "|---|{}", "---|".repeat(width));
        for &row in rows.iter().skip(usize::from(header_row)) {
            let row_ref = assign(row);
            let mut values: Vec<String> = Vec::with_capacity(width);
            for cell in cells(dom, row) {
                let mut text = cell_text(&subtree_text(dom, cell, oracle));
                let mut controls = String::new();
                let mut names = Vec::new();
                for control in dom.descendants(cell) {
                    let Some(role) = role_for(dom, control) else {
                        continue;
                    };
                    if !is_interactive(role) || is_hidden(dom, control, oracle) {
                        continue;
                    }
                    let r = assign(control);
                    let name = collapse_whitespace(&subtree_text(dom, control, oracle));
                    if !controls.is_empty() {
                        controls.push(' ');
                    }
                    if name.is_empty() {
                        let _ = write!(controls, "[{r} {role}]");
                    } else {
                        let _ = write!(controls, "[{r} {role} {}]", quote(&truncate(&name, 40)));
                    }
                    names.push(name);
                }
                // A cell that only shows its controls' names shows the
                // controls.
                if !names.is_empty() && cell_text(&names.join(" ")) == text {
                    text.clear();
                }
                if !controls.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&controls);
                }
                values.push(text);
            }
            values.resize(width, String::new());
            let _ = writeln!(out, "| {row_ref} | {} |", values.join(" | "));
        }
        out.push('\n');
    }
    out
}

/// One match of a text search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    /// The ref of the element around the match.
    pub r#ref: Option<String>,
    pub role: &'static str,
    /// Text around the match, the match in `**`.
    pub context: String,
}

/// Elements that make a sentence's context: blocks, cells, controls.
fn is_context(dom: &Dom, id: NodeId) -> bool {
    let Some(el) = dom.element(id) else {
        return false;
    };
    if el.has_attr("role") {
        return true;
    }
    el.is_html()
        && matches!(
            &*el.name.local,
            "p" | "li"
                | "td"
                | "th"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "a"
                | "button"
                | "label"
                | "dt"
                | "dd"
                | "figcaption"
                | "blockquote"
                | "pre"
                | "caption"
                | "summary"
                | "legend"
                | "option"
                | "article"
                | "section"
                | "div"
        )
}

/// Searches the visible text for `matches` (which gives the byte ranges of
/// the matches in a text), returning at most `limit` hits and the number
/// of matches found in all.
pub fn find(
    dom: &Dom,
    oracle: &dyn StyleOracle,
    mut refs: Option<RefScope<'_>>,
    root: Option<NodeId>,
    matches: &dyn Fn(&str) -> Vec<(usize, usize)>,
    limit: usize,
) -> (Vec<FindHit>, usize) {
    let root = root.unwrap_or_else(|| dom.document());
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut hits = Vec::new();
    let mut total = 0;
    for n in dom.descendants(root) {
        let NodeKind::Text(t) = dom.kind(n) else {
            continue;
        };
        if t.trim().is_empty() {
            continue;
        }
        let Some(parent) = dom.parent_element(n) else {
            continue;
        };
        if ["script", "style", "noscript", "template"]
            .iter()
            .any(|s| is_html(dom, parent, s))
            || hidden(dom, parent, oracle)
        {
            continue;
        }
        let context = std::iter::once(parent)
            .chain(dom.ancestors(parent))
            .find(|&a| is_context(dom, a))
            .unwrap_or(parent);
        if !seen.insert(context) {
            continue;
        }
        let text = collapse_whitespace(&subtree_text(dom, context, oracle));
        for (start, end) in matches(&text) {
            total += 1;
            if hits.len() >= limit {
                continue;
            }
            let from = floor(&text, start.saturating_sub(60));
            let to = ceil(&text, (end + 60).min(text.len()));
            let mut snippet = String::new();
            if from > 0 {
                snippet.push('…');
            }
            let _ = write!(
                snippet,
                "{}**{}**{}",
                &text[from..start],
                &text[start..end],
                &text[end..to]
            );
            if to < text.len() {
                snippet.push('…');
            }
            let role = match role_for(dom, context) {
                None | Some("none") => "generic",
                Some(role) => role,
            };
            hits.push(FindHit {
                r#ref: refs
                    .as_mut()
                    .map(|s| format!("e{}", s.assign(dom, context, oracle))),
                role,
                context: snippet,
            });
        }
    }
    (hits, total)
}

fn floor(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// The HTML of `root` (the body when `None`), without scripts, styles and
/// comments: markup as a page author wrote it, for what the other views
/// leave out (classes, data attributes).
pub fn html(dom: &Dom, root: Option<NodeId>) -> String {
    let root = root
        .or_else(|| {
            dom.descendants(dom.document())
                .find(|&n| is_html(dom, n, "body"))
        })
        .unwrap_or_else(|| dom.document());
    let mut out = String::new();
    write_html(dom, root, &mut out);
    out
}

fn escape(text: &str, attribute: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' if !attribute => out.push_str("&lt;"),
            '>' if !attribute => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
    out
}

fn write_html(dom: &Dom, id: NodeId, out: &mut String) {
    match dom.kind(id) {
        NodeKind::Text(t) => out.push_str(&escape(t, false)),
        NodeKind::Element(el) => {
            let local = &*el.name.local;
            if el.is_html() && matches!(local, "script" | "style" | "noscript" | "template") {
                return;
            }
            let _ = write!(out, "<{local}");
            for attr in el.attrs.iter() {
                let _ = write!(
                    out,
                    " {}=\"{}\"",
                    &*attr.name.local,
                    escape(&attr.value, true)
                );
            }
            out.push('>');
            if el.is_html() && VOID.contains(&local) {
                return;
            }
            for child in dom.children(id) {
                write_html(dom, child, out);
            }
            let _ = write!(out, "</{local}>");
        }
        NodeKind::Document(_) | NodeKind::DocumentFragment(_) => {
            for child in dom.children(id) {
                write_html(dom, child, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::RefTable;
    use crate::visibility::AttributeOracle;
    use catpaw_dom::parse_html;

    fn dom(html: &str) -> Dom {
        parse_html(html, &Default::default()).dom
    }

    #[test]
    fn tables_render_as_gfm_with_row_refs() {
        let d = dom("<table><caption>Cart</caption><tr><th>Item<th>Qty<th>\
             <tr><td>Wool socks<td>2<td><button>Remove</button></table>");
        let mut refs = RefTable::new();
        let text = tables(&d, &AttributeOracle, Some(RefScope::plain(&mut refs)), None);
        assert_eq!(
            text,
            "table e1 \"Cart\" (2 rows)\n| ref | Item | Qty |  |\n|---|---|---|---|\n\
             | e2 | Wool socks | 2 | [e3 button \"Remove\"] |\n\n"
        );
    }

    #[test]
    fn find_gives_context_and_counts() {
        let d = dom(
            "<p>Shipping is <b>free</b> over $50.</p><ul><li>free returns</li></ul>\
             <p hidden>free hidden</p>",
        );
        let mut refs = RefTable::new();
        let matcher = |t: &str| -> Vec<(usize, usize)> {
            t.match_indices("free")
                .map(|(i, m)| (i, i + m.len()))
                .collect()
        };
        let (hits, total) = find(
            &d,
            &AttributeOracle,
            Some(RefScope::plain(&mut refs)),
            None,
            &matcher,
            20,
        );
        assert_eq!(total, 2);
        assert_eq!(hits[0].role, "paragraph");
        assert_eq!(hits[0].context, "Shipping is **free** over $50.");
        assert_eq!(hits[1].role, "listitem");
    }

    #[test]
    fn html_leaves_out_scripts_and_styles() {
        let d = dom(
            "<body><div class=a>x &amp; y<script>s()</script><style>p{}</style><br></div></body>",
        );
        assert_eq!(
            html(&d, None),
            "<body><div class=\"a\">x &amp; y<br></div></body>"
        );
    }
}
