//! `CatNode`: the handle Stylo's DOM traits are implemented on.
//!
//! Stylo's style-sharing cache is a type-erased buffer sized for a
//! pointer-sized element handle (it asserts `size_of::<E>() == 8`), so the
//! handle is just the `NodeId`. The arena and the style table it refers to
//! are supplied through a thread-local context that [`with_style_context`]
//! installs for the duration of a restyle; handles must not be used outside
//! that scope.

use std::cell::Cell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::sync::atomic::Ordering;

use catpaw_dom::{Dom, ElementData, NodeId, NodeKind, ns};
use markup5ever::{LocalName, LocalNameStaticSet, Namespace, NamespaceStaticSet};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BLOOM_HASH_MASK;
use selectors::matching::{ElementSelectorFlags, MatchingContext, QuirksMode, VisitedHandlingMode};
use selectors::sink::Push;
use selectors::{Element, OpaqueElement};
use slotmap::Key as _;
use style::Atom;
use style::CaseSensitivityExt as _;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::bloom::each_relevant_element_hash;
use style::context::SharedStyleContext;
use style::data::{ElementData as StyloElementData, ElementDataMut, ElementDataRef};
use style::dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot};
use style::properties::PropertyDeclarationBlock;
use style::selector_parser::{NonTSPseudoClass, PseudoElement, SelectorImpl};
use style::servo_arc::{Arc, ArcBorrow};
use style::shared_lock::{Locked, SharedRwLock};
use style::stylist::CascadeData;
use style::values::{AtomIdent, AtomString, GenericAtomIdent};
use style_dom::ElementState;

use crate::table::{StyleSlot, StyleTable};

thread_local! {
    static CONTEXT: Cell<Option<(NonNull<Dom>, NonNull<StyleTable>)>> = const { Cell::new(None) };
}

struct ContextGuard {
    previous: Option<(NonNull<Dom>, NonNull<StyleTable>)>,
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        CONTEXT.with(|c| c.set(self.previous));
    }
}

/// Runs `f` with `dom` and `table` installed as the current style context on
/// this thread. Every [`CatNode`] created or used inside `f` resolves through
/// them. Neither may be mutated for the duration (the style table only uses
/// interior mutability, which is what Stylo expects).
pub fn with_style_context<R>(dom: &Dom, table: &StyleTable, f: impl FnOnce() -> R) -> R {
    let previous = CONTEXT.with(|c| c.replace(Some((NonNull::from(dom), NonNull::from(table)))));
    let _guard = ContextGuard { previous };
    f()
}

/// A pointer-sized handle to a node, valid inside [`with_style_context`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CatNode {
    pub id: NodeId,
}

const _: () = assert!(std::mem::size_of::<CatNode>() == 8);

impl Hash for CatNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl fmt::Debug for CatNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.dom().kind(self.id) {
            NodeKind::Element(el) => write!(f, "<{}#{:?}>", el.name.local, self.id.data()),
            other => write!(
                f,
                "{:?}#{:?}",
                std::mem::discriminant(other),
                self.id.data()
            ),
        }
    }
}

impl CatNode {
    pub fn new(id: NodeId) -> Self {
        Self { id }
    }

    fn context() -> (NonNull<Dom>, NonNull<StyleTable>) {
        CONTEXT
            .with(|c| c.get())
            .expect("CatNode used outside with_style_context")
    }

    fn dom(&self) -> &'static Dom {
        // SAFETY: `with_style_context` guarantees the arena outlives every
        // handle used inside its scope and is not mutated meanwhile; the
        // 'static lifetime is a stand-in for that scope.
        unsafe { Self::context().0.as_ref() }
    }

    fn table(&self) -> &'static StyleTable {
        // SAFETY: as for `dom`.
        unsafe { Self::context().1.as_ref() }
    }

    fn element(&self) -> Option<&'static ElementData> {
        self.dom().element(self.id)
    }

    fn element_data(&self) -> &'static ElementData {
        self.element().expect("not an element")
    }

    fn slot(&self) -> &'static StyleSlot {
        self.table()
            .slot(self.id)
            .expect("style slot missing: StyleEngine::ensure_slots was not run")
    }

    fn ffi(&self) -> u64 {
        self.id.data().as_ffi()
    }

    fn is_html_element_named(&self, local: &str) -> bool {
        self.dom().is_html_element(self.id, local)
    }
}

impl NodeInfo for CatNode {
    fn is_element(&self) -> bool {
        self.dom().is_element(self.id)
    }

    fn is_text_node(&self) -> bool {
        matches!(self.dom().kind(self.id), NodeKind::Text(_))
    }
}

fn convert_quirks(mode: markup5ever::interface::QuirksMode) -> QuirksMode {
    match mode {
        markup5ever::interface::QuirksMode::Quirks => QuirksMode::Quirks,
        markup5ever::interface::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
        markup5ever::interface::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
    }
}

impl TDocument for CatNode {
    type ConcreteNode = CatNode;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        convert_quirks(self.dom().quirks_mode())
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.table().lock()
    }
}

impl TShadowRoot for CatNode {
    type ConcreteNode = CatNode;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        unreachable!("shadow roots are never handed to Stylo yet")
    }

    fn style_data<'b>(&self) -> Option<&'b CascadeData>
    where
        Self: 'b,
    {
        None
    }
}

impl TNode for CatNode {
    type ConcreteElement = CatNode;
    type ConcreteDocument = CatNode;
    type ConcreteShadowRoot = CatNode;

    fn parent_node(&self) -> Option<Self> {
        self.dom().parent(self.id).map(CatNode::new)
    }

    fn first_child(&self) -> Option<Self> {
        self.dom().first_child(self.id).map(CatNode::new)
    }

    fn last_child(&self) -> Option<Self> {
        self.dom().last_child(self.id).map(CatNode::new)
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.dom().prev_sibling(self.id).map(CatNode::new)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.dom().next_sibling(self.id).map(CatNode::new)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        CatNode::new(self.dom().document())
    }

    fn is_in_document(&self) -> bool {
        self.dom().is_connected(self.id)
    }

    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.parent_node().and_then(|n| n.as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.ffi() as usize)
    }

    fn debug_id(self) -> usize {
        self.ffi() as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        if self.is_element() { Some(*self) } else { None }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        if matches!(self.dom().kind(self.id), NodeKind::Document(_)) {
            Some(*self)
        } else {
            None
        }
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        None
    }
}

/// Children of a node, for Stylo's traversal.
pub struct CatChildren {
    next: Option<NodeId>,
}

impl Iterator for CatChildren {
    type Item = CatNode;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        let node = CatNode::new(id);
        self.next = node.dom().next_sibling(id);
        Some(node)
    }
}

impl Element for CatNode {
    type Impl = SelectorImpl;

    fn opaque(&self) -> OpaqueElement {
        // Stylo wants a pointer-sized unique token; the node id (+1 so it is
        // never null) is unique for the life of the arena and never
        // dereferenced.
        let ptr = NonNull::new((self.ffi() as usize).wrapping_add(1) as *mut ())
            .expect("node ids are never usize::MAX");
        OpaqueElement::from_non_null_ptr(ptr)
    }

    fn parent_element(&self) -> Option<Self> {
        TElement::traversal_parent(self)
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        false
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        None
    }

    fn is_pseudo_element(&self) -> bool {
        false
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let dom = self.dom();
        let mut cur = dom.prev_sibling(self.id);
        while let Some(id) = cur {
            if dom.is_element(id) {
                return Some(CatNode::new(id));
            }
            cur = dom.prev_sibling(id);
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let dom = self.dom();
        let mut cur = dom.next_sibling(self.id);
        while let Some(id) = cur {
            if dom.is_element(id) {
                return Some(CatNode::new(id));
            }
            cur = dom.next_sibling(id);
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.dom().child_elements(self.id).next().map(CatNode::new)
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.element().is_some_and(|e| e.name.ns == ns!(html))
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.element().is_some_and(|e| e.name.local == *local_name)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element().is_some_and(|e| e.name.ns == *ns)
    }

    fn is_same_type(&self, other: &Self) -> bool {
        match (self.element(), other.element()) {
            (Some(a), Some(b)) => a.name.local == b.name.local && a.name.ns == b.name.ns,
            _ => false,
        }
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&GenericAtomIdent<NamespaceStaticSet>>,
        local_name: &GenericAtomIdent<LocalNameStaticSet>,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        let Some(el) = self.element() else {
            return false;
        };
        el.attrs.iter().any(|attr| {
            attr.name.local == local_name.0
                && match ns {
                    NamespaceConstraint::Any => true,
                    NamespaceConstraint::Specific(ns) => attr.name.ns == ns.0,
                }
                && operation.eval_str(&attr.value)
        })
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &NonTSPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let state = match self.table().slot(self.id) {
            Some(slot) => slot.state,
            // Selector queries run without style slots: derive the state.
            None => crate::engine::element_state(self.dom(), self.id),
        };
        match *pseudo_class {
            NonTSPseudoClass::Active => state.contains(ElementState::ACTIVE),
            NonTSPseudoClass::AnyLink => state.intersects(ElementState::VISITED_OR_UNVISITED),
            NonTSPseudoClass::Checked => state.contains(ElementState::CHECKED),
            NonTSPseudoClass::Disabled => state.contains(ElementState::DISABLED),
            NonTSPseudoClass::Enabled => state.contains(ElementState::ENABLED),
            NonTSPseudoClass::Focus => state.contains(ElementState::FOCUS),
            NonTSPseudoClass::Hover => state.contains(ElementState::HOVER),
            NonTSPseudoClass::Link => state.contains(ElementState::UNVISITED),
            NonTSPseudoClass::Visited => state.contains(ElementState::VISITED),
            NonTSPseudoClass::Required => state.contains(ElementState::REQUIRED),
            NonTSPseudoClass::Optional => state.contains(ElementState::OPTIONAL_),
            NonTSPseudoClass::ReadOnly => state.contains(ElementState::READONLY),
            NonTSPseudoClass::ReadWrite => state.contains(ElementState::READWRITE),
            NonTSPseudoClass::Indeterminate => state.contains(ElementState::INDETERMINATE),
            NonTSPseudoClass::PlaceholderShown => state.contains(ElementState::PLACEHOLDER_SHOWN),
            NonTSPseudoClass::Target => state.contains(ElementState::URLTARGET),
            _ => false,
        }
    }

    fn match_pseudo_element(
        &self,
        _pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        false
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        let self_flags = flags.for_self();
        if !self_flags.is_empty()
            && let Some(slot) = self.table().slot(self.id)
        {
            slot.selector_flags
                .set(slot.selector_flags.get() | self_flags);
        }
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty()
            && let Some(parent) = TElement::traversal_parent(self)
            && let Some(slot) = parent.table().slot(parent.id)
        {
            slot.selector_flags
                .set(slot.selector_flags.get() | parent_flags);
        }
    }

    fn is_link(&self) -> bool {
        self.element().is_some_and(|e| {
            e.is_html() && matches!(&*e.name.local, "a" | "area" | "link") && e.has_attr("href")
        })
    }

    fn is_html_slot_element(&self) -> bool {
        false
    }

    fn has_id(&self, id: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        match self.table().slot(self.id) {
            Some(slot) => slot
                .id_atom
                .as_ref()
                .is_some_and(|own| case_sensitivity.eq_atom(own, id)),
            None => self
                .element()
                .and_then(|e| e.id())
                .is_some_and(|own| case_sensitivity.eq_atom(&Atom::from(own), id)),
        }
    }

    fn has_class(&self, search_name: &AtomIdent, case_sensitivity: CaseSensitivity) -> bool {
        self.element().is_some_and(|e| {
            e.classes().any(|class| {
                let atom = Atom::from(class);
                case_sensitivity.eq_atom(&atom, search_name)
            })
        })
    }

    fn imported_part(&self, _name: &AtomIdent) -> Option<AtomIdent> {
        None
    }

    fn is_part(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn is_empty(&self) -> bool {
        let dom = self.dom();
        dom.children(self.id).all(|c| match dom.kind(c) {
            NodeKind::Element(_) => false,
            NodeKind::Text(t) => t.is_empty(),
            _ => true,
        })
    }

    fn is_root(&self) -> bool {
        let dom = self.dom();
        dom.parent(self.id) == Some(dom.document())
    }

    fn has_custom_state(&self, _name: &AtomIdent) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, filter: &mut selectors::bloom::BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

impl TElement for CatNode {
    type ConcreteNode = CatNode;
    type TraversalChildrenIterator = CatChildren;

    fn as_node(&self) -> Self::ConcreteNode {
        *self
    }

    fn traversal_children(&self) -> LayoutIterator<Self::TraversalChildrenIterator> {
        LayoutIterator(CatChildren {
            next: self.dom().first_child(self.id),
        })
    }

    fn is_html_element(&self) -> bool {
        self.element().is_some_and(|e| e.name.ns == ns!(html))
    }

    fn is_mathml_element(&self) -> bool {
        self.element().is_some_and(|e| e.name.ns == ns!(mathml))
    }

    fn is_svg_element(&self) -> bool {
        self.element().is_some_and(|e| e.name.ns == ns!(svg))
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.slot().style_attribute.as_ref().map(Arc::borrow_arc)
    }

    fn state(&self) -> ElementState {
        self.slot().state
    }

    fn has_part_attr(&self) -> bool {
        false
    }

    fn exports_any_part(&self) -> bool {
        false
    }

    fn id(&self) -> Option<&Atom> {
        self.slot().id_atom.as_ref()
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        if let Some(el) = self.element() {
            for class in el.classes() {
                let atom = Atom::from(class);
                callback(AtomIdent::cast(&atom));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        if let Some(el) = self.element() {
            for attr in &el.attrs {
                callback(&GenericAtomIdent(attr.name.local.clone()));
            }
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        self.slot().dirty_descendants.load(Ordering::SeqCst)
    }

    fn has_snapshot(&self) -> bool {
        self.slot().has_snapshot.load(Ordering::SeqCst)
    }

    fn handled_snapshot(&self) -> bool {
        self.slot().snapshot_handled.load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.slot().snapshot_handled.store(true, Ordering::SeqCst);
    }

    unsafe fn set_dirty_descendants(&self) {
        self.slot().dirty_descendants.store(true, Ordering::SeqCst);
    }

    unsafe fn unset_dirty_descendants(&self) {
        self.slot().dirty_descendants.store(false, Ordering::SeqCst);
    }

    fn store_children_to_process(&self, _n: isize) {
        unimplemented!("only used by the parallel post-order traversal")
    }

    fn did_process_child(&self) -> isize {
        unimplemented!("only used by the parallel post-order traversal")
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        let slot = self.slot();
        slot.has_data.store(true, Ordering::SeqCst);
        slot.data.borrow_mut()
    }

    unsafe fn clear_data(&self) {
        let slot = self.slot();
        *slot.data.borrow_mut() = StyloElementData::default();
        slot.has_data.store(false, Ordering::SeqCst);
    }

    fn has_data(&self) -> bool {
        self.table()
            .slot(self.id)
            .is_some_and(|s| s.has_data.load(Ordering::SeqCst))
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        let slot = self.table().slot(self.id)?;
        if slot.has_data.load(Ordering::SeqCst) {
            Some(slot.data.borrow())
        } else {
            None
        }
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        let slot = self.table().slot(self.id)?;
        if slot.has_data.load(Ordering::SeqCst) {
            Some(slot.data.borrow_mut())
        } else {
            None
        }
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        false
    }

    fn has_animations(&self, _context: &SharedStyleContext) -> bool {
        false
    }

    fn has_css_animations(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn has_css_transitions(
        &self,
        _context: &SharedStyleContext,
        _pseudo_element: Option<PseudoElement>,
    ) -> bool {
        false
    }

    fn animation_rule(
        &self,
        _context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn transition_rule(
        &self,
        _context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        None
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        None
    }

    fn get_attr(&self, attr: &style::LocalName, ns: &style::Namespace) -> Option<String> {
        self.element()?
            .attrs
            .iter()
            .find(|a| a.name.local == attr.0 && a.name.ns == ns.0)
            .map(|a| a.value.clone())
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        None
    }

    fn match_element_lang(
        &self,
        _override_lang: Option<Option<style::selector_parser::AttrValue>>,
        _value: &style::selector_parser::Lang,
    ) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        let dom = self.dom();
        self.is_html_element_named("body")
            && dom
                .parent(self.id)
                .is_some_and(|p| dom.parent(p) == Some(dom.document()))
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        _hints: &mut V,
    ) where
        V: Push<ApplicableDeclarationBlock>,
    {
        // Legacy presentational attributes (width=, bgcolor=, align=...) map
        // to layout properties; they land with the layout crate.
    }

    fn local_name(&self) -> &LocalName {
        &self.element_data().name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.element_data().name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        Default::default()
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.slot().selector_flags.get().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self.slot().selector_flags.get();
        if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING
        } else {
            ElementSelectorFlags::empty()
        }
    }
}
