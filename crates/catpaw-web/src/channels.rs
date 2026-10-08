//! `MessageChannel`, `MessagePort` and `BroadcastChannel` within one
//! realm. Ports are not transferable to other realms yet: a port sent to
//! a frame or worker does not arrive there.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use catpaw_js::{EventTargetRef, Fallible, ObjectId, Value};

use crate::frames::{self, MessageData};
use crate::generated::{self as web};
use crate::page::{Cx, PageState};
use crate::{Web, event_loop, platform_object};

/// A port of a channel. Messages posted to it queue until the port is
/// started (`start()`, or a `message` handler being set), then dispatch
/// on the entangled port.
pub struct MessagePortObject {
    /// The other end, until either side closes.
    other: Cell<Option<ObjectId>>,
    started: Cell<bool>,
    closed: Cell<bool>,
    /// Messages for this port, waiting for it to start.
    pending: RefCell<VecDeque<Value>>,
}
platform_object!(MessagePortObject, MessagePort);

pub struct MessageChannelObject {
    port1: ObjectId,
    port2: ObjectId,
}
platform_object!(MessageChannelObject, MessageChannel);

/// A `BroadcastChannel`: every open channel of the same name in this
/// realm receives what one of them posts.
pub struct BroadcastChannelObject {
    name: String,
    closed: Cell<bool>,
}
platform_object!(BroadcastChannelObject, BroadcastChannel);

/// The open broadcast channels of the page.
#[derive(Default)]
pub struct Channels {
    broadcast: RefCell<Vec<ObjectId>>,
}

fn new_port(cx: &mut Cx<'_>) -> ObjectId {
    let id = cx.page.alloc(MessagePortObject {
        other: Cell::new(None),
        started: Cell::new(false),
        closed: Cell::new(false),
        pending: RefCell::new(VecDeque::new()),
    });
    // A port stays reachable while its channel may deliver to it: the
    // other end holds it. Both are pinned until closed.
    cx.pin(id);
    id
}

fn with_port<R>(
    cx: &Cx<'_>,
    port: ObjectId,
    f: impl FnOnce(&MessagePortObject) -> R,
) -> Fallible<R> {
    cx.page.with::<MessagePortObject, _>(port, |p| f(p))
}

/// Queues a task delivering `data` on `port`, if the port is started.
/// Otherwise the message waits.
fn deliver(page: &PageState, port: ObjectId, data: Value) {
    let started = page.try_with::<MessagePortObject, _>(port, |p| {
        if p.closed.get() {
            return None;
        }
        if !p.started.get() {
            p.pending.borrow_mut().push_back(data.clone());
            return Some(false);
        }
        Some(true)
    });
    if started != Some(Some(true)) {
        return;
    }
    event_loop::queue_task(page, "port message", move |cx| {
        let open = cx
            .page
            .try_with::<MessagePortObject, _>(port, |p| !p.closed.get())
            .unwrap_or(false);
        if open {
            frames::dispatch_message(
                cx,
                EventTargetRef::Object(port),
                MessageData::Value(data),
                String::new(),
                None,
            );
        }
    });
}

/// Starts a port: what waited is delivered.
pub(crate) fn start_port(page: &PageState, port: ObjectId) {
    let waiting = page.try_with::<MessagePortObject, _>(port, |p| {
        p.started.set(true);
        std::mem::take(&mut *p.pending.borrow_mut())
    });
    for data in waiting.into_iter().flatten() {
        deliver(page, port, data);
    }
}

/// Setting a `message` handler on a port starts it (HTML: the port
/// message queue is enabled by `onmessage`).
pub(crate) fn handler_set(page: &PageState, target: EventTargetRef, name: &str) {
    if name == "message"
        && let EventTargetRef::Object(id) = target
        && page.try_with::<MessagePortObject, _>(id, |_| ()).is_some()
    {
        start_port(page, id);
    }
}

fn close_port(cx: &mut Cx<'_>, port: ObjectId) {
    let other = with_port(cx, port, |p| {
        p.closed.set(true);
        p.other.take()
    })
    .unwrap_or(None);
    cx.unpin(port);
    if let Some(other) = other {
        let _ = with_port(cx, other, |p| {
            p.other.set(None);
        });
    }
}

impl web::MessageChannelImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        let port1 = new_port(cx);
        let port2 = new_port(cx);
        with_port(cx, port1, |p| p.other.set(Some(port2)))?;
        with_port(cx, port2, |p| p.other.set(Some(port1)))?;
        Ok(cx.page.alloc(MessageChannelObject { port1, port2 }))
    }

    fn port1(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page.with::<MessageChannelObject, _>(this, |c| c.port1)
    }

    fn port2(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page.with::<MessageChannelObject, _>(this, |c| c.port2)
    }
}

impl web::MessagePortImpl for Web {
    fn post_message(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        _transfer: Vec<Value>,
    ) -> Fallible<()> {
        let other = with_port(cx, this, |p| {
            (!p.closed.get()).then(|| p.other.get()).flatten()
        })?;
        let data = cx.script.structured_clone(&message)?;
        if let Some(other) = other {
            deliver(cx.page, other, data);
        }
        Ok(())
    }

    fn post_message_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        _options: web::StructuredSerializeOptions,
    ) -> Fallible<()> {
        Self::post_message(cx, this, message, Vec::new())
    }

    fn start(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_port(cx, this, |_| ())?;
        start_port(cx.page, this);
        Ok(())
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_port(cx, this, |_| ())?;
        close_port(cx, this);
        Ok(())
    }
}

impl web::BroadcastChannelImpl for Web {
    fn constructor(cx: &mut Cx<'_>, name: String) -> Fallible<ObjectId> {
        let id = cx.page.alloc(BroadcastChannelObject {
            name,
            closed: Cell::new(false),
        });
        cx.pin(id);
        cx.page.channels.broadcast.borrow_mut().push(id);
        Ok(id)
    }

    fn name(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<BroadcastChannelObject, _>(this, |c| c.name.clone())
    }

    fn post_message(cx: &mut Cx<'_>, this: ObjectId, message: Value) -> Fallible<()> {
        let (name, closed) = cx
            .page
            .with::<BroadcastChannelObject, _>(this, |c| (c.name.clone(), c.closed.get()))?;
        if closed {
            return Err(catpaw_js::Exception::dom(
                "InvalidStateError",
                "the BroadcastChannel is closed",
            ));
        }
        let data = cx.script.structured_clone(&message)?;
        let targets: Vec<ObjectId> = cx
            .page
            .channels
            .broadcast
            .borrow()
            .iter()
            .copied()
            .filter(|&c| {
                c != this
                    && cx
                        .page
                        .try_with::<BroadcastChannelObject, _>(c, |o| {
                            o.name == name && !o.closed.get()
                        })
                        .unwrap_or(false)
            })
            .collect();
        for target in targets {
            let data = data.clone();
            event_loop::queue_task(cx.page, "broadcast message", move |cx| {
                // Not inside the call: listeners may change the URL
                // (pushState), which must not find it borrowed.
                let origin = frames::origin_of(&cx.page.url.borrow());
                frames::dispatch_message(
                    cx,
                    EventTargetRef::Object(target),
                    MessageData::Value(data),
                    origin,
                    None,
                );
            });
        }
        Ok(())
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let was_open = cx
            .page
            .with::<BroadcastChannelObject, _>(this, |c| !c.closed.replace(true))?;
        if was_open {
            cx.page
                .channels
                .broadcast
                .borrow_mut()
                .retain(|&c| c != this);
            cx.unpin(this);
        }
        Ok(())
    }
}
