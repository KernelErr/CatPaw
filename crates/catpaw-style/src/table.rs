//! Per-element style data kept beside the arena.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use catpaw_dom::{Attr, NodeId};
use selectors::matching::ElementSelectorFlags;
use slotmap::SecondaryMap;
use style::Atom;
use style::data::ElementDataWrapper;
use style::properties::PropertyDeclarationBlock;
use style::servo_arc::Arc;
use style::shared_lock::{Locked, SharedRwLock};
use style::stylist::CascadeData;
use style_dom::ElementState;

/// Everything Stylo wants to read or write on an element.
pub struct StyleSlot {
    pub data: ElementDataWrapper,
    pub has_data: AtomicBool,
    pub selector_flags: Cell<ElementSelectorFlags>,
    pub dirty_descendants: AtomicBool,
    pub has_snapshot: AtomicBool,
    pub snapshot_handled: AtomicBool,
    /// The parsed `style` attribute.
    pub style_attribute: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    /// The `id` attribute interned for `:id` matching.
    pub id_atom: Option<Atom>,
    /// Pseudo-class state derived from attributes (and later, from the
    /// engine's live state: hover, focus, checkedness...).
    pub state: ElementState,
    /// The element's attributes as the slot last saw them: what the
    /// parsed `style` attribute and `id_atom` come from, and what a
    /// restyle compares against to tell what changed.
    pub attrs: Vec<Attr>,
}

impl StyleSlot {
    /// Forgets the computed style, so that the element is styled afresh.
    pub fn clear_data(&mut self) {
        self.data = ElementDataWrapper::default();
        self.has_data.store(false, Ordering::SeqCst);
        self.dirty_descendants.store(false, Ordering::SeqCst);
        self.has_snapshot.store(false, Ordering::SeqCst);
        self.snapshot_handled.store(false, Ordering::SeqCst);
    }
}

impl Default for StyleSlot {
    fn default() -> Self {
        Self {
            data: ElementDataWrapper::default(),
            has_data: AtomicBool::new(false),
            selector_flags: Cell::new(ElementSelectorFlags::empty()),
            dirty_descendants: AtomicBool::new(false),
            has_snapshot: AtomicBool::new(false),
            snapshot_handled: AtomicBool::new(false),
            style_attribute: None,
            id_atom: None,
            state: ElementState::empty(),
            attrs: Vec::new(),
        }
    }
}

/// The side table: one [`StyleSlot`] per element node, the rules of the
/// shadow roots that have style sheets of their own, and the document's
/// shared lock for stylesheet data.
pub struct StyleTable {
    slots: SecondaryMap<NodeId, StyleSlot>,
    shadows: SecondaryMap<NodeId, Arc<CascadeData>>,
    lock: SharedRwLock,
}

impl StyleTable {
    pub fn new(lock: SharedRwLock) -> Self {
        Self {
            slots: SecondaryMap::new(),
            shadows: SecondaryMap::new(),
            lock,
        }
    }

    /// The rules of a shadow root's own style sheets, if it has any.
    pub fn shadow_rules(&self, shadow_root: NodeId) -> Option<&CascadeData> {
        self.shadows.get(shadow_root).map(|data| &**data)
    }

    /// Sets (or with `None`, drops) a shadow root's rules.
    pub fn set_shadow_rules(&mut self, shadow_root: NodeId, rules: Option<Arc<CascadeData>>) {
        match rules {
            Some(rules) => {
                self.shadows.insert(shadow_root, rules);
            }
            None => {
                self.shadows.remove(shadow_root);
            }
        }
    }

    /// The shadow roots that have rules of their own.
    pub fn shadow_roots(&self) -> Vec<NodeId> {
        self.shadows.keys().collect()
    }

    pub fn lock(&self) -> &SharedRwLock {
        &self.lock
    }

    pub fn slot(&self, id: NodeId) -> Option<&StyleSlot> {
        self.slots.get(id)
    }

    pub fn slot_mut(&mut self, id: NodeId) -> Option<&mut StyleSlot> {
        self.slots.get_mut(id)
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.slots.contains_key(id)
    }

    /// Returns the slot for `id`, creating an empty one if needed. The bool
    /// tells whether it was just created.
    pub fn ensure(&mut self, id: NodeId) -> (&mut StyleSlot, bool) {
        let created = !self.slots.contains_key(id);
        if created {
            self.slots.insert(id, StyleSlot::default());
        }
        (self.slots.get_mut(id).expect("just inserted"), created)
    }

    pub fn remove(&mut self, id: NodeId) {
        self.slots.remove(id);
    }

    /// Forgets every computed style, so that the next restyle starts from
    /// scratch.
    pub fn clear_data(&mut self) {
        for slot in self.slots.values_mut() {
            slot.clear_data();
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
