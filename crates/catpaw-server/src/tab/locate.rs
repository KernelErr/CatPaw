//! Finding elements by what they show: `text:` and `role "name"` targets.
//!
//! The search runs over what a snapshot shows (frames included), so a
//! target means what the agent read. It never guesses: an exact match
//! beats a partial one, a single element to act on beats others, and
//! anything still tied is an `AmbiguousTarget` listing the candidates.
//! A `role "name"` target names its element in full, as the snapshot
//! shows it (a name the snapshot cut short, ending in `…`, names what it
//! begins); one that only appears inside a name is listed, not taken:
//! the "Password" inside `textbox "Username Password"` is often another
//! field.

use catpaw_agent::a11y::{is_interactive, subtree_text};
use catpaw_agent::snapshot::{LineKind, quote};
use catpaw_agent::{ExtraAttrs, Filter};
use catpaw_engine::FrameId;
use catpaw_protocol::wording::{ErrorCode, advice};
use catpaw_web::agent;

use super::{Aim, GroupState};
use crate::oracle::EngineOracle;
use crate::output::Failure;

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A candidate: its ref, whether it matched exactly, and whether it is
/// something to act on.
struct Candidate {
    r: u32,
    exact: bool,
    actionable: bool,
}

impl GroupState {
    pub(super) fn locate(
        &mut self,
        tab: u32,
        role: Option<&str>,
        text: &str,
    ) -> Result<Aim, Failure> {
        let needle = normalize(text);
        let cut = role.and(needle.strip_suffix('…'));
        let what = match role {
            Some(role) => format!("{role} {}", quote(text)),
            None => format!("text:{text}"),
        };
        // The latest snapshot serves when the page has not changed since.
        let lines = match self.fresh_lines(tab) {
            Some(lines) => lines,
            None => {
                self.model(tab, Filter::Interesting, ExtraAttrs::default(), None)?
                    .lines
            }
        };
        let mut found: Vec<Candidate> = Vec::new();
        let mut text_parents: Vec<(Option<u32>, bool)> = Vec::new();
        let mut parent_stack: Vec<(u16, u32)> = Vec::new();
        for line in &lines {
            while parent_stack.last().is_some_and(|&(d, _)| d >= line.depth) {
                parent_stack.pop();
            }
            match &line.kind {
                LineKind::Element {
                    r,
                    role: line_role,
                    name,
                    attrs,
                    text: inline,
                    ..
                } => {
                    parent_stack.push((line.depth, *r));
                    // Rich text editors take text as a textbox does.
                    let editable = attrs.iter().any(|(k, _)| *k == "editable");
                    if role.is_some_and(|wanted| {
                        wanted != *line_role && !(wanted == "textbox" && editable)
                    }) {
                        continue;
                    }
                    let shown = [Some(name.as_str()), inline.as_deref()];
                    let mut best: Option<bool> = None;
                    for value in shown.into_iter().flatten() {
                        let value = normalize(value);
                        if value.is_empty() {
                            continue;
                        }
                        if value == needle || cut.is_some_and(|cut| value.starts_with(cut)) {
                            best = Some(true);
                        } else if value.contains(&needle) && best.is_none() {
                            best = Some(false);
                        }
                    }
                    if let Some(exact) = best {
                        found.push(Candidate {
                            r: *r,
                            exact,
                            actionable: is_interactive(line_role)
                                || attrs.iter().any(|(k, _)| *k == "clickable"),
                        });
                    }
                }
                LineKind::Text(t) if role.is_none() => {
                    let value = normalize(t);
                    if value.contains(&needle) {
                        let parent = parent_stack.last().map(|&(_, parent)| parent);
                        text_parents.push((parent, value == needle));
                    }
                }
                _ => {}
            }
        }
        // Texts outside any named element: the smallest element holding
        // the text, under the element whose line holds it.
        if found.is_empty() {
            for (parent, exact) in text_parents {
                if let Some(r) = self.smallest_holding(tab, parent, &needle)
                    && !found.iter().any(|c| c.r == r)
                {
                    found.push(Candidate {
                        r,
                        exact,
                        actionable: false,
                    });
                }
            }
        }
        if found.is_empty() {
            // The name under another role is likely what was meant: say
            // so rather than leave the agent guessing.
            let elsewhere: Vec<u32> = match role {
                Some(_) => lines
                    .iter()
                    .filter_map(|line| match &line.kind {
                        LineKind::Element { r, name, .. } if normalize(name) == needle => Some(*r),
                        _ => None,
                    })
                    .collect(),
                None => Vec::new(),
            };
            if !elsewhere.is_empty() {
                let listed: Vec<String> = elsewhere
                    .iter()
                    .take(5)
                    .map(|&r| self.describe_in_context(tab, r))
                    .collect();
                let more = if elsewhere.len() > 5 { ", …" } else { "" };
                return Err(Failure::new(
                    ErrorCode::NotFound,
                    format!(
                        "{what} matches nothing; with that name: {}{more}",
                        listed.join(", ")
                    ),
                )
                .with(advice::OTHER_ROLE));
            }
            return Err(
                Failure::new(ErrorCode::NotFound, format!("{what} matches nothing"))
                    .with(advice::UNKNOWN_REF),
            );
        }
        // Exact matches first; then one thing to act on among them.
        if found.iter().any(|c| c.exact) {
            found.retain(|c| c.exact);
        } else if role.is_some() {
            let listed: Vec<String> = found
                .iter()
                .take(5)
                .map(|c| self.describe(tab, c.r))
                .collect();
            let more = if found.len() > 5 { ", …" } else { "" };
            return Err(Failure::new(
                ErrorCode::NotFound,
                format!(
                    "{what} names nothing in full; in part: {}{more}",
                    listed.join(", ")
                ),
            )
            .with(advice::FULL_NAME));
        }
        if found.len() > 1 && found.iter().filter(|c| c.actionable).count() == 1 {
            found.retain(|c| c.actionable);
        }
        if found.len() > 1 {
            let listed: Vec<String> = found
                .iter()
                .take(5)
                .map(|c| self.describe_in_context(tab, c.r))
                .collect();
            let mut message = format!(
                "{what} matches {} elements: {}",
                found.len(),
                listed.join(", ")
            );
            if found.len() > 5 {
                message.push_str(", …");
            }
            return Err(Failure::new(ErrorCode::AmbiguousTarget, message).with(advice::AMBIGUOUS));
        }
        let r = found[0].r;
        let key = self
            .tabs
            .get(&tab)
            .and_then(|t| t.refs.entry(r))
            .map(|e| e.key)
            .ok_or_else(|| Failure::new(ErrorCode::NotFound, format!("{what} matches nothing")))?;
        Ok(Aim {
            frame: FrameId(key.frame),
            node: key.node,
            r,
            point: None,
            retargeted: None,
        })
    }

    /// The deepest element under `parent` whose visible text holds
    /// `needle`, with a ref.
    /// `parent` is the ref of the element whose line holds the text, or
    /// none for text at the top of the tab's page.
    fn smallest_holding(&mut self, tab: u32, parent: Option<u32>, needle: &str) -> Option<u32> {
        let entry = self.tabs.get(&tab)?;
        let (frame, start) = match parent {
            Some(parent) => {
                let key = entry.refs.entry(parent)?.key;
                (FrameId(key.frame), Some(key.node))
            }
            None => (entry.root, None),
        };
        let state = self.page.frame_state(frame)?.clone();
        let node = agent::with_styles(&state, |engine, dom| {
            let oracle = EngineOracle {
                engine,
                page: &state,
            };
            let root = || dom.child_elements(dom.document()).next();
            let mut best = start.or_else(root)?;
            loop {
                let next = dom.children(best).find(|&c| {
                    dom.is_element(c) && normalize(&subtree_text(dom, c, &oracle)).contains(needle)
                });
                match next {
                    Some(child) => best = child,
                    None => break Some(best),
                }
            }
        })?;
        self.ref_for(tab, frame, node)
    }
}
