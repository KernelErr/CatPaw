//! Events: the `Event` object, listener lists, event handler attributes and
//! the dispatch algorithm (<https://dom.spec.whatwg.org/#dispatching-events>).

use std::cell::Cell;
use std::rc::Rc;

use catpaw_dom::{FragmentKind, NodeId, NodeKind};
use catpaw_js::{Callback, EventTargetRef, Exception, Fallible, ObjectId, Value};

use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

pub const PHASE_NONE: u16 = 0;
pub const PHASE_CAPTURING: u16 = 1;
pub const PHASE_AT_TARGET: u16 = 2;
pub const PHASE_BUBBLING: u16 = 3;

/// What the interfaces derived from `Event` add to it.
#[derive(Clone, Debug, Default)]
pub enum EventData {
    #[default]
    None,
    Error {
        message: String,
        filename: String,
        lineno: u32,
        colno: u32,
        error: Value,
    },
    PromiseRejection {
        promise: Value,
        reason: Value,
    },
    HashChange {
        old_url: String,
        new_url: String,
    },
    PopState {
        state: Value,
    },
    Progress {
        length_computable: bool,
        loaded: f64,
        total: f64,
    },
    /// The UI event family; see `ui_events`.
    Ui(Box<crate::ui_events::UiEvent>),
    /// `submit`: the button that submitted, if any.
    Submit {
        submitter: Option<NodeId>,
    },
    /// `formdata`: the entry list as a `FormData`, pinned by the form.
    FormData {
        form_data: ObjectId,
    },
    /// `close` on a WebSocket.
    Close {
        was_clean: bool,
        code: u16,
        reason: String,
    },
    /// `message`: see `frames`.
    Message {
        data: Value,
        origin: String,
        last_event_id: String,
        source: Option<web::WindowProxyOrMessagePort>,
    },
}

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
    /// State of the more specific event interfaces.
    pub data: EventData,
}

platform_object!(Event, |e| e.iface, pinned = |e| e.held());

impl Event {
    /// The objects the event pins: a drag event's `DataTransfer`.
    fn held(&self) -> Vec<ObjectId> {
        match &self.data {
            EventData::Ui(state) => state.data_transfer.into_iter().collect(),
            _ => Vec::new(),
        }
    }

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
            data: EventData::None,
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
    crate::channels::handler_set(cx.page, target, type_);
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

/// Whether a listener for `type_` on `target` is passive when its options
/// do not say: scrolling-related events on the window, the document, its
/// document element and its body are
/// (<https://dom.spec.whatwg.org/#default-passive-value>).
fn default_passive(cx: &Cx<'_>, target: EventTargetRef, type_: &str) -> bool {
    if !matches!(type_, "touchstart" | "touchmove" | "wheel" | "mousewheel") {
        return false;
    }
    match target {
        EventTargetRef::Window => true,
        EventTargetRef::Node(node) => {
            let dom = cx.dom();
            let document = dom.owner_document(node);
            node == document
                || dom.child_elements(document).next() == Some(node)
                || crate::document::body(&dom, document) == Some(node)
        }
        EventTargetRef::Object(_) => false,
    }
}

/// `addEventListener`. Returns the flag that marks the new listener as
/// removed, or `None` if an identical listener was already registered.
pub fn add_listener(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    type_: &str,
    callback: Callback,
    capture: bool,
    once: bool,
    passive: bool,
) -> Option<Rc<Cell<bool>>> {
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
        return None;
    }
    let removed = Rc::new(Cell::new(false));
    push_listener(
        cx.page,
        target,
        Listener {
            type_: Rc::from(type_),
            kind: ListenerKind::Listener(callback),
            capture,
            once,
            passive,
            removed: removed.clone(),
        },
    );
    Some(removed)
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

/// The targets an event dispatched at `target` visits, innermost first,
/// each with the target as it is seen from there: from outside a shadow
/// tree, the tree's host stands for what is inside it.
fn event_path(
    page: &PageState,
    target: EventTargetRef,
    type_: &str,
    composed: bool,
) -> Vec<(EventTargetRef, EventTargetRef)> {
    let EventTargetRef::Node(node) = target else {
        return vec![(target, target)];
    };
    let dom = page.dom.borrow();
    if !dom.contains(node) {
        return vec![(target, target)];
    }
    let own_root = dom.root_of(node);
    let mut seen = target;
    let mut path = vec![(target, seen)];
    let mut at = node;
    loop {
        if let Some(parent) = dom.parent(at) {
            at = parent;
            path.push((EventTargetRef::Node(at), seen));
            continue;
        }
        match dom.kind(at) {
            NodeKind::DocumentFragment(FragmentKind::ShadowRoot { host, .. }) => {
                // An event that is not composed stays in the tree it was
                // dispatched in.
                if !composed && at == own_root {
                    break;
                }
                at = *host;
                seen = EventTargetRef::Node(at);
                path.push((EventTargetRef::Node(at), seen));
            }
            // The window is the parent of its document for events, except
            // for `load`.
            NodeKind::Document(_) if at == dom.document() && type_ != "load" => {
                path.push((EventTargetRef::Window, seen));
                break;
            }
            _ => break,
        }
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
        // `window.onerror` receives the error's details instead of the
        // event, and cancels by returning true.
        let error_args = (is_handler && current == EventTargetRef::Window && type_ == "error")
            .then(|| error_handler_args(cx.page, event))
            .flatten();
        let result = match &error_args {
            Some(args) => cx.script.call(&callback, &Value::from(current), args),
            None => cx
                .script
                .call(&callback, &Value::from(current), &[Value::Object(event)]),
        };
        let _ = cx
            .page
            .with::<Event, _>(event, |e| e.in_passive_listener = false);
        cx.page.current_event.set(previous);

        match result {
            Ok(Value::Bool(true)) if error_args.is_some() => {
                let _ = cx.page.with::<Event, _>(event, Event::cancel);
            }
            // An event handler returning false cancels the event.
            Ok(Value::Bool(false)) if is_handler && error_args.is_none() => {
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
    let Ok((type_, bubbles, composed)) = cx.page.with::<Event, _>(event, |e| {
        e.dispatching = true;
        e.target = Some(target);
        (e.type_.clone(), e.bubbles, e.composed)
    }) else {
        return true;
    };
    let path = event_path(cx.page, target, &type_, composed);
    let _ = cx
        .page
        .with::<Event, _>(event, |e| e.path = path.iter().map(|(t, _)| *t).collect());
    cx.pin(event);
    // A target inside a shadow tree is not told of afterwards.
    let in_shadow = match target {
        EventTargetRef::Node(node) => {
            let dom = cx.dom();
            matches!(
                dom.kind(dom.root_of(node)),
                NodeKind::DocumentFragment(FragmentKind::ShadowRoot { .. })
            )
        }
        _ => false,
    };

    // Capture: outermost to innermost. The target itself only runs its
    // capturing listeners here. At each step the target is the one seen
    // from there.
    for &(current, seen) in path.iter().rev() {
        let phase = if seen == current {
            PHASE_AT_TARGET
        } else {
            PHASE_CAPTURING
        };
        let _ = cx.page.with::<Event, _>(event, |e| e.target = Some(seen));
        invoke(cx, current, event, &type_, phase, true);
    }
    // Bubble: innermost to outermost.
    for (i, &(current, seen)) in path.iter().enumerate() {
        let at_target = seen == current;
        // An event that does not bubble still reaches the targets it is
        // retargeted to.
        if i > 0 && !bubbles && !at_target {
            continue;
        }
        let phase = if at_target {
            PHASE_AT_TARGET
        } else {
            PHASE_BUBBLING
        };
        let _ = cx.page.with::<Event, _>(event, |e| e.target = Some(seen));
        invoke(cx, current, event, &type_, phase, false);
    }

    let canceled = cx
        .page
        .with::<Event, _>(event, |e| {
            e.phase = PHASE_NONE;
            e.current_target = None;
            e.path.clear();
            if in_shadow {
                e.target = None;
            }
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
        let (capture, once, passive, signal) = match options {
            web::AddEventListenerOptionsOrBoolean::Boolean(capture) => {
                (capture, false, false, None)
            }
            web::AddEventListenerOptionsOrBoolean::AddEventListenerOptions(o) => {
                let passive = o
                    .passive
                    .unwrap_or_else(|| default_passive(cx, this, &type_));
                (o.capture, o.once, passive, o.signal)
            }
        };
        // A listener tied to an aborted signal is never added.
        if signal.is_some_and(|s| crate::abort::abort_reason(cx.page, s).is_some()) {
            return Ok(());
        }
        let removed = add_listener(cx, this, &type_, callback, capture, once, passive);
        if let (Some(signal), Some(removed)) = (signal, removed) {
            crate::abort::add_algorithm(
                cx.page,
                signal,
                Rc::new(move |cx, _reason| {
                    removed.set(true);
                    if let Some(list) = cx.page.listeners.borrow_mut().get_mut(&this) {
                        list.retain(|l| !l.removed.get());
                    }
                }),
            );
        }
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
                return None;
            }
            e.trusted = false;
            let mouse = matches!(
                e.iface,
                InterfaceId::MouseEvent | InterfaceId::PointerEvent | InterfaceId::WheelEvent
            );
            Some((mouse && e.type_ == "click", e.bubbles))
        })?;
        let Some((activation, bubbles)) = ready else {
            return Err(Exception::invalid_state(
                "The event is already being dispatched or is not initialized",
            ));
        };
        // A click `MouseEvent` runs activation behavior, whoever sends it.
        if activation && let EventTargetRef::Node(el) = this {
            return Ok(crate::activation::dispatch_click(cx, el, event, bubbles));
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

// ---- error reporting -------------------------------------------------------

/// The arguments `window.onerror` is called with, if `event` is an
/// `ErrorEvent`.
fn error_handler_args(page: &PageState, event: ObjectId) -> Option<Vec<Value>> {
    page.try_with::<Event, _>(event, |e| match &e.data {
        EventData::Error {
            message,
            filename,
            lineno,
            colno,
            error,
        } => Some(vec![
            Value::String(message.clone()),
            Value::String(filename.clone()),
            Value::Number(f64::from(*lineno)),
            Value::Number(f64::from(*colno)),
            error.clone(),
        ]),
        _ => None,
    })
    .flatten()
}

/// Reports an uncaught exception
/// (<https://html.spec.whatwg.org/multipage/#report-an-exception>): an
/// `error` event at the window, then a console message unless a handler
/// cancelled the event.
pub fn report_exception(cx: &mut Cx<'_>, exception: &Exception) {
    let text = format!("Uncaught {}", cx.script.describe_exception(exception));
    cx.page.errors.borrow_mut().push(text.clone());

    // An exception thrown while one is being reported is only logged.
    let handled = if cx.page.reporting_error.replace(true) {
        false
    } else {
        let mut event = Event::new("error", false, true, cx.page.clock.peek());
        event.iface = InterfaceId::ErrorEvent;
        event.trusted = true;
        event.data = EventData::Error {
            message: text.lines().next().unwrap_or_default().to_string(),
            filename: cx.page.url.borrow().to_string(),
            lineno: 0,
            colno: 0,
            error: match exception {
                Exception::Thrown(root) => Value::Opaque(root.clone()),
                Exception::Value(value) => value.clone(),
                _ => Value::Undefined,
            },
        };
        let event = cx.page.alloc(event);
        let proceed = dispatch(cx, EventTargetRef::Window, event);
        cx.page.reporting_error.set(false);
        !proceed
    };
    if !handled {
        cx.page.log(crate::page::ConsoleLevel::Error, text);
    }
}

/// Fires `unhandledrejection` at the window for a rejected promise nobody
/// handled. Returns `false` if a listener cancelled the event, in which
/// case the rejection counts as handled.
pub fn report_unhandled_rejection(cx: &mut Cx<'_>, promise: Value, reason: Value) -> bool {
    let mut event = Event::new("unhandledrejection", false, true, cx.page.clock.peek());
    event.iface = InterfaceId::PromiseRejectionEvent;
    event.trusted = true;
    event.data = EventData::PromiseRejection { promise, reason };
    let event = cx.page.alloc(event);
    dispatch(cx, EventTargetRef::Window, event)
}

fn derived_event(
    cx: &Cx<'_>,
    iface: InterfaceId,
    type_: String,
    flags: (bool, bool, bool),
    data: EventData,
) -> ObjectId {
    let (bubbles, cancelable, composed) = flags;
    let mut event = Event::new(type_, bubbles, cancelable, cx.page.clock.peek());
    event.iface = iface;
    event.composed = composed;
    event.data = data;
    cx.page.alloc(event)
}

impl web::ErrorEventImpl for Web {
    fn message(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        get(cx, this, |e| match &e.data {
            EventData::Error { message, .. } => message.clone(),
            _ => String::new(),
        })
    }

    fn filename(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        get(cx, this, |e| match &e.data {
            EventData::Error { filename, .. } => filename.clone(),
            _ => String::new(),
        })
    }

    fn lineno(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        get(cx, this, |e| match &e.data {
            EventData::Error { lineno, .. } => *lineno,
            _ => 0,
        })
    }

    fn colno(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        get(cx, this, |e| match &e.data {
            EventData::Error { colno, .. } => *colno,
            _ => 0,
        })
    }

    fn error(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        get(cx, this, |e| match &e.data {
            EventData::Error { error, .. } => error.clone(),
            _ => Value::Undefined,
        })
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::ErrorEventInit,
    ) -> Fallible<ObjectId> {
        Ok(derived_event(
            cx,
            InterfaceId::ErrorEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            EventData::Error {
                message: init.message,
                filename: init.filename,
                lineno: init.lineno,
                colno: init.colno,
                error: init.error,
            },
        ))
    }
}

impl web::PromiseRejectionEventImpl for Web {
    fn promise(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        get(cx, this, |e| match &e.data {
            EventData::PromiseRejection { promise, .. } => promise.clone(),
            _ => Value::Undefined,
        })
    }

    fn reason(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        get(cx, this, |e| match &e.data {
            EventData::PromiseRejection { reason, .. } => reason.clone(),
            _ => Value::Undefined,
        })
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::PromiseRejectionEventInit,
    ) -> Fallible<ObjectId> {
        Ok(derived_event(
            cx,
            InterfaceId::PromiseRejectionEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            EventData::PromiseRejection {
                promise: init.promise,
                reason: init.reason,
            },
        ))
    }
}

impl web::HashChangeEventImpl for Web {
    fn old_url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        get(cx, this, |e| match &e.data {
            EventData::HashChange { old_url, .. } => old_url.clone(),
            _ => String::new(),
        })
    }

    fn new_url(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        get(cx, this, |e| match &e.data {
            EventData::HashChange { new_url, .. } => new_url.clone(),
            _ => String::new(),
        })
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::HashChangeEventInit,
    ) -> Fallible<ObjectId> {
        Ok(derived_event(
            cx,
            InterfaceId::HashChangeEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            EventData::HashChange {
                old_url: init.old_url,
                new_url: init.new_url,
            },
        ))
    }
}

impl web::PopStateEventImpl for Web {
    fn state(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        get(cx, this, |e| match &e.data {
            EventData::PopState { state } => state.clone(),
            _ => Value::Null,
        })
    }

    fn has_ua_visual_transition(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<bool> {
        Ok(false)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::PopStateEventInit,
    ) -> Fallible<ObjectId> {
        Ok(derived_event(
            cx,
            InterfaceId::PopStateEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            EventData::PopState { state: init.state },
        ))
    }
}

/// Creates a `ProgressEvent`.
pub fn progress_event(cx: &Cx<'_>, type_: &str, loaded: f64, total: Option<f64>) -> ObjectId {
    let mut event = Event::new(type_, false, false, cx.page.clock.peek());
    event.iface = InterfaceId::ProgressEvent;
    event.trusted = true;
    event.data = EventData::Progress {
        length_computable: total.is_some(),
        loaded,
        total: total.unwrap_or(0.0),
    };
    cx.page.alloc(event)
}

impl web::ProgressEventImpl for Web {
    fn length_computable(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        get(cx, this, |e| match &e.data {
            EventData::Progress {
                length_computable, ..
            } => *length_computable,
            _ => false,
        })
    }

    fn loaded(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        get(cx, this, |e| match &e.data {
            EventData::Progress { loaded, .. } => *loaded,
            _ => 0.0,
        })
    }

    fn total(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        get(cx, this, |e| match &e.data {
            EventData::Progress { total, .. } => *total,
            _ => 0.0,
        })
    }

    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::ProgressEventInit,
    ) -> Fallible<ObjectId> {
        Ok(derived_event(
            cx,
            InterfaceId::ProgressEvent,
            type_,
            (init.bubbles, init.cancelable, init.composed),
            EventData::Progress {
                length_computable: init.length_computable,
                loaded: init.loaded,
                total: init.total,
            },
        ))
    }
}
