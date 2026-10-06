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
}

impl Default for DocumentData {
    fn default() -> Self {
        Self {
            quirks_mode: QuirksMode::NoQuirks,
            url: None,
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
    ShadowRoot { host: NodeId, open: bool },
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

/// The arena. See the module documentation.
#[derive(Debug)]
pub struct Dom {
    nodes: SlotMap<NodeId, Node>,
    document: NodeId,
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
        Self { nodes, document }
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

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }

    /// Panics if `id` is stale.
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id]
    }

    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.nodes[id].kind
    }

    pub fn element(&self, id: NodeId) -> Option<&ElementData> {
        self.nodes.get(id).and_then(Node::as_element)
    }

    pub fn element_mut(&mut self, id: NodeId) -> Option<&mut ElementData> {
        self.nodes.get_mut(id).and_then(Node::as_element_mut)
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

    pub fn create(&mut self, kind: NodeKind) -> NodeId {
        self.nodes.insert(Node::new(kind))
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

    /// True if the Document is an inclusive ancestor of `id`.
    pub fn is_connected(&self, id: NodeId) -> bool {
        self.root_of(id) == self.document
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
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Some(node) = self.nodes.remove(n) {
                let mut c = node.first_child;
                while let Some(child) = c {
                    stack.push(child);
                    c = self.nodes.get(child).and_then(|x| x.next_sibling);
                }
                if let NodeKind::Element(el) = node.kind
                    && let Some(t) = el.template_contents
                {
                    stack.push(t);
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
                (NodeKind::Element(cloned), el.template_contents)
            }
            other => (other.clone(), None),
        };
        let new = self.create(kind);
        if let Some(contents) = template_contents {
            let new_contents = self.create_fragment(FragmentKind::TemplateContents { host: new });
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
