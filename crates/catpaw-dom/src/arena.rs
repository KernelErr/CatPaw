//! The node arena.
//!
//! One [`Dom`] holds every node of an engine thread in a single `SlotMap`.
//! Handles are generational ([`NodeId`]), so a stale handle fails safely
//! instead of aliasing a reused slot. Tree structure is stored as explicit
//! parent / sibling / child links on each node, which keeps insertion and
//! removal O(1) and lets traversals run without allocation.

use markup5ever::interface::QuirksMode;
use markup5ever::{LocalName, Namespace, QualName, ns};
use slotmap::{SlotMap, new_key_type};
use url::Url;

new_key_type! {
    /// Generational handle to a node in a [`Dom`].
    pub struct NodeId;
}

/// A content attribute. Values are stored as owned `String`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: QualName,
    pub value: String,
}

impl Attr {
    pub fn new(name: QualName, value: impl Into<String>) -> Self {
        Self {
            name,
            value: value.into(),
        }
    }

    /// An attribute in the null namespace, as written in HTML markup.
    pub fn html(local: &str, value: impl Into<String>) -> Self {
        Self::new(QualName::new(None, ns!(), LocalName::from(local)), value)
    }

    pub fn local(&self) -> &LocalName {
        &self.name.local
    }
}

/// Element-specific node data.
#[derive(Debug, Clone)]
pub struct ElementData {
    pub name: QualName,
    pub attrs: Vec<Attr>,
    /// For `<template>`: the DocumentFragment holding the template contents.
    pub template_contents: Option<NodeId>,
    /// Set by the parser for `<annotation-xml encoding="text/html|application/xhtml+xml">`.
    pub mathml_annotation_xml_integration_point: bool,
    /// The parser's "already started" flag for `<script>`.
    pub script_already_started: bool,
    /// Form owner assigned by the parser's form element pointer.
    pub form_owner: Option<NodeId>,
    /// What became of the element as a custom element.
    pub custom_element_state: CustomElementState,
    /// The `is` value of a customized built-in element.
    pub is_value: Option<String>,
    /// The shadow root attached to the element, if any.
    pub shadow_root: Option<NodeId>,
}

/// What became of an element as a custom element. The states are the DOM
/// Standard's, with "uncustomized" and "undefined" told apart by the
/// element's name instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CustomElementState {
    /// Not (yet) upgraded: ordinary, or waiting for a definition.
    #[default]
    Undefined,
    /// Its constructor threw.
    Failed,
    /// Upgraded or constructed, by the definition with this index in the
    /// page's registry.
    Custom(u32),
}

impl ElementData {
    pub fn new(name: QualName, attrs: Vec<Attr>) -> Self {
        Self {
            name,
            attrs,
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
            script_already_started: false,
            form_owner: None,
            custom_element_state: CustomElementState::Undefined,
            is_value: None,
            shadow_root: None,
        }
    }

    pub fn local_name(&self) -> &LocalName {
        &self.name.local
    }

    pub fn namespace(&self) -> &Namespace {
        &self.name.ns
    }

    pub fn is_html(&self) -> bool {
        self.name.ns == ns!(html)
    }

    /// Looks up an attribute in the null namespace by local name.
    pub fn attr(&self, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.ns.is_empty() && &*a.name.local == local)
            .map(|a| a.value.as_str())
    }

    /// Looks up a namespaced attribute (e.g. `xlink:href`).
    pub fn attr_ns(&self, ns: &Namespace, local: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name.ns == *ns && &*a.name.local == local)
            .map(|a| a.value.as_str())
    }

    pub fn has_attr(&self, local: &str) -> bool {
        self.attr(local).is_some()
    }

    /// Sets an attribute, replacing an existing one with the same qualified name.
    pub fn set_attr(&mut self, name: QualName, value: impl Into<String>) {
        let value = value.into();
        match self.attrs.iter_mut().find(|a| a.name == name) {
            Some(existing) => existing.value = value,
            None => self.attrs.push(Attr::new(name, value)),
        }
    }

    pub fn remove_attr(&mut self, local: &str) -> Option<Attr> {
        let idx = self
            .attrs
            .iter()
            .position(|a| a.name.ns.is_empty() && &*a.name.local == local)?;
        Some(self.attrs.remove(idx))
    }

    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// The tokens of the `class` attribute, split on ASCII whitespace.
    pub fn classes(&self) -> impl Iterator<Item = &str> {
        self.attr("class")
            .into_iter()
            .flat_map(|c| c.split_ascii_whitespace())
    }

    pub fn has_class(&self, class: &str) -> bool {
        self.classes().any(|c| c == class)
    }
}

/// Document-specific node data.
#[derive(Debug, Clone)]
pub struct DocumentData {
    pub quirks_mode: QuirksMode,
    pub url: Option<Url>,
    /// An XML document rather than an HTML document: names keep their case.
    pub is_xml: bool,
    /// The content type, where it is not `text/html`.
    pub content_type: Option<String>,
}

impl Default for DocumentData {
    fn default() -> Self {
        Self {
            quirks_mode: QuirksMode::NoQuirks,
            url: None,
            is_xml: false,
            content_type: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctypeData {
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

/// What a DocumentFragment node stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentKind {
    /// A plain `DocumentFragment`.
    Plain,
    /// The contents of a `<template>` element.
    TemplateContents { host: NodeId },
    /// A shadow root attached to `host`.
    ShadowRoot {
        host: NodeId,
        open: bool,
        delegates_focus: bool,
        clonable: bool,
        serializable: bool,
    },
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Document(DocumentData),
    Doctype(DoctypeData),
    Element(ElementData),
    Text(String),
    Comment(String),
    ProcessingInstruction { target: String, data: String },
    DocumentFragment(FragmentKind),
}

/// A node: tree links plus kind-specific data.
#[derive(Debug)]
pub struct Node {
    parent: Option<NodeId>,
    prev_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    /// The node document. A document is its own.
    owner: NodeId,
    pub kind: NodeKind,
}

impl Node {
    pub fn new(kind: NodeKind) -> Self {
        Self {
            parent: None,
            prev_sibling: None,
            next_sibling: None,
            first_child: None,
            last_child: None,
            owner: NodeId::default(),
            kind,
        }
    }

    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }
    pub fn prev_sibling(&self) -> Option<NodeId> {
        self.prev_sibling
    }
    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next_sibling
    }
    pub fn first_child(&self) -> Option<NodeId> {
        self.first_child
    }
    pub fn last_child(&self) -> Option<NodeId> {
        self.last_child
    }

    pub fn as_element(&self) -> Option<&ElementData> {
        match &self.kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_element_mut(&mut self) -> Option<&mut ElementData> {
        match &mut self.kind {
            NodeKind::Element(e) => Some(e),
            _ => None,
        }
    }

    pub fn is_element(&self) -> bool {
        matches!(self.kind, NodeKind::Element(_))
    }

    pub fn is_text(&self) -> bool {
        matches!(self.kind, NodeKind::Text(_))
    }

    pub fn as_text(&self) -> Option<&str> {
        match &self.kind {
            NodeKind::Text(t) => Some(t),
            _ => None,
        }
    }
}

/// A change to the tree structure, as logged for mutation observers. The
/// siblings are those next to `node` at the time of the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeChange {
    Inserted {
        parent: NodeId,
        node: NodeId,
        prev: Option<NodeId>,
        next: Option<NodeId>,
    },
    Removed {
        parent: NodeId,
        node: NodeId,
        prev: Option<NodeId>,
        next: Option<NodeId>,
    },
}

/// A write to the arena, as the journal keeps it for the caches derived
/// from the tree (styles, layout, indexes): see [`Dom::changes_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// `node`, with its subtree, was inserted into `parent`.
    Inserted { parent: NodeId, node: NodeId },
    /// `node`, with its subtree, was removed from `parent`.
    Removed { parent: NodeId, node: NodeId },
    /// The node's own data may have changed: an element's attributes,
    /// shadow root or template contents, a character data node's data, a
    /// document's URL or quirks mode. Bookkeeping that nothing derived
    /// from the tree reads (see [`Dom::set_script_already_started`]) is
    /// not logged.
    Data(NodeId),
    /// The detached subtree rooted at the node was freed.
    Freed(NodeId),
}

/// How many changes the journal keeps. Past that it starts over, and a
/// cache that last looked before then rebuilds from the tree, which is
/// what it would do for that many changes anyway.
const JOURNAL_LIMIT: usize = 4096;

/// The arena. See the module documentation.
#[derive(Debug)]
pub struct Dom {
    nodes: SlotMap<NodeId, Node>,
    document: NodeId,
    /// The number of changes ever logged: always `journal_base` plus the
    /// length of `journal`.
    version: u64,
    /// The most recent changes, oldest first.
    journal: Vec<Change>,
    /// The version before the first change in `journal`.
    journal_base: u64,
    /// The tree changes since the log was last taken, while logging is on.
    changes: Option<Vec<TreeChange>>,
}

impl Default for Dom {
    fn default() -> Self {
        Self::new()
    }
}

impl Dom {
    /// Creates an arena containing one empty Document node.
    pub fn new() -> Self {
        let mut nodes = SlotMap::with_key();
        let document = nodes.insert(Node::new(NodeKind::Document(DocumentData::default())));
        nodes[document].owner = document;
        Self {
            nodes,
            document,
            version: 0,
            journal: Vec::new(),
            journal_base: 0,
            changes: None,
        }
    }

    pub fn with_url(url: Option<Url>) -> Self {
        let mut dom = Self::new();
        dom.document_data_mut().url = url;
        dom
    }

    /// The Document node.
    pub fn document(&self) -> NodeId {
        self.document
    }

    /// A counter that moves with every change to the tree or to a node's
    /// data (each mutable access counts as one, whether or not it changed
    /// anything). Caches derived from the tree, such as live collections,
    /// compare it to know when to recompute; those that can tell which
    /// changes matter to them read the journal instead
    /// ([`Dom::changes_since`]). Bookkeeping writes that nothing derived
    /// from the tree reads leave it alone.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The changes made since the arena was at `version`, oldest first;
    /// `None` when the journal no longer reaches back that far (the caller
    /// then starts over from the tree as it is). The nodes named may have
    /// changed again, moved or been freed since: the journal says where to
    /// look, the tree says what is there now.
    pub fn changes_since(&self, version: u64) -> Option<&[Change]> {
        if version < self.journal_base || version > self.version {
            return None;
        }
        Some(&self.journal[(version - self.journal_base) as usize..])
    }

    fn log(&mut self, change: Change) {
        if self.journal.len() >= JOURNAL_LIMIT {
            self.journal.clear();
            self.journal_base = self.version;
        }
        self.journal.push(change);
        self.version += 1;
    }

    /// Turns the log of tree changes on or off. Turning it off discards
    /// what was logged.
    pub fn log_changes(&mut self, on: bool) {
        match (on, &self.changes) {
            (true, None) => self.changes = Some(Vec::new()),
            (false, Some(_)) => self.changes = None,
            _ => {}
        }
    }

    /// The tree changes logged since the last call.
    pub fn take_changes(&mut self) -> Vec<TreeChange> {
        self.changes
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// Number of live nodes in the arena (including detached ones).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The node, to change; the change is logged (see [`Change::Data`]).
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if !self.nodes.contains_key(id) {
            return None;
        }
        self.log(Change::Data(id));
        self.nodes.get_mut(id)
    }

    /// Panics if `id` is stale.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// The node, to change; the change is logged. Panics if `id` is stale.
    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        assert!(self.nodes.contains_key(id), "stale node id");
        self.log(Change::Data(id));
        &mut self.nodes[id]
    }

    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.nodes[id].kind
    }

    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        self.nodes.get(id).and_then(Node::as_element)
    }

    /// The element, to change; the change is logged.
    pub fn element_mut(&mut self, id: NodeId) -> Option<&mut ElementData> {
        if !self.is_element(id) {
            return None;
        }
        self.log(Change::Data(id));
        self.nodes.get_mut(id).and_then(Node::as_element_mut)
    }

    /// The bookkeeping fields of an element: written without logging a
    /// change or moving the version, since nothing derived from the tree
    /// (styles, layout, indexes, collections) reads them.
    fn bookkeeping_mut(&mut self, id: NodeId) -> Option<&mut ElementData> {
        self.nodes.get_mut(id).and_then(Node::as_element_mut)
    }

    /// Sets the "already started" flag of a `<script>` (bookkeeping: not
    /// logged).
    pub fn set_script_already_started(&mut self, id: NodeId, started: bool) {
        if let Some(el) = self.bookkeeping_mut(id) {
            el.script_already_started = started;
        }
    }

    /// Sets what became of an element as a custom element (bookkeeping:
    /// not logged, as long as no selector depends on it; `:defined` does
    /// not match yet).
    pub fn set_custom_element_state(&mut self, id: NodeId, state: CustomElementState) {
        if let Some(el) = self.bookkeeping_mut(id) {
            el.custom_element_state = state;
        }
    }

    /// Sets the `is` value of a customized built-in element (bookkeeping:
    /// not logged).
    pub fn set_is_value(&mut self, id: NodeId, is: Option<String>) {
        if let Some(el) = self.bookkeeping_mut(id) {
            el.is_value = is;
        }
    }

    pub fn is_element(&self, id: NodeId) -> bool {
        self.nodes.get(id).is_some_and(Node::is_element)
    }

    /// True if `id` is an HTML element with the given local name.
    pub fn is_html_element(&self, id: NodeId, local: &str) -> bool {
        self.element(id)
            .is_some_and(|e| e.is_html() && &*e.name.local == local)
    }

    pub fn document_data(&self) -> &DocumentData {
        match &self.nodes[self.document].kind {
            NodeKind::Document(d) => d,
            _ => unreachable!("document node is always a Document"),
        }
    }

    pub fn document_data_mut(&mut self) -> &mut DocumentData {
        self.log(Change::Data(self.document));
        match &mut self.nodes[self.document].kind {
            NodeKind::Document(d) => d,
            _ => unreachable!("document node is always a Document"),
        }
    }

    pub fn quirks_mode(&self) -> QuirksMode {
        self.document_data().quirks_mode
    }

    pub fn url(&self) -> Option<&Url> {
        self.document_data().url.as_ref()
    }

    // ---- creation -------------------------------------------------------

    /// Creates a detached node whose node document is the arena's own
    /// document.
    pub fn create(&mut self, kind: NodeKind) -> NodeId {
        let id = self.nodes.insert(Node::new(kind));
        self.nodes[id].owner = self.document;
        id
    }

    /// Creates another document in the arena, such as the one `DOMParser`
    /// returns. Unlike the arena's own document it belongs to no page.
    pub fn create_document(&mut self, data: DocumentData) -> NodeId {
        let id = self.nodes.insert(Node::new(NodeKind::Document(data)));
        self.nodes[id].owner = id;
        id
    }

    /// The node document of `id`. A document is its own.
    pub fn owner_document(&self, id: NodeId) -> NodeId {
        self.nodes[id].owner
    }

    /// Makes `document` the node document of `id`, of its descendants and
    /// of the contents of the templates among them.
    pub fn adopt_subtree(&mut self, id: NodeId, document: NodeId) {
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let node = &mut self.nodes[n];
            match &node.kind {
                NodeKind::Document(_) => {}
                NodeKind::Element(el) => {
                    node.owner = document;
                    stack.extend(el.template_contents);
                    stack.extend(el.shadow_root);
                }
                _ => node.owner = document,
            }
            stack.extend(self.children(n));
        }
    }

    /// The data of the document `id`, if it is a document.
    pub fn document_data_of(&self, id: NodeId) -> Option<&DocumentData> {
        match &self.nodes.get(id)?.kind {
            NodeKind::Document(d) => Some(d),
            _ => None,
        }
    }

    pub fn document_data_of_mut(&mut self, id: NodeId) -> Option<&mut DocumentData> {
        if !matches!(self.nodes.get(id)?.kind, NodeKind::Document(_)) {
            return None;
        }
        self.log(Change::Data(id));
        match &mut self.nodes.get_mut(id)?.kind {
            NodeKind::Document(d) => Some(d),
            _ => None,
        }
    }

    /// Whether `id` is in a document tree: that of the arena's own document
    /// or of another one. See [`Dom::is_connected`] for the former alone.
    pub fn in_document_tree(&self, id: NodeId) -> bool {
        matches!(
            self.nodes[self.shadow_including_root(id)].kind,
            NodeKind::Document(_)
        )
    }

    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attr>) -> NodeId {
        self.create(NodeKind::Element(ElementData::new(name, attrs)))
    }

    /// Creates an element in the HTML namespace.
    pub fn create_html_element(&mut self, local: &str, attrs: Vec<Attr>) -> NodeId {
        self.create_element(
            QualName::new(None, ns!(html), LocalName::from(local)),
            attrs,
        )
    }

    pub fn create_text(&mut self, text: impl Into<String>) -> NodeId {
        self.create(NodeKind::Text(text.into()))
    }

    pub fn create_comment(&mut self, text: impl Into<String>) -> NodeId {
        self.create(NodeKind::Comment(text.into()))
    }

    pub fn create_processing_instruction(
        &mut self,
        target: impl Into<String>,
        data: impl Into<String>,
    ) -> NodeId {
        self.create(NodeKind::ProcessingInstruction {
            target: target.into(),
            data: data.into(),
        })
    }

    pub fn create_fragment(&mut self, kind: FragmentKind) -> NodeId {
        self.create(NodeKind::DocumentFragment(kind))
    }

    // ---- links ----------------------------------------------------------

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].parent
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].first_child
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].last_child
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].next_sibling
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        self.nodes[id].prev_sibling
    }

    pub fn has_children(&self, id: NodeId) -> bool {
        self.nodes[id].first_child.is_some()
    }

    /// The children of `id`, in tree order.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.nodes[id].first_child,
        }
    }

    pub fn child_elements(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id).filter(move |&c| self.is_element(c))
    }

    /// The parent element of `id`, if the parent is an element.
    pub fn parent_element(&self, id: NodeId) -> Option<NodeId> {
        self.parent(id).filter(|&p| self.is_element(p))
    }

    /// Pre-order traversal of the subtree rooted at `id`, including `id`.
    /// Template contents and shadow trees are separate trees and are not visited.
    pub fn traverse(&self, id: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root: id,
            next: Some(id),
        }
    }

    /// Pre-order traversal of the descendants of `id`, excluding `id`.
    pub fn descendants(&self, id: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root: id,
            next: self.nodes[id].first_child,
        }
    }

    /// Ancestors of `id`, nearest first, excluding `id`.
    pub fn ancestors(&self, id: NodeId) -> Ancestors<'_> {
        Ancestors {
            dom: self,
            next: self.nodes[id].parent,
        }
    }

    /// The root of the tree containing `id` (the node without a parent).
    pub fn root_of(&self, id: NodeId) -> NodeId {
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            cur = p;
        }
        cur
    }

    /// The root of the tree containing `id`, crossing from a shadow root to
    /// its host: the shadow-including root.
    pub fn shadow_including_root(&self, id: NodeId) -> NodeId {
        let mut root = self.root_of(id);
        while let NodeKind::DocumentFragment(FragmentKind::ShadowRoot { host, .. }) =
            &self.nodes[root].kind
        {
            root = self.root_of(*host);
        }
        root
    }

    /// True if `id` is in the Document, shadow trees included.
    pub fn is_connected(&self, id: NodeId) -> bool {
        self.shadow_including_root(id) == self.document
    }

    /// The children an element is rendered with: those of its shadow tree
    /// if it has one, where a `<slot>` stands for the host's children it is
    /// assigned (by `slot` attribute, or all the unslotted ones for the
    /// default slot) and falls back to its own; otherwise its own children.
    pub fn rendered_children(&self, id: NodeId) -> Vec<NodeId> {
        let Some(el) = self.element(id) else {
            return self.children(id).collect();
        };
        if let Some(shadow) = el.shadow_root {
            return self.children(shadow).collect();
        }
        if el.is_html() && &*el.name.local == "slot" {
            let assigned = self.assigned_nodes(id);
            if !assigned.is_empty() {
                return assigned;
            }
        }
        self.children(id).collect()
    }

    /// The host's children assigned to a `slot` element in a shadow tree:
    /// elements whose `slot` attribute names it, and for the default slot
    /// the elements without one and the text nodes. Empty for a slot
    /// outside a shadow tree.
    pub fn assigned_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let Some(el) = self.element(slot) else {
            return Vec::new();
        };
        if !el.is_html() || &*el.name.local != "slot" {
            return Vec::new();
        }
        let root = self.root_of(slot);
        let NodeKind::DocumentFragment(FragmentKind::ShadowRoot { host, .. }) =
            &self.nodes[root].kind
        else {
            return Vec::new();
        };
        let name = el.attr("name").unwrap_or_default();
        self.children(*host)
            .filter(|&c| match &self.nodes[c].kind {
                NodeKind::Element(child) => child.attr("slot").unwrap_or_default() == name,
                NodeKind::Text(_) => name.is_empty(),
                _ => false,
            })
            .collect()
    }

    /// The slot in the parent's shadow tree that `node` is assigned to.
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        let parent = self.parent(node)?;
        let shadow = self.element(parent)?.shadow_root?;
        let name = match &self.nodes[node].kind {
            NodeKind::Element(el) => el.attr("slot").unwrap_or_default(),
            NodeKind::Text(_) => "",
            _ => return None,
        };
        self.descendants(shadow).find(|&n| {
            self.element(n).is_some_and(|el| {
                el.is_html()
                    && &*el.name.local == "slot"
                    && el.attr("name").unwrap_or_default() == name
            })
        })
    }

    /// `id` and its descendants in tree order, with the shadow trees of the
    /// elements among them visited after their hosts.
    pub fn shadow_including_descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            out.push(n);
            if let NodeKind::Element(el) = &self.nodes[n].kind
                && let Some(shadow) = el.shadow_root
            {
                stack.push(shadow);
            }
            let children: Vec<NodeId> = self.children(n).collect();
            stack.extend(children.into_iter().rev());
        }
        out
    }

    /// The next node in pre-order after `id` within the subtree rooted at `root`.
    pub fn next_in_preorder(&self, id: NodeId, root: NodeId) -> Option<NodeId> {
        if let Some(c) = self.nodes[id].first_child {
            return Some(c);
        }
        let mut cur = id;
        loop {
            if cur == root {
                return None;
            }
            if let Some(s) = self.nodes[cur].next_sibling {
                return Some(s);
            }
            cur = self.nodes[cur].parent?;
        }
    }

    // ---- mutation -------------------------------------------------------

    /// Appends `child` as the last child of `parent`, detaching it from any
    /// previous parent first.
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.insert_before(parent, child, None);
    }

    /// Inserts `child` into `parent` before `reference` (or at the end when
    /// `reference` is `None`). `reference` must be a child of `parent`.
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, reference: Option<NodeId>) {
        debug_assert_ne!(parent, child, "cannot insert a node into itself");
        debug_assert!(
            reference.is_none_or(|r| self.nodes[r].parent == Some(parent)),
            "reference node is not a child of parent"
        );
        self.detach(child);
        self.log(Change::Inserted {
            parent,
            node: child,
        });
        match reference {
            None => {
                let prev = self.nodes[parent].last_child;
                {
                    let c = &mut self.nodes[child];
                    c.parent = Some(parent);
                    c.prev_sibling = prev;
                    c.next_sibling = None;
                }
                match prev {
                    Some(p) => self.nodes[p].next_sibling = Some(child),
                    None => self.nodes[parent].first_child = Some(child),
                }
                self.nodes[parent].last_child = Some(child);
                if let Some(log) = &mut self.changes {
                    log.push(TreeChange::Inserted {
                        parent,
                        node: child,
                        prev,
                        next: None,
                    });
                }
            }
            Some(reference) => {
                let prev = self.nodes[reference].prev_sibling;
                {
                    let c = &mut self.nodes[child];
                    c.parent = Some(parent);
                    c.prev_sibling = prev;
                    c.next_sibling = Some(reference);
                }
                self.nodes[reference].prev_sibling = Some(child);
                match prev {
                    Some(p) => self.nodes[p].next_sibling = Some(child),
                    None => self.nodes[parent].first_child = Some(child),
                }
                if let Some(log) = &mut self.changes {
                    log.push(TreeChange::Inserted {
                        parent,
                        node: child,
                        prev,
                        next: Some(reference),
                    });
                }
            }
        }
    }

    /// Removes `id` from its parent. The node and its subtree stay alive in
    /// the arena until [`Dom::remove_subtree`] frees them.
    pub fn detach(&mut self, id: NodeId) {
        let (parent, prev, next) = {
            let n = &self.nodes[id];
            (n.parent, n.prev_sibling, n.next_sibling)
        };
        let Some(parent) = parent else {
            return;
        };
        self.log(Change::Removed { parent, node: id });
        if let Some(log) = &mut self.changes {
            log.push(TreeChange::Removed {
                parent,
                node: id,
                prev,
                next,
            });
        }
        match prev {
            Some(p) => self.nodes[p].next_sibling = next,
            None => self.nodes[parent].first_child = next,
        }
        match next {
            Some(n) => self.nodes[n].prev_sibling = prev,
            None => self.nodes[parent].last_child = prev,
        }
        let n = &mut self.nodes[id];
        n.parent = None;
        n.prev_sibling = None;
        n.next_sibling = None;
    }

    /// Moves all children of `from` to the end of `to`, preserving order.
    pub fn reparent_children(&mut self, from: NodeId, to: NodeId) {
        let children: Vec<NodeId> = self.children(from).collect();
        for c in children {
            self.append_child(to, c);
        }
    }

    /// Detaches `id` and frees it together with its whole subtree
    /// (including template contents).
    pub fn remove_subtree(&mut self, id: NodeId) {
        self.detach(id);
        self.log(Change::Freed(id));
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Some(node) = self.nodes.remove(n) {
                let mut c = node.first_child;
                while let Some(child) = c {
                    stack.push(child);
                    c = self.nodes.get(child).and_then(|x| x.next_sibling);
                }
                if let NodeKind::Element(el) = node.kind {
                    stack.extend(el.template_contents);
                    stack.extend(el.shadow_root);
                }
            }
        }
    }

    /// Deep-clones `id` (including template contents) into a new detached
    /// subtree and returns its root. Parser flags such as "already started"
    /// and the form owner are not copied, per the DOM cloning steps.
    pub fn clone_subtree(&mut self, id: NodeId) -> NodeId {
        let (kind, template_contents) = match &self.nodes[id].kind {
            NodeKind::Element(el) => {
                let mut cloned = ElementData::new(el.name.clone(), el.attrs.clone());
                cloned.mathml_annotation_xml_integration_point =
                    el.mathml_annotation_xml_integration_point;
                cloned.is_value = el.is_value.clone();
                (NodeKind::Element(cloned), el.template_contents)
            }
            other => (other.clone(), None),
        };
        // A clone belongs to the document of its original; a cloned
        // document is its own.
        let is_document = matches!(kind, NodeKind::Document(_));
        let owner = self.nodes[id].owner;
        let new = self.create(kind);
        self.nodes[new].owner = if is_document { new } else { owner };
        if let Some(contents) = template_contents {
            let new_contents = self.create_fragment(FragmentKind::TemplateContents { host: new });
            self.nodes[new_contents].owner = owner;
            let kids: Vec<NodeId> = self.children(contents).collect();
            for c in kids {
                let cc = self.clone_subtree(c);
                self.append_child(new_contents, cc);
            }
            self.element_mut(new).unwrap().template_contents = Some(new_contents);
        }
        let kids: Vec<NodeId> = self.children(id).collect();
        for c in kids {
            let cc = self.clone_subtree(c);
            self.append_child(new, cc);
        }
        if is_document {
            self.adopt_subtree(new, new);
        }
        new
    }

    // ---- text -----------------------------------------------------------

    /// The `textContent` of `id`: own data for character data nodes, the
    /// concatenated text descendants for elements and fragments.
    pub fn text_content(&self, id: NodeId) -> String {
        match &self.nodes[id].kind {
            NodeKind::Text(s) | NodeKind::Comment(s) => s.clone(),
            NodeKind::ProcessingInstruction { data, .. } => data.clone(),
            NodeKind::Document(_) | NodeKind::Doctype(_) => String::new(),
            NodeKind::Element(_) | NodeKind::DocumentFragment(_) => {
                let mut out = String::new();
                for d in self.descendants(id) {
                    if let NodeKind::Text(s) = &self.nodes[d].kind {
                        out.push_str(s);
                    }
                }
                out
            }
        }
    }

    /// Looks up an attribute on an element node.
    pub fn attr(&self, id: NodeId, local: &str) -> Option<&str> {
        self.element(id).and_then(|e| e.attr(local))
    }

    /// The local name of an element node.
    pub fn local_name(&self, id: NodeId) -> Option<&LocalName> {
        self.element(id).map(ElementData::local_name)
    }
}

/// Iterator over the children of a node.
pub struct Children<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.dom.nodes[cur].next_sibling;
        Some(cur)
    }
}

/// Pre-order iterator over a subtree.
pub struct Descendants<'a> {
    dom: &'a Dom,
    root: NodeId,
    next: Option<NodeId>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.dom.next_in_preorder(cur, self.root);
        Some(cur)
    }
}

/// Iterator over ancestors, nearest first.
pub struct Ancestors<'a> {
    dom: &'a Dom,
    next: Option<NodeId>,
}

impl Iterator for Ancestors<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.next?;
        self.next = self.dom.nodes[cur].parent;
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(dom: &mut Dom, local: &str) -> NodeId {
        dom.create_html_element(local, vec![])
    }

    #[test]
    fn insert_and_detach_keep_links_consistent() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let a = p(&mut dom, "a");
        let b = p(&mut dom, "b");
        let c = p(&mut dom, "c");
        dom.append_child(doc, a);
        dom.append_child(doc, c);
        dom.insert_before(doc, b, Some(c));
        let order: Vec<_> = dom.children(doc).collect();
        assert_eq!(order, vec![a, b, c]);
        assert_eq!(dom.prev_sibling(b), Some(a));
        assert_eq!(dom.next_sibling(b), Some(c));

        dom.detach(b);
        assert_eq!(dom.children(doc).collect::<Vec<_>>(), vec![a, c]);
        assert_eq!(dom.next_sibling(a), Some(c));
        assert_eq!(dom.prev_sibling(c), Some(a));
        assert_eq!(dom.parent(b), None);
        assert!(dom.contains(b), "detached nodes stay in the arena");

        dom.detach(a);
        dom.detach(c);
        assert_eq!(dom.first_child(doc), None);
        assert_eq!(dom.last_child(doc), None);
    }

    #[test]
    fn preorder_traversal_and_text_content() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let root = p(&mut dom, "div");
        let child = p(&mut dom, "span");
        let t1 = dom.create_text("hello ");
        let t2 = dom.create_text("world");
        dom.append_child(doc, root);
        dom.append_child(root, t1);
        dom.append_child(root, child);
        dom.append_child(child, t2);
        let order: Vec<_> = dom.traverse(root).collect();
        assert_eq!(order, vec![root, t1, child, t2]);
        assert_eq!(dom.descendants(root).count(), 3);
        assert_eq!(dom.text_content(root), "hello world");
        assert_eq!(
            dom.ancestors(t2).collect::<Vec<_>>(),
            vec![child, root, doc]
        );
        assert!(dom.is_connected(t2));
    }

    #[test]
    fn the_journal_logs_changes_and_skips_bookkeeping() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let script = p(&mut dom, "script");
        let div = p(&mut dom, "div");
        let start = dom.version();
        // Creating detached nodes changes nothing anyone derives.
        assert_eq!(dom.changes_since(start), Some(&[][..]));

        dom.append_child(doc, div);
        dom.append_child(div, script);
        dom.element_mut(div)
            .unwrap()
            .set_attr(QualName::new(None, ns!(), LocalName::from("class")), "x");
        let after_writes = dom.version();
        assert_eq!(
            dom.changes_since(start).unwrap(),
            &[
                Change::Inserted {
                    parent: doc,
                    node: div
                },
                Change::Inserted {
                    parent: div,
                    node: script
                },
                Change::Data(div),
            ]
        );

        // Bookkeeping that no cache reads is written without a trace.
        dom.set_script_already_started(script, true);
        dom.set_custom_element_state(div, CustomElementState::Failed);
        dom.set_is_value(div, Some("x-y".into()));
        assert_eq!(dom.version(), after_writes);
        assert!(dom.element(script).unwrap().script_already_started);
        assert_eq!(
            dom.element(div).unwrap().custom_element_state,
            CustomElementState::Failed
        );
        assert_eq!(dom.element(div).unwrap().is_value.as_deref(), Some("x-y"));

        // Moving a node logs both ends; freeing a subtree says so.
        dom.append_child(doc, script);
        dom.remove_subtree(div);
        assert_eq!(
            dom.changes_since(after_writes).unwrap(),
            &[
                Change::Removed {
                    parent: div,
                    node: script
                },
                Change::Inserted {
                    parent: doc,
                    node: script
                },
                Change::Removed {
                    parent: doc,
                    node: div
                },
                Change::Freed(div),
            ]
        );
        // Writes through a stale id are not changes.
        let before = dom.version();
        assert!(dom.element_mut(div).is_none());
        assert!(dom.get_mut(div).is_none());
        assert_eq!(dom.version(), before);
    }

    #[test]
    fn a_full_journal_starts_over() {
        let mut dom = Dom::new();
        let a = p(&mut dom, "a");
        let start = dom.version();
        for _ in 0..JOURNAL_LIMIT {
            dom.node_mut(a);
        }
        assert_eq!(
            dom.changes_since(start).map(<[Change]>::len),
            Some(JOURNAL_LIMIT)
        );
        dom.node_mut(a);
        assert_eq!(dom.changes_since(start), None, "too far back");
        let now = dom.version();
        assert_eq!(dom.changes_since(now), Some(&[][..]));
        assert_eq!(dom.changes_since(now - 1), Some(&[Change::Data(a)][..]));
        assert_eq!(
            dom.changes_since(now + 1),
            None,
            "not a version of this arena"
        );
    }

    #[test]
    fn remove_subtree_frees_nodes_and_template_contents() {
        let mut dom = Dom::new();
        let doc = dom.document();
        let tpl = p(&mut dom, "template");
        let contents = dom.create_fragment(FragmentKind::TemplateContents { host: tpl });
        dom.element_mut(tpl).unwrap().template_contents = Some(contents);
        let inner = p(&mut dom, "b");
        dom.append_child(contents, inner);
        dom.append_child(doc, tpl);
        let before = dom.len();
        dom.remove_subtree(tpl);
        assert_eq!(dom.len(), before - 3);
        assert!(!dom.contains(inner));
        assert_eq!(dom.first_child(doc), None);
    }
}
