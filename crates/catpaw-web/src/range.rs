//! `Range`, `StaticRange` and `document.createRange()`.
//!
//! A live range's boundary points follow the tree: the node operations in
//! `node` call the hooks here as the specification's insertion, removal,
//! character-data and text-splitting steps have them. Ranges are kept in
//! a registry of weak ids, so a range script has let go of is swept like
//! any other object.
//!
//! Without layout, `getBoundingClientRect()` is an empty rectangle, and
//! `getClientRects()` is absent.

use std::cell::RefCell;
use std::cmp::Ordering;

use catpaw_dom::{Dom, FragmentKind, NodeId, NodeKind};
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::generated::{self as web, InterfaceId, StaticRangeInit};
use crate::page::{Cx, PageState};
use crate::{Web, element, node, platform_object};

/// A boundary point: a node and an offset into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub node: NodeId,
    pub offset: u32,
}

pub struct RangeObject {
    start: Boundary,
    end: Boundary,
    /// A live range follows the tree; a static one does not.
    live: bool,
}
platform_object!(RangeObject, |r| if r.live {
    InterfaceId::Range
} else {
    InterfaceId::StaticRange
});

/// The live ranges, by id; a swept one drops out at the next sweep.
#[derive(Default)]
pub struct Ranges {
    live: RefCell<Vec<ObjectId>>,
}

const START_TO_START: u16 = 0;
const START_TO_END: u16 = 1;
const END_TO_END: u16 = 2;
const END_TO_START: u16 = 3;

// ------------------------------------------------------------ tree helpers

/// <https://dom.spec.whatwg.org/#concept-node-length>
pub(crate) fn node_length(dom: &Dom, id: NodeId) -> u32 {
    match dom.kind(id) {
        NodeKind::Doctype(_) => 0,
        NodeKind::Text(t) | NodeKind::Comment(t) => node::utf16_len(t),
        NodeKind::ProcessingInstruction { data, .. } => node::utf16_len(data),
        _ => dom.children(id).count() as u32,
    }
}

fn is_character_data(dom: &Dom, id: NodeId) -> bool {
    matches!(
        dom.kind(id),
        NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. }
    )
}

fn index_in_parent(dom: &Dom, id: NodeId) -> u32 {
    dom.parent(id)
        .and_then(|p| dom.children(p).position(|c| c == id))
        .unwrap_or(0) as u32
}

fn is_inclusive_ancestor(dom: &Dom, ancestor: NodeId, node: NodeId) -> bool {
    ancestor == node || dom.ancestors(node).any(|a| a == ancestor)
}

/// The child indices from the root down to `id`.
fn path(dom: &Dom, id: NodeId) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = id;
    while let Some(parent) = dom.parent(cur) {
        out.push(index_in_parent(dom, cur));
        cur = parent;
    }
    out.reverse();
    out
}

/// The order of two nodes of the same tree: an ancestor comes before its
/// descendants.
fn tree_order(dom: &Dom, a: NodeId, b: NodeId) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    path(dom, a).cmp(&path(dom, b))
}

/// <https://dom.spec.whatwg.org/#concept-range-bp-position>
fn position(dom: &Dom, a: Boundary, b: Boundary) -> Ordering {
    if a.node == b.node {
        return a.offset.cmp(&b.offset);
    }
    if tree_order(dom, a.node, b.node) == Ordering::Greater {
        return position(dom, b, a).reverse();
    }
    if is_inclusive_ancestor(dom, a.node, b.node) {
        // The child of a's node on the way to b's node.
        let mut child = b.node;
        while dom.parent(child) != Some(a.node) {
            child = dom.parent(child).expect("an ancestor");
        }
        if index_in_parent(dom, child) < a.offset {
            return Ordering::Greater;
        }
    }
    Ordering::Less
}

/// <https://dom.spec.whatwg.org/#contained>
fn contained(dom: &Dom, node: NodeId, start: Boundary, end: Boundary) -> bool {
    dom.root_of(node) == dom.root_of(start.node)
        && position(dom, Boundary { node, offset: 0 }, start) == Ordering::Greater
        && position(
            dom,
            Boundary {
                node,
                offset: node_length(dom, node),
            },
            end,
        ) == Ordering::Less
}

/// <https://dom.spec.whatwg.org/#partially-contained>
fn partially_contained(dom: &Dom, node: NodeId, start: Boundary, end: Boundary) -> bool {
    is_inclusive_ancestor(dom, node, start.node) != is_inclusive_ancestor(dom, node, end.node)
}

fn common_ancestor(dom: &Dom, start: Boundary, end: Boundary) -> NodeId {
    let mut container = start.node;
    while !is_inclusive_ancestor(dom, container, end.node) {
        container = dom.parent(container).expect("a common root");
    }
    container
}

// ----------------------------------------------------------- range access

fn range<R>(cx: &Cx<'_>, id: ObjectId, f: impl FnOnce(&mut RangeObject) -> R) -> Fallible<R> {
    cx.page.with::<RangeObject, _>(id, f)
}

fn bounds(cx: &Cx<'_>, id: ObjectId) -> Fallible<(Boundary, Boundary)> {
    range(cx, id, |r| (r.start, r.end))
}

/// A range's boundary points, for the selection.
pub(crate) fn bounds_of(cx: &Cx<'_>, id: ObjectId) -> Fallible<(Boundary, Boundary)> {
    bounds(cx, id)
}

/// The order of two boundary points of one tree.
pub(crate) fn compare_points(dom: &Dom, a: Boundary, b: Boundary) -> Ordering {
    position(dom, a, b)
}

/// Makes a live range, registered for updates.
pub(crate) fn new_range(cx: &Cx<'_>, start: Boundary, end: Boundary) -> ObjectId {
    let id = cx.page.alloc(RangeObject {
        start,
        end,
        live: true,
    });
    let registry = &cx.page.ranges;
    let mut live = registry.live.borrow_mut();
    if live.len() > 64 {
        live.retain(|&r| cx.page.object_exists(r));
    }
    live.push(id);
    id
}

/// Runs `f` on every live range's boundary points.
fn for_each_live(page: &PageState, mut f: impl FnMut(&mut Boundary)) {
    let ids: Vec<ObjectId> = page.ranges.live.borrow().clone();
    for id in ids {
        let _ = page.try_with::<RangeObject, _>(id, |r| {
            f(&mut r.start);
            f(&mut r.end);
        });
    }
}

// ---------------------------------------------------------- mutation hooks

/// Before `count` nodes are inserted into `parent` at `index`.
pub(crate) fn nodes_inserting(page: &PageState, parent: NodeId, index: u32, count: u32) {
    for_each_live(page, |bp| {
        if bp.node == parent && bp.offset > index {
            bp.offset += count;
        }
    });
}

/// Before `node`, the child of `parent` at `index`, is removed:
/// <https://dom.spec.whatwg.org/#concept-node-remove> steps 5 to 8.
pub(crate) fn node_removing(page: &PageState, dom: &Dom, node: NodeId, parent: NodeId, index: u32) {
    for_each_live(page, |bp| {
        if is_inclusive_ancestor(dom, node, bp.node) {
            *bp = Boundary {
                node: parent,
                offset: index,
            };
        } else if bp.node == parent && bp.offset > index {
            bp.offset -= 1;
        }
    });
}

/// After `count` code units at `offset` of `node`'s data were replaced
/// by `data_len` new ones: <https://dom.spec.whatwg.org/#concept-cd-replace>.
pub(crate) fn data_replaced(
    page: &PageState,
    node: NodeId,
    offset: u32,
    count: u32,
    data_len: u32,
) {
    for_each_live(page, |bp| {
        if bp.node != node {
            return;
        }
        if bp.offset > offset && bp.offset <= offset + count {
            bp.offset = offset;
        } else if bp.offset > offset + count {
            bp.offset = (bp.offset as i64 + data_len as i64 - count as i64).max(0) as u32;
        }
    });
}

/// After `new_node` was inserted after `node` (at `index + 1`) in a
/// `splitText(offset)`, before `node`'s data is cut.
pub(crate) fn text_split(
    page: &PageState,
    node: NodeId,
    new_node: NodeId,
    offset: u32,
    parent: Option<NodeId>,
    index: u32,
) {
    for_each_live(page, |bp| {
        if bp.node == node && bp.offset > offset {
            *bp = Boundary {
                node: new_node,
                offset: bp.offset - offset,
            };
        } else if parent.is_some() && Some(bp.node) == parent && bp.offset == index + 1 {
            bp.offset += 1;
        }
    });
}

// ------------------------------------------------------------- boundaries

fn check_boundary(cx: &Cx<'_>, node: NodeId, offset: u32) -> Fallible<()> {
    node::check(cx, node)?;
    let dom = cx.dom();
    if matches!(dom.kind(node), NodeKind::Doctype(_)) {
        return Err(Exception::invalid_node_type(
            "The node is a doctype, which cannot hold a boundary point.",
        ));
    }
    if offset > node_length(&dom, node) {
        return Err(Exception::index_size(
            "The offset is larger than the node's length.",
        ));
    }
    Ok(())
}

/// <https://dom.spec.whatwg.org/#concept-range-bp-set>
fn set_boundary(
    cx: &mut Cx<'_>,
    this: ObjectId,
    node: NodeId,
    offset: u32,
    start: bool,
) -> Fallible<()> {
    check_boundary(cx, node, offset)?;
    let bp = Boundary { node, offset };
    let (current_start, current_end) = bounds(cx, this)?;
    let dom = cx.dom();
    let same_root = dom.root_of(node) == dom.root_of(current_start.node);
    let (new_start, new_end) = if start {
        if !same_root || position(&dom, bp, current_end) == Ordering::Greater {
            (bp, bp)
        } else {
            (bp, current_end)
        }
    } else if !same_root || position(&dom, bp, current_start) == Ordering::Less {
        (bp, bp)
    } else {
        (current_start, bp)
    };
    drop(dom);
    range(cx, this, |r| {
        r.start = new_start;
        r.end = new_end;
    })
}

fn parent_point(cx: &Cx<'_>, node: NodeId, after: bool) -> Fallible<Boundary> {
    node::check(cx, node)?;
    let dom = cx.dom();
    let parent = dom
        .parent(node)
        .ok_or_else(|| Exception::invalid_node_type("The node has no parent."))?;
    let index = index_in_parent(&dom, node) + u32::from(after);
    Ok(Boundary {
        node: parent,
        offset: index,
    })
}

// ----------------------------------------------------------- the contents

/// `string()` of a range: the text within it.
fn range_text(dom: &Dom, start: Boundary, end: Boundary) -> String {
    let text_of = |n: NodeId| match dom.kind(n) {
        NodeKind::Text(t) => Some(t.clone()),
        _ => None,
    };
    let mut out = String::new();
    if start.node == end.node
        && let Some(t) = text_of(start.node)
    {
        return node::substring_utf16(&t, start.offset, end.offset - start.offset)
            .unwrap_or_default();
    }
    if let Some(t) = text_of(start.node) {
        let len = node::utf16_len(&t);
        out.push_str(
            &node::substring_utf16(&t, start.offset, len - start.offset).unwrap_or_default(),
        );
    }
    let root = common_ancestor(dom, start, end);
    for n in dom.descendants(root) {
        if let Some(t) = text_of(n)
            && contained(dom, n, start, end)
        {
            out.push_str(&t);
        }
    }
    if let Some(t) = text_of(end.node) {
        out.push_str(&node::substring_utf16(&t, 0, end.offset).unwrap_or_default());
    }
    out
}

/// The children of the common ancestor that frame the contents: the
/// first and last partially contained ones, and the contained ones.
struct Framing {
    first_partial: Option<NodeId>,
    last_partial: Option<NodeId>,
    contained: Vec<NodeId>,
}

fn framing(dom: &Dom, start: Boundary, end: Boundary) -> Fallible<Framing> {
    let common = common_ancestor(dom, start, end);
    let first_partial = (start.node != common).then(|| {
        dom.children(common)
            .find(|&c| is_inclusive_ancestor(dom, c, start.node))
            .expect("a child towards the start")
    });
    let last_partial = (end.node != common).then(|| {
        dom.children(common)
            .find(|&c| is_inclusive_ancestor(dom, c, end.node))
            .expect("a child towards the end")
    });
    let contained_children: Vec<NodeId> = dom
        .children(common)
        .filter(|&c| contained(dom, c, start, end))
        .collect();
    if contained_children
        .iter()
        .any(|&c| matches!(dom.kind(c), NodeKind::Doctype(_)))
    {
        return Err(Exception::hierarchy_request(
            "The range contains a doctype.",
        ));
    }
    Ok(Framing {
        first_partial,
        last_partial,
        contained: contained_children,
    })
}

fn new_fragment(cx: &Cx<'_>, document: NodeId) -> NodeId {
    let mut dom = cx.dom_mut();
    let fragment = dom.create_fragment(FragmentKind::Plain);
    dom.adopt_subtree(fragment, document);
    fragment
}

fn append(cx: &mut Cx<'_>, parent: NodeId, child: NodeId) {
    node::insert(cx, child, parent, None, false);
}

/// A clone of a character-data node holding only `offset..offset + count`.
fn clone_data_part(cx: &mut Cx<'_>, source: NodeId, offset: u32, count: u32) -> Fallible<NodeId> {
    let clone = node::clone_node(cx, source, false)?;
    let data = node::char_data(&cx.dom(), source)
        .map(str::to_string)
        .unwrap_or_default();
    let part = node::substring_utf16(&data, offset, count)?;
    node::set_char_data(cx, clone, part)?;
    Ok(clone)
}

/// Cuts `offset..offset + count` out of a character-data node.
fn delete_data_part(cx: &mut Cx<'_>, node: NodeId, offset: u32, count: u32) -> Fallible<()> {
    let data = node::char_data(&cx.dom(), node)
        .map(str::to_string)
        .unwrap_or_default();
    let new = node::replace_utf16(&data, offset, count, "")?;
    node::set_char_data_span(cx, node, new, offset, count)
}

/// <https://dom.spec.whatwg.org/#concept-range-clone>
fn clone_contents(cx: &mut Cx<'_>, start: Boundary, end: Boundary) -> Fallible<NodeId> {
    let document = cx.dom().owner_document(start.node);
    let fragment = new_fragment(cx, document);
    if start == end {
        return Ok(fragment);
    }
    if start.node == end.node && is_character_data(&cx.dom(), start.node) {
        let clone = clone_data_part(cx, start.node, start.offset, end.offset - start.offset)?;
        append(cx, fragment, clone);
        return Ok(fragment);
    }
    let Framing {
        first_partial,
        last_partial,
        contained: contained_children,
    } = framing(&cx.dom(), start, end)?;
    if let Some(first) = first_partial {
        if is_character_data(&cx.dom(), first) {
            let len = node_length(&cx.dom(), first);
            let clone = clone_data_part(cx, first, start.offset, len - start.offset)?;
            append(cx, fragment, clone);
        } else {
            let clone = node::clone_node(cx, first, false)?;
            append(cx, fragment, clone);
            let inner_end = Boundary {
                node: first,
                offset: node_length(&cx.dom(), first),
            };
            let sub = clone_contents(cx, start, inner_end)?;
            append(cx, clone, sub);
        }
    }
    for child in contained_children {
        let clone = node::clone_node(cx, child, true)?;
        append(cx, fragment, clone);
    }
    if let Some(last) = last_partial {
        if is_character_data(&cx.dom(), last) {
            let clone = clone_data_part(cx, last, 0, end.offset)?;
            append(cx, fragment, clone);
        } else {
            let clone = node::clone_node(cx, last, false)?;
            append(cx, fragment, clone);
            let inner_start = Boundary {
                node: last,
                offset: 0,
            };
            let sub = clone_contents(cx, inner_start, end)?;
            append(cx, clone, sub);
        }
    }
    Ok(fragment)
}

/// Where a range collapses to once its contents are gone.
fn point_after_extraction(dom: &Dom, start: Boundary, end: Boundary) -> Boundary {
    if is_inclusive_ancestor(dom, start.node, end.node) {
        return start;
    }
    let mut reference = start.node;
    while let Some(parent) = dom.parent(reference) {
        if is_inclusive_ancestor(dom, parent, end.node) {
            return Boundary {
                node: parent,
                offset: index_in_parent(dom, reference) + 1,
            };
        }
        reference = parent;
    }
    start
}

/// <https://dom.spec.whatwg.org/#concept-range-extract>
fn extract_contents(
    cx: &mut Cx<'_>,
    start: Boundary,
    end: Boundary,
) -> Fallible<(NodeId, Boundary)> {
    let document = cx.dom().owner_document(start.node);
    let fragment = new_fragment(cx, document);
    if start == end {
        return Ok((fragment, start));
    }
    if start.node == end.node && is_character_data(&cx.dom(), start.node) {
        let clone = clone_data_part(cx, start.node, start.offset, end.offset - start.offset)?;
        append(cx, fragment, clone);
        delete_data_part(cx, start.node, start.offset, end.offset - start.offset)?;
        return Ok((fragment, start));
    }
    let Framing {
        first_partial,
        last_partial,
        contained: contained_children,
    } = framing(&cx.dom(), start, end)?;
    let new_point = point_after_extraction(&cx.dom(), start, end);
    if let Some(first) = first_partial {
        if is_character_data(&cx.dom(), first) {
            let len = node_length(&cx.dom(), first);
            let clone = clone_data_part(cx, first, start.offset, len - start.offset)?;
            append(cx, fragment, clone);
            delete_data_part(cx, first, start.offset, len - start.offset)?;
        } else {
            let clone = node::clone_node(cx, first, false)?;
            append(cx, fragment, clone);
            let inner_end = Boundary {
                node: first,
                offset: node_length(&cx.dom(), first),
            };
            let (sub, _) = extract_contents(cx, start, inner_end)?;
            append(cx, clone, sub);
        }
    }
    for child in contained_children {
        append(cx, fragment, child);
    }
    if let Some(last) = last_partial {
        if is_character_data(&cx.dom(), last) {
            let clone = clone_data_part(cx, last, 0, end.offset)?;
            append(cx, fragment, clone);
            delete_data_part(cx, last, 0, end.offset)?;
        } else {
            let clone = node::clone_node(cx, last, false)?;
            append(cx, fragment, clone);
            let inner_start = Boundary {
                node: last,
                offset: 0,
            };
            let (sub, _) = extract_contents(cx, inner_start, end)?;
            append(cx, clone, sub);
        }
    }
    Ok((fragment, new_point))
}

/// <https://dom.spec.whatwg.org/#dom-range-deletecontents>
fn delete_contents(cx: &mut Cx<'_>, start: Boundary, end: Boundary) -> Fallible<Boundary> {
    if start == end {
        return Ok(start);
    }
    if start.node == end.node && is_character_data(&cx.dom(), start.node) {
        delete_data_part(cx, start.node, start.offset, end.offset - start.offset)?;
        return Ok(start);
    }
    // Nodes to remove: the contained ones whose parent is not contained.
    let to_remove: Vec<NodeId> = {
        let dom = cx.dom();
        let root = common_ancestor(&dom, start, end);
        dom.descendants(root)
            .filter(|&n| {
                contained(&dom, n, start, end)
                    && !dom
                        .parent(n)
                        .is_some_and(|p| contained(&dom, p, start, end))
            })
            .collect()
    };
    let new_point = point_after_extraction(&cx.dom(), start, end);
    if is_character_data(&cx.dom(), start.node) {
        let len = node_length(&cx.dom(), start.node);
        delete_data_part(cx, start.node, start.offset, len - start.offset)?;
    }
    for n in to_remove {
        node::remove(cx, n, false);
    }
    if is_character_data(&cx.dom(), end.node) {
        delete_data_part(cx, end.node, 0, end.offset)?;
    }
    Ok(new_point)
}

/// <https://dom.spec.whatwg.org/#concept-range-insert>
fn insert_node(cx: &mut Cx<'_>, this: ObjectId, node_to_insert: NodeId) -> Fallible<()> {
    let (start, end) = bounds(cx, this)?;
    let (start_is_text, parent, mut reference) = {
        let dom = cx.dom();
        match dom.kind(start.node) {
            NodeKind::ProcessingInstruction { .. } | NodeKind::Comment(_) => {
                return Err(Exception::hierarchy_request(
                    "The range starts in a comment or processing instruction.",
                ));
            }
            NodeKind::Text(_) if dom.parent(start.node).is_none() => {
                return Err(Exception::hierarchy_request(
                    "The range starts in a text node without a parent.",
                ));
            }
            NodeKind::Text(_) => (
                true,
                dom.parent(start.node).expect("checked"),
                Some(start.node),
            ),
            _ => (
                false,
                start.node,
                dom.children(start.node).nth(start.offset as usize),
            ),
        }
    };
    if start_is_text {
        let new_text = <Web as web::TextImpl>::split_text(cx, start.node, start.offset)?;
        reference = Some(new_text);
    }
    if reference == Some(node_to_insert) {
        reference = cx.dom().next_sibling(node_to_insert);
    }
    if cx.dom().parent(node_to_insert).is_some() {
        node::remove(cx, node_to_insert, false);
    }
    let mut new_offset = match reference {
        Some(r) => index_in_parent(&cx.dom(), r),
        None => node_length(&cx.dom(), parent),
    };
    new_offset += match cx.dom().kind(node_to_insert) {
        NodeKind::DocumentFragment(_) => node_length(&cx.dom(), node_to_insert),
        _ => 1,
    };
    node::pre_insert(cx, node_to_insert, parent, reference)?;
    if start == end {
        range(cx, this, |r| {
            r.end = Boundary {
                node: parent,
                offset: new_offset,
            };
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------- bindings

impl web::AbstractRangeImpl for Web {
    fn start_container(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        range(cx, this, |r| r.start.node)
    }

    fn start_offset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        range(cx, this, |r| r.start.offset)
    }

    fn end_container(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        range(cx, this, |r| r.end.node)
    }

    fn end_offset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        range(cx, this, |r| r.end.offset)
    }

    fn collapsed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        range(cx, this, |r| r.start == r.end)
    }
}

impl web::StaticRangeImpl for Web {
    fn constructor(cx: &mut Cx<'_>, init: StaticRangeInit) -> Fallible<ObjectId> {
        for node in [init.start_container, init.end_container] {
            node::check(cx, node)?;
            if matches!(cx.dom().kind(node), NodeKind::Doctype(_)) {
                return Err(Exception::invalid_node_type(
                    "A doctype cannot hold a boundary point.",
                ));
            }
        }
        Ok(cx.page.alloc(RangeObject {
            start: Boundary {
                node: init.start_container,
                offset: init.start_offset,
            },
            end: Boundary {
                node: init.end_container,
                offset: init.end_offset,
            },
            live: false,
        }))
    }
}

impl web::RangeImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        let document = cx.page.document();
        let point = Boundary {
            node: document,
            offset: 0,
        };
        Ok(new_range(cx, point, point))
    }

    fn common_ancestor_container(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        let (start, end) = bounds(cx, this)?;
        Ok(common_ancestor(&cx.dom(), start, end))
    }

    fn set_start(cx: &mut Cx<'_>, this: ObjectId, node: NodeId, offset: u32) -> Fallible<()> {
        set_boundary(cx, this, node, offset, true)
    }

    fn set_end(cx: &mut Cx<'_>, this: ObjectId, node: NodeId, offset: u32) -> Fallible<()> {
        set_boundary(cx, this, node, offset, false)
    }

    fn set_start_before(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        let bp = parent_point(cx, node, false)?;
        set_boundary(cx, this, bp.node, bp.offset, true)
    }

    fn set_start_after(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        let bp = parent_point(cx, node, true)?;
        set_boundary(cx, this, bp.node, bp.offset, true)
    }

    fn set_end_before(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        let bp = parent_point(cx, node, false)?;
        set_boundary(cx, this, bp.node, bp.offset, false)
    }

    fn set_end_after(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        let bp = parent_point(cx, node, true)?;
        set_boundary(cx, this, bp.node, bp.offset, false)
    }

    fn collapse(cx: &mut Cx<'_>, this: ObjectId, to_start: bool) -> Fallible<()> {
        range(cx, this, |r| {
            if to_start {
                r.end = r.start;
            } else {
                r.start = r.end;
            }
        })
    }

    fn select_node(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        let before = parent_point(cx, node, false)?;
        let after = Boundary {
            node: before.node,
            offset: before.offset + 1,
        };
        range(cx, this, |r| {
            r.start = before;
            r.end = after;
        })
    }

    fn select_node_contents(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        check_boundary(cx, node, 0)?;
        let length = node_length(&cx.dom(), node);
        range(cx, this, |r| {
            r.start = Boundary { node, offset: 0 };
            r.end = Boundary {
                node,
                offset: length,
            };
        })
    }

    fn compare_boundary_points(
        cx: &mut Cx<'_>,
        this: ObjectId,
        how: u16,
        source_range: ObjectId,
    ) -> Fallible<i16> {
        if how > END_TO_START {
            return Err(Exception::not_supported(
                "The comparison method is not one of the four.",
            ));
        }
        let (start, end) = bounds(cx, this)?;
        let (other_start, other_end) = bounds(cx, source_range)?;
        let dom = cx.dom();
        if dom.root_of(start.node) != dom.root_of(other_start.node) {
            return Err(Exception::wrong_document(
                "The ranges are in different documents.",
            ));
        }
        let (this_point, other_point) = match how {
            START_TO_START => (start, other_start),
            START_TO_END => (end, other_start),
            END_TO_END => (end, other_end),
            _ => (start, other_end),
        };
        Ok(match position(&dom, this_point, other_point) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }

    fn delete_contents(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let (start, end) = bounds(cx, this)?;
        let point = delete_contents(cx, start, end)?;
        range(cx, this, |r| {
            r.start = point;
            r.end = point;
        })
    }

    fn extract_contents(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        let (start, end) = bounds(cx, this)?;
        let (fragment, point) = extract_contents(cx, start, end)?;
        range(cx, this, |r| {
            r.start = point;
            r.end = point;
        })?;
        Ok(fragment)
    }

    fn clone_contents(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        let (start, end) = bounds(cx, this)?;
        clone_contents(cx, start, end)
    }

    fn insert_node(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<()> {
        node::check(cx, node)?;
        insert_node(cx, this, node)
    }

    /// <https://dom.spec.whatwg.org/#dom-range-surroundcontents>
    fn surround_contents(cx: &mut Cx<'_>, this: ObjectId, new_parent: NodeId) -> Fallible<()> {
        node::check(cx, new_parent)?;
        let (start, end) = bounds(cx, this)?;
        {
            let dom = cx.dom();
            let root = common_ancestor(&dom, start, end);
            let splits_a_non_text = dom.traverse(root).any(|n| {
                !matches!(dom.kind(n), NodeKind::Text(_))
                    && partially_contained(&dom, n, start, end)
            });
            if splits_a_non_text {
                return Err(Exception::invalid_state(
                    "The range partially selects a non-Text node.",
                ));
            }
            if matches!(
                dom.kind(new_parent),
                NodeKind::Document(_) | NodeKind::Doctype(_) | NodeKind::DocumentFragment(_)
            ) {
                return Err(Exception::invalid_node_type(
                    "The parent cannot be a document, doctype or fragment.",
                ));
            }
        }
        let fragment = Self::extract_contents(cx, this)?;
        let children: Vec<NodeId> = cx.dom().children(new_parent).collect();
        for child in children {
            node::remove(cx, child, false);
        }
        insert_node(cx, this, new_parent)?;
        node::pre_insert(cx, fragment, new_parent, None)?;
        Self::select_node(cx, this, new_parent)
    }

    fn clone_range(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let (start, end) = bounds(cx, this)?;
        Ok(new_range(cx, start, end))
    }

    fn detach(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        range(cx, this, |_| ())
    }

    fn is_point_in_range(
        cx: &mut Cx<'_>,
        this: ObjectId,
        node: NodeId,
        offset: u32,
    ) -> Fallible<bool> {
        node::check(cx, node)?;
        let (start, end) = bounds(cx, this)?;
        let dom = cx.dom();
        if dom.root_of(node) != dom.root_of(start.node) {
            return Ok(false);
        }
        if matches!(dom.kind(node), NodeKind::Doctype(_)) {
            return Err(Exception::invalid_node_type(
                "A doctype cannot hold a boundary point.",
            ));
        }
        if offset > node_length(&dom, node) {
            return Err(Exception::index_size(
                "The offset is larger than the node's length.",
            ));
        }
        let bp = Boundary { node, offset };
        Ok(position(&dom, bp, start) != Ordering::Less
            && position(&dom, bp, end) != Ordering::Greater)
    }

    fn compare_point(cx: &mut Cx<'_>, this: ObjectId, node: NodeId, offset: u32) -> Fallible<i16> {
        node::check(cx, node)?;
        let (start, end) = bounds(cx, this)?;
        let dom = cx.dom();
        if dom.root_of(node) != dom.root_of(start.node) {
            return Err(Exception::wrong_document(
                "The node is in another document.",
            ));
        }
        if matches!(dom.kind(node), NodeKind::Doctype(_)) {
            return Err(Exception::invalid_node_type(
                "A doctype cannot hold a boundary point.",
            ));
        }
        if offset > node_length(&dom, node) {
            return Err(Exception::index_size(
                "The offset is larger than the node's length.",
            ));
        }
        let bp = Boundary { node, offset };
        Ok(if position(&dom, bp, start) == Ordering::Less {
            -1
        } else if position(&dom, bp, end) == Ordering::Greater {
            1
        } else {
            0
        })
    }

    /// <https://dom.spec.whatwg.org/#dom-range-intersectsnode>
    fn intersects_node(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<bool> {
        node::check(cx, node)?;
        let (start, end) = bounds(cx, this)?;
        let dom = cx.dom();
        if dom.root_of(node) != dom.root_of(start.node) {
            return Ok(false);
        }
        let Some(parent) = dom.parent(node) else {
            return Ok(true);
        };
        let offset = index_in_parent(&dom, node);
        let before = Boundary {
            node: parent,
            offset,
        };
        let after = Boundary {
            node: parent,
            offset: offset + 1,
        };
        Ok(position(&dom, before, end) == Ordering::Less
            && position(&dom, after, start) == Ordering::Greater)
    }

    /// <https://w3c.github.io/DOM-Parsing/#dom-range-createcontextualfragment>
    fn create_contextual_fragment(
        cx: &mut Cx<'_>,
        this: ObjectId,
        string: String,
    ) -> Fallible<NodeId> {
        let (start, _) = bounds(cx, this)?;
        let context = {
            let dom = cx.dom();
            let element = if dom.is_element(start.node) {
                Some(start.node)
            } else {
                dom.parent_element(start.node)
            };
            let is_html_root = element.is_some_and(|e| {
                dom.element(e)
                    .is_some_and(|el| el.is_html() && &*el.name.local == "html")
                    && node::in_html_document(&dom, e)
            });
            if is_html_root { None } else { element }
        };
        let context = match context {
            Some(element) => element,
            None => {
                let body = cx.dom_mut().create_html_element("body", Vec::new());
                let document = cx.dom().owner_document(start.node);
                cx.dom_mut().adopt_subtree(body, document);
                body
            }
        };
        let fragment = element::parse_fragment_for(cx, &string, context);
        Ok(fragment)
    }

    fn get_bounding_client_rect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        range(cx, this, |_| ())?;
        Ok(element::zero_rect(cx))
    }

    fn stringify(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let (start, end) = bounds(cx, this)?;
        Ok(range_text(&cx.dom(), start, end))
    }
}

/// `document.createRange()`.
pub(crate) fn create_range(cx: &Cx<'_>, document: NodeId) -> ObjectId {
    let point = Boundary {
        node: document,
        offset: 0,
    };
    new_range(cx, point, point)
}
