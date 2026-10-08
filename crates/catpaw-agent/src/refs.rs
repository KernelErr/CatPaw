//! Element references (`eN`): allocated the first time a node is shown,
//! monotonic, never reused (ADR 0005).
//!
//! A ref names a node of one document of one frame. Documents are told
//! apart by an epoch the embedder supplies, because a new document's arena
//! can hand out the very keys the old one used. A ref whose node left the
//! document, or whose document was replaced, is stale; looking it up says
//! why and suggests the live ref that most likely took its place.

use std::collections::HashMap;

use catpaw_dom::{Dom, NodeId};

use crate::a11y::{role_for, subtree_text};
use crate::snapshot::cap_name;
use crate::visibility::StyleOracle;

/// What a ref points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RefKey {
    pub frame: u32,
    pub epoch: u64,
    pub node: NodeId,
}

impl RefKey {
    /// A node of the only document there is (parse-only pipelines).
    pub fn plain(node: NodeId) -> Self {
        Self {
            frame: 0,
            epoch: 0,
            node,
        }
    }
}

/// Why a ref no longer resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    /// The node left its document.
    Removed,
    /// The frame navigated to another document.
    Navigated,
    /// The frame itself is gone.
    FrameClosed,
}

impl StaleReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StaleReason::Removed => "removed",
            StaleReason::Navigated => "navigated away",
            StaleReason::FrameClosed => "frame closed",
        }
    }
}

/// What the table remembers of a ref: where it points and how it was last
/// shown, so that errors can name it and replacements can be found.
#[derive(Clone, Debug)]
pub struct RefEntry {
    pub key: RefKey,
    pub role: &'static str,
    pub name: String,
    /// The ref of the nearest shown ancestor, when there was one.
    pub parent: Option<u32>,
    pub stale: Option<StaleReason>,
    /// The ref that took this one's place when the page re-rendered it.
    pub replaced_by: Option<u32>,
    /// The pass (see [`RefTable::begin_pass`]) that first showed the node.
    pub born: u64,
    /// The last pass that showed it.
    pub seen: u64,
    /// When it went with its document or frame: the table's count of
    /// documents left then.
    gone_at: u64,
}

/// Why a ref could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefError {
    /// Not of the form `e12`.
    BadSyntax(String),
    /// Never handed out.
    Unknown(u32),
    /// Handed out for a document the tab left long ago, and forgotten.
    Forgotten(u32),
    Stale {
        r: u32,
        reason: StaleReason,
        role: &'static str,
        name: String,
        /// The live ref that most likely took its place.
        suggestion: Option<u32>,
    },
}

#[derive(Debug, Default)]
pub struct RefTable {
    next: u32,
    by_key: HashMap<RefKey, u32>,
    entries: HashMap<u32, RefEntry>,
    /// The current pass over the page.
    pass: u64,
    /// Documents and frames left so far.
    left: u64,
}

/// Refs of the documents a tab left are kept for this many more, so that
/// an error can still say what one was; then they are forgotten.
const KEEP_LEFT: u64 = 2;

impl RefTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a pass over the page (a snapshot, or a model of it): refs
    /// first handed out from here on are younger than every node the
    /// passes before saw.
    pub fn begin_pass(&mut self) {
        self.pass += 1;
    }

    /// The ref of `key`, allocating one the first time. Role, name and
    /// parent are refreshed on every call: they describe the node as it
    /// was last shown.
    pub fn get_or_assign(
        &mut self,
        key: RefKey,
        role: &'static str,
        name: &str,
        parent: Option<u32>,
    ) -> u32 {
        if let Some(&r) = self.by_key.get(&key) {
            if let Some(entry) = self.entries.get_mut(&r) {
                entry.role = role;
                if entry.name != name {
                    entry.name = name.to_string();
                }
                entry.parent = parent;
                entry.seen = self.pass;
            }
            return r;
        }
        self.next += 1;
        let r = self.next;
        self.by_key.insert(key, r);
        self.entries.insert(
            r,
            RefEntry {
                key,
                role,
                name: name.to_string(),
                parent,
                stale: None,
                replaced_by: None,
                born: self.pass,
                seen: self.pass,
                gone_at: 0,
            },
        );
        r
    }

    pub fn get(&self, key: RefKey) -> Option<u32> {
        self.by_key.get(&key).copied()
    }

    pub fn entry(&self, r: u32) -> Option<&RefEntry> {
        self.entries.get(&r)
    }

    /// `e12` (or `12`) as a number.
    pub fn parse(text: &str) -> Option<u32> {
        let t = text.trim();
        let t = t.strip_prefix('e').unwrap_or(t);
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    }

    /// The node of a ref, whatever its state; for single-document callers.
    pub fn resolve(&self, text: &str) -> Option<NodeId> {
        let r = Self::parse(text)?;
        self.entries.get(&r).map(|e| e.key.node)
    }

    /// Looks a ref up for use. `is_live` says whether a node is still in
    /// its document (and that document is current); a ref that fails it
    /// is marked stale and reported with a suggestion.
    pub fn lookup(
        &mut self,
        text: &str,
        is_live: impl Fn(&RefKey) -> bool,
    ) -> Result<RefKey, RefError> {
        let r = Self::parse(text).ok_or_else(|| RefError::BadSyntax(text.trim().to_string()))?;
        let entry = self.entries.get(&r).ok_or(if r >= 1 && r <= self.next {
            RefError::Forgotten(r)
        } else {
            RefError::Unknown(r)
        })?;
        if entry.stale.is_none() && is_live(&entry.key) {
            return Ok(entry.key);
        }
        let reason = entry.stale.unwrap_or(StaleReason::Removed);
        if let Some(entry) = self.entries.get_mut(&r) {
            entry.stale = Some(reason);
        }
        let entry = &self.entries[&r];
        Err(RefError::Stale {
            r,
            reason,
            role: entry.role,
            name: entry.name.clone(),
            suggestion: self.suggest(r, &is_live),
        })
    }

    /// The live ref that took a stale one's place, for an error to
    /// suggest: as [`RefTable::replacement`] finds it.
    pub fn suggest(&self, r: u32, is_live: impl Fn(&RefKey) -> bool) -> Option<u32> {
        self.replacement(r, is_live)
    }

    /// The live ref that took a removed one's place, when there is no
    /// doubt about it: the one a diff saw the page render in its place, or
    /// else the only node first shown after the removed one was last seen
    /// with the same frame, role and name, under a parent of the same role
    /// and name. A node the page showed alongside it (a row alike) is
    /// never one. Refs gone with their document have no replacement.
    pub fn replacement(&self, r: u32, is_live: impl Fn(&RefKey) -> bool) -> Option<u32> {
        let entry = self.entries.get(&r)?;
        if entry
            .stale
            .is_some_and(|reason| reason != StaleReason::Removed)
        {
            return None;
        }
        if let Some(next) = entry.replaced_by
            && let Some(e) = self.entries.get(&next)
            && e.stale.is_none()
            && is_live(&e.key)
        {
            return Some(next);
        }
        let parent_sig = self.parent_signature(entry.parent);
        let mut found = self.entries.iter().filter(|&(&other, e)| {
            other != r
                && e.born > entry.seen
                && e.stale.is_none()
                && e.key.frame == entry.key.frame
                && e.role == entry.role
                && e.name == entry.name
                && self.parent_signature(e.parent) == parent_sig
                && is_live(&e.key)
        });
        let first = found.next().map(|(&other, _)| other);
        if found.next().is_some() { None } else { first }
    }

    /// The other live-looking refs with the same frame, role and name.
    pub fn namesakes(&self, r: u32) -> impl Iterator<Item = &RefEntry> + '_ {
        let entry = self.entries.get(&r);
        self.entries.iter().filter_map(move |(&other, e)| {
            let entry = entry?;
            (other != r
                && e.stale.is_none()
                && e.key.frame == entry.key.frame
                && e.role == entry.role
                && e.name == entry.name)
                .then_some(e)
        })
    }

    /// The nearest shown ancestor of a ref that has a name, else its
    /// nearest shown ancestor: where it is, for telling namesakes apart.
    pub fn context_of(&self, r: u32) -> Option<u32> {
        let first = self.entries.get(&r)?.parent?;
        let mut at = Some(first);
        for _ in 0..8 {
            let Some(entry) = at.and_then(|p| self.entries.get(&p)) else {
                break;
            };
            if !entry.name.is_empty() {
                return at;
            }
            at = entry.parent;
        }
        Some(first)
    }

    /// The role and name of a parent ref, for matching replacements.
    fn parent_signature(&self, parent: Option<u32>) -> Option<(&'static str, &str)> {
        parent
            .and_then(|p| self.entries.get(&p))
            .map(|e| (e.role, e.name.as_str()))
    }

    /// Records that the page re-rendered `old` as `new`.
    pub fn set_replaced(&mut self, old: u32, new: u32) {
        if let Some(entry) = self.entries.get_mut(&old) {
            entry.replaced_by = Some(new);
            entry.stale.get_or_insert(StaleReason::Removed);
        }
    }

    /// Marks a ref stale (its node left the document).
    pub fn mark_removed(&mut self, r: u32) {
        if let Some(entry) = self.entries.get_mut(&r) {
            entry.stale.get_or_insert(StaleReason::Removed);
        }
    }

    /// The frame now shows a document of `epoch`: refs into its other
    /// documents are stale.
    pub fn document_replaced(&mut self, frame: u32, epoch: u64) {
        self.left += 1;
        for entry in self.entries.values_mut() {
            if entry.key.frame == frame && entry.key.epoch != epoch && entry.stale.is_none() {
                entry.stale = Some(StaleReason::Navigated);
                entry.gone_at = self.left;
            }
        }
        self.forget_long_gone();
    }

    /// Forgets the refs of documents left more than [`KEEP_LEFT`] ago.
    fn forget_long_gone(&mut self) {
        let left = self.left;
        let gone: Vec<u32> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                matches!(
                    e.stale,
                    Some(StaleReason::Navigated | StaleReason::FrameClosed)
                ) && e.gone_at + KEEP_LEFT < left
            })
            .map(|(&r, _)| r)
            .collect();
        for r in gone {
            if let Some(entry) = self.entries.remove(&r) {
                self.by_key.remove(&entry.key);
            }
        }
    }

    /// The frame is gone: all its refs are stale.
    pub fn frame_closed(&mut self, frame: u32) {
        self.left += 1;
        for entry in self.entries.values_mut() {
            if entry.key.frame == frame && entry.stale.is_none() {
                entry.stale = Some(StaleReason::FrameClosed);
                entry.gone_at = self.left;
            }
        }
        self.forget_long_gone();
    }

    /// Number of refs ever handed out.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A ref table seen from one document: refs handed out through it name
/// nodes of that document (the read views share the snapshot's refs).
pub struct RefScope<'a> {
    refs: &'a mut RefTable,
    frame: u32,
    epoch: u64,
}

impl<'a> RefScope<'a> {
    pub fn new(refs: &'a mut RefTable, frame: u32, epoch: u64) -> Self {
        Self { refs, frame, epoch }
    }

    /// The only document there is (parse-only pipelines).
    pub fn plain(refs: &'a mut RefTable) -> Self {
        Self::new(refs, 0, 0)
    }

    /// A shorter-lived scope over the same table.
    pub fn reborrow(&mut self) -> RefScope<'_> {
        RefScope {
            refs: self.refs,
            frame: self.frame,
            epoch: self.epoch,
        }
    }

    /// The ref of `node`, allocating one (named from its role and text)
    /// the first time.
    pub fn assign(&mut self, dom: &Dom, node: NodeId, oracle: &dyn StyleOracle) -> u32 {
        let key = RefKey {
            frame: self.frame,
            epoch: self.epoch,
            node,
        };
        if let Some(r) = self.refs.get(key) {
            return r;
        }
        let role = match role_for(dom, node) {
            None | Some("none") => "generic",
            Some(role) => role,
        };
        let text = subtree_text(dom, node, oracle);
        let name = cap_name(&text).unwrap_or(text);
        self.refs.get_or_assign(key, role, &name, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catpaw_dom::parse_html;

    #[test]
    fn refs_are_monotonic_and_go_stale_with_their_document() {
        let doc = parse_html("<button>A</button><button>B</button>", &Default::default());
        let buttons: Vec<NodeId> = doc
            .dom
            .descendants(doc.dom.document())
            .filter(|&n| doc.dom.is_html_element(n, "button"))
            .collect();
        let mut refs = RefTable::new();
        let key = |node| RefKey {
            frame: 0,
            epoch: 1,
            node,
        };
        let a = refs.get_or_assign(key(buttons[0]), "button", "A", None);
        let b = refs.get_or_assign(key(buttons[1]), "button", "B", None);
        assert_eq!((a, b), (1, 2));
        assert_eq!(refs.get_or_assign(key(buttons[0]), "button", "A", None), 1);
        assert_eq!(refs.lookup("e2", |_| true), Ok(key(buttons[1])));
        assert_eq!(
            refs.lookup("x", |_| true),
            Err(RefError::BadSyntax("x".into()))
        );
        assert_eq!(refs.lookup("e9", |_| true), Err(RefError::Unknown(9)));

        refs.document_replaced(0, 2);
        match refs.lookup("e1", |_| true) {
            Err(RefError::Stale { reason, name, .. }) => {
                assert_eq!(reason, StaleReason::Navigated);
                assert_eq!(name, "A");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refs_of_documents_left_long_ago_are_forgotten() {
        let doc = parse_html("<a href=/a>A</a>", &Default::default());
        let a = doc
            .dom
            .descendants(doc.dom.document())
            .find(|&n| doc.dom.is_html_element(n, "a"))
            .unwrap();
        let mut refs = RefTable::new();
        let key = |epoch| RefKey {
            frame: 0,
            epoch,
            node: a,
        };
        let first = refs.get_or_assign(key(1), "link", "A", None);
        for epoch in 2..=5 {
            refs.document_replaced(0, epoch);
            refs.get_or_assign(key(epoch), "link", "A", None);
        }
        assert!(matches!(
            refs.lookup(&format!("e{first}"), |_| true),
            Err(RefError::Forgotten(r)) if r == first
        ));
        assert!(refs.len() < 5, "the oldest went");
        assert!(matches!(
            refs.lookup("e999", |_| true),
            Err(RefError::Unknown(999))
        ));
    }

    #[test]
    fn a_removed_ref_suggests_its_replacement() {
        let doc = parse_html(
            "<ul><li>Socks <button>Remove</button></li></ul><ul><li>Socks <button>Remove</button></li></ul>",
            &Default::default(),
        );
        let nodes: Vec<NodeId> = doc
            .dom
            .descendants(doc.dom.document())
            .filter(|&n| doc.dom.is_html_element(n, "li") || doc.dom.is_html_element(n, "button"))
            .collect();
        let k = |n| RefKey::plain(n);
        let removed = nodes[1];
        let live = |key: &RefKey| key.node != removed;
        // Two rows alike, shown together: removing one leaves the other
        // standing, not taking its place.
        let mut refs = RefTable::new();
        refs.begin_pass();
        let row1 = refs.get_or_assign(k(nodes[0]), "listitem", "Socks", None);
        let old = refs.get_or_assign(k(nodes[1]), "button", "Remove", Some(row1));
        let row2 = refs.get_or_assign(k(nodes[2]), "listitem", "Socks", None);
        let other = refs.get_or_assign(k(nodes[3]), "button", "Remove", Some(row2));
        match refs.lookup(&format!("e{old}"), live) {
            Err(RefError::Stale {
                reason, suggestion, ..
            }) => {
                assert_eq!(reason, StaleReason::Removed);
                assert_eq!(suggestion, None, "the row alike is not its replacement");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(refs.replacement(old, live), None);
        // A diff that saw the page render one in its place says so.
        refs.set_replaced(old, other);
        assert_eq!(refs.suggest(old, live), Some(other));
        assert_eq!(refs.replacement(old, live), Some(other));

        // A node first shown after the removed one was last seen, alike
        // and alone, is its re-rendering.
        let mut rerender = RefTable::new();
        rerender.begin_pass();
        let row = rerender.get_or_assign(k(nodes[0]), "listitem", "Socks", None);
        let old = rerender.get_or_assign(k(nodes[1]), "button", "Remove", Some(row));
        rerender.begin_pass();
        let row = rerender.get_or_assign(k(nodes[0]), "listitem", "Socks", None);
        let new = rerender.get_or_assign(k(nodes[3]), "button", "Remove", Some(row));
        let _ = rerender.lookup(&format!("e{old}"), live);
        assert_eq!(rerender.replacement(old, live), Some(new));
        rerender.document_replaced(0, 9);
        assert_eq!(rerender.replacement(old, live), None);
    }
}
