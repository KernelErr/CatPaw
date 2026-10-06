//! Tree traversal: `TreeWalker` and `NodeIterator`
//! (<https://dom.spec.whatwg.org/#traversal>).

use std::cell::RefCell;

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::generated as web;
use crate::node;
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

const ACCEPT: u16 = 1;
const REJECT: u16 = 2;
const SKIP: u16 = 3;

/// What both kinds of traverser filter with.
struct Filtering {
    what_to_show: u32,
    filter: Option<Callback>,
    /// The filter is running: it may not use the traverser itself.
    active: bool,
}

pub struct TreeWalkerObject {
    root: NodeId,
    current: NodeId,
    filtering: Filtering,
}
platform_object!(TreeWalkerObject, TreeWalker);

pub struct NodeIteratorObject {
    root: NodeId,
    reference: NodeId,
    pointer_before_reference: bool,
    filtering: Filtering,
}
platform_object!(NodeIteratorObject, NodeIterator);

/// The node iterators there are, which follow removals from the tree.
#[derive(Default)]
pub(crate) struct Traversers {
    iterators: RefCell<Vec<ObjectId>>,
}

fn node_type(dom: &Dom, id: NodeId) -> u32 {
    u32::from(match dom.kind(id) {
        NodeKind::Element(_) => node::ELEMENT_NODE,
        NodeKind::Text(_) => node::TEXT_NODE,
        NodeKind::ProcessingInstruction { .. } => node::PROCESSING_INSTRUCTION_NODE,
        NodeKind::Comment(_) => node::COMMENT_NODE,
        NodeKind::Document(_) => node::DOCUMENT_NODE,
        NodeKind::Doctype(_) => node::DOCUMENT_TYPE_NODE,
        NodeKind::DocumentFragment(_) => node::DOCUMENT_FRAGMENT_NODE,
    })
}

/// What a filter's return value stands for as an `unsigned short`.
fn verdict(value: &Value) -> u16 {
    let number = match value {
        Value::Number(n) => *n,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    if number.is_finite() {
        number.trunc().rem_euclid(65536.0) as u16
    } else {
        0
    }
}

/// How a traverser object lends out its filtering state.
trait Traverser: 'static {
    fn filtering(&mut self) -> &mut Filtering;
}

impl Traverser for TreeWalkerObject {
    fn filtering(&mut self) -> &mut Filtering {
        &mut self.filtering
    }
}

impl Traverser for NodeIteratorObject {
    fn filtering(&mut self) -> &mut Filtering {
        &mut self.filtering
    }
}

/// <https://dom.spec.whatwg.org/#concept-node-filter>
fn filter<T: Traverser + crate::PlatformObject>(
    cx: &mut Cx<'_>,
    this: ObjectId,
    node: NodeId,
) -> Fallible<u16> {
    let shown = 1u32 << (node_type(&cx.dom(), node) - 1);
    let callback = cx.page.with::<T, _>(this, |t| {
        let filtering = t.filtering();
        if filtering.active {
            return Err(Exception::invalid_state(
                "The filter cannot use the traverser it is filtering for",
            ));
        }
        if filtering.what_to_show & shown == 0 {
            return Ok(Err(SKIP));
        }
        match &filtering.filter {
            None => Ok(Err(ACCEPT)),
            Some(callback) => {
                filtering.active = true;
                Ok(Ok(callback.clone()))
            }
        }
    })??;
    let callback = match callback {
        Ok(callback) => callback,
        Err(decided) => return Ok(decided),
    };
    let result = cx
        .script
        .call(&callback, &Value::Undefined, &[Value::Node(node)]);
    cx.page
        .with::<T, _>(this, |t| t.filtering().active = false)?;
    result.map(|value| verdict(&value))
}

fn walker<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut TreeWalkerObject) -> R,
) -> Fallible<R> {
    cx.page.with::<TreeWalkerObject, _>(this, f)
}

fn accept(cx: &mut Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<u16> {
    filter::<TreeWalkerObject>(cx, this, node)
}

/// Moves the walker to `node` and returns it.
fn arrive(cx: &Cx<'_>, this: ObjectId, node: NodeId) -> Fallible<Option<NodeId>> {
    walker(cx, this, |w| w.current = node)?;
    Ok(Some(node))
}

/// <https://dom.spec.whatwg.org/#concept-traverse-children>
fn traverse_children(cx: &mut Cx<'_>, this: ObjectId, first: bool) -> Fallible<Option<NodeId>> {
    let (root, current) = walker(cx, this, |w| (w.root, w.current))?;
    let child_of = |dom: &Dom, n: NodeId| {
        if first {
            dom.first_child(n)
        } else {
            dom.last_child(n)
        }
    };
    let mut node = child_of(&cx.dom(), current);
    while let Some(n) = node {
        let result = accept(cx, this, n)?;
        if result == ACCEPT {
            return arrive(cx, this, n);
        }
        let dom = cx.dom();
        if result == SKIP
            && let Some(child) = child_of(&dom, n)
        {
            node = Some(child);
            continue;
        }
        let mut n = n;
        loop {
            let sibling = if first {
                dom.next_sibling(n)
            } else {
                dom.prev_sibling(n)
            };
            if sibling.is_some() {
                node = sibling;
                break;
            }
            match dom.parent(n) {
                Some(parent) if parent != root && parent != current => n = parent,
                _ => return Ok(None),
            }
        }
    }
    Ok(None)
}

/// <https://dom.spec.whatwg.org/#concept-traverse-siblings>
fn traverse_siblings(cx: &mut Cx<'_>, this: ObjectId, next: bool) -> Fallible<Option<NodeId>> {
    let (root, current) = walker(cx, this, |w| (w.root, w.current))?;
    let mut node = current;
    if node == root {
        return Ok(None);
    }
    let sibling_of = |dom: &Dom, n: NodeId| {
        if next {
            dom.next_sibling(n)
        } else {
            dom.prev_sibling(n)
        }
    };
    loop {
        let mut sibling = sibling_of(&cx.dom(), node);
        while let Some(s) = sibling {
            node = s;
            let result = accept(cx, this, node)?;
            if result == ACCEPT {
                return arrive(cx, this, node);
            }
            let dom = cx.dom();
            sibling = if next {
                dom.first_child(node)
            } else {
                dom.last_child(node)
            };
            if result == REJECT || sibling.is_none() {
                sibling = sibling_of(&dom, node);
            }
        }
        let parent = cx.dom().parent(node);
        match parent {
            Some(parent) if parent != root => node = parent,
            _ => return Ok(None),
        }
        if accept(cx, this, node)? == ACCEPT {
            return Ok(None);
        }
    }
}

impl web::TreeWalkerImpl for Web {
    fn root(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        walker(cx, this, |w| w.root)
    }

    fn what_to_show(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        walker(cx, this, |w| w.filtering.what_to_show)
    }

    fn filter(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<Callback>> {
        walker(cx, this, |w| w.filtering.filter.clone())
    }

    fn current_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        walker(cx, this, |w| w.current)
    }

    fn set_current_node(cx: &mut Cx<'_>, this: ObjectId, value: NodeId) -> Fallible<()> {
        node::check(cx, value)?;
        walker(cx, this, |w| w.current = value)
    }

    fn parent_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        let (root, mut node) = walker(cx, this, |w| (w.root, w.current))?;
        while node != root {
            let Some(parent) = cx.dom().parent(node) else {
                break;
            };
            node = parent;
            if accept(cx, this, node)? == ACCEPT {
                return arrive(cx, this, node);
            }
        }
        Ok(None)
    }

    fn first_child(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        traverse_children(cx, this, true)
    }

    fn last_child(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        traverse_children(cx, this, false)
    }

    fn previous_sibling(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        traverse_siblings(cx, this, false)
    }

    fn next_sibling(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        traverse_siblings(cx, this, true)
    }

    fn previous_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        let (root, mut node) = walker(cx, this, |w| (w.root, w.current))?;
        while node != root {
            let mut sibling = cx.dom().prev_sibling(node);
            while let Some(s) = sibling {
                node = s;
                let mut result = accept(cx, this, node)?;
                // Down to the last node of what this sibling holds.
                loop {
                    let last = cx.dom().last_child(node);
                    match last {
                        Some(last) if result != REJECT => {
                            node = last;
                            result = accept(cx, this, node)?;
                        }
                        _ => break,
                    }
                }
                if result == ACCEPT {
                    return arrive(cx, this, node);
                }
                sibling = cx.dom().prev_sibling(node);
            }
            if node == root {
                return Ok(None);
            }
            let Some(parent) = cx.dom().parent(node) else {
                return Ok(None);
            };
            node = parent;
            if accept(cx, this, node)? == ACCEPT {
                return arrive(cx, this, node);
            }
        }
        Ok(None)
    }

    fn next_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        let (root, mut node) = walker(cx, this, |w| (w.root, w.current))?;
        let mut result = ACCEPT;
        loop {
            loop {
                let first = cx.dom().first_child(node);
                match first {
                    Some(first) if result != REJECT => {
                        node = first;
                        result = accept(cx, this, node)?;
                        if result == ACCEPT {
                            return arrive(cx, this, node);
                        }
                    }
                    _ => break,
                }
            }
            // On to what follows the node, and its ancestors, inside the
            // root.
            let following = {
                let dom = cx.dom();
                let mut at = Some(node);
                loop {
                    match at {
                        None => break None,
                        Some(n) if n == root => break None,
                        Some(n) => match dom.next_sibling(n) {
                            Some(sibling) => break Some(sibling),
                            None => at = dom.parent(n),
                        },
                    }
                }
            };
            let Some(following) = following else {
                return Ok(None);
            };
            node = following;
            result = accept(cx, this, node)?;
            if result == ACCEPT {
                return arrive(cx, this, node);
            }
        }
    }
}

/// The node after `node` in the tree order of `root`'s subtree.
fn following(dom: &Dom, node: NodeId, root: NodeId) -> Option<NodeId> {
    if let Some(child) = dom.first_child(node) {
        return Some(child);
    }
    let mut at = node;
    loop {
        if at == root {
            return None;
        }
        if let Some(sibling) = dom.next_sibling(at) {
            return Some(sibling);
        }
        at = dom.parent(at)?;
    }
}

/// The last node of `node`'s subtree in tree order.
fn last_inclusive_descendant(dom: &Dom, mut node: NodeId) -> NodeId {
    while let Some(last) = dom.last_child(node) {
        node = last;
    }
    node
}

/// The node before `node` in the tree order of `root`'s subtree.
fn preceding(dom: &Dom, node: NodeId, root: NodeId) -> Option<NodeId> {
    if node == root {
        return None;
    }
    match dom.prev_sibling(node) {
        Some(sibling) => Some(last_inclusive_descendant(dom, sibling)),
        None => dom.parent(node),
    }
}

fn iterator<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut NodeIteratorObject) -> R,
) -> Fallible<R> {
    cx.page.with::<NodeIteratorObject, _>(this, f)
}

/// <https://dom.spec.whatwg.org/#concept-nodeiterator-traverse>
fn iterate(cx: &mut Cx<'_>, this: ObjectId, next: bool) -> Fallible<Option<NodeId>> {
    let (root, mut node, mut before) = iterator(cx, this, |i| {
        (i.root, i.reference, i.pointer_before_reference)
    })?;
    loop {
        if next {
            if before {
                before = false;
            } else {
                let Some(following) = following(&cx.dom(), node, root) else {
                    return Ok(None);
                };
                node = following;
            }
        } else if before {
            let Some(preceding) = preceding(&cx.dom(), node, root) else {
                return Ok(None);
            };
            node = preceding;
        } else {
            before = true;
        }
        if filter::<NodeIteratorObject>(cx, this, node)? == ACCEPT {
            break;
        }
    }
    iterator(cx, this, |i| {
        i.reference = node;
        i.pointer_before_reference = before;
    })?;
    Ok(Some(node))
}

/// <https://dom.spec.whatwg.org/#nodeiterator-pre-removing-steps>: called
/// before `node` is removed from its parent, so that iterators standing in
/// what goes away step aside.
pub(crate) fn before_removal(page: &PageState, node: NodeId) {
    if page.traversers.iterators.borrow().is_empty() {
        return;
    }
    let iterators = page.traversers.iterators.borrow().clone();
    let dom = page.dom.borrow();
    for id in iterators {
        let _ = page.try_with::<NodeIteratorObject, _>(id, |i| {
            let inside = i.reference == node || dom.ancestors(i.reference).any(|a| a == node);
            if !inside || node == i.root {
                return;
            }
            if i.pointer_before_reference {
                // The first node after what is removed, if the root has one.
                let mut at = node;
                let next = loop {
                    if at == i.root {
                        break None;
                    }
                    if let Some(sibling) = dom.next_sibling(at) {
                        break Some(sibling);
                    }
                    match dom.parent(at) {
                        Some(parent) => at = parent,
                        None => break None,
                    }
                };
                if let Some(next) = next {
                    i.reference = next;
                    return;
                }
                i.pointer_before_reference = false;
            }
            i.reference = match dom.prev_sibling(node) {
                Some(sibling) => last_inclusive_descendant(&dom, sibling),
                None => dom.parent(node).unwrap_or(i.root),
            };
        });
    }
}

impl web::NodeIteratorImpl for Web {
    fn root(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        iterator(cx, this, |i| i.root)
    }

    fn reference_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        iterator(cx, this, |i| i.reference)
    }

    fn pointer_before_reference_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        iterator(cx, this, |i| i.pointer_before_reference)
    }

    fn what_to_show(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        iterator(cx, this, |i| i.filtering.what_to_show)
    }

    fn filter(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<Callback>> {
        iterator(cx, this, |i| i.filtering.filter.clone())
    }

    fn next_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        iterate(cx, this, true)
    }

    fn previous_node(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        iterate(cx, this, false)
    }

    fn detach(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        // Kept for old code; it does nothing.
        iterator(cx, this, |_| ())
    }
}

/// `document.createTreeWalker()`.
pub(crate) fn tree_walker(
    cx: &Cx<'_>,
    root: NodeId,
    what_to_show: u32,
    filter: Option<Callback>,
) -> Fallible<ObjectId> {
    node::check(cx, root)?;
    Ok(cx.page.alloc(TreeWalkerObject {
        root,
        current: root,
        filtering: Filtering {
            what_to_show,
            filter,
            active: false,
        },
    }))
}

/// `document.createNodeIterator()`.
pub(crate) fn node_iterator(
    cx: &Cx<'_>,
    root: NodeId,
    what_to_show: u32,
    filter: Option<Callback>,
) -> Fallible<ObjectId> {
    node::check(cx, root)?;
    let page = cx.page;
    let id = page.alloc(NodeIteratorObject {
        root,
        reference: root,
        pointer_before_reference: true,
        filtering: Filtering {
            what_to_show,
            filter,
            active: false,
        },
    });
    let mut iterators = page.traversers.iterators.borrow_mut();
    // Iterators script has let go of are forgotten here as well.
    if iterators.len() >= 64 {
        iterators.retain(|i| page.interface_of(*i).is_some());
    }
    iterators.push(id);
    Ok(id)
}
