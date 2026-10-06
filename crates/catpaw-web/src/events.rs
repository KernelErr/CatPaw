//! Events: the `Event` object, listener lists, event handler attributes and
//! the dispatch algorithm (<https://dom.spec.whatwg.org/#dispatching-events>).

use std::cell::Cell;
use std::rc::Rc;

use catpaw_dom::NodeKind;
use catpaw_js::{Callback, EventTargetRef, Exception, Fallible, ObjectId, Value};

use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

pub const PHASE_NONE: u16 = 0;
pub const PHASE_CAPTURING: u16 = 1;
pub const PHASE_AT_TARGET: u16 = 2;
pub const PHASE_BUBBLING: u16 = 3;

/// The state behind every event interface (`Event`, `CustomEvent`, ...).
pub struct Event {
    pub iface: InterfaceId,
    pub type_: String,
    pub bubbles: bool,
    pub cancelable: bool,
    pub composed: bool,
    pub target: Option<EventTargetRef>,
    pub current_target: Option<EventTargetRef>,
    pub phase: u16,
    pub stop_propagation: bool,
    pub stop_immediate: bool,
    pub canceled: bool,
    pub in_passive_listener: bool,
    pub dispatching: bool,
    pub initialized: bool,
    pub trusted: bool,
    /// Milliseconds since the time origin.
    pub time_stamp: f64,
    /// The targets the event visits, innermost first (while dispatching).
    pub path: Vec<EventTargetRef>,
    /// `CustomEvent.detail`.
    pub detail: Value,
}

platform_object!(Event, |e| e.iface);

impl Event {
    /// An initialized, untrusted event of the base `Event` interface.
    pub fn new(type_: impl Into<String>, bubbles: bool, cancelable: bool, time_stamp: f64) -> Self {
        Self {
            iface: InterfaceId::Event,
            type_: type_.into(),
            bubbles,
            cancelable,
            composed: false,
            target: None,
            current_target: None,
            phase: PHASE_NONE,
            stop_propagation: false,
            stop_immediate: false,
            canceled: false,
            in_passive_listener: false,
            dispatching: false,
            initialized: true,
            trusted: false,
            time_stamp,
            path: Vec::new(),
            detail: Value::Null,
        }
    }

    fn cancel(&mut self) {
        if self.cancelable && !self.in_passive_listener {
            self.canceled = true;
        }
    }
}

/// The object behind `new EventTarget()`.
pub struct EventTargetObject;
platform_object!(EventTargetObject, EventTarget);

#[derive(Clone)]
pub(crate) enum ListenerKind {
    /// Added with `addEventListener`.
    Listener(Callback),
    /// The slot of an event handler attribute (`onclick`); `None` while the
    /// handler is unset.
    Handler(Option<Callback>),
}

#[derive(Clone)]
pub struct Listener {
    pub(crate) type_: Rc<str>,
    pub(crate) kind: ListenerKind,
    pub(crate) capture: bool,
    pub(crate) once: bool,
    pub(crate) passive: bool,
    pub(crate) removed: Rc<Cell<bool>>,
}

/// Event handlers that `<body>` and `<frameset>` forward to the window.
const WINDOW_FORWARDED: &[&str] = &[
    "afterprint",
    "beforeprint",
    "beforeunload",
    "blur",
    "error",
    "focus",
    "hashchange",
    "languagechange",
    "load",
    "message",
    "messageerror",
    "offline",
    "online",
    "pagehide",
    "pageshow",
    "popstate",
    "rejectionhandled",
    "resize",
    "scroll",
    "storage",
    "unhandledrejection",
    "unload",
];

fn is_body_like(page: &PageState, target: EventTargetRef) -> bool {
    let EventTargetRef::Node(n) = target else {
        return false;
    };
    let dom = page.dom.borrow();
    dom.is_html_element(n, "body") || dom.is_html_element(n, "frameset")
}

/// Where the handler for `type_` set on `target` actually lives.
fn handler_owner(page: &PageState, target: EventTargetRef, type_: &str) -> EventTargetRef {
    if WINDOW_FORWARDED.contains(&type_) && is_body_like(page, target) {
        EventTargetRef::Window
    } else {
        target
    }
}

/// The source of the `on<type>` content attribute that applies to `owner`.
fn content_handler_source(page: &PageState, owner: EventTargetRef, type_: &str) -> Option<String> {
    let dom = page.dom.borrow();
    let name = format!("on{type_}");
    match owner {
        EventTargetRef::Node(n) => dom.attr(n, &name).map(str::to_string),
        EventTargetRef::Window if WINDOW_FORWARDED.contains(&type_) => {
            let html = dom.child_elements(dom.document()).next()?;
            let body = dom
                .child_elements(html)
                .find(|&c| dom.is_html_element(c, "body") || dom.is_html_element(c, "frameset"))?;
            dom.attr(body, &name).map(str::to_string)
        }
        _ => None,
    }
}

fn has_handler_slot(page: &PageState, owner: EventTargetRef, type_: &str) -> bool {
    page.listeners.borrow().get(&owner).is_some_and(|list| {
        list.iter()
            .any(|l| &*l.type_ == type_ && matches!(l.kind, ListenerKind::Handler(_)))
    })
}

fn push_listener(page: &PageState, target: EventTargetRef, listener: Listener) {
    page.listeners
        .borrow_mut()
        .entry(target)
        .or_default()
        .push(listener);
}

/// Compiles the `on<type>` content attribute into the handler slot if the
/// slot does not exist yet.
fn activate_content_handler(cx: &mut Cx<'_>, owner: EventTargetRef, type_: &str) {
    if has_handler_slot(cx.page, owner, type_) {
        return;
    }
    let Some(source) = content_handler_source(cx.page, owner, type_) else {
        return;
    };
    let url = cx.page.url.borrow().to_string();
    let compiled = match cx.script.compile_function(&["event"], &source, &url) {
        Ok(callback) => Some(callback),
        Err(e) => {
            cx.report_exception(&e);
            None
        }
    };
    push_listener(
        cx.page,
        owner,
        Listener {
            type_: Rc::from(type_),
            kind: ListenerKind::Handler(compiled),
            capture: false,
            once: false,
            passive: false,
            removed: Rc::new(Cell::new(false)),
        },
    );
}

/// Drops the compiled handler for an `on*` content attribute that changed,
/// so the next dispatch (or read of the IDL attribute) recompiles it.
pub(crate) fn content_handler_changed(page: &PageState, node: catpaw_dom::NodeId, attr: &str) {
    let Some(type_) = attr.strip_prefix("on") else {
        return;
    };
    let owner = handler_owner(page, EventTargetRef::Node(node), type_);
    if let Some(list) = page.listeners.borrow_mut().get_mut(&owner) {
        list.retain(|l| {
            let stale = &*l.type_ == type_ && matches!(l.kind, ListenerKind::Handler(_));
            if stale {
                l.removed.set(true);
            }
            !stale
        });
    }
}

/// The getter of an event handler IDL attribute (`el.onclick`).
pub fn event_handler(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
) -> Fallible<Option<Callback>> {
    let owner = handler_owner(cx.page, target, type_);
    activate_content_handler(cx, owner, type_);
    let listeners = cx.page.listeners.borrow();
    Ok(listeners.get(&owner).and_then(|list| {
        list.iter().find_map(|l| match &l.kind {
            ListenerKind::Handler(h) if &*l.type_ == type_ => h.clone(),
            _ => None,
        })
    }))
}

/// The setter of an event handler IDL attribute.
pub fn set_event_handler(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
    handler: Option<Callback>,
) -> Fallible<()> {
    let owner = handler_owner(cx.page, target, type_);
    let mut listeners = cx.page.listeners.borrow_mut();
    let list = listeners.entry(owner).or_default();
    for l in list.iter_mut() {
        if &*l.type_ == type_
            && let ListenerKind::Handler(slot) = &mut l.kind
        {
            *slot = handler;
            return Ok(());
        }
    }
    // The slot is created on first use, even when set to null: it then
    // shadows the content attribute, as in browsers.
    list.push(Listener {
        type_: Rc::from(type_),
        kind: ListenerKind::Handler(handler),
        capture: false,
        once: false,
        passive: false,
        removed: Rc::new(Cell::new(false)),
    });
    Ok(())
}

/// `addEventListener`.
pub fn add_listener(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
    callback: Callback,
    capture: bool,
    once: bool,
    passive: bool,
) {
    let existing: Vec<Callback> = cx
        .page
        .listeners
        .borrow()
        .get(&target)
        .map(|list| {
            list.iter()
                .filter(|l| &*l.type_ == type_ && l.capture == capture)
                .filter_map(|l| match &l.kind {
                    ListenerKind::Listener(c) => Some(c.clone()),
                    ListenerKind::Handler(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if existing
        .iter()
        .any(|c| cx.script.same_callback(c, &callback))
    {
        return;
    }
    push_listener(
        cx.page,
        target,
        Listener {
            type_: Rc::from(type_),
            kind: ListenerKind::Listener(callback),
            capture,
            once,
            passive,
            removed: Rc::new(Cell::new(false)),
        },
    );
}

/// `removeEventListener`.
pub fn remove_listener(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
    callback: &Callback,
    capture: bool,
) {
    let candidates: Vec<(Rc<Cell<bool>>, Callback)> = cx
        .page
        .listeners
        .borrow()
        .get(&target)
        .map(|list| {
            list.iter()
                .filter(|l| &*l.type_ == type_ && l.capture == capture)
                .filter_map(|l| match &l.kind {
                    ListenerKind::Listener(c) => Some((l.removed.clone(), c.clone())),
                    ListenerKind::Handler(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for (removed, candidate) in candidates {
        if cx.script.same_callback(&candidate, callback) {
            removed.set(true);
            if let Some(list) = cx.page.listeners.borrow_mut().get_mut(&target) {
                list.retain(|l| !l.removed.get());
            }
            return;
        }
    }
}

/// Whether anything listens for `type_` on `target` itself.
pub fn has_listeners(page: &PageState, target: EventTargetRef, type_: &str) -> bool {
    page.listeners
        .borrow()
        .get(&target)
        .is_some_and(|list| list.iter().any(|l| &*l.type_ == type_))
}

/// The targets an event dispatched at `target` visits, innermost first.
fn event_path(page: &PageState, target: EventTargetRef, type_: &str) -> Vec<EventTargetRef> {
    let EventTargetRef::Node(node) = target else {
        return vec![target];
    };
    let dom = page.dom.borrow();
    if !dom.contains(node) {
        return vec![target];
    }
    let mut path = vec![target];
    let mut root = node;
    for ancestor in dom.ancestors(node) {
        path.push(EventTargetRef::Node(ancestor));
        root = ancestor;
    }
    // The window is the document's parent for events, except for `load`.
    if matches!(dom.kind(root), NodeKind::Document(_)) && type_ != "load" {
        path.push(EventTargetRef::Window);
    }
    path
}

fn invoke(
    cx: &mut Cx<'_>,
    current: EventTargetRef,
    event: ObjectId,
    type_: &str,
    phase: u16,
    capture: bool,
) {
    let stopped = cx
        .page
        .with::<Event, _>(event, |e| {
            if e.stop_propagation {
                return true;
            }
            e.phase = phase;
            e.current_target = Some(current);
            false
        })
        .unwrap_or(true);
    if stopped {
        return;
    }
    if !capture {
        activate_content_handler(cx, current, type_);
    }
    let listeners: Vec<Listener> = cx
        .page
        .listeners
        .borrow()
        .get(&current)
        .map(|list| {
            list.iter()
                .filter(|l| &*l.type_ == type_ && l.capture == capture)
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    for listener in listeners {
        if listener.removed.get() {
            continue;
        }
        let (callback, is_handler) = match &listener.kind {
            ListenerKind::Listener(c) => (c.clone(), false),
            ListenerKind::Handler(Some(c)) => (c.clone(), true),
            ListenerKind::Handler(None) => continue,
        };
        if listener.once {
            listener.removed.set(true);
            if let Some(list) = cx.page.listeners.borrow_mut().get_mut(&current) {
                list.retain(|l| !l.removed.get());
            }
        }

        let previous = cx.page.current_event.replace(Some(event));
        let _ = cx
            .page
            .with::<Event, _>(event, |e| e.in_passive_listener = listener.passive);
        let result = cx
            .script
            .call(&callback, &Value::from(current), &[Value::Object(event)]);
        let _ = cx
            .page
            .with::<Event, _>(event, |e| e.in_passive_listener = false);
        cx.page.current_event.set(previous);

        match result {
            // An event handler returning false cancels the event.
            Ok(Value::Bool(false)) if is_handler => {
                let _ = cx.page.with::<Event, _>(event, Event::cancel);
            }
            Ok(_) => {}
            Err(e) => cx.report_exception(&e),
        }
        let stop = cx
            .page
            .with::<Event, _>(event, |e| e.stop_immediate)
            .unwrap_or(true);
        if stop {
            break;
        }
    }
}

/// Dispatches `event` at `target`. Returns `false` if it was canceled.
pub fn dispatch(cx: &mut Cx<'_>, target: EventTargetRef, event: ObjectId) -> bool {
    let Ok((type_, bubbles)) = cx.page.with::<Event, _>(event, |e| {
        e.dispatching = true;
        e.target = Some(target);
        (e.type_.clone(), e.bubbles)
    }) else {
        return true;
    };
    let path = event_path(cx.page, target, &type_);
    let _ = cx.page.with::<Event, _>(event, |e| e.path = path.clone());
    cx.pin(event);

    // Capture: outermost to innermost. The target itself only runs its
    // capturing listeners here.
    for (i, &current) in path.iter().enumerate().rev() {
        let phase = if i == 0 {
            PHASE_AT_TARGET
        } else {
            PHASE_CAPTURING
        };
        invoke(cx, current, event, &type_, phase, true);
    }
    // Bubble: innermost to outermost.
    for (i, &current) in path.iter().enumerate() {
        if i > 0 && !bubbles {
            break;
        }
        let phase = if i == 0 {
            PHASE_AT_TARGET
        } else {
            PHASE_BUBBLING
        };
        invoke(cx, current, event, &type_, phase, false);
    }

    let canceled = cx
        .page
        .with::<Event, _>(event, |e| {
            e.phase = PHASE_NONE;
            e.current_target = None;
            e.path.clear();
            e.dispatching = false;
            e.stop_propagation = false;
            e.stop_immediate = false;
            e.canceled
        })
        .unwrap_or(false);
    cx.unpin(event);
    !canceled
}

/// Creates a trusted event of the base `Event` interface.
pub fn create_event(page: &PageState, type_: &str, bubbles: bool, cancelable: bool) -> ObjectId {
    let mut event = Event::new(type_, bubbles, cancelable, page.clock.peek());
    event.trusted = true;
    page.alloc(event)
}

/// Creates and dispatches a trusted event. Returns `false` if canceled.
pub fn fire(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
    bubbles: bool,
    cancelable: bool,
) -> bool {
    let event = create_event(cx.page, type_, bubbles, cancelable);
    // Pin across the dispatch so that unpinning afterwards frees the event
    // when script never saw it.
    cx.pin(event);
    let result = dispatch(cx, target, event);
    cx.unpin(event);
    result
}

// ---------------------------------------------------------------- bindings

impl web::EventTargetImpl for Web {
    fn add_event_listener(
        cx: &mut Cx<'_>,
        this: EventTargetRef,
        type_: String,
        callback: Option<Callback>,
        options: web::AddEventListenerOptionsOrBoolean,
    ) -> Fallible<()> {
        let Some(callback) = callback else {
            return Ok(());
        };
        let (capture, once, passive) = match options {
            web::AddEventListenerOptionsOrBoolean::Boolean(capture) => (capture, false, false),
            web::AddEventListenerOptionsOrBoolean::AddEventListenerOptions(o) => {
                (o.capture, o.once, o.passive.unwrap_or(false))
            }
        };
        add_listener(cx, this, &type_, callback, capture, once, passive);
        Ok(())
    }

    fn remove_event_listener(
        cx: &mut Cx<'_>,
        this: EventTargetRef,
        type_: String,
        callback: Option<Callback>,
        options: web::EventListenerOptionsOrBoolean,
    ) -> Fallible<()> {
        let Some(callback) = callback else {
            return Ok(());
        };
        let capture = match options {
            web::EventListenerOptionsOrBoolean::Boolean(capture) => capture,
            web::EventListenerOptionsOrBoolean::EventListenerOptions(o) => o.capture,
        };
        remove_listener(cx, this, &type_, &callback, capture);
        Ok(())
    }

    fn dispatch_event(cx: &mut Cx<'_>, this: EventTargetRef, event: ObjectId) -> Fallible<bool> {
        let ready = cx.page.with::<Event, _>(event, |e| {
            if e.dispatching || !e.initialized {
                return false;
            }
            e.trusted = false;
            true
        })?;
        if !ready {
            return Err(Exception::invalid_state(
                "The event is already being dispatched or is not initialized",
            ));
        }
        Ok(dispatch(cx, this, event))
    }

    fn constructor(cx: &mut Cx<'_>) -> Fallible<EventTargetRef> {
        Ok(EventTargetRef::Object(cx.page.alloc(EventTargetObject)))
    }
}

fn get<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut Event) -> R) -> Fallible<R> {
    cx.page.with::<Event, _>(this, f)
}

impl web::EventImpl for Web {
    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        get(cx, this, |e| e.type_.clone())
    }

    fn target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<EventTargetRef>> {
        get(cx, this, |e| e.target)
    }

    fn src_element(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<EventTargetRef>> {
        get(cx, this, |e| e.target)
    }

    fn current_target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<EventTargetRef>> {
        get(cx, this, |e| e.current_target)
    }

    fn composed_path(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<EventTargetRef>> {
        get(cx, this, |e| e.path.clone())
    }

    fn event_phase(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        get(cx, this, |e| e.phase)
    }

    fn stop_propagation(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        get(cx, this, |e| e.stop_propagation = true)
    }

    fn cancel_bubble(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.stop_propagation)
    }

    fn set_cancel_bubble(cx: &mut Cx<'_>, this: ObjectId, value: bool) -> Fallible<()> {
        get(cx, this, |e| {
            if value {
                e.stop_propagation = true;
            }
        })
    }

    fn stop_immediate_propagation(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        get(cx, this, |e| {
            e.stop_propagation = true;
            e.stop_immediate = true;
        })
    }

    fn bubbles(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.bubbles)
    }

    fn cancelable(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.cancelable)
    }

    fn return_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| !e.canceled)
    }

    fn set_return_value(cx: &mut Cx<'_>, this: ObjectId, value: bool) -> Fallible<()> {
        get(cx, this, |e| {
            if !value {
                e.cancel();
            }
        })
    }

    fn prevent_default(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        get(cx, this, Event::cancel)
    }

    fn default_prevented(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.canceled)
    }

    fn composed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.composed)
    }

    fn is_trusted(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| e.trusted)
    }

    fn time_stamp(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        get(cx, this, |e| e.time_stamp)
    }

    fn init_event(
        cx: &mut Cx<'_>,
        this: ObjectId,
        type_: String,
        bubbles: bool,
        cancelable: bool,
    ) -> Fallible<()> {
        get(cx, this, |e| initialize(e, type_, bubbles, cancelable))
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        event_init_dict: web::EventInit,
    ) -> Fallible<ObjectId> {
        let mut event = Event::new(
            type_,
            event_init_dict.bubbles,
            event_init_dict.cancelable,
            cx.page.clock.peek(),
        );
        event.composed = event_init_dict.composed;
        Ok(cx.page.alloc(event))
    }
}

/// <https://dom.spec.whatwg.org/#concept-event-initialize>, as used by the
/// legacy `init*Event` methods.
fn initialize(e: &mut Event, type_: String, bubbles: bool, cancelable: bool) {
    if e.dispatching {
        return;
    }
    e.initialized = true;
    e.stop_propagation = false;
    e.stop_immediate = false;
    e.canceled = false;
    e.trusted = false;
    e.target = None;
    e.type_ = type_;
    e.bubbles = bubbles;
    e.cancelable = cancelable;
}

/// An uninitialized event, as `document.createEvent` returns.
pub(crate) fn uninitialized_event(page: &PageState, iface: InterfaceId) -> ObjectId {
    let mut event = Event::new("", false, false, page.clock.peek());
    event.iface = iface;
    event.initialized = false;
    page.alloc(event)
}

impl web::CustomEventImpl for Web {
    fn detail(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        get(cx, this, |e| e.detail.clone())
    }

    fn init_custom_event(
        cx: &mut Cx<'_>,
        this: ObjectId,
        type_: String,
        bubbles: bool,
        cancelable: bool,
        detail: Value,
    ) -> Fallible<()> {
        get(cx, this, |e| {
            if !e.dispatching {
                initialize(e, type_, bubbles, cancelable);
                e.detail = detail;
            }
        })
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        event_init_dict: web::CustomEventInit,
    ) -> Fallible<ObjectId> {
        let mut event = Event::new(
            type_,
            event_init_dict.bubbles,
            event_init_dict.cancelable,
            cx.page.clock.peek(),
        );
        event.iface = InterfaceId::CustomEvent;
        event.composed = event_init_dict.composed;
        event.detail = event_init_dict.detail;
        Ok(cx.page.alloc(event))
    }
}
