//! `MutationObserver` (<https://dom.spec.whatwg.org/#mutation-observers>).
//!
//! Records are queued where the DOM Standard queues them: by the tree
//! mutation algorithms in [`crate::node`], and by attribute and character
//! data changes. What the parser inserts is read from the arena's change
//! log, which is kept while the parser runs. Observers are notified from a
//! microtask.
//!
//! Not reported: text the parser appends to an existing text node, and
//! attributes it adds to an existing element.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use catpaw_dom::{NodeId, TreeChange};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::collections;
use crate::generated as web;
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

struct Options {
    child_list: bool,
    attributes: bool,
    character_data: bool,
    subtree: bool,
    attribute_old_value: bool,
    character_data_old_value: bool,
    attribute_filter: Option<Vec<String>>,
}

/// An entry of a node's registered observer list.
struct Registered {
    observer: ObjectId,
    options: Rc<Options>,
    /// Set for a transient registration, which follows a subtree removed
    /// from an observed tree until its observer is next notified: the node
    /// of the registration it was made from.
    source: Option<NodeId>,
}

/// The page's mutation observer state.
#[derive(Default)]
pub(crate) struct Observers {
    /// The registered observer list of each observed node.
    registered: RefCell<HashMap<NodeId, Vec<Registered>>>,
    /// The observers to notify, in the order they came to need it.
    pending: RefCell<Vec<ObjectId>>,
    /// The microtask that notifies them is queued.
    microtask_queued: Cell<bool>,
}

pub struct MutationObserverObject {
    callback: Callback,
    /// The nodes whose registered observer lists name this observer.
    nodes: Vec<NodeId>,
    /// The nodes carrying transient registrations for it.
    transient: Vec<NodeId>,
    /// The record queue.
    records: Vec<ObjectId>,
}
platform_object!(MutationObserverObject, MutationObserver);

pub struct MutationRecordObject {
    kind: &'static str,
    target: NodeId,
    added: Vec<NodeId>,
    removed: Vec<NodeId>,
    previous_sibling: Option<NodeId>,
    next_sibling: Option<NodeId>,
    attribute_name: Option<String>,
    attribute_namespace: Option<String>,
    old_value: Option<String>,
}
platform_object!(MutationRecordObject, MutationRecord);

/// What a record reports.
enum Change<'a> {
    Attributes {
        name: &'a str,
        namespace: Option<&'a str>,
        old: Option<&'a str>,
    },
    CharacterData {
        old: &'a str,
    },
    ChildList {
        added: &'a [NodeId],
        removed: &'a [NodeId],
        previous: Option<NodeId>,
        next: Option<NodeId>,
    },
}

impl Change<'_> {
    fn record(&self, target: NodeId, with_old_value: bool) -> MutationRecordObject {
        let mut record = MutationRecordObject {
            kind: "childList",
            target,
            added: Vec::new(),
            removed: Vec::new(),
            previous_sibling: None,
            next_sibling: None,
            attribute_name: None,
            attribute_namespace: None,
            old_value: None,
        };
        match *self {
            Change::Attributes {
                name,
                namespace,
                old,
            } => {
                record.kind = "attributes";
                record.attribute_name = Some(name.to_string());
                record.attribute_namespace = namespace.map(str::to_string);
                if with_old_value {
                    record.old_value = old.map(str::to_string);
                }
            }
            Change::CharacterData { old } => {
                record.kind = "characterData";
                if with_old_value {
                    record.old_value = Some(old.to_string());
                }
            }
            Change::ChildList {
                added,
                removed,
                previous,
                next,
            } => {
                record.added = added.to_vec();
                record.removed = removed.to_vec();
                record.previous_sibling = previous;
                record.next_sibling = next;
            }
        }
        record
    }
}

/// Whether anything is observed (the fast path for every mutation).
pub(crate) fn active(page: &PageState) -> bool {
    !page.mutation.registered.borrow().is_empty()
}

/// Has `observer` notified at the next microtask checkpoint.
fn schedule(page: &PageState, observer: ObjectId) {
    {
        let mut pending = page.mutation.pending.borrow_mut();
        if !pending.contains(&observer) {
            pending.push(observer);
        }
    }
    if !page.mutation.microtask_queued.replace(true) {
        page.queue_microtask(notify);
    }
}

/// <https://dom.spec.whatwg.org/#queue-a-mutation-record>
fn queue(page: &PageState, target: NodeId, change: Change<'_>) {
    // The interested observers, each with whether it wants the old value.
    let mut interested: Vec<(ObjectId, bool)> = Vec::new();
    {
        let dom = page.dom.borrow();
        if !dom.contains(target) {
            return;
        }
        let registered = page.mutation.registered.borrow();
        for node in std::iter::once(target).chain(dom.ancestors(target)) {
            let Some(list) = registered.get(&node) else {
                continue;
            };
            for entry in list {
                let options = &entry.options;
                if node != target && !options.subtree {
                    continue;
                }
                let wants_old = match &change {
                    Change::Attributes {
                        name, namespace, ..
                    } => {
                        let filtered_out = options.attribute_filter.as_ref().is_some_and(|f| {
                            namespace.is_some() || !f.iter().any(|listed| listed == name)
                        });
                        if !options.attributes || filtered_out {
                            continue;
                        }
                        options.attribute_old_value
                    }
                    Change::CharacterData { .. } => {
                        if !options.character_data {
                            continue;
                        }
                        options.character_data_old_value
                    }
                    Change::ChildList { .. } => {
                        if !options.child_list {
                            continue;
                        }
                        false
                    }
                };
                match interested.iter_mut().find(|(o, _)| *o == entry.observer) {
                    Some((_, old)) => *old |= wants_old,
                    None => interested.push((entry.observer, wants_old)),
                }
            }
        }
    }
    for (observer, with_old_value) in interested {
        let record = page.alloc(change.record(target, with_old_value));
        let queued = page
            .try_with::<MutationObserverObject, _>(observer, |o| o.records.push(record))
            .is_some();
        if queued {
            schedule(page, observer);
        } else {
            page.free_object(record);
        }
    }
}

/// Queues an `attributes` record. `old` is the value the attribute had, if
/// it existed.
pub(crate) fn queue_attribute(
    page: &PageState,
    element: NodeId,
    name: &str,
    namespace: Option<&str>,
    old: Option<&str>,
) {
    if active(page) {
        let change = Change::Attributes {
            name,
            namespace,
            old,
        };
        queue(page, element, change);
    }
}

/// Queues a `characterData` record. `old` is the data the node had.
pub(crate) fn queue_character_data(page: &PageState, node: NodeId, old: &str) {
    if active(page) {
        queue(page, node, Change::CharacterData { old });
    }
}

/// Queues a `childList` record: `added` went in and `removed` came out
/// between `previous` and `next`.
pub(crate) fn queue_child_list(
    page: &PageState,
    target: NodeId,
    added: &[NodeId],
    removed: &[NodeId],
    previous: Option<NodeId>,
    next: Option<NodeId>,
) {
    if active(page) && !(added.is_empty() && removed.is_empty()) {
        let change = Change::ChildList {
            added,
            removed,
            previous,
            next,
        };
        queue(page, target, change);
    }
}

/// Called when `node` was removed from `parent`. Those observing the
/// subtree it was in keep hearing about `node`'s own subtree until they are
/// next notified, although it is no longer below what they observe.
pub(crate) fn node_removed(page: &PageState, node: NodeId, parent: NodeId) {
    if !active(page) {
        return;
    }
    let mut followers: Vec<Registered> = Vec::new();
    {
        let dom = page.dom.borrow();
        let registered = page.mutation.registered.borrow();
        let already = registered.get(&node);
        for ancestor in std::iter::once(parent).chain(dom.ancestors(parent)) {
            for entry in registered.get(&ancestor).into_iter().flatten() {
                if !entry.options.subtree {
                    continue;
                }
                let source = entry.source.or(Some(ancestor));
                let is_same = |r: &Registered| r.observer == entry.observer && r.source == source;
                if already.is_some_and(|list| list.iter().any(is_same))
                    || followers.iter().any(is_same)
                {
                    continue;
                }
                followers.push(Registered {
                    observer: entry.observer,
                    options: entry.options.clone(),
                    source,
                });
            }
        }
    }
    if followers.is_empty() {
        return;
    }
    for follower in &followers {
        let observer = follower.observer;
        let known = page.try_with::<MutationObserverObject, _>(observer, |o| {
            if !o.transient.contains(&node) {
                o.transient.push(node);
            }
        });
        if known.is_some() {
            // The registration ends when the observer is notified, which
            // must therefore happen even if it gets no record.
            schedule(page, observer);
        }
    }
    let mut registered = page.mutation.registered.borrow_mut();
    registered.entry(node).or_default().extend(followers);
}

/// Reports the tree changes the parser made.
pub(crate) fn parser_changed(page: &PageState, changes: &[TreeChange]) {
    if !active(page) {
        return;
    }
    for change in changes {
        match *change {
            TreeChange::Inserted {
                parent,
                node,
                prev,
                next,
            } => queue_child_list(page, parent, &[node], &[], prev, next),
            TreeChange::Removed {
                parent,
                node,
                prev,
                next,
            } => {
                node_removed(page, node, parent);
                queue_child_list(page, parent, &[], &[node], prev, next);
            }
        }
    }
}

/// Removes the registrations of `observer` on `nodes` that `drop` selects.
fn unregister(
    page: &PageState,
    observer: ObjectId,
    nodes: &[NodeId],
    drop: impl Fn(&Registered) -> bool,
) {
    let mut registered = page.mutation.registered.borrow_mut();
    for node in nodes {
        let Some(list) = registered.get_mut(node) else {
            continue;
        };
        list.retain(|r| r.observer != observer || !drop(r));
        if list.is_empty() {
            registered.remove(node);
        }
    }
}

/// <https://dom.spec.whatwg.org/#notify-mutation-observers>
fn notify(cx: &mut Cx<'_>) {
    let page = cx.page;
    page.mutation.microtask_queued.set(false);
    let observers = std::mem::take(&mut *page.mutation.pending.borrow_mut());
    for observer in observers {
        let taken = page.try_with::<MutationObserverObject, _>(observer, |o| {
            (
                o.callback.clone(),
                std::mem::take(&mut o.records),
                std::mem::take(&mut o.transient),
            )
        });
        let Some((callback, records, transient)) = taken else {
            continue;
        };
        unregister(page, observer, &transient, |r| r.source.is_some());
        if records.is_empty() {
            continue;
        }
        let records = Value::Array(records.into_iter().map(Value::Object).collect());
        let this = Value::Object(observer);
        if let Err(e) = cx.script.call(&callback, &this, &[records, this.clone()]) {
            cx.report_exception(&e);
        }
    }
}

impl web::MutationObserverImpl for Web {
    fn observe(
        cx: &mut Cx<'_>,
        this: ObjectId,
        target: NodeId,
        options: web::MutationObserverInit,
    ) -> Fallible<()> {
        // Asking for old values or a filter implies observing that kind.
        let attributes = options
            .attributes
            .unwrap_or(options.attribute_old_value.is_some() || options.attribute_filter.is_some());
        let character_data = options
            .character_data
            .unwrap_or(options.character_data_old_value.is_some());
        let attribute_old_value = options.attribute_old_value.unwrap_or(false);
        let character_data_old_value = options.character_data_old_value.unwrap_or(false);
        if !options.child_list && !attributes && !character_data {
            return Err(Exception::type_error(
                "The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.",
            ));
        }
        if (attribute_old_value || options.attribute_filter.is_some()) && !attributes {
            return Err(Exception::type_error(
                "The options object may only set 'attributeOldValue' or 'attributeFilter' when 'attributes' is true or not present.",
            ));
        }
        if character_data_old_value && !character_data {
            return Err(Exception::type_error(
                "The options object may only set 'characterDataOldValue' when 'characterData' is true or not present.",
            ));
        }
        let options = Rc::new(Options {
            child_list: options.child_list,
            attributes,
            character_data,
            subtree: options.subtree,
            attribute_old_value,
            character_data_old_value,
            attribute_filter: options.attribute_filter,
        });

        let page = cx.page;
        let transient = page.with::<MutationObserverObject, _>(this, |o| o.transient.clone())?;
        let replaced = {
            let mut registered = page.mutation.registered.borrow_mut();
            let list = registered.entry(target).or_default();
            let existing = list
                .iter_mut()
                .find(|r| r.observer == this && r.source.is_none());
            match existing {
                Some(existing) => {
                    existing.options = options;
                    true
                }
                None => {
                    list.push(Registered {
                        observer: this,
                        options,
                        source: None,
                    });
                    false
                }
            }
        };
        if replaced {
            // The subtrees followed on behalf of the old options are let go.
            unregister(page, this, &transient, |r| r.source == Some(target));
        } else {
            let first = page.with::<MutationObserverObject, _>(this, |o| {
                o.nodes.push(target);
                o.nodes.len() == 1
            })?;
            if first {
                // An observer is kept alive by the nodes it observes, not
                // by script holding on to it.
                cx.pin(this);
            }
        }
        Ok(())
    }

    fn disconnect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let page = cx.page;
        let (nodes, transient, records) = page.with::<MutationObserverObject, _>(this, |o| {
            (
                std::mem::take(&mut o.nodes),
                std::mem::take(&mut o.transient),
                std::mem::take(&mut o.records),
            )
        })?;
        unregister(page, this, &nodes, |_| true);
        unregister(page, this, &transient, |_| true);
        page.mutation.pending.borrow_mut().retain(|o| *o != this);
        // Undelivered records were never seen by script.
        for record in records {
            page.free_object(record);
        }
        if !nodes.is_empty() {
            cx.unpin(this);
        }
        Ok(())
    }

    fn take_records(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        cx.page
            .with::<MutationObserverObject, _>(this, |o| std::mem::take(&mut o.records))
    }

    fn constructor(cx: &mut Cx<'_>, callback: Callback) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(MutationObserverObject {
            callback,
            nodes: Vec::new(),
            transient: Vec::new(),
            records: Vec::new(),
        }))
    }
}

fn record<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&MutationRecordObject) -> R,
) -> Fallible<R> {
    cx.page.with::<MutationRecordObject, _>(this, |r| f(r))
}

impl web::MutationRecordImpl for Web {
    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        record(cx, this, |r| r.kind.to_string())
    }

    fn target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        record(cx, this, |r| r.target)
    }

    fn added_nodes(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let nodes = record(cx, this, |r| r.added.clone())?;
        Ok(collections::static_node_list(cx.page, nodes))
    }

    fn removed_nodes(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let nodes = record(cx, this, |r| r.removed.clone())?;
        Ok(collections::static_node_list(cx.page, nodes))
    }

    fn previous_sibling(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        record(cx, this, |r| r.previous_sibling)
    }

    fn next_sibling(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        record(cx, this, |r| r.next_sibling)
    }

    fn attribute_name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        record(cx, this, |r| r.attribute_name.clone())
    }

    fn attribute_namespace(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        record(cx, this, |r| r.attribute_namespace.clone())
    }

    fn old_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        record(cx, this, |r| r.old_value.clone())
    }
}
