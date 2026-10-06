//! The `Node` tree: mutation algorithms with their validity checks
//! (<https://dom.spec.whatwg.org/#mutation-algorithms>) and the `Node`,
//! `CharacterData`, `Text`, `Comment`, `DocumentFragment` and `DocumentType`
//! interfaces plus the child/parent node mixins.

use catpaw_dom::{Dom, ElementData, FragmentKind, NodeId, NodeKind};
use catpaw_js::{Exception, Fallible, ObjectId};
use catpaw_style::Selectors;

use crate::collections::{self, ListSource};
use crate::generated::{self as web, NodeOrString};
use crate::page::Cx;
use crate::{Web, mutation_observer, scripting};

pub const ELEMENT_NODE: u16 = 1;
pub const ATTRIBUTE_NODE: u16 = 2;
pub const TEXT_NODE: u16 = 3;
pub const PROCESSING_INSTRUCTION_NODE: u16 = 7;
pub const COMMENT_NODE: u16 = 8;
pub const DOCUMENT_NODE: u16 = 9;
pub const DOCUMENT_TYPE_NODE: u16 = 10;
pub const DOCUMENT_FRAGMENT_NODE: u16 = 11;

const DOCUMENT_POSITION_DISCONNECTED: u16 = 0x01;
const DOCUMENT_POSITION_PRECEDING: u16 = 0x02;
const DOCUMENT_POSITION_FOLLOWING: u16 = 0x04;
const DOCUMENT_POSITION_CONTAINS: u16 = 0x08;
const DOCUMENT_POSITION_CONTAINED_BY: u16 = 0x10;
const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: u16 = 0x20;

/// The qualified name of an element (`prefix:local`).
/// The URL that relative URLs in `node` are resolved against: the base URL
/// of its document. `None` for a document without a URL.
pub(crate) fn base_url(cx: &Cx<'_>, node: NodeId) -> Option<url::Url> {
    let dom = cx.dom();
    let document = dom.owner_document(node);
    if document == dom.document() {
        return Some(cx.page.base_url());
    }
    dom.document_data_of(document).and_then(|d| d.url.clone())
}

/// Whether `node`'s document is an HTML document (rather than an XML one).
pub(crate) fn in_html_document(dom: &Dom, node: NodeId) -> bool {
    crate::document::is_html_document(dom, dom.owner_document(node))
}

pub fn qualified_name(el: &ElementData) -> String {
    match &el.name.prefix {
        Some(prefix) => format!("{}:{}", prefix, el.name.local),
        None => el.name.local.to_string(),
    }
}

/// The error for an id that no longer names a node.
fn stale() -> Exception {
    Exception::type_error("Illegal invocation")
}

/// Fails if `id` is not a live node.
pub(crate) fn check(cx: &Cx<'_>, id: NodeId) -> Fallible<()> {
    if cx.dom().contains(id) {
        Ok(())
    } else {
        Err(stale())
    }
}

// ---- UTF-16 helpers: DOM offsets count UTF-16 code units -----------------

pub(crate) fn utf16_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// Replaces `count` code units of `data` at `offset` with `replacement`.
fn replace_utf16(data: &str, offset: u32, count: u32, replacement: &str) -> Fallible<String> {
    let units: Vec<u16> = data.encode_utf16().collect();
    let length = units.len() as u32;
    if offset > length {
        return Err(Exception::index_size(
            "The offset is larger than the data's length",
        ));
    }
    let end = offset.saturating_add(count).min(length);
    let mut out: Vec<u16> = Vec::with_capacity(units.len() + replacement.len());
    out.extend_from_slice(&units[..offset as usize]);
    out.extend(replacement.encode_utf16());
    out.extend_from_slice(&units[end as usize..]);
    Ok(String::from_utf16_lossy(&out))
}

fn substring_utf16(data: &str, offset: u32, count: u32) -> Fallible<String> {
    let units: Vec<u16> = data.encode_utf16().collect();
    let length = units.len() as u32;
    if offset > length {
        return Err(Exception::index_size(
            "The offset is larger than the data's length",
        ));
    }
    let end = offset.saturating_add(count).min(length);
    Ok(String::from_utf16_lossy(
        &units[offset as usize..end as usize],
    ))
}

// ---- character data -------------------------------------------------------

pub(crate) fn char_data(dom: &Dom, id: NodeId) -> Option<&str> {
    match dom.get(id).map(|n| &n.kind) {
        Some(NodeKind::Text(s) | NodeKind::Comment(s)) => Some(s),
        Some(NodeKind::ProcessingInstruction { data, .. }) => Some(data),
        _ => None,
    }
}

pub(crate) fn set_char_data(cx: &Cx<'_>, id: NodeId, value: String) -> Fallible<()> {
    let mut dom = cx.dom_mut();
    let old = match dom.get_mut(id).map(|n| &mut n.kind) {
        Some(NodeKind::Text(s) | NodeKind::Comment(s)) => std::mem::replace(s, value),
        Some(NodeKind::ProcessingInstruction { data, .. }) => std::mem::replace(data, value),
        _ => return Err(stale()),
    };
    drop(dom);
    crate::mutation_observer::queue_character_data(cx.page, id, &old);
    scripting::character_data_changed(cx, id);
    Ok(())
}

fn data_of(cx: &Cx<'_>, id: NodeId) -> Fallible<String> {
    char_data(&cx.dom(), id)
        .map(str::to_string)
        .ok_or_else(stale)
}

// ---- validity -------------------------------------------------------------

fn hierarchy(message: &str) -> Exception {
    Exception::hierarchy_request(message)
}

/// Whether `node` is `other` or one of its ancestors, crossing from a
/// template's contents (or a shadow root) to its host.
fn is_host_including_inclusive_ancestor(dom: &Dom, node: NodeId, other: NodeId) -> bool {
    let mut cur = other;
    loop {
        if cur == node {
            return true;
        }
        match dom.parent(cur) {
            Some(p) => cur = p,
            None => match dom.kind(cur) {
                NodeKind::DocumentFragment(
                    FragmentKind::TemplateContents { host } | FragmentKind::ShadowRoot { host, .. },
                ) if dom.contains(*host) => cur = *host,
                _ => return false,
            },
        }
    }
}

fn is_doctype(dom: &Dom, id: NodeId) -> bool {
    matches!(dom.kind(id), NodeKind::Doctype(_))
}

fn following_siblings(dom: &Dom, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(dom.next_sibling(id), move |&n| dom.next_sibling(n))
}

fn preceding_siblings(dom: &Dom, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(dom.prev_sibling(id), move |&n| dom.prev_sibling(n))
}

/// The checks shared by insertion and replacement. `child` is the reference
/// child (insert) or the child being replaced (`replacing`).
fn ensure_validity(
    dom: &Dom,
    node: NodeId,
    parent: NodeId,
    child: Option<NodeId>,
    replacing: bool,
) -> Fallible<()> {
    if !dom.contains(node) || !dom.contains(parent) || child.is_some_and(|c| !dom.contains(c)) {
        return Err(stale());
    }
    let parent_is_document = matches!(dom.kind(parent), NodeKind::Document(_));
    if !matches!(
        dom.kind(parent),
        NodeKind::Document(_) | NodeKind::DocumentFragment(_) | NodeKind::Element(_)
    ) {
        return Err(hierarchy("This node type does not support children"));
    }
    if is_host_including_inclusive_ancestor(dom, node, parent) {
        return Err(hierarchy("The new child element contains the parent"));
    }
    if let Some(child) = child
        && dom.parent(child) != Some(parent)
    {
        return Err(Exception::not_found(
            "The reference node is not a child of this node",
        ));
    }
    match dom.kind(node) {
        NodeKind::Document(_) => {
            return Err(hierarchy("A document cannot be inserted into another node"));
        }
        NodeKind::Text(_) if parent_is_document => {
            return Err(hierarchy("A text node cannot be a child of a document"));
        }
        NodeKind::Doctype(_) if !parent_is_document => {
            return Err(hierarchy("A doctype can only be a child of a document"));
        }
        _ => {}
    }
    if !parent_is_document {
        return Ok(());
    }

    let other_element_child = dom
        .child_elements(parent)
        .any(|e| !(replacing && Some(e) == child));
    let doctype_follows =
        child.is_some_and(|c| following_siblings(dom, c).any(|s| is_doctype(dom, s)));
    let element_error = || hierarchy("A document can have only one element child");
    match dom.kind(node) {
        NodeKind::DocumentFragment(_) => {
            let elements = dom.child_elements(node).count();
            let has_text = dom.children(node).any(|c| dom.node(c).is_text());
            if elements > 1 || has_text {
                return Err(element_error());
            }
            if elements == 1
                && (other_element_child
                    || (!replacing && child.is_some_and(|c| is_doctype(dom, c)))
                    || doctype_follows)
            {
                return Err(element_error());
            }
        }
        NodeKind::Element(_) => {
            if other_element_child
                || (!replacing && child.is_some_and(|c| is_doctype(dom, c)))
                || doctype_follows
            {
                return Err(element_error());
            }
        }
        NodeKind::Doctype(_) => {
            let other_doctype = dom
                .children(parent)
                .any(|c| is_doctype(dom, c) && !(replacing && Some(c) == child));
            let element_precedes =
                child.is_some_and(|c| preceding_siblings(dom, c).any(|s| dom.is_element(s)));
            let no_child_but_element =
                !replacing && child.is_none() && dom.child_elements(parent).next().is_some();
            if other_doctype || element_precedes || no_child_but_element {
                return Err(hierarchy(
                    "A document can have only one doctype, before its element",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

// ---- mutation -------------------------------------------------------------

/// The nodes inserting `node` puts in the tree: a fragment stands for its
/// children.
fn nodes_of(dom: &Dom, node: NodeId) -> Vec<NodeId> {
    if matches!(dom.kind(node), NodeKind::DocumentFragment(_)) {
        dom.children(node).collect()
    } else {
        vec![node]
    }
}

/// <https://dom.spec.whatwg.org/#concept-node-insert>, without the validity
/// checks: inserts `node` into `parent` before `child`. With
/// `suppress_observers` the caller reports the insertion to mutation
/// observers itself.
pub(crate) fn insert(
    cx: &mut Cx<'_>,
    node: NodeId,
    parent: NodeId,
    child: Option<NodeId>,
    suppress_observers: bool,
) {
    let (is_fragment, nodes) = {
        let dom = cx.dom();
        let is_fragment = matches!(dom.kind(node), NodeKind::DocumentFragment(_));
        (is_fragment, nodes_of(&dom, node))
    };
    if nodes.is_empty() {
        return;
    }
    if is_fragment {
        for &n in &nodes {
            remove(cx, n, true);
        }
        mutation_observer::queue_child_list(cx.page, node, &[], &nodes, None, None);
    } else {
        // The node leaves the tree it was in.
        remove(cx, node, false);
    }
    let previous = {
        let mut dom = cx.dom_mut();
        let previous = match child {
            Some(child) => dom.prev_sibling(child),
            None => dom.last_child(parent),
        };
        let document = dom.owner_document(parent);
        for &n in &nodes {
            dom.insert_before(parent, n, child);
            if dom.owner_document(n) != document {
                dom.adopt_subtree(n, document);
            }
        }
        previous
    };
    if !suppress_observers {
        mutation_observer::queue_child_list(cx.page, parent, &nodes, &[], previous, child);
    }
    scripting::nodes_inserted(cx, parent, &nodes);
}

/// <https://dom.spec.whatwg.org/#concept-node-pre-insert>
pub(crate) fn pre_insert(
    cx: &mut Cx<'_>,
    node: NodeId,
    parent: NodeId,
    child: Option<NodeId>,
) -> Fallible<NodeId> {
    let reference = {
        let dom = cx.dom();
        ensure_validity(&dom, node, parent, child, false)?;
        if child == Some(node) {
            dom.next_sibling(node)
        } else {
            child
        }
    };
    insert(cx, node, parent, reference, false);
    Ok(node)
}

pub(crate) fn append(cx: &mut Cx<'_>, node: NodeId, parent: NodeId) -> Fallible<NodeId> {
    pre_insert(cx, node, parent, None)
}

/// <https://dom.spec.whatwg.org/#concept-node-remove>: removes `node` from
/// its parent, if it has one, keeping it (and its subtree) alive. With
/// `suppress_observers` the caller reports the removal to mutation
/// observers itself.
pub(crate) fn remove(cx: &mut Cx<'_>, node: NodeId, suppress_observers: bool) {
    if cx.dom().contains(node) && cx.dom().parent(node).is_some() {
        crate::traversal::before_removal(cx.page, node);
    }
    let (parent, previous, next) = {
        let mut dom = cx.dom_mut();
        if !dom.contains(node) {
            return;
        }
        let Some(parent) = dom.parent(node) else {
            return;
        };
        let siblings = (dom.prev_sibling(node), dom.next_sibling(node));
        dom.detach(node);
        (parent, siblings.0, siblings.1)
    };
    crate::stylesheets::subtree_removed(cx.page, node);
    if mutation_observer::active(cx.page) {
        mutation_observer::node_removed(cx.page, node, parent);
        if !suppress_observers {
            mutation_observer::queue_child_list(cx.page, parent, &[], &[node], previous, next);
        }
    }
}

/// <https://dom.spec.whatwg.org/#concept-node-replace>
pub(crate) fn replace(
    cx: &mut Cx<'_>,
    child: NodeId,
    node: NodeId,
    parent: NodeId,
) -> Fallible<NodeId> {
    let (reference, previous, nodes) = {
        let dom = cx.dom();
        ensure_validity(&dom, node, parent, Some(child), true)?;
        // `node` itself is on its way out of where it is.
        let next = dom.next_sibling(child);
        let reference = if next == Some(node) {
            dom.next_sibling(node)
        } else {
            next
        };
        let previous = dom.prev_sibling(child);
        let previous = if previous == Some(node) {
            dom.prev_sibling(node)
        } else {
            previous
        };
        (reference, previous, nodes_of(&dom, node))
    };
    remove(cx, child, true);
    insert(cx, node, parent, reference, true);
    mutation_observer::queue_child_list(cx.page, parent, &nodes, &[child], previous, reference);
    Ok(child)
}

/// <https://dom.spec.whatwg.org/#concept-node-replace-all>
pub(crate) fn replace_all(cx: &mut Cx<'_>, node: Option<NodeId>, parent: NodeId) {
    let (removed, added) = {
        let dom = cx.dom();
        let removed: Vec<NodeId> = dom.children(parent).collect();
        let added = node.map(|n| nodes_of(&dom, n)).unwrap_or_default();
        (removed, added)
    };
    for &child in &removed {
        remove(cx, child, true);
    }
    if let Some(node) = node {
        insert(cx, node, parent, None, true);
    }
    mutation_observer::queue_child_list(cx.page, parent, &added, &removed, None, None);
}

/// <https://dom.spec.whatwg.org/#string-replace-all>
pub(crate) fn string_replace_all(cx: &mut Cx<'_>, text: &str, parent: NodeId) {
    let node = (!text.is_empty()).then(|| cx.dom_mut().create_text(text));
    replace_all(cx, node, parent);
}

/// <https://dom.spec.whatwg.org/#converting-nodes-into-a-node>
pub(crate) fn convert_nodes(cx: &mut Cx<'_>, nodes: Vec<NodeOrString>) -> Fallible<NodeId> {
    let (ids, fragment) = {
        let mut dom = cx.dom_mut();
        let mut ids = Vec::with_capacity(nodes.len());
        for n in nodes {
            ids.push(match n {
                NodeOrString::Node(id) => {
                    if !dom.contains(id) {
                        return Err(stale());
                    }
                    id
                }
                NodeOrString::String(s) => dom.create_text(s),
            });
        }
        if ids.len() == 1 {
            return Ok(ids[0]);
        }
        (ids, dom.create_fragment(FragmentKind::Plain))
    };
    for id in ids {
        // Moving a node into the fragment takes it from its old parent.
        remove(cx, id, false);
        cx.dom_mut().append_child(fragment, id);
    }
    Ok(fragment)
}

/// Creates an element node, with the contents fragment a `<template>` needs.
pub(crate) fn create_element_node(dom: &mut Dom, name: catpaw_dom::QualName) -> NodeId {
    let is_template = name.ns == catpaw_dom::ns!(html) && &*name.local == "template";
    let id = dom.create_element(name, Vec::new());
    if is_template {
        let contents = dom.create_fragment(FragmentKind::TemplateContents { host: id });
        if let Some(el) = dom.element_mut(id) {
            el.template_contents = Some(contents);
        }
    }
    id
}

/// <https://dom.spec.whatwg.org/#concept-node-clone>
pub(crate) fn clone_node(cx: &Cx<'_>, node: NodeId, deep: bool) -> Fallible<NodeId> {
    let mut dom = cx.dom_mut();
    if !dom.contains(node) {
        return Err(stale());
    }
    if deep {
        return Ok(dom.clone_subtree(node));
    }
    if let NodeKind::Document(data) = dom.kind(node) {
        let data = data.clone();
        return Ok(dom.create_document(data));
    }
    let kind = match dom.kind(node) {
        NodeKind::Element(el) => {
            let mut cloned = ElementData::new(el.name.clone(), el.attrs.clone());
            cloned.mathml_annotation_xml_integration_point =
                el.mathml_annotation_xml_integration_point;
            NodeKind::Element(cloned)
        }
        NodeKind::DocumentFragment(_) => NodeKind::DocumentFragment(FragmentKind::Plain),
        other => other.clone(),
    };
    let is_template = matches!(&kind, NodeKind::Element(el)
        if el.is_html() && &*el.name.local == "template");
    let new = dom.create(kind);
    if is_template {
        let contents = dom.create_fragment(FragmentKind::TemplateContents { host: new });
        if let Some(el) = dom.element_mut(new) {
            el.template_contents = Some(contents);
        }
    }
    // A clone belongs to the document of its original.
    let document = dom.owner_document(node);
    dom.adopt_subtree(new, document);
    Ok(new)
}

fn nodes_equal(dom: &Dom, a: NodeId, b: NodeId) -> bool {
    let same = match (dom.kind(a), dom.kind(b)) {
        (NodeKind::Doctype(x), NodeKind::Doctype(y)) => x == y,
        (NodeKind::Element(x), NodeKind::Element(y)) => {
            x.name.ns == y.name.ns
                && x.name.prefix == y.name.prefix
                && x.name.local == y.name.local
                && x.attrs.len() == y.attrs.len()
                && x.attrs.iter().all(|attr| {
                    y.attrs.iter().any(|other| {
                        other.name.ns == attr.name.ns
                            && other.name.local == attr.name.local
                            && other.value == attr.value
                    })
                })
        }
        (NodeKind::Text(x), NodeKind::Text(y)) | (NodeKind::Comment(x), NodeKind::Comment(y)) => {
            x == y
        }
        (
            NodeKind::ProcessingInstruction {
                target: t1,
                data: d1,
            },
            NodeKind::ProcessingInstruction {
                target: t2,
                data: d2,
            },
        ) => t1 == t2 && d1 == d2,
        (NodeKind::Document(_), NodeKind::Document(_))
        | (NodeKind::DocumentFragment(_), NodeKind::DocumentFragment(_)) => true,
        _ => false,
    };
    if !same {
        return false;
    }
    let mut x = dom.first_child(a);
    let mut y = dom.first_child(b);
    loop {
        match (x, y) {
            (None, None) => return true,
            (Some(p), Some(q)) => {
                if !nodes_equal(dom, p, q) {
                    return false;
                }
                x = dom.next_sibling(p);
                y = dom.next_sibling(q);
            }
            _ => return false,
        }
    }
}

/// Whether `a` precedes `b` in tree order; both must be in the same tree
/// and neither an ancestor of the other.
fn precedes(dom: &Dom, a: NodeId, b: NodeId) -> bool {
    let chain = |n: NodeId| {
        let mut v: Vec<NodeId> = std::iter::once(n).chain(dom.ancestors(n)).collect();
        v.reverse();
        v
    };
    let (ca, cb) = (chain(a), chain(b));
    let common = ca.iter().zip(&cb).take_while(|(x, y)| x == y).count();
    let (Some(&x), Some(&y)) = (ca.get(common), cb.get(common)) else {
        return ca.len() < cb.len();
    };
    following_siblings(dom, x).any(|s| s == y)
}

/// The parent to parse and insert relative to for `before`/`after`/
/// `replaceWith`: the first sibling in the given direction not in `nodes`.
fn viable_sibling(
    dom: &Dom,
    node: NodeId,
    nodes: &[NodeOrString],
    forward: bool,
) -> Option<NodeId> {
    let excluded = |id: NodeId| {
        nodes
            .iter()
            .any(|n| matches!(n, NodeOrString::Node(x) if *x == id))
    };
    let step = |n: NodeId| {
        if forward {
            dom.next_sibling(n)
        } else {
            dom.prev_sibling(n)
        }
    };
    let mut cur = step(node);
    while let Some(n) = cur {
        if !excluded(n) {
            return Some(n);
        }
        cur = step(n);
    }
    None
}

// ---------------------------------------------------------------- bindings

impl web::NodeImpl for Web {
    fn node_type(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u16> {
        check(cx, this)?;
        Ok(match cx.dom().kind(this) {
            NodeKind::Element(_) => ELEMENT_NODE,
            NodeKind::Text(_) => TEXT_NODE,
            NodeKind::ProcessingInstruction { .. } => PROCESSING_INSTRUCTION_NODE,
            NodeKind::Comment(_) => COMMENT_NODE,
            NodeKind::Document(_) => DOCUMENT_NODE,
            NodeKind::Doctype(_) => DOCUMENT_TYPE_NODE,
            NodeKind::DocumentFragment(_) => DOCUMENT_FRAGMENT_NODE,
        })
    }

    fn node_name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        check(cx, this)?;
        let dom = cx.dom();
        Ok(match dom.kind(this) {
            NodeKind::Element(el) => {
                let name = qualified_name(el);
                if el.is_html() && in_html_document(&dom, this) {
                    name.to_ascii_uppercase()
                } else {
                    name
                }
            }
            NodeKind::Text(_) => "#text".to_string(),
            NodeKind::ProcessingInstruction { target, .. } => target.clone(),
            NodeKind::Comment(_) => "#comment".to_string(),
            NodeKind::Document(_) => "#document".to_string(),
            NodeKind::Doctype(d) => d.name.clone(),
            NodeKind::DocumentFragment(_) => "#document-fragment".to_string(),
        })
    }

    fn base_uri(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        check(cx, this)?;
        Ok(base_url(cx, this).map_or_else(|| "about:blank".to_string(), |url| url.to_string()))
    }

    fn is_connected(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        check(cx, this)?;
        Ok(cx.dom().in_document_tree(this))
    }

    fn owner_document(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        let document = cx.dom().owner_document(this);
        Ok((this != document).then_some(document))
    }

    fn get_root_node(
        cx: &mut Cx<'_>,
        this: NodeId,
        _options: web::GetRootNodeOptions,
    ) -> Fallible<NodeId> {
        check(cx, this)?;
        Ok(cx.dom().root_of(this))
    }

    fn parent_node(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().parent(this))
    }

    fn parent_element(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().parent_element(this))
    }

    fn has_child_nodes(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        check(cx, this)?;
        Ok(cx.dom().has_children(this))
    }

    fn child_nodes(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(collections::node_list(
            cx.page,
            ListSource::ChildNodes(this),
        ))
    }

    fn first_child(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().first_child(this))
    }

    fn last_child(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().last_child(this))
    }

    fn previous_sibling(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().prev_sibling(this))
    }

    fn next_sibling(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().next_sibling(this))
    }

    fn node_value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<String>> {
        Ok(char_data(&cx.dom(), this).map(str::to_string))
    }

    fn set_node_value(cx: &mut Cx<'_>, this: NodeId, value: Option<String>) -> Fallible<()> {
        if char_data(&cx.dom(), this).is_some() {
            set_char_data(cx, this, value.unwrap_or_default())?;
        }
        Ok(())
    }

    fn text_content(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<String>> {
        check(cx, this)?;
        let dom = cx.dom();
        Ok(match dom.kind(this) {
            NodeKind::Document(_) | NodeKind::Doctype(_) => None,
            _ => Some(dom.text_content(this)),
        })
    }

    fn set_text_content(cx: &mut Cx<'_>, this: NodeId, value: Option<String>) -> Fallible<()> {
        check(cx, this)?;
        let value = value.unwrap_or_default();
        let is_container = matches!(
            cx.dom().kind(this),
            NodeKind::Element(_) | NodeKind::DocumentFragment(_)
        );
        if is_container {
            string_replace_all(cx, &value, this);
            Ok(())
        } else if char_data(&cx.dom(), this).is_some() {
            set_char_data(cx, this, value)
        } else {
            Ok(())
        }
    }

    fn normalize(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        check(cx, this)?;
        let texts: Vec<NodeId> = {
            let dom = cx.dom();
            dom.descendants(this)
                .filter(|&n| dom.node(n).is_text())
                .collect()
        };
        for text in texts {
            // What follows `text`: its data and the text nodes to merge.
            let (data, merged) = {
                let dom = cx.dom();
                // Already merged into a previous sibling.
                if dom.parent(text).is_none() {
                    continue;
                }
                let mut data = dom.node(text).as_text().unwrap_or_default().to_string();
                if data.is_empty() {
                    (data, Vec::new())
                } else {
                    let merged: Vec<NodeId> = following_siblings(&dom, text)
                        .take_while(|&n| dom.node(n).is_text())
                        .collect();
                    for &n in &merged {
                        data.push_str(dom.node(n).as_text().unwrap_or_default());
                    }
                    (data, merged)
                }
            };
            if data.is_empty() {
                remove(cx, text, false);
                continue;
            }
            if merged.is_empty() {
                continue;
            }
            set_char_data(cx, text, data)?;
            for n in merged {
                remove(cx, n, false);
            }
        }
        Ok(())
    }

    fn clone_node(cx: &mut Cx<'_>, this: NodeId, subtree: bool) -> Fallible<NodeId> {
        clone_node(cx, this, subtree)
    }

    fn is_equal_node(cx: &mut Cx<'_>, this: NodeId, other_node: Option<NodeId>) -> Fallible<bool> {
        check(cx, this)?;
        let dom = cx.dom();
        Ok(other_node.is_some_and(|o| dom.contains(o) && nodes_equal(&dom, this, o)))
    }

    fn is_same_node(_cx: &mut Cx<'_>, this: NodeId, other_node: Option<NodeId>) -> Fallible<bool> {
        Ok(other_node == Some(this))
    }

    fn compare_document_position(cx: &mut Cx<'_>, this: NodeId, other: NodeId) -> Fallible<u16> {
        check(cx, this)?;
        check(cx, other)?;
        if this == other {
            return Ok(0);
        }
        let dom = cx.dom();
        if dom.root_of(this) != dom.root_of(other) {
            // Any consistent order will do for nodes in different trees.
            let order = if other < this {
                DOCUMENT_POSITION_PRECEDING
            } else {
                DOCUMENT_POSITION_FOLLOWING
            };
            return Ok(DOCUMENT_POSITION_DISCONNECTED
                | DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC
                | order);
        }
        if dom.ancestors(this).any(|a| a == other) {
            return Ok(DOCUMENT_POSITION_CONTAINS | DOCUMENT_POSITION_PRECEDING);
        }
        if dom.ancestors(other).any(|a| a == this) {
            return Ok(DOCUMENT_POSITION_CONTAINED_BY | DOCUMENT_POSITION_FOLLOWING);
        }
        Ok(if precedes(&dom, other, this) {
            DOCUMENT_POSITION_PRECEDING
        } else {
            DOCUMENT_POSITION_FOLLOWING
        })
    }

    fn contains(cx: &mut Cx<'_>, this: NodeId, other: Option<NodeId>) -> Fallible<bool> {
        let dom = cx.dom();
        Ok(other
            .is_some_and(|o| dom.contains(o) && (o == this || dom.ancestors(o).any(|a| a == this))))
    }

    fn insert_before(
        cx: &mut Cx<'_>,
        this: NodeId,
        node: NodeId,
        child: Option<NodeId>,
    ) -> Fallible<NodeId> {
        pre_insert(cx, node, this, child)
    }

    fn append_child(cx: &mut Cx<'_>, this: NodeId, node: NodeId) -> Fallible<NodeId> {
        append(cx, node, this)
    }

    fn replace_child(
        cx: &mut Cx<'_>,
        this: NodeId,
        node: NodeId,
        child: NodeId,
    ) -> Fallible<NodeId> {
        replace(cx, child, node, this)
    }

    fn remove_child(cx: &mut Cx<'_>, this: NodeId, child: NodeId) -> Fallible<NodeId> {
        check(cx, this)?;
        check(cx, child)?;
        if cx.dom().parent(child) != Some(this) {
            return Err(Exception::not_found(
                "The node to be removed is not a child of this node",
            ));
        }
        remove(cx, child, false);
        Ok(child)
    }
}

impl web::ParentNodeImpl for Web {
    fn children(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(collections::html_collection(
            cx.page,
            ListSource::ChildElements(this),
        ))
    }

    fn first_element_child(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().child_elements(this).next())
    }

    fn last_element_child(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        Ok(cx.dom().child_elements(this).last())
    }

    fn child_element_count(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        check(cx, this)?;
        Ok(cx.dom().child_elements(this).count() as u32)
    }

    fn prepend(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let node = convert_nodes(cx, nodes)?;
        let first = cx.dom().first_child(this);
        pre_insert(cx, node, this, first).map(drop)
    }

    fn append(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let node = convert_nodes(cx, nodes)?;
        append(cx, node, this).map(drop)
    }

    fn replace_children(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let node = convert_nodes(cx, nodes)?;
        ensure_validity(&cx.dom(), node, this, None, false)?;
        replace_all(cx, Some(node), this);
        Ok(())
    }

    fn query_selector(
        cx: &mut Cx<'_>,
        this: NodeId,
        selectors: String,
    ) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        let parsed = parse_selectors(&selectors)?;
        Ok(catpaw_style::query::query_first(&cx.dom(), this, &parsed))
    }

    fn query_selector_all(cx: &mut Cx<'_>, this: NodeId, selectors: String) -> Fallible<ObjectId> {
        check(cx, this)?;
        let parsed = parse_selectors(&selectors)?;
        let items = catpaw_style::query::query_all(&cx.dom(), this, &parsed);
        Ok(collections::static_node_list(cx.page, items))
    }
}

pub(crate) fn parse_selectors(selectors: &str) -> Fallible<Selectors> {
    Selectors::parse(selectors)
        .ok_or_else(|| Exception::syntax(format!("'{selectors}' is not a valid selector")))
}

impl web::ChildNodeImpl for Web {
    fn before(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let Some(parent) = cx.dom().parent(this) else {
            return Ok(());
        };
        let viable_previous = viable_sibling(&cx.dom(), this, &nodes, false);
        let node = convert_nodes(cx, nodes)?;
        let reference = match viable_previous {
            Some(p) => cx.dom().next_sibling(p),
            None => cx.dom().first_child(parent),
        };
        pre_insert(cx, node, parent, reference).map(drop)
    }

    fn after(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let Some(parent) = cx.dom().parent(this) else {
            return Ok(());
        };
        let viable_next = viable_sibling(&cx.dom(), this, &nodes, true);
        let node = convert_nodes(cx, nodes)?;
        pre_insert(cx, node, parent, viable_next).map(drop)
    }

    fn replace_with(cx: &mut Cx<'_>, this: NodeId, nodes: Vec<NodeOrString>) -> Fallible<()> {
        check(cx, this)?;
        let Some(parent) = cx.dom().parent(this) else {
            return Ok(());
        };
        let viable_next = viable_sibling(&cx.dom(), this, &nodes, true);
        let node = convert_nodes(cx, nodes)?;
        if cx.dom().parent(this) == Some(parent) {
            replace(cx, this, node, parent).map(drop)
        } else {
            pre_insert(cx, node, parent, viable_next).map(drop)
        }
    }

    fn remove(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        remove(cx, this, false);
        Ok(())
    }
}

impl web::NonDocumentTypeChildNodeImpl for Web {
    fn previous_element_sibling(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        let dom = cx.dom();
        Ok(preceding_siblings(&dom, this).find(|&s| dom.is_element(s)))
    }

    fn next_element_sibling(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        let dom = cx.dom();
        Ok(following_siblings(&dom, this).find(|&s| dom.is_element(s)))
    }
}

impl web::NonElementParentNodeImpl for Web {
    fn get_element_by_id(
        cx: &mut Cx<'_>,
        this: NodeId,
        element_id: String,
    ) -> Fallible<Option<NodeId>> {
        check(cx, this)?;
        let dom = cx.dom();
        if this != dom.document() {
            return Ok(dom
                .descendants(this)
                .find(|&n| dom.element(n).is_some_and(|e| e.id() == Some(&element_id))));
        }
        // The document keeps an index of ids, rebuilt when the tree changed.
        let mut index = cx.page.id_index.borrow_mut();
        if index.0 != dom.version() {
            index.1.clear();
            for n in dom.descendants(this) {
                if let Some(id) = dom.element(n).and_then(|e| e.id())
                    && !id.is_empty()
                    && !index.1.contains_key(id)
                {
                    index.1.insert(id.to_string(), n);
                }
            }
            index.0 = dom.version();
        }
        Ok(index.1.get(&element_id).copied())
    }
}

impl web::CharacterDataImpl for Web {
    fn data(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        data_of(cx, this)
    }

    fn set_data(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        set_char_data(cx, this, value)
    }

    fn length(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        Ok(utf16_len(&data_of(cx, this)?))
    }

    fn substring_data(cx: &mut Cx<'_>, this: NodeId, offset: u32, count: u32) -> Fallible<String> {
        substring_utf16(&data_of(cx, this)?, offset, count)
    }

    fn append_data(cx: &mut Cx<'_>, this: NodeId, data: String) -> Fallible<()> {
        let mut current = data_of(cx, this)?;
        current.push_str(&data);
        set_char_data(cx, this, current)
    }

    fn insert_data(cx: &mut Cx<'_>, this: NodeId, offset: u32, data: String) -> Fallible<()> {
        let new = replace_utf16(&data_of(cx, this)?, offset, 0, &data)?;
        set_char_data(cx, this, new)
    }

    fn delete_data(cx: &mut Cx<'_>, this: NodeId, offset: u32, count: u32) -> Fallible<()> {
        let new = replace_utf16(&data_of(cx, this)?, offset, count, "")?;
        set_char_data(cx, this, new)
    }

    fn replace_data(
        cx: &mut Cx<'_>,
        this: NodeId,
        offset: u32,
        count: u32,
        data: String,
    ) -> Fallible<()> {
        let new = replace_utf16(&data_of(cx, this)?, offset, count, &data)?;
        set_char_data(cx, this, new)
    }
}

impl web::TextImpl for Web {
    fn split_text(cx: &mut Cx<'_>, this: NodeId, offset: u32) -> Fallible<NodeId> {
        let data = data_of(cx, this)?;
        let length = utf16_len(&data);
        if offset > length {
            return Err(Exception::index_size(
                "The offset is larger than the text's length",
            ));
        }
        let tail = substring_utf16(&data, offset, length - offset)?;
        let head = substring_utf16(&data, 0, offset)?;
        let new = cx.dom_mut().create_text(tail);
        let position = {
            let dom = cx.dom();
            dom.parent(this).map(|p| (p, dom.next_sibling(this)))
        };
        if let Some((parent, next)) = position {
            insert(cx, new, parent, next, false);
        }
        set_char_data(cx, this, head)?;
        Ok(new)
    }

    fn whole_text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        check(cx, this)?;
        let dom = cx.dom();
        let is_text = |n: &NodeId| dom.node(*n).is_text();
        let mut first = this;
        while let Some(prev) = dom.prev_sibling(first).filter(is_text) {
            first = prev;
        }
        let mut out = String::new();
        let mut cur = Some(first);
        while let Some(n) = cur.filter(is_text) {
            out.push_str(dom.node(n).as_text().unwrap_or_default());
            cur = dom.next_sibling(n);
        }
        Ok(out)
    }

    fn constructor(cx: &mut Cx<'_>, data: String) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_text(data))
    }
}

impl web::CommentImpl for Web {
    fn constructor(cx: &mut Cx<'_>, data: String) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_comment(data))
    }
}

impl web::DocumentFragmentImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<NodeId> {
        Ok(cx.dom_mut().create_fragment(FragmentKind::Plain))
    }
}

fn doctype<R>(
    cx: &Cx<'_>,
    this: NodeId,
    f: impl FnOnce(&catpaw_dom::DoctypeData) -> R,
) -> Fallible<R> {
    let dom = cx.dom();
    match dom.get(this).map(|n| &n.kind) {
        Some(NodeKind::Doctype(d)) => Ok(f(d)),
        _ => Err(stale()),
    }
}

impl web::DocumentTypeImpl for Web {
    fn name(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        doctype(cx, this, |d| d.name.clone())
    }

    fn public_id(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        doctype(cx, this, |d| d.public_id.clone())
    }

    fn system_id(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        doctype(cx, this, |d| d.system_id.clone())
    }
}
