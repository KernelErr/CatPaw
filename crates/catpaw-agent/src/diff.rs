//! Snapshot diffs: what changed between two snapshots of one document
//! (ADR 0005, amended).
//!
//! The diff works on the tree the lines describe, not on the lines: an
//! element is the same element when its ref is, a text is the i-th text of
//! its parent. Lines come out in document order:
//!
//! ```text
//! ~ e11 spinbutton "Quantity" [value=2 → 3]
//! ~ e6 text[2] "208 points" → "209 points"
//! + e41 status "Cart updated" (in e6, after e8)
//! + e52 row "Beanie" (replaces e13)
//! - e14 row "Socks" (+3 descendants)
//! > e19 button "Checkout" (now in e30, after e29)
//! ```
//!
//! Diff lines always use the compact form, whatever form snapshots use.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use crate::snapshot::{Format, LineKind, SnapLine, attr_value, quote, render_line, truncate};

/// A snapshot's lines as a tree.
struct Tree<'a> {
    lines: &'a [SnapLine],
    /// The parent element (line index) of each line.
    parent: Vec<Option<usize>>,
    /// Element ref to line index.
    by_ref: HashMap<u32, usize>,
    /// The children (line indexes) of each element, and of the top (`None`).
    children: HashMap<Option<usize>, Vec<usize>>,
}

impl<'a> Tree<'a> {
    fn new(lines: &'a [SnapLine]) -> Self {
        let mut parent = vec![None; lines.len()];
        let mut by_ref = HashMap::new();
        let mut children: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        let mut stack: Vec<(u16, usize)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if matches!(line.kind, LineKind::Truncated(_) | LineKind::More { .. }) {
                continue;
            }
            while stack.last().is_some_and(|&(d, _)| d >= line.depth) {
                stack.pop();
            }
            let p = stack.last().map(|&(_, j)| j);
            parent[i] = p;
            children.entry(p).or_default().push(i);
            if let LineKind::Element { r, .. } = line.kind {
                by_ref.insert(r, i);
                stack.push((line.depth, i));
            }
        }
        Self {
            lines,
            parent,
            by_ref,
            children,
        }
    }

    fn r(&self, i: usize) -> Option<u32> {
        match self.lines[i].kind {
            LineKind::Element { r, .. } => Some(r),
            _ => None,
        }
    }

    fn parent_ref(&self, i: usize) -> Option<u32> {
        self.parent[i].and_then(|p| self.r(p))
    }

    fn kids(&self, i: Option<usize>) -> &[usize] {
        self.children.get(&i).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The element just before `i` among its parent's children.
    fn previous_element(&self, i: usize) -> Option<u32> {
        let siblings = self.kids(self.parent[i]);
        let at = siblings.iter().position(|&s| s == i)?;
        siblings[..at].iter().rev().find_map(|&s| self.r(s))
    }

    /// Lines below `i`.
    fn descendants(&self, i: usize) -> usize {
        let depth = self.lines[i].depth;
        self.lines[i + 1..]
            .iter()
            .take_while(|l| l.depth > depth)
            .count()
    }

    /// The texts among the children of `parent`, in order.
    fn texts(&self, parent: Option<usize>) -> Vec<(usize, &'a str)> {
        self.kids(parent)
            .iter()
            .filter_map(|&i| match &self.lines[i].kind {
                LineKind::Text(t) => Some((i, t.as_str())),
                _ => None,
            })
            .collect()
    }
}

/// Where a node sits and what it is: parent ref, role and name.
type Signature = (Option<u32>, &'static str, String);

/// What changed between two snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// The diff lines, in document order.
    pub lines: Vec<String>,
    pub changed: usize,
    pub added: usize,
    pub removed: usize,
    pub moved: usize,
    pub unchanged: usize,
    /// Re-rendered elements: (old ref, new ref), the subtrees' too.
    pub replaced: Vec<(u32, u32)>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The lines, each ending in a newline.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// The counts that are not zero (`changed=3 added=1`), or `no
    /// changes`.
    pub fn stats(&self) -> String {
        if self.is_empty() {
            return "no changes".to_string();
        }
        [
            ("changed", self.changed),
            ("added", self.added),
            ("removed", self.removed),
            ("moved", self.moved),
        ]
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(key, n)| format!("{key}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
    }
}

/// `in e6, after e8` and its variants.
fn position(parent: Option<u32>, after: Option<u32>) -> String {
    match (parent, after) {
        (Some(p), Some(s)) => format!("in e{p}, after e{s}"),
        (Some(p), None) => format!("in e{p}, first"),
        (None, Some(s)) => format!("after e{s}"),
        (None, None) => "first".to_string(),
    }
}

/// An element's head: `e12 link "Home" [attrs]`, without its inline text.
fn head(line: &SnapLine) -> String {
    let mut bare = line.clone();
    bare.depth = 0;
    if let LineKind::Element { text, .. } = &mut bare.kind {
        *text = None;
    }
    let mut out = String::new();
    render_line(&mut out, &bare, Format::Compact);
    out
}

/// `e12 link "Home"`: ref, role and name only.
fn label(line: &SnapLine) -> String {
    match &line.kind {
        LineKind::Element { r, role, name, .. } if name.is_empty() => format!("e{r} {role}"),
        LineKind::Element { r, role, name, .. } => format!("e{r} {role} {}", quote(name)),
        LineKind::Text(t) => format!("text {}", quote(&truncate(t, 80))),
        LineKind::Truncated(n) => format!("[truncated: {n} more nodes]"),
        LineKind::More { count, .. } => format!("[more={count} nodes]"),
    }
}

fn inline_text(line: &SnapLine) -> Option<&str> {
    match &line.kind {
        LineKind::Element { text, .. } => text.as_deref(),
        _ => None,
    }
}

/// The attribute changes of an element, in canonical order.
fn attr_changes(old: &[(&'static str, String)], new: &[(&'static str, String)]) -> String {
    let mut keys: Vec<&'static str> = Vec::new();
    for (k, _) in new.iter().chain(old.iter()) {
        if !keys.contains(k) {
            keys.push(k);
        }
    }
    keys.sort_by_key(|k| crate::snapshot::attr_rank(k));
    let value = |attrs: &[(&'static str, String)], key: &str| {
        attrs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    };
    let mut out = String::new();
    for key in keys {
        match (value(old, key), value(new, key)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(a), Some(b)) => {
                let _ = write!(out, " [{key}={} → {}]", attr_value(&a), attr_value(&b));
            }
            (Some(a), None) if a.is_empty() => {
                let _ = write!(out, " [{key} → -]");
            }
            (Some(a), None) => {
                let _ = write!(out, " [{key}={} → -]", attr_value(&a));
            }
            (None, Some(b)) if b.is_empty() => {
                let _ = write!(out, " [- → {key}]");
            }
            (None, Some(b)) => {
                let _ = write!(out, " [{key}=- → {}]", attr_value(&b));
            }
            (None, None) => {}
        }
    }
    out
}

/// The longest common subsequence of two sequences, as index pairs.
fn lcs<T: PartialEq>(a: &[T], b: &[T]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    // Long sequences are compared greedily: the table would be too big.
    if n * m > 4_000_000 {
        let mut pairs = Vec::new();
        let mut j = 0;
        for (i, x) in a.iter().enumerate() {
            if let Some(k) = b[j..].iter().position(|y| y == x) {
                pairs.push((i, j + k));
                j += k + 1;
            }
        }
        return pairs;
    }
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * (m + 1) + j] = if a[i] == b[j] {
                table[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut pairs = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if table[(i + 1) * (m + 1) + j] >= table[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// Compares two snapshots of one document.
pub fn diff(old: &[SnapLine], new: &[SnapLine]) -> Diff {
    let a = Tree::new(old);
    let b = Tree::new(new);
    let old_refs: HashSet<u32> = a.by_ref.keys().copied().collect();
    let new_refs: HashSet<u32> = b.by_ref.keys().copied().collect();
    let mut out = Diff::default();
    // (sort key, line)
    let mut items: Vec<(f64, String)> = Vec::new();

    // Where an old line goes in the new document: just before the first
    // element after it (and after its subtree) that survived, else just
    // after the last one before it.
    let old_key = |i: usize| -> f64 {
        let end = i + 1 + a.descendants(i);
        let next = (end..old.len())
            .find_map(|j| a.r(j).filter(|r| new_refs.contains(r)))
            .and_then(|r| b.by_ref.get(&r).copied());
        if let Some(k) = next {
            return k as f64 - 0.5 + i as f64 * 1e-6;
        }
        let previous = (0..i)
            .rev()
            .find_map(|j| a.r(j).filter(|r| new_refs.contains(r)))
            .and_then(|r| b.by_ref.get(&r).copied());
        previous.map_or(new.len() as f64, |k| k as f64 + 0.5) + i as f64 * 1e-6
    };

    // Added and removed subtrees, by their topmost node.
    let added: Vec<usize> = (0..new.len())
        .filter(|&i| {
            b.r(i).is_some_and(|r| !old_refs.contains(&r))
                && b.parent[i]
                    .and_then(|p| b.r(p))
                    .is_none_or(|p| old_refs.contains(&p))
        })
        .collect();
    let removed: Vec<usize> = (0..old.len())
        .filter(|&i| {
            a.r(i).is_some_and(|r| !new_refs.contains(&r))
                && a.parent[i]
                    .and_then(|p| a.r(p))
                    .is_none_or(|p| new_refs.contains(&p))
        })
        .collect();

    // A removed node and an added one in the same place, with the same
    // role and name, are one node the page rendered again.
    let signature = |t: &Tree, i: usize| -> Option<Signature> {
        match &t.lines[i].kind {
            LineKind::Element { role, name, .. } => Some((t.parent_ref(i), *role, name.clone())),
            _ => None,
        }
    };
    let mut by_sig: HashMap<Signature, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for &i in &removed {
        if let Some(sig) = signature(&a, i) {
            by_sig.entry(sig).or_default().0.push(i);
        }
    }
    for &j in &added {
        if let Some(sig) = signature(&b, j) {
            by_sig.entry(sig).or_default().1.push(j);
        }
    }
    let mut replaces: HashMap<usize, usize> = HashMap::new();
    let mut replaced_old: HashSet<usize> = HashSet::new();
    for (olds, news) in by_sig.values() {
        if let ([i], [j]) = (olds.as_slice(), news.as_slice()) {
            replaces.insert(*j, *i);
            replaced_old.insert(*i);
            pair_subtrees(&a, *i, &b, *j, &mut out.replaced);
        }
    }

    for &j in &added {
        let line = &b.lines[j];
        let mut text = format!("+ {}", head(line));
        match replaces.get(&j) {
            Some(&i) => {
                let _ = write!(text, " (replaces e{})", a.r(i).unwrap_or(0));
            }
            None => {
                let _ = write!(
                    text,
                    " ({})",
                    position(b.parent_ref(j), b.previous_element(j))
                );
            }
        }
        if let Some(t) = inline_text(line) {
            let _ = write!(text, ": {t}");
        }
        let depth = line.depth;
        for k in j + 1..j + 1 + b.descendants(j) {
            let mut child = b.lines[k].clone();
            child.depth = child.depth - depth + 1;
            text.push('\n');
            render_line(&mut text, &child, Format::Compact);
        }
        out.added += 1;
        items.push((j as f64, text));
    }
    for &i in &removed {
        if replaced_old.contains(&i) {
            continue;
        }
        let mut text = format!("- {}", label(&a.lines[i]));
        let below = a.descendants(i);
        if below > 0 {
            let _ = write!(
                text,
                " (+{below} descendant{})",
                if below == 1 { "" } else { "s" }
            );
        }
        out.removed += 1;
        items.push((old_key(i), text));
    }

    // Elements in both: changed, moved, or neither.
    let mut moved: HashSet<u32> = HashSet::new();
    for (parent_old, kids_old) in &a.children {
        let parent_ref = parent_old.and_then(|p| a.r(p));
        let parent_new = match parent_ref {
            Some(r) => match b.by_ref.get(&r) {
                Some(&j) => Some(j),
                None => continue,
            },
            None => None,
        };
        let seq_old: Vec<u32> = kids_old
            .iter()
            .filter_map(|&i| a.r(i))
            .filter(|r| new_refs.contains(r))
            .collect();
        let seq_new: Vec<u32> = b
            .kids(parent_new)
            .iter()
            .filter_map(|&j| b.r(j))
            .filter(|r| old_refs.contains(r))
            .collect();
        let kept: HashSet<u32> = lcs(&seq_old, &seq_new)
            .into_iter()
            .map(|(i, _)| seq_old[i])
            .collect();
        for r in seq_new {
            if !kept.contains(&r) {
                moved.insert(r);
            }
        }
    }
    for (&r, &j) in &b.by_ref {
        let Some(&i) = a.by_ref.get(&r) else {
            continue;
        };
        if a.parent_ref(i) != b.parent_ref(j) {
            moved.insert(r);
        }
        let (old_line, new_line) = (&a.lines[i], &b.lines[j]);
        let (
            LineKind::Element {
                role: old_role,
                name: old_name,
                attrs: old_attrs,
                ..
            },
            LineKind::Element {
                role, name, attrs, ..
            },
        ) = (&old_line.kind, &new_line.kind)
        else {
            continue;
        };
        let attrs_changed = attr_changes(old_attrs, attrs);
        let (old_text, new_text) = (inline_text(old_line), inline_text(new_line));
        let changed = old_role != role
            || old_name != name
            || !attrs_changed.is_empty()
            || old_text != new_text;
        if moved.contains(&r) {
            out.moved += 1;
            items.push((
                j as f64 + 0.25,
                format!(
                    "> {} (now {})",
                    label(new_line),
                    position(b.parent_ref(j), b.previous_element(j))
                ),
            ));
        }
        if !changed {
            if !moved.contains(&r) {
                out.unchanged += 1;
            }
            continue;
        }
        let mut text = format!("~ e{r} ");
        if old_role != role {
            let _ = write!(text, "{old_role} → {role}");
        } else {
            text.push_str(role);
        }
        if old_name != name {
            let _ = write!(text, " {} → {}", quote(old_name), quote(name));
        } else if !name.is_empty() {
            let _ = write!(text, " {}", quote(name));
        }
        text.push_str(&attrs_changed);
        if old_text != new_text {
            let _ = write!(
                text,
                ": {} → {}",
                old_text.unwrap_or("-"),
                new_text.unwrap_or("-")
            );
        }
        out.changed += 1;
        items.push((j as f64, text));
    }

    // Texts of parents in both.
    let mut parents: Vec<(Option<usize>, Option<usize>)> = vec![(None, None)];
    for (&r, &j) in &b.by_ref {
        if let Some(&i) = a.by_ref.get(&r) {
            parents.push((Some(i), Some(j)));
        }
    }
    for (pi, pj) in parents {
        let old_texts = a.texts(pi);
        let new_texts = b.texts(pj);
        if old_texts
            .iter()
            .map(|t| t.1)
            .eq(new_texts.iter().map(|t| t.1))
        {
            continue;
        }
        let owner = pj.and_then(|j| b.r(j));
        let prefix = owner.map(|r| format!("e{r} ")).unwrap_or_default();
        let olds: Vec<&str> = old_texts.iter().map(|t| t.1).collect();
        let news: Vec<&str> = new_texts.iter().map(|t| t.1).collect();
        let pairs = lcs(&olds, &news);
        // Walk the gaps between matched texts: a gap with texts on both
        // sides is changes, the rest additions or removals.
        let mut bounds = pairs.clone();
        bounds.push((olds.len(), news.len()));
        let (mut i0, mut j0) = (0, 0);
        for (i1, j1) in bounds {
            let gap_old: Vec<usize> = (i0..i1).collect();
            let gap_new: Vec<usize> = (j0..j1).collect();
            let common = gap_old.len().min(gap_new.len());
            for k in 0..common {
                let (oi, nj) = (gap_old[k], gap_new[k]);
                out.changed += 1;
                items.push((new_texts[nj].0 as f64, {
                    let (old, new) = excerpts(olds[oi], news[nj], 120);
                    format!("~ {prefix}text[{nj}] {} → {}", quote(&old), quote(&new))
                }));
            }
            for &nj in &gap_new[common..] {
                out.added += 1;
                items.push((
                    new_texts[nj].0 as f64,
                    format!("+ {prefix}text[{nj}] {}", quote(&truncate(news[nj], 120))),
                ));
            }
            for &oi in &gap_old[common..] {
                out.removed += 1;
                items.push((
                    old_key(old_texts[oi].0),
                    format!("- {prefix}text[{oi}] {}", quote(&truncate(olds[oi], 120))),
                ));
            }
            i0 = i1 + 1;
            j0 = j1 + 1;
        }
    }

    items.sort_by(|x, y| x.0.total_cmp(&y.0));
    out.lines = items.into_iter().map(|(_, line)| line).collect();
    out
}

/// Excerpts of two texts, at most `max` characters each, from a little
/// before where they first differ (`…` marks what is left out), so that a
/// change far into a long text shows.
fn excerpts(old: &str, new: &str, max: usize) -> (String, String) {
    let first = old
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let start = first.saturating_sub(max / 4);
    let cut = |text: &str| {
        if start == 0 {
            return truncate(text, max);
        }
        let rest: String = text.chars().skip(start).collect();
        format!("…{}", truncate(&rest, max - 1))
    };
    (cut(old), cut(new))
}

/// Pairs the nodes of a re-rendered subtree with the old one's, where the
/// two have the same shape: same roles and names, in order.
fn pair_subtrees(a: &Tree, i: usize, b: &Tree, j: usize, out: &mut Vec<(u32, u32)>) {
    if let (Some(ra), Some(rb)) = (a.r(i), b.r(j)) {
        out.push((ra, rb));
    }
    let ka: Vec<usize> = a
        .kids(Some(i))
        .iter()
        .copied()
        .filter(|&k| a.r(k).is_some())
        .collect();
    let kb: Vec<usize> = b
        .kids(Some(j))
        .iter()
        .copied()
        .filter(|&k| b.r(k).is_some())
        .collect();
    if ka.len() != kb.len() {
        return;
    }
    for (&x, &y) in ka.iter().zip(&kb) {
        let same = match (&a.lines[x].kind, &b.lines[y].kind) {
            (
                LineKind::Element {
                    role: r1, name: n1, ..
                },
                LineKind::Element {
                    role: r2, name: n2, ..
                },
            ) => r1 == r2 && n1 == n2,
            _ => false,
        };
        if same {
            pair_subtrees(a, x, b, y, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(depth: u16, r: u32, role: &'static str, name: &str) -> SnapLine {
        SnapLine {
            depth,
            kind: LineKind::Element {
                r,
                role,
                name: name.to_string(),
                attrs: Vec::new(),
                has_children: false,
                text: None,
            },
        }
    }

    fn with_attrs(mut line: SnapLine, attrs: &[(&'static str, &str)]) -> SnapLine {
        if let LineKind::Element { attrs: a, .. } = &mut line.kind {
            *a = attrs.iter().map(|(k, v)| (*k, v.to_string())).collect();
        }
        line
    }

    fn with_text(mut line: SnapLine, t: &str) -> SnapLine {
        if let LineKind::Element { text, .. } = &mut line.kind {
            *text = Some(t.to_string());
        }
        line
    }

    fn text(depth: u16, t: &str) -> SnapLine {
        SnapLine {
            depth,
            kind: LineKind::Text(t.to_string()),
        }
    }

    #[test]
    fn nothing_changed() {
        let lines = vec![el(0, 1, "main", ""), el(1, 2, "button", "Go")];
        let d = diff(&lines, &lines);
        assert!(d.is_empty());
        assert_eq!(d.stats(), "no changes");
    }

    #[test]
    fn a_change_far_into_a_long_text_shows() {
        let start = "a".repeat(300);
        let old = vec![el(0, 1, "main", ""), text(1, &format!("{start} old end"))];
        let new = vec![el(0, 1, "main", ""), text(1, &format!("{start} new end"))];
        let d = diff(&old, &new);
        let text = d.text();
        assert!(text.contains("old end\" → \"…"), "{text}");
        assert!(text.ends_with("new end\"\n"), "{text}");
        assert!(text.contains("\"…aaa"), "{text}");
    }

    #[test]
    fn values_names_texts_additions_and_removals() {
        let old = vec![
            el(0, 1, "main", ""),
            with_attrs(el(1, 2, "spinbutton", "Quantity"), &[("value", "2")]),
            with_attrs(el(1, 3, "button", "Pay $44.00"), &[("disabled", "")]),
            text(1, "208 points"),
            el(1, 4, "row", "Socks"),
            el(2, 5, "cell", "Socks"),
            with_text(el(1, 6, "paragraph", ""), "idle"),
        ];
        let new = vec![
            el(0, 1, "main", ""),
            with_attrs(el(1, 2, "spinbutton", "Quantity"), &[("value", "3")]),
            el(1, 3, "button", "Pay $58.00"),
            text(1, "209 points"),
            el(1, 7, "status", "Cart updated"),
            with_text(el(1, 6, "paragraph", ""), "bought"),
        ];
        let d = diff(&old, &new);
        assert_eq!(
            d.lines,
            [
                "~ e2 spinbutton \"Quantity\" [value=2 → 3]",
                "~ e3 button \"Pay $44.00\" → \"Pay $58.00\" [disabled → -]",
                "~ e1 text[0] \"208 points\" → \"209 points\"",
                "+ e7 status \"Cart updated\" (in e1, after e3)",
                "- e4 row \"Socks\" (+1 descendant)",
                "~ e6 paragraph: idle → bought",
            ]
        );
        assert_eq!(d.stats(), "changed=4 added=1 removed=1");
    }

    #[test]
    fn a_rerendered_node_replaces_the_old_one() {
        let old = vec![
            el(0, 1, "list", ""),
            el(1, 2, "listitem", "Beanie"),
            el(2, 3, "button", "Remove"),
        ];
        let new = vec![
            el(0, 1, "list", ""),
            el(1, 8, "listitem", "Beanie"),
            el(2, 9, "button", "Remove"),
        ];
        let d = diff(&old, &new);
        assert_eq!(
            d.lines,
            ["+ e8 listitem \"Beanie\" (replaces e2)\n    e9 button \"Remove\""]
        );
        assert_eq!(d.replaced, [(2, 8), (3, 9)]);
    }

    #[test]
    fn moves_are_reported_once() {
        let old = vec![
            el(0, 1, "list", ""),
            el(1, 2, "button", "A"),
            el(1, 3, "button", "B"),
            el(1, 4, "button", "C"),
        ];
        let new = vec![
            el(0, 1, "list", ""),
            el(1, 4, "button", "C"),
            el(1, 2, "button", "A"),
            el(1, 3, "button", "B"),
        ];
        let d = diff(&old, &new);
        assert_eq!(d.lines, ["> e4 button \"C\" (now in e1, first)"]);
        assert_eq!(d.moved, 1);
    }
}
