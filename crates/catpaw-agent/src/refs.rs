//! Element references (`eN`): allocated the first time a node is shown,
//! monotonic, never reused (ADR 0005).
//!
//! A ref names a node of one document of one frame. Documents are told
//! apart by an epoch the embedder supplies, because a new document's arena
//! can hand out the very keys the old one used. A ref whose node left the
//! document, or whose document was replaced, is stale; looking it up says
//! why and suggests the node rendered again in its place, when there is
//! evidence that it is the same item.

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
    /// Its place among the elements that ancestor showed (the first is
    /// 0); unknown until a snapshot shows it.
    index: Option<u32>,
    /// What it showed, with what was under it (see
    /// [`crate::snapshot::Fingerprint`]).
    content: u64,
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
    /// Handed out for a node gone long ago, and forgotten:
    /// [`RefTable::forgotten`] says whether it was removed from the page
    /// or went with its document.
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
    /// Why each forgotten ref went, two bits a ref (see
    /// [`RefTable::forgotten`]).
    forgotten_why: Vec<u64>,
}

/// Refs of the documents a tab left are kept for this many more, so that
/// an error can still say what one was; then they are forgotten.
const KEEP_LEFT: u64 = 2;
/// The ref of a node gone from the page is kept for this many passes
/// after the last one that showed it, so that an error can still say what
/// it was and what took its place; then it is forgotten.
const KEEP_PASSES: u64 = 8;

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
                index: None,
                content: 0,
                stale: None,
                replaced_by: None,
                born: self.pass,
                seen: self.pass,
                gone_at: 0,
            },
        );
        r
    }

    /// Records where a snapshot showed a node, the `index`-th element
    /// under its parent, and what it showed (`content`, its
    /// [`crate::snapshot::Fingerprint`]): the evidence that a node shown
    /// later in its place is the same item rendered again.
    pub fn placed(&mut self, r: u32, index: u32, content: u64) {
        if let Some(entry) = self.entries.get_mut(&r) {
            entry.index = Some(index);
            entry.content = content;
        }
    }

    /// Names a ref in errors and results by `name`: a nameless container
    /// by what it holds.
    pub fn set_name(&mut self, r: u32, name: &str) {
        if let Some(entry) = self.entries.get_mut(&r)
            && entry.name != name
        {
            entry.name = name.to_string();
        }
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

    /// The live ref that took a removed one's place, when there is
    /// evidence that it is the same item rendered again: the one a diff
    /// saw the page render in its place, or else the only node first shown
    /// after the removed one was last seen that sits where it sat (in its
    /// frame, under its parent or the parent's replacement, at its place
    /// among the elements there) and shows what it showed (its role, name
    /// and texts, its descendants' too). A row alike that slid into its
    /// place, or the next page's row there, is never one. Refs gone with
    /// their document have no replacement.
    pub fn replacement(&self, r: u32, is_live: impl Fn(&RefKey) -> bool) -> Option<u32> {
        self.replacement_with(r, &is_live)
    }

    fn replacement_with(&self, r: u32, is_live: &dyn Fn(&RefKey) -> bool) -> Option<u32> {
        let entry = self.entries.get(&r)?;
        if entry
            .stale
            .is_some_and(|reason| reason != StaleReason::Removed)
        {
            return None;
        }
        if let Some(next) = entry.replaced_by
            && self.is_shown(next, is_live)
        {
            return Some(next);
        }
        let index = entry.index?;
        let parent = match entry.parent {
            Some(p) if !self.is_shown(p, is_live) => Some(self.replacement_with(p, is_live)?),
            parent => parent,
        };
        let mut found = self.entries.iter().filter(|&(&other, e)| {
            other != r
                && e.born > entry.seen
                && e.key.frame == entry.key.frame
                && e.parent == parent
                && e.index == Some(index)
                && e.role == entry.role
                && e.content == entry.content
                && e.stale.is_none()
                && is_live(&e.key)
        });
        let first = found.next().map(|(&other, _)| other);
        if found.next().is_some() { None } else { first }
    }

    /// The live ref with a removed one's role that a later pass showed
    /// where it was (under its parent, or what is now in the parent's
    /// place, at its place among the elements there): what an error can
    /// name when there is no replacement, as another element that only
    /// took its place.
    pub fn occupant(&self, r: u32, is_live: impl Fn(&RefKey) -> bool) -> Option<u32> {
        self.occupant_with(r, &is_live)
    }

    fn occupant_with(&self, r: u32, is_live: &dyn Fn(&RefKey) -> bool) -> Option<u32> {
        let entry = self.entries.get(&r)?;
        if entry
            .stale
            .is_some_and(|reason| reason != StaleReason::Removed)
        {
            return None;
        }
        let index = entry.index?;
        let parent = match entry.parent {
            Some(p) if !self.is_shown(p, is_live) => Some(self.occupant_with(p, is_live)?),
            parent => parent,
        };
        self.entries
            .iter()
            .filter(|&(&other, e)| {
                other != r
                    && e.seen > entry.seen
                    && e.key.frame == entry.key.frame
                    && e.parent == parent
                    && e.index == Some(index)
                    && e.role == entry.role
                    && e.stale.is_none()
                    && is_live(&e.key)
            })
            .max_by_key(|(_, e)| e.seen)
            .map(|(&other, _)| other)
    }

    /// Whether a ref's node is still in the page.
    fn is_shown(&self, r: u32, is_live: &dyn Fn(&RefKey) -> bool) -> bool {
        self.entries
            .get(&r)
            .is_some_and(|e| e.stale.is_none() && is_live(&e.key))
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
        let gone: Vec<(u32, StaleReason)> = self
            .entries
            .iter()
            .filter_map(|(&r, e)| match e.stale {
                Some(reason @ (StaleReason::Navigated | StaleReason::FrameClosed))
                    if e.gone_at + KEEP_LEFT < left =>
                {
                    Some((r, reason))
                }
                _ => None,
            })
            .collect();
        for (r, reason) in gone {
            self.forget(r, reason);
        }
    }

    /// Forgets the refs into `frame`'s document of `epoch` whose nodes
    /// have left it (`is_live` says whether a node is still in it) and
    /// were last shown more than [`KEEP_PASSES`] passes ago: a page that
    /// keeps rendering new nodes leaves a table the size of what it shows,
    /// not of all it ever showed. A node only hidden keeps its ref.
    pub fn forget_removed(&mut self, frame: u32, epoch: u64, is_live: impl Fn(NodeId) -> bool) {
        let pass = self.pass;
        let gone: Vec<u32> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                e.key.frame == frame
                    && e.key.epoch == epoch
                    && e.seen + KEEP_PASSES < pass
                    && !is_live(e.key.node)
            })
            .map(|(&r, _)| r)
            .collect();
        for r in gone {
            self.forget(r, StaleReason::Removed);
        }
    }

    /// Drops a ref's entry, keeping why it went.
    fn forget(&mut self, r: u32, reason: StaleReason) {
        if let Some(entry) = self.entries.remove(&r) {
            self.by_key.remove(&entry.key);
        }
        let (word, shift) = (r as usize / 32, (r % 32) * 2);
        if self.forgotten_why.len() <= word {
            self.forgotten_why.resize(word + 1, 0);
        }
        let why: u64 = match reason {
            StaleReason::Removed => 1,
            StaleReason::Navigated => 2,
            StaleReason::FrameClosed => 3,
        };
        self.forgotten_why[word] |= why << shift;
    }

    /// Why a forgotten ref went: `Removed` when its node left a page the
    /// tab still shows, `Navigated` or `FrameClosed` when it went with its
    /// document. `None` for a ref the table has not forgotten.
    pub fn forgotten(&self, r: u32) -> Option<StaleReason> {
        let word = self.forgotten_why.get(r as usize / 32)?;
        match (word >> ((r % 32) * 2)) & 3 {
            1 => Some(StaleReason::Removed),
            2 => Some(StaleReason::Navigated),
            3 => Some(StaleReason::FrameClosed),
            _ => None,
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

    /// Number of refs the table remembers (not those it forgot).
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
    use crate::snapshot::{SnapshotOptions, Snapshotter};
    use crate::visibility::AttributeOracle;
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
        assert_eq!(refs.forgotten(first), Some(StaleReason::Navigated));
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
        for (r, index, content) in [(row1, 0, 1), (old, 0, 2), (row2, 1, 1), (other, 0, 2)] {
            refs.placed(r, index, content);
        }
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

        // A node first shown after the removed one was last seen, in its
        // place and showing what it showed, is its re-rendering.
        let mut rerender = RefTable::new();
        rerender.begin_pass();
        let row = rerender.get_or_assign(k(nodes[0]), "listitem", "Socks", None);
        let old = rerender.get_or_assign(k(nodes[1]), "button", "Remove", Some(row));
        rerender.placed(old, 0, 7);
        rerender.begin_pass();
        let row = rerender.get_or_assign(k(nodes[0]), "listitem", "Socks", None);
        let new = rerender.get_or_assign(k(nodes[3]), "button", "Remove", Some(row));
        rerender.placed(new, 0, 7);
        let _ = rerender.lookup(&format!("e{old}"), live);
        assert_eq!(rerender.replacement(old, live), Some(new));
        // Elsewhere among the row's elements, or showing something else,
        // it is another node.
        rerender.placed(new, 1, 7);
        assert_eq!(rerender.replacement(old, live), None);
        rerender.placed(new, 0, 8);
        assert_eq!(rerender.replacement(old, live), None);
        rerender.placed(new, 0, 7);
        rerender.document_replaced(0, 9);
        assert_eq!(rerender.replacement(old, live), None);
    }

    /// A pass over the page, as a snapshot makes one.
    fn look(dom: &Dom, refs: &mut RefTable) {
        refs.begin_pass();
        Snapshotter::new(dom, &AttributeOracle, refs).body(&SnapshotOptions::default());
    }

    fn live(dom: &Dom) -> impl Fn(&RefKey) -> bool + '_ {
        |key| dom.contains(key.node) && dom.is_connected(key.node)
    }

    #[test]
    fn refs_of_nodes_long_gone_from_the_page_are_forgotten() {
        let mut page = parse_html(
            "<ul><li>Socks <button>Remove</button></li><li>Hats <button>Remove</button></li></ul>\
             <p hidden>Shipped in a week</p>",
            &Default::default(),
        );
        let dom = &page.dom;
        let first = |local: &str| {
            dom.descendants(dom.document())
                .find(|&n| dom.is_html_element(n, local))
                .unwrap()
        };
        let (row, button, note) = (first("li"), first("button"), first("p"));
        let mut refs = RefTable::new();
        look(&page.dom, &mut refs);
        let remove = refs.get(RefKey::plain(button)).unwrap();
        // A read view gave the hidden note a ref; it is still in the page.
        let hidden = RefScope::plain(&mut refs).assign(&page.dom, note, &AttributeOracle);
        let known = refs.len();

        page.dom.detach(row);
        look(&page.dom, &mut refs);
        // Just removed, the ref still says what it was.
        match refs.lookup(&format!("e{remove}"), live(&page.dom)) {
            Err(RefError::Stale {
                reason, role, name, ..
            }) => assert_eq!(
                (reason, role, name.as_str()),
                (StaleReason::Removed, "button", "Remove")
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(refs.forgotten(remove), None);
        // Long gone, it is forgotten, and why is kept.
        for _ in 0..KEEP_PASSES {
            look(&page.dom, &mut refs);
        }
        assert_eq!(
            refs.lookup(&format!("e{remove}"), live(&page.dom)),
            Err(RefError::Forgotten(remove))
        );
        assert_eq!(refs.forgotten(remove), Some(StaleReason::Removed));
        assert_eq!(refs.len(), known - 2, "the row and its button went");
        assert!(refs.entry(hidden).is_some());
        assert_eq!(refs.forgotten(hidden), None);
    }

    #[test]
    fn a_row_that_took_a_deleted_rows_place_is_not_its_replacement() {
        let mut page = parse_html(
            "<ul><li>Item 1 <button>Delete</button></li><li>Item 2 <button>Delete</button></li>\
             <li>Item 3 <button>Delete</button></li></ul>\
             <div hidden><li>Item 4 <button>Delete</button></li><li>Item 1 <button>Delete</button></li></div>",
            &Default::default(),
        );
        let dom = &page.dom;
        let rows: Vec<NodeId> = dom
            .descendants(dom.document())
            .filter(|&n| dom.is_html_element(n, "li"))
            .collect();
        let buttons: Vec<NodeId> = rows
            .iter()
            .map(|&row| {
                dom.descendants(row)
                    .find(|&n| dom.is_html_element(n, "button"))
                    .unwrap()
            })
            .collect();
        let list = dom.parent_element(rows[0]).unwrap();
        let delete = |refs: &RefTable, row: usize| refs.get(RefKey::plain(buttons[row])).unwrap();
        let mut refs = RefTable::new();
        look(&page.dom, &mut refs);
        let (second, third) = (delete(&refs, 1), delete(&refs, 2));

        // Item 2 is deleted, and item 4 slides in at the end of the list.
        page.dom.detach(rows[1]);
        page.dom.append_child(list, rows[3]);
        look(&page.dom, &mut refs);
        match refs.lookup(&format!("e{second}"), live(&page.dom)) {
            Err(RefError::Stale { suggestion, .. }) => assert_eq!(suggestion, None),
            other => panic!("{other:?}"),
        }
        assert_eq!(refs.replacement(second, live(&page.dom)), None);
        // In its place now: item 3's button, slid up.
        assert_eq!(refs.occupant(second, live(&page.dom)), Some(third));

        // Item 1 rendered again, the same in the same place, is item 1.
        let first = delete(&refs, 0);
        page.dom.insert_before(list, rows[4], Some(rows[0]));
        page.dom.detach(rows[0]);
        look(&page.dom, &mut refs);
        assert_eq!(
            refs.replacement(first, live(&page.dom)),
            Some(delete(&refs, 4))
        );
    }
}
