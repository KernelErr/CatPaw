//! Following the document: the arena's journal of changes, turned into
//! slot updates and Stylo invalidations (restyle hints, snapshots of the
//! attributes and state an element had, dirty bits on the way to it), so
//! that a restyle only restyles what the changes can have affected.
//!
//! The journal says where to look; what is there now, compared with what
//! the slots saw last time, says what changed. A write that changed
//! nothing a selector or the cascade reads (an attribute set to the value
//! it had, bookkeeping) restyles nothing.

use std::sync::atomic::Ordering;

use catpaw_dom::{Attr, Change, Dom, NodeId, NodeKind};
use selectors::matching::ElementSelectorFlags;
use style::attr::{AttrIdentifier, AttrValue};
use style::dom::TNode;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::parse_style_attribute;
use style::servo_arc::Arc;
use style::stylesheets::CssRuleType;
use style::values::GenericAtomIdent;
use style_dom::ElementState;

use crate::engine::{StyleEngine, element_state};
use crate::node::CatNode;
use crate::table::StyleTable;

/// What refreshing a slot found changed.
#[derive(Default)]
struct SlotChange {
    /// The attributes the slot had, when they changed.
    attrs: Option<Vec<Attr>>,
    /// The state the slot had, when it changed.
    state: Option<ElementState>,
    /// The `style` attribute changed.
    style: bool,
}

/// A null-namespace attribute of a list.
fn attr_of<'a>(attrs: &'a [Attr], local: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name.ns.is_empty() && &*a.name.local == local)
        .map(|a| a.value.as_str())
}

/// Sets the dirty-descendants bit on `from` and its ancestors in the flat
/// tree (the way the traversal goes: through slots and shadow hosts), so
/// that the traversal finds its way down to what is below `from`.
fn mark_dirty(dom: &Dom, table: &StyleTable, from: NodeId) {
    let mut current = Some(from);
    while let Some(id) = current {
        if !dom.is_element(id) {
            // From the top of a shadow tree, on from its host.
            current = dom.shadow_host(id);
            continue;
        }
        if let Some(slot) = table.slot(id) {
            slot.dirty_descendants.store(true, Ordering::SeqCst);
        }
        current = dom.flat_parent(id);
    }
}

/// Attributes as Stylo's snapshots hold them.
fn snapshot_attrs(attrs: &[Attr]) -> Vec<(AttrIdentifier, AttrValue)> {
    attrs
        .iter()
        .map(|attr| {
            let local = GenericAtomIdent(attr.name.local.clone());
            let ident = AttrIdentifier {
                local_name: local.clone(),
                name: local,
                namespace: GenericAtomIdent(attr.name.ns.clone()),
                prefix: attr.name.prefix.clone().map(GenericAtomIdent),
            };
            let plain = attr.name.ns.is_empty();
            let value = match &*attr.name.local {
                "class" if plain => AttrValue::from_serialized_tokenlist(attr.value.clone()),
                "id" if plain => AttrValue::from_atomic(attr.value.clone()),
                _ => AttrValue::String(attr.value.clone()),
            };
            (ident, value)
        })
        .collect()
}

impl StyleEngine {
    /// Brings the slots up to date with the document and records what the
    /// changes since the last sync mean for the styles.
    pub(crate) fn sync(&mut self, dom: &Dom) {
        let version = dom.version();
        if self.synced == Some(version) {
            return;
        }
        self.resolved.clear();
        match self.synced.and_then(|synced| dom.changes_since(synced)) {
            Some(changes) => {
                for &change in changes {
                    self.apply(dom, change);
                }
            }
            None => self.refresh_all(dom),
        }
        self.synced = Some(version);
    }

    /// Refreshes the slot of every connected element: the first time, or
    /// when the journal no longer reaches back to the last sync. Styles
    /// start over.
    fn refresh_all(&mut self, dom: &Dom) {
        if self.styled {
            self.invalidate();
        }
        for id in dom.shadow_including_descendants(dom.document()) {
            if dom.is_element(id) {
                self.refresh_slot(dom, id);
                if let Some(shadow) = dom.element(id).and_then(|e| e.shadow_root) {
                    self.known_shadows.insert(shadow);
                }
            }
        }
    }

    fn apply(&mut self, dom: &Dom, change: Change) {
        match change {
            Change::Inserted { parent, node } => self.inserted(dom, parent, node),
            Change::Removed { parent, node } => self.removed(dom, parent, node),
            Change::Data(node) => self.data_changed(dom, node),
            Change::Freed(_) => {}
        }
    }

    /// Brings an element's slot up to date with its attributes and state,
    /// and says what changed.
    fn refresh_slot(&mut self, dom: &Dom, id: NodeId) -> SlotChange {
        let Some(el) = dom.element(id) else {
            return SlotChange::default();
        };
        let state = element_state(dom, id);
        let url_data = &self.url_data;
        let quirks = self.quirks_mode;
        let lock = self.table.lock().clone();
        let (slot, _) = self.table.ensure(id);
        let mut change = SlotChange::default();
        if slot.state != state {
            change.state = Some(std::mem::replace(&mut slot.state, state));
        }
        if slot.attrs != el.attrs {
            let style = el.attr("style");
            if attr_of(&slot.attrs, "style") != style {
                slot.style_attribute = style.map(|css| {
                    let block =
                        parse_style_attribute(css, url_data, None, quirks, CssRuleType::Style);
                    Arc::new(lock.wrap(block))
                });
                change.style = true;
            }
            let id_attr = el.id();
            if attr_of(&slot.attrs, "id") != id_attr {
                slot.id_atom = id_attr.map(style::Atom::from);
            }
            change.attrs = Some(std::mem::replace(&mut slot.attrs, el.attrs.clone()));
        }
        change
    }

    /// `node` came into `parent`: its elements get fresh slots and no
    /// style (they are styled from scratch), and what its coming affects
    /// around it is restyled.
    fn inserted(&mut self, dom: &Dom, parent: NodeId, node: NodeId) {
        if !dom.contains(node) || !dom.is_connected(node) {
            return;
        }
        for id in dom.shadow_including_descendants(node) {
            if !dom.is_element(id) {
                continue;
            }
            self.refresh_slot(dom, id);
            if let Some(slot) = self.table.slot_mut(id) {
                slot.clear_data();
            }
            if let Some(shadow) = dom.element(id).and_then(|e| e.shadow_root) {
                self.known_shadows.insert(shadow);
            }
        }
        // Where it went since is logged too.
        if dom.parent(node) == Some(parent) {
            self.children_changed(dom, parent, node, true);
        }
    }

    /// `node` left `parent`: it keeps no style (unless it came back, which
    /// the journal says later), and what its going affects is restyled.
    fn removed(&mut self, dom: &Dom, parent: NodeId, node: NodeId) {
        if dom.contains(node) && !dom.is_connected(node) {
            for id in dom.shadow_including_descendants(node) {
                if let Some(slot) = self.table.slot_mut(id) {
                    slot.clear_data();
                }
            }
        }
        self.children_changed(dom, parent, node, false);
    }

    /// A child came into or left `parent`. New children have no style, so
    /// the traversal has to reach them; `parent` itself may stop or start
    /// matching `:empty`, and its other children `:first-child`,
    /// `:nth-child()` or a sibling combinator, as the selector flags left
    /// by matching say.
    fn children_changed(&mut self, dom: &Dom, parent: NodeId, child: NodeId, inserted: bool) {
        if !self.styled || !dom.contains(parent) {
            return;
        }
        match dom.kind(parent) {
            NodeKind::Element(el) => {
                // A host's children are rendered through its slots, which
                // they may now go to or leave.
                if el.shadow_root.is_some() {
                    if dom.is_connected(parent) {
                        self.restyle_subtree(dom, parent);
                    }
                    return;
                }
            }
            // The root element came or went (or something else at the top
            // whose kind is gone with it): start over.
            NodeKind::Document(_) if parent == dom.document() => {
                if !dom.contains(child) || dom.is_element(child) {
                    self.invalidate();
                }
                return;
            }
            // The top of a shadow tree: the traversal reaches it through
            // the host.
            _ => {
                if let Some(host) = dom.shadow_host(parent)
                    && dom.is_connected(host)
                    && self
                        .table
                        .slot(host)
                        .is_some_and(|slot| slot.has_data.load(Ordering::SeqCst))
                {
                    self.pending = true;
                    mark_dirty(dom, &self.table, host);
                }
                return;
            }
        }
        if !dom.is_connected(parent) {
            return;
        }
        let Some(slot) = self.table.slot(parent) else {
            return;
        };
        if !slot.has_data.load(Ordering::SeqCst) {
            // Not styled (inside `display: none`): neither are its children.
            return;
        }
        let flags = slot.selector_flags.get();
        self.pending = true;
        mark_dirty(dom, &self.table, parent);
        if flags.contains(ElementSelectorFlags::HAS_EMPTY_SELECTOR) {
            self.restyle_subtree(dom, parent);
            self.restyle_later_siblings(dom, parent);
        }
        let all = ElementSelectorFlags::HAS_SLOW_SELECTOR
            | ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS
            | ElementSelectorFlags::HAS_SLOW_SELECTOR_NTH_OF
            | ElementSelectorFlags::MAY_HAVE_TREE_COUNTING_FUNCTION;
        if flags.intersects(all) {
            let children: Vec<NodeId> = dom.child_elements(parent).collect();
            for el in children {
                self.restyle_subtree(dom, el);
            }
        } else if flags.contains(ElementSelectorFlags::HAS_EDGE_CHILD_SELECTOR) {
            // The first and last children now, and the old ones next to a
            // newcomer.
            let mut edges = vec![
                dom.child_elements(parent).next(),
                dom.child_elements(parent).last(),
            ];
            if inserted && dom.parent(child) == Some(parent) {
                let element_sibling = |mut next: Option<NodeId>, forward: bool| {
                    while let Some(n) = next {
                        if dom.is_element(n) {
                            return Some(n);
                        }
                        next = if forward {
                            dom.next_sibling(n)
                        } else {
                            dom.prev_sibling(n)
                        };
                    }
                    None
                };
                edges.push(element_sibling(dom.prev_sibling(child), false));
                edges.push(element_sibling(dom.next_sibling(child), true));
            }
            for el in edges.into_iter().flatten() {
                self.restyle_subtree(dom, el);
            }
        }
    }

    /// Restyles an element and everything in it.
    fn restyle_subtree(&mut self, dom: &Dom, el: NodeId) {
        let Some(slot) = self.table.slot(el) else {
            return;
        };
        if !slot.has_data.load(Ordering::SeqCst) {
            return;
        }
        slot.data
            .borrow_mut()
            .hint
            .insert(RestyleHint::restyle_subtree());
        if let Some(parent) = dom.parent(el) {
            mark_dirty(dom, &self.table, parent);
        }
        self.pending = true;
    }

    /// Restyles the siblings after `el`, when a sibling combinator was
    /// matched against the children of its parent (a change to `el` that
    /// selectors see may change what those match).
    fn restyle_later_siblings(&mut self, dom: &Dom, el: NodeId) {
        let Some(parent) = dom.parent_element(el) else {
            return;
        };
        let flags = self
            .table
            .slot(parent)
            .map(|slot| slot.selector_flags.get())
            .unwrap_or(ElementSelectorFlags::empty());
        if !flags.intersects(
            ElementSelectorFlags::HAS_SLOW_SELECTOR
                | ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS,
        ) {
            return;
        }
        let mut next = dom.next_sibling(el);
        while let Some(sibling) = next {
            if dom.is_element(sibling) {
                self.restyle_subtree(dom, sibling);
            }
            next = dom.next_sibling(sibling);
        }
    }

    /// A node's own data changed.
    fn data_changed(&mut self, dom: &Dom, node: NodeId) {
        if !dom.contains(node) {
            return;
        }
        match dom.kind(node) {
            NodeKind::Element(_) => {
                if dom.is_connected(node) {
                    self.element_changed(dom, node);
                }
            }
            // Of character data, selectors only see whether text makes
            // its parent not `:empty`.
            NodeKind::Text(_) => {
                if !self.styled {
                    return;
                }
                let Some(parent) = dom.parent_element(node) else {
                    return;
                };
                let empty_selector = self.table.slot(parent).is_some_and(|slot| {
                    slot.selector_flags
                        .get()
                        .contains(ElementSelectorFlags::HAS_EMPTY_SELECTOR)
                });
                if empty_selector && dom.is_connected(parent) {
                    self.restyle_subtree(dom, parent);
                    self.restyle_later_siblings(dom, parent);
                }
            }
            // The quirks mode is the page's to set (`set_quirks_mode`);
            // nothing else of a document, doctype or fragment is styled.
            _ => {}
        }
    }

    /// A connected element's attributes may have changed.
    fn element_changed(&mut self, dom: &Dom, el: NodeId) {
        let change = self.refresh_slot(dom, el);
        // A shadow root attached since needs slots for its tree, and the
        // host is rendered with it from now on: its own children only
        // through slots, and the style they had goes.
        if let Some(shadow) = dom.element(el).and_then(|e| e.shadow_root) {
            for id in dom.shadow_including_descendants(shadow) {
                if dom.is_element(id) && !self.table.contains(id) {
                    self.refresh_slot(dom, id);
                }
            }
            if self.known_shadows.insert(shadow) {
                for id in dom.descendants(el).skip(1) {
                    if let Some(slot) = self.table.slot_mut(id) {
                        slot.clear_data();
                    }
                }
                self.restyle_subtree(dom, el);
            }
        }
        // Slotting: an element that names another slot, or a slot that
        // takes another name, moves nodes between slots.
        if let Some(old) = &change.attrs {
            let slot_changed = attr_of(old, "slot") != dom.attr(el, "slot");
            let name_changed =
                dom.is_html_element(el, "slot") && attr_of(old, "name") != dom.attr(el, "name");
            let host = if slot_changed {
                dom.parent(el)
                    .filter(|&p| dom.element(p).is_some_and(|e| e.shadow_root.is_some()))
            } else if name_changed {
                dom.containing_shadow_root(el)
                    .and_then(|root| dom.shadow_host(root))
            } else {
                None
            };
            if let Some(host) = host {
                if slot_changed && let Some(slot) = self.table.slot_mut(el) {
                    slot.clear_data();
                }
                self.restyle_subtree(dom, host);
            }
        }
        let Some(old_attrs) = change.attrs else {
            if let Some(state) = change.state {
                self.snapshot(dom, el, None, Some(state));
            }
            return;
        };
        // A fieldset's `disabled` decides the state of the controls in it.
        let disabled_changed =
            attr_of(&old_attrs, "disabled").is_some() != dom.attr(el, "disabled").is_some();
        if disabled_changed && dom.is_html_element(el, "fieldset") {
            let descendants: Vec<NodeId> =
                dom.descendants(el).filter(|&d| dom.is_element(d)).collect();
            for d in descendants {
                let state = element_state(dom, d);
                let old = self.table.slot_mut(d).and_then(|slot| {
                    (slot.state != state).then(|| std::mem::replace(&mut slot.state, state))
                });
                if let Some(old) = old {
                    self.snapshot(dom, d, None, Some(old));
                }
            }
        }
        if change.style && self.styled {
            // The cascade takes the new declarations without matching
            // selectors again.
            if let Some(slot) = self.table.slot(el)
                && slot.has_data.load(Ordering::SeqCst)
            {
                slot.data
                    .borrow_mut()
                    .hint
                    .insert(RestyleHint::RESTYLE_STYLE_ATTRIBUTE);
                if let Some(parent) = dom.parent(el) {
                    mark_dirty(dom, &self.table, parent);
                }
                self.pending = true;
            }
        }
        self.snapshot(dom, el, Some(old_attrs), change.state);
    }

    /// Records what an element looked like at the last restyle, for
    /// Stylo's invalidation to compare with what it looks like now: the
    /// attributes it had (`attrs`) and its state (`state`), whichever
    /// changed. The first snapshot since the last restyle is the one that
    /// counts; later ones only add to the list of what changed.
    fn snapshot(
        &mut self,
        dom: &Dom,
        el: NodeId,
        attrs: Option<Vec<Attr>>,
        state: Option<ElementState>,
    ) {
        if !self.styled {
            return;
        }
        let Some(slot) = self.table.slot(el) else {
            return;
        };
        // An element without style has nothing to invalidate: nor have its
        // descendants and siblings (it is inside `display: none`).
        if !slot.has_data.load(Ordering::SeqCst) {
            return;
        }
        let Some(current) = dom.element(el).map(|e| e.attrs.as_slice()) else {
            return;
        };
        let snapshot = self.snapshots.entry(CatNode::new(el).opaque()).or_default();
        if let Some(state) = state
            && snapshot.state.is_none()
        {
            snapshot.state = Some(state);
        }
        if let Some(old) = attrs {
            let mut note = |attr: &Attr| {
                let plain = attr.name.ns.is_empty();
                match &*attr.name.local {
                    "class" if plain => snapshot.class_changed = true,
                    "id" if plain => snapshot.id_changed = true,
                    _ => snapshot.other_attributes_changed = true,
                }
                let local = GenericAtomIdent(attr.name.local.clone());
                if !snapshot.changed_attrs.contains(&local) {
                    snapshot.changed_attrs.push(local);
                }
            };
            for attr in &old {
                match current.iter().find(|a| a.name == attr.name) {
                    Some(now) if now.value == attr.value => {}
                    _ => note(attr),
                }
            }
            for attr in current {
                if !old.iter().any(|a| a.name == attr.name) {
                    note(attr);
                }
            }
            if snapshot.attrs.is_none() {
                snapshot.attrs = Some(snapshot_attrs(&old));
            }
        }
        if !slot.has_snapshot.swap(true, Ordering::SeqCst) {
            slot.snapshot_handled.store(false, Ordering::SeqCst);
            self.snapshotted.push(el);
        }
        // The traversal looks at snapshots on its way down to the element,
        // from its parent (the root element's, before it starts).
        if let Some(parent) = dom.parent(el) {
            mark_dirty(dom, &self.table, parent);
        }
        self.pending = true;
    }
}
