//! Fitting a snapshot into a byte budget.
//!
//! Over budget, a snapshot first shows only the start of long texts
//! (`… [+1830 chars]`), then only the start of its long lists, as much as
//! fits (`[more=460 nodes after e212]`), then folds containers, deepest
//! first and those with the fewest things to act on first
//! (`[collapsed=12]`). Whatever still does not fit is cut at the end. The
//! ref of a folded container, as `root` (with `after` for a list), shows
//! what was left out.

use std::collections::HashMap;

use crate::a11y::is_interactive;
use crate::snapshot::{Format, LineKind, SnapLine, cap_text, render_line};

/// Items of a long list shown before the rest is left out.
const LIST_HEAD: usize = 10;
/// Children that make a list long.
const LONG_LIST: usize = 25;

/// A snapshot fitted into a budget.
#[derive(Debug, Clone)]
pub struct Fitted {
    pub lines: Vec<SnapLine>,
    /// Lines left out.
    pub hidden: usize,
}

fn actionable(line: &SnapLine) -> bool {
    match &line.kind {
        LineKind::Element { role, attrs, .. } => {
            is_interactive(role) || attrs.iter().any(|(k, _)| *k == "clickable")
        }
        _ => false,
    }
}

fn line_ref(line: &SnapLine) -> Option<u32> {
    match line.kind {
        LineKind::Element { r, .. } => Some(r),
        _ => None,
    }
}

/// A line with its text, when long, cut to its start.
fn capped(line: &SnapLine) -> SnapLine {
    let mut line = line.clone();
    if let LineKind::Text(t) | LineKind::Element { text: Some(t), .. } = &mut line.kind
        && let Some(short) = cap_text(t)
    {
        *t = short;
    }
    line
}

/// Fits `lines` into `max` bytes as rendered in `format`.
pub fn fit(lines: &[SnapLine], format: Format, max: usize) -> Fitted {
    let size = |line: &SnapLine| {
        let mut out = String::new();
        render_line(&mut out, line, format);
        out.len() + 1
    };
    if lines.iter().map(size).sum::<usize>() <= max {
        return Fitted {
            lines: lines.to_vec(),
            hidden: 0,
        };
    }
    // Long texts show their start first.
    let lines: Vec<SnapLine> = lines.iter().map(capped).collect();
    let n = lines.len();
    let sizes: Vec<usize> = lines.iter().map(size).collect();
    let mut total: usize = sizes.iter().sum();
    if total <= max {
        return Fitted { lines, hidden: 0 };
    }
    // The tree: where each subtree ends, and each node's children.
    let mut end = vec![0usize; n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..n {
        while stack
            .last()
            .is_some_and(|&j| lines[j].depth >= lines[i].depth)
        {
            stack.pop();
        }
        if let Some(&parent) = stack.last() {
            children[parent].push(i);
        }
        stack.push(i);
    }
    for i in (0..n).rev() {
        end[i] = children[i].last().map_or(i + 1, |&c| end[c]);
    }
    let mut hidden = vec![false; n];
    let mut collapsed: HashMap<usize, usize> = HashMap::new();
    // Parent → (index of the first child left out, lines left out).
    let mut more: HashMap<usize, (usize, usize)> = HashMap::new();
    let visible_bytes = |hidden: &[bool], from: usize, to: usize| -> usize {
        (from..to).filter(|&k| !hidden[k]).map(|k| sizes[k]).sum()
    };

    // Long lists keep their first items.
    // Prose with many links is not a list.
    let list_like = |line: &SnapLine| match &line.kind {
        LineKind::Element { role, .. } => !matches!(
            *role,
            "paragraph"
                | "heading"
                | "blockquote"
                | "cell"
                | "gridcell"
                | "columnheader"
                | "rowheader"
                | "caption"
                | "listitem"
                | "term"
                | "definition"
                | "figure"
        ),
        _ => false,
    };
    let mut lists: Vec<(usize, usize)> = (0..n)
        .filter(|&i| children[i].len() >= LONG_LIST && list_like(&lines[i]))
        .map(|i| {
            let cut = children[i][LIST_HEAD];
            (i, visible_bytes(&hidden, cut, end[i]))
        })
        .collect();
    lists.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (i, _) in lists {
        if total <= max {
            break;
        }
        if hidden[i] {
            continue;
        }
        // Leave out the last items, as many as the budget needs (the
        // first ones always stay).
        let needed = total - max + 32;
        let kids = &children[i];
        let mut from = kids.len();
        let mut saving = 0;
        while from > LIST_HEAD && saving < needed {
            from -= 1;
            saving += visible_bytes(&hidden, kids[from], end[kids[from]]);
        }
        if from == kids.len() {
            continue;
        }
        let cut = kids[from];
        let saved = visible_bytes(&hidden, cut, end[i]);
        let count = (cut..end[i]).filter(|&k| !hidden[k]).count();
        for flag in &mut hidden[cut..end[i]] {
            *flag = true;
        }
        more.insert(i, (cut, count));
        total = total.saturating_sub(saved) + 32;
    }

    // Then containers, the fewest things to act on and the deepest first.
    if total > max {
        let mut containers: Vec<(usize, f64)> = (0..n)
            .filter(|&i| !children[i].is_empty() && !actionable(&lines[i]))
            .filter(|&i| matches!(lines[i].kind, LineKind::Element { .. }))
            .map(|i| {
                let inside = end[i] - i - 1;
                let acting = (i + 1..end[i]).filter(|&k| actionable(&lines[k])).count();
                (i, acting as f64 / inside.max(1) as f64)
            })
            .collect();
        // Deepest first, so that folding trims the tree from its leaves and
        // stops close to the budget; at one depth, the fewest things to act
        // on first.
        containers.sort_by(|a, b| {
            lines[b.0]
                .depth
                .cmp(&lines[a.0].depth)
                .then(a.1.total_cmp(&b.1))
                .then(a.0.cmp(&b.0))
        });
        for (i, _) in containers {
            if total <= max {
                break;
            }
            if hidden[i] {
                continue;
            }
            let saved = visible_bytes(&hidden, i + 1, end[i]);
            if saved == 0 {
                continue;
            }
            let count = (i + 1..end[i]).filter(|&k| !hidden[k]).count();
            for flag in &mut hidden[i + 1..end[i]] {
                *flag = true;
            }
            for k in i + 1..end[i] {
                collapsed.remove(&k);
                more.remove(&k);
            }
            more.remove(&i);
            collapsed.insert(i, count + collapsed.get(&i).copied().unwrap_or(0));
            total = total.saturating_sub(saved) + 16;
        }
    }

    let mut out = Vec::with_capacity(n);
    let mut left_out = hidden.iter().filter(|&&h| h).count();
    // Where `more` lines go: after the last shown line before the cut.
    let mut more_at: HashMap<usize, (u16, usize, Option<u32>)> = HashMap::new();
    for (&parent, &(cut, count)) in &more {
        let last_shown = children[parent]
            .iter()
            .take_while(|&&c| c < cut)
            .last()
            .copied();
        let after = children[parent]
            .iter()
            .take_while(|&&c| c < cut)
            .filter_map(|&c| line_ref(&lines[c]))
            .last();
        let at = last_shown.map_or(parent, |c| end[c] - 1);
        more_at.insert(at, (lines[cut].depth, count, after));
    }
    for i in 0..n {
        if !hidden[i] {
            let mut line = lines[i].clone();
            if let (Some(count), LineKind::Element { attrs, .. }) =
                (collapsed.get(&i), &mut line.kind)
            {
                attrs.push(("collapsed", count.to_string()));
            }
            out.push(line);
        }
        if let Some(&(depth, count, after)) = more_at.get(&i) {
            out.push(SnapLine {
                depth,
                kind: LineKind::More { count, after },
            });
        }
    }
    // A last resort: cut at the end.
    let mut size = 0;
    let mut cut = out.len();
    for (k, line) in out.iter().enumerate() {
        let mut text = String::new();
        render_line(&mut text, line, format);
        size += text.len() + 1;
        if size > max {
            cut = k;
            break;
        }
    }
    if cut < out.len() {
        let rest = out.len() - cut;
        left_out += rest;
        out.truncate(cut);
        out.push(SnapLine {
            depth: 0,
            kind: LineKind::Truncated(rest),
        });
    }
    Fitted {
        lines: out,
        hidden: left_out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::render_lines;

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

    fn text(depth: u16, t: &str) -> SnapLine {
        SnapLine {
            depth,
            kind: LineKind::Text(t.to_string()),
        }
    }

    #[test]
    fn long_texts_are_cut_before_anything_is_left_out() {
        let long = "word ".repeat(100);
        let lines = vec![el(0, 1, "main", ""), text(1, long.trim()), text(1, "short")];
        let all = fit(&lines, Format::Compact, 10_000);
        assert_eq!(all.lines, lines, "within budget, texts stay whole");
        let fitted = fit(&lines, Format::Compact, 300);
        assert_eq!(fitted.hidden, 0);
        let shown = render_lines(&fitted.lines, Format::Compact);
        assert!(shown.contains("… [+300 chars]"), "{shown}");
        assert!(shown.contains("text: short"), "{shown}");
    }

    #[test]
    fn a_long_list_keeps_as_many_items_as_fit() {
        let mut lines = vec![el(0, 1, "list", "")];
        for i in 0..40 {
            lines.push(el(1, 2 + i, "link", &format!("Item {i}")));
        }
        let all: usize = lines
            .iter()
            .map(|l| {
                let mut out = String::new();
                render_line(&mut out, l, Format::Compact);
                out.len() + 1
            })
            .sum();
        // Room for all but a few items: only those are left out.
        let fitted = fit(&lines, Format::Compact, all - 60);
        let shown = render_lines(&fitted.lines, Format::Compact);
        assert!(shown.contains("link \"Item 30\""), "{shown}");
        assert!(!shown.contains("link \"Item 39\""), "{shown}");
        assert!(shown.contains("[more="), "{shown}");
    }

    #[test]
    fn small_snapshots_are_left_alone() {
        let lines = vec![el(0, 1, "main", ""), el(1, 2, "button", "Go")];
        let fitted = fit(&lines, Format::Compact, 1000);
        assert_eq!(fitted.lines, lines);
        assert_eq!(fitted.hidden, 0);
    }

    #[test]
    fn long_lists_show_their_start_and_prose_folds_first() {
        let mut lines = vec![el(0, 1, "main", ""), el(1, 2, "article", "")];
        for k in 0..20 {
            lines.push(text(
                2,
                &format!("A long paragraph of prose number {k} that goes on"),
            ));
        }
        lines.push(el(1, 3, "list", ""));
        for k in 0..40 {
            lines.push(el(2, 10 + k, "link", &format!("Item {k}")));
        }
        let fitted = fit(&lines, Format::Compact, 900);
        let text = render_lines(&fitted.lines, Format::Compact);
        assert!(text.contains("e2 article [collapsed=20]\n"), "{text}");
        assert!(
            text.contains("    e19 link \"Item 9\"\n    [more=30 nodes after e19]\n"),
            "{text}"
        );
        assert!(text.len() <= 900, "{text}");
        assert_eq!(fitted.hidden, 50);
    }
}
