//! Frames: what a page knows of the frame tree it is in, and the commands
//! it hands the embedder to grow it.
//!
//! Every frame is a page of its own (its own document, scripts and event
//! loop), run by the embedder on the same thread. A page sees other
//! frames' windows as remote windows: `iframe.contentWindow`, `parent`,
//! `top` and a message's `source` offer what a cross-origin window does
//! (messages, the frame tree, focus) and no document, whatever the
//! origins. The embedder opens frames, routes messages and reports loads.
//!
//! Messages between frames are carried as JSON: what `JSON.stringify`
//! cannot represent (functions, cycles, `Map`s) does not cross. Within
//! one page the structured clone is used.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use catpaw_dom::NodeId;
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value, WindowRef};
use url::Url;

use crate::events::{self, Event, EventData};
use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, event_loop, node, platform_object};

/// A frame in the tree the embedder keeps; the top-level page is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameId(pub u32);

/// A message's data on its way to another frame.
#[derive(Clone, Debug)]
pub enum MessageData {
    /// A primitive, or a value already of this page's realm.
    Value(Value),
    /// A JSON text, parsed in the receiving realm.
    Json(String),
}

/// The shape of the frame tree, shared by every frame of a page: who is
/// whose parent, and the next id. A page records the frames it opens;
/// the embedder removes what it closes.
#[derive(Debug)]
pub struct FrameTree {
    next: u32,
    /// The parent of each frame; for a popup, its opener.
    parents: HashMap<FrameId, FrameId>,
    /// Popups: tops of their own, with an opener rather than a parent.
    popups: HashSet<FrameId>,
    /// How the dialogs of every frame in the tree are answered.
    pub dialog_policy: crate::page::DialogPolicy,
}

impl Default for FrameTree {
    fn default() -> Self {
        Self {
            next: 1,
            parents: HashMap::new(),
            popups: HashSet::new(),
            dialog_policy: Default::default(),
        }
    }
}

impl FrameTree {
    fn allocate(&mut self, parent: FrameId) -> FrameId {
        let id = FrameId(self.next);
        self.next += 1;
        self.parents.insert(id, parent);
        id
    }

    fn allocate_popup(&mut self, opener: FrameId) -> FrameId {
        let id = self.allocate(opener);
        self.popups.insert(id);
        id
    }

    pub fn parent_of(&self, frame: FrameId) -> Option<FrameId> {
        if self.popups.contains(&frame) {
            return None;
        }
        self.parents.get(&frame).copied()
    }

    /// The frame that opened a popup.
    pub fn opener_of(&self, frame: FrameId) -> Option<FrameId> {
        if self.popups.contains(&frame) {
            self.parents.get(&frame).copied()
        } else {
            None
        }
    }

    pub fn is_popup(&self, frame: FrameId) -> bool {
        self.popups.contains(&frame)
    }

    /// Whether the frame is open (the top one always is).
    pub fn contains(&self, frame: FrameId) -> bool {
        frame == FrameId(0) || self.parents.contains_key(&frame)
    }

    pub fn children_of(&self, frame: FrameId) -> Vec<FrameId> {
        let mut children: Vec<FrameId> = self
            .parents
            .iter()
            .filter(|(_, p)| **p == frame)
            .map(|(c, _)| *c)
            .collect();
        children.sort();
        children
    }

    /// Forgets a frame and the frames inside it (and the popups it
    /// opened).
    pub fn remove(&mut self, frame: FrameId) {
        for child in self.children_of(frame) {
            self.remove(child);
        }
        self.parents.remove(&frame);
        self.popups.remove(&frame);
    }

    fn is_ancestor(&self, ancestor: FrameId, of: FrameId) -> bool {
        let mut at = of;
        while let Some(parent) = self.parent_of(at) {
            if parent == ancestor {
                return true;
            }
            at = parent;
        }
        false
    }

    fn top_of(&self, frame: FrameId) -> FrameId {
        let mut at = frame;
        while let Some(parent) = self.parent_of(at) {
            at = parent;
        }
        at
    }
}

/// Something the embedder is asked to do with frames.
#[derive(Clone, Debug)]
pub enum FrameCommand {
    /// Open the frame `frame` for an `iframe` element, with the URL its
    /// `src` names, the markup of its `srcdoc`, or neither (`about:blank`).
    /// The page has already placed the frame in the tree; the embedder
    /// answers with [`frame_loaded`] or [`frame_failed`].
    Open {
        frame: FrameId,
        element: NodeId,
        url: Option<Url>,
        srcdoc: Option<String>,
    },
    /// The frame's element left the document, or was pointed elsewhere,
    /// or a popup closed itself: the frame and the frames inside it go.
    Close { frame: FrameId },
    /// `window.open()`: a popup, a top-level page of its own with this
    /// page as its opener. The page has placed it in the tree.
    OpenPopup { frame: FrameId, url: Url },
    /// A message for another frame's window.
    PostMessage {
        to: FrameId,
        data: MessageData,
        target_origin: String,
    },
}

/// The page's place in the frame tree, its child frames, and the commands
/// waiting for the embedder.
#[derive(Default)]
pub struct FrameState {
    /// This page's frame; `None` until the embedder places it (the top
    /// page is frame 0).
    id: Cell<Option<FrameId>>,
    /// The tree the page is part of.
    tree: RefCell<Rc<RefCell<FrameTree>>>,
    /// The child frames by their `iframe` element.
    children: RefCell<HashMap<NodeId, FrameId>>,
    /// The remote window object made for each frame, so that
    /// `iframe.contentWindow === iframe.contentWindow`.
    proxies: RefCell<HashMap<FrameId, ObjectId>>,
    /// The `iframe` elements whose frames the embedder has been asked to
    /// open and has not reported loaded: each delays the document's `load`.
    pending: RefCell<HashSet<NodeId>>,
    /// The page asked to close itself (`window.close()` in a popup).
    closing: Cell<bool>,
    commands: RefCell<Vec<FrameCommand>>,
}

impl FrameState {
    /// Where the page sits: its own id, in `tree`.
    pub fn place(&self, id: FrameId, tree: Rc<RefCell<FrameTree>>) {
        self.id.set(Some(id));
        *self.tree.borrow_mut() = tree;
    }

    /// The tree the page is part of.
    pub fn tree(&self) -> Rc<RefCell<FrameTree>> {
        self.tree.borrow().clone()
    }

    pub fn id(&self) -> Option<FrameId> {
        self.id.get()
    }

    pub fn parent(&self) -> Option<FrameId> {
        self.id().and_then(|id| self.tree().borrow().parent_of(id))
    }

    /// The frame that opened this page as a popup.
    pub fn opener(&self) -> Option<FrameId> {
        self.id().and_then(|id| self.tree().borrow().opener_of(id))
    }

    /// Whether this page is a popup (`window.open()` made it).
    pub fn is_popup(&self) -> bool {
        self.id()
            .is_some_and(|id| self.tree().borrow().is_popup(id))
    }

    /// Whether the page closed itself.
    pub fn is_closing(&self) -> bool {
        self.closing.get()
    }

    pub fn top(&self) -> Option<FrameId> {
        self.id().map(|id| self.tree().borrow().top_of(id))
    }

    /// Forgets `element`'s frame (closed by the embedder, or pointed
    /// elsewhere).
    pub fn detach_child(&self, element: NodeId) -> Option<FrameId> {
        let frame = self.children.borrow_mut().remove(&element)?;
        self.tree().borrow_mut().remove(frame);
        Some(frame)
    }

    /// The `iframe` element of a child frame.
    pub fn element_of(&self, frame: FrameId) -> Option<NodeId> {
        self.children
            .borrow()
            .iter()
            .find(|(_, f)| **f == frame)
            .map(|(e, _)| *e)
    }

    /// The frame of an `iframe` element, once opened.
    pub fn child_of(&self, element: NodeId) -> Option<FrameId> {
        self.children.borrow().get(&element).copied()
    }

    /// The child frames, with their elements.
    pub fn children(&self) -> Vec<(NodeId, FrameId)> {
        let mut children: Vec<(NodeId, FrameId)> = self
            .children
            .borrow()
            .iter()
            .map(|(e, f)| (*e, *f))
            .collect();
        children.sort_by_key(|(_, f)| *f);
        children
    }

    /// Whether a frame may be addressed from here: it is open, and it is
    /// this page's ancestor, descendant, or shares its top.
    fn is_open(&self, frame: FrameId) -> bool {
        self.tree().borrow().contains(frame)
    }

    /// Takes the commands queued since the last call.
    pub fn take_commands(&self) -> Vec<FrameCommand> {
        std::mem::take(&mut *self.commands.borrow_mut())
    }

    pub fn has_commands(&self) -> bool {
        !self.commands.borrow().is_empty()
    }

    fn push(&self, command: FrameCommand) {
        self.commands.borrow_mut().push(command);
    }
}

/// The remote window of a frame.
pub struct RemoteWindowObject {
    frame: FrameId,
}
platform_object!(RemoteWindowObject, CatPawRemoteWindow);

/// The remote window object for `frame`, made once per page.
pub(crate) fn remote_window(cx: &mut Cx<'_>, frame: FrameId) -> ObjectId {
    if let Some(id) = cx.page.frames.proxies.borrow().get(&frame) {
        return *id;
    }
    let id = cx.page.alloc(RemoteWindowObject { frame });
    // Kept for the page's life: frames come and go, their windows stay
    // comparable.
    cx.pin(id);
    cx.page.frames.proxies.borrow_mut().insert(frame, id);
    id
}

/// The window of `frame` as this page sees it.
fn window_of(cx: &mut Cx<'_>, frame: FrameId) -> WindowRef {
    if cx.page.frames.id() == Some(frame) {
        WindowRef::Local
    } else {
        WindowRef::Remote(remote_window(cx, frame))
    }
}

/// The frame a window stands for.
pub fn frame_of_window(page: &PageState, window: WindowRef) -> Option<FrameId> {
    match window {
        WindowRef::Local => page.frames.id(),
        WindowRef::Remote(id) => page.try_with::<RemoteWindowObject, _>(id, |w| w.frame),
    }
}

/// Whether `ancestor` is above `of` in the tree.
pub fn is_ancestor(page: &PageState, ancestor: FrameId, of: FrameId) -> bool {
    page.frames.tree().borrow().is_ancestor(ancestor, of)
}

/// `parent` as the page sees it: the parent frame's window, or its own
/// when it is the top.
pub(crate) fn parent_window(cx: &mut Cx<'_>) -> WindowRef {
    match cx.page.frames.parent() {
        Some(parent) => window_of(cx, parent),
        None => WindowRef::Local,
    }
}

/// `opener` as the page sees it: the window that opened it, for a popup.
pub(crate) fn opener_window(cx: &mut Cx<'_>) -> Option<WindowRef> {
    let opener = cx.page.frames.opener()?;
    Some(window_of(cx, opener))
}

/// `window.open(url)`: a popup, when the user just acted on the page
/// (browsers block popups otherwise). Returns its window.
pub(crate) fn open_popup(cx: &mut Cx<'_>, url: Option<Url>) -> Option<WindowRef> {
    let me = cx.page.frames.id()?;
    if !cx.page.consume_user_activation() {
        cx.page.log(
            crate::page::ConsoleLevel::Warn,
            "window.open() was blocked: no user activation",
        );
        return None;
    }
    let url = url.unwrap_or_else(|| Url::parse("about:blank").expect("about:blank parses"));
    let frame = cx.page.frames.tree().borrow_mut().allocate_popup(me);
    cx.page.frames.push(FrameCommand::OpenPopup { frame, url });
    Some(window_of(cx, frame))
}

/// `window.close()`: a popup goes away; other pages ignore it, as
/// browsers do for windows script did not open.
pub(crate) fn close_self(page: &PageState) {
    if !page.frames.is_popup() || page.frames.closing.replace(true) {
        return;
    }
    if let Some(me) = page.frames.id() {
        page.frames.push(FrameCommand::Close { frame: me });
    }
}

pub(crate) fn top_window(cx: &mut Cx<'_>) -> WindowRef {
    match cx.page.frames.top() {
        Some(top) => window_of(cx, top),
        None => WindowRef::Local,
    }
}

/// The origin a page's messages carry.
pub fn origin_of(url: &Url) -> String {
    let origin = url.origin();
    if origin.is_tuple() {
        origin.ascii_serialization()
    } else {
        "null".to_string()
    }
}

/// Whether a message aimed at `target_origin` may be delivered to a page
/// of `origin` (serialized, see [`origin_of`]). `/` means the sender's own
/// origin, which the caller resolves.
pub fn origin_allows(target_origin: &str, origin: &str) -> bool {
    match target_origin {
        "*" => true,
        other => Url::parse(other)
            .map(|t| origin_of(&t) == origin)
            .unwrap_or(false),
    }
}

// --------------------------------------------------------------- messages

/// Dispatches a trusted `message` event at `target`, `data` parsed in
/// this realm.
pub(crate) fn dispatch_message(
    cx: &mut Cx<'_>,
    target: EventTargetRef,
    data: MessageData,
    origin: String,
    source: Option<WindowRef>,
) {
    let data = match data {
        MessageData::Value(v) => v,
        MessageData::Json(text) => cx.script.parse_json(&text).unwrap_or(Value::Undefined),
    };
    let mut event = Event::new("message", false, false, cx.page.clock.peek());
    event.iface = InterfaceId::MessageEvent;
    event.trusted = true;
    event.data = EventData::Message {
        data,
        origin,
        last_event_id: String::new(),
        source: source.map(web::WindowProxyOrMessagePort::WindowProxy),
    };
    let event = cx.page.alloc(event);
    cx.pin(event);
    events::dispatch(cx, target, event);
    cx.unpin(event);
}

/// Queues a `message` event on the window: `data` from a frame at
/// `origin`, `source` being that frame's window as this page sees it.
pub fn deliver_message(
    page: &PageState,
    data: MessageData,
    origin: String,
    source: Option<FrameId>,
) {
    event_loop::queue_task(page, "message", move |cx| {
        let source = source.map(|frame| window_of(cx, frame));
        dispatch_message(cx, EventTargetRef::Window, data, origin, source);
    });
}

/// `data` as it crosses to another realm.
pub(crate) fn portable(cx: &mut Cx<'_>, data: &Value) -> Fallible<MessageData> {
    Ok(match data {
        Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            MessageData::Value(data.clone())
        }
        other => match cx.script.stringify_json(other)? {
            Some(text) => MessageData::Json(text),
            None => MessageData::Value(Value::Undefined),
        },
    })
}

/// The sender's own origin stands in for `/`.
fn resolve_target_origin(page: &PageState, target_origin: String) -> String {
    if target_origin == "/" {
        origin_of(&page.url.borrow())
    } else {
        target_origin
    }
}

/// `postMessage` to the window of `frame`, this page's own included.
pub(crate) fn post_to_frame(
    cx: &mut Cx<'_>,
    frame: Option<FrameId>,
    data: Value,
    target_origin: String,
) -> Fallible<()> {
    let target_origin = resolve_target_origin(cx.page, target_origin);
    if target_origin != "*" && Url::parse(&target_origin).is_err() {
        return Err(Exception::dom(
            "SyntaxError",
            format!("Invalid target origin '{target_origin}' in a call to 'postMessage'"),
        ));
    }
    let me = cx.page.frames.id();
    if frame.is_none() || frame == me {
        let url = cx.page.url.borrow().clone();
        if !origin_allows(&target_origin, &origin_of(&url)) {
            return Ok(());
        }
        let data = cx.script.structured_clone(&data)?;
        deliver_message(cx.page, MessageData::Value(data), origin_of(&url), me);
        return Ok(());
    }
    let data = portable(cx, &data)?;
    cx.page.frames.push(FrameCommand::PostMessage {
        to: frame.unwrap_or(FrameId(0)),
        data,
        target_origin,
    });
    Ok(())
}

// ------------------------------------------------------------------ hooks

/// An `iframe` was inserted into the document, or its `src`/`srcdoc`
/// changed: its frame is (re)opened.
pub(crate) fn iframe_changed(page: &PageState, el: NodeId) {
    let (url, srcdoc) = {
        let dom = page.dom.borrow();
        if !dom.is_html_element(el, "iframe") || !dom.is_connected(el) {
            return;
        }
        let srcdoc = dom.attr(el, "srcdoc").map(str::to_string);
        let url = dom
            .attr(el, "src")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|src| page.base_url().join(src).ok());
        (url, srcdoc)
    };
    let Some(me) = page.frames.id() else {
        // Not placed in a tree: the embedder keeps no frames.
        return;
    };
    if let Some(old) = page.frames.detach_child(el) {
        page.frames.push(FrameCommand::Close { frame: old });
    }
    let frame = page.frames.tree().borrow_mut().allocate(me);
    page.frames.children.borrow_mut().insert(el, frame);
    if page.frames.pending.borrow_mut().insert(el) {
        crate::scripting::block_load(page);
    }
    page.frames.push(FrameCommand::Open {
        frame,
        element: el,
        url,
        srcdoc,
    });
}

/// The element is no longer waiting for its frame.
fn settle_pending(page: &PageState, el: NodeId) {
    if page.frames.pending.borrow_mut().remove(&el) {
        crate::scripting::unblock_load(page);
    }
}

/// The size an `iframe` element's frame gets: its border box, or the
/// default of 300 by 150 when it has no box yet.
pub fn frame_viewport(page: &PageState, element: NodeId) -> (u32, u32) {
    let rect = crate::layout::bounding_client_rect(page, element);
    if rect.width > 0.0 && rect.height > 0.0 {
        (rect.width.round() as u32, rect.height.round() as u32)
    } else {
        (300, 150)
    }
}

/// `iframe` elements among `inserted` (subtree roots, shadow trees
/// included) get frames.
pub(crate) fn nodes_inserted(page: &PageState, inserted: &[NodeId]) {
    let iframes: Vec<NodeId> = {
        let dom = page.dom.borrow();
        inserted
            .iter()
            .flat_map(|&n| dom.shadow_including_descendants(n))
            .filter(|&n| dom.is_html_element(n, "iframe"))
            .collect()
    };
    for el in iframes {
        iframe_changed(page, el);
    }
}

/// The subtree at `removed` is leaving the document: the frames of iframes
/// in it close.
pub(crate) fn subtree_removed(page: &PageState, removed: NodeId) {
    let elements: Vec<NodeId> = {
        let children = page.frames.children.borrow();
        if children.is_empty() {
            return;
        }
        let dom = page.dom.borrow();
        dom.shadow_including_descendants(removed)
            .into_iter()
            .filter(|n| children.contains_key(n))
            .collect()
    };
    for el in elements {
        settle_pending(page, el);
        if let Some(frame) = page.frames.detach_child(el) {
            page.frames.push(FrameCommand::Close { frame });
        }
    }
}

/// The embedder answers an `Open` it could not carry out: the frame is
/// forgotten, and `load` fires on the element as it does for a document
/// that failed to load.
pub fn frame_failed(page: &PageState, element: NodeId) {
    page.frames.detach_child(element);
    frame_loaded(page, element);
}

/// The embedder answers an `Open`: the frame of `element` finished
/// loading. `load` fires on the element and the document's `load` no
/// longer waits for it.
pub fn frame_loaded(page: &PageState, element: NodeId) {
    event_loop::queue_task(page, "iframe load", move |cx| {
        if cx.dom().contains(element) {
            events::fire(cx, EventTargetRef::Node(element), "load", false, false);
        }
        settle_pending(cx.page, element);
    });
}

// --------------------------------------------------------------- bindings

fn frame_of(cx: &Cx<'_>, this: ObjectId) -> Fallible<FrameId> {
    cx.page.with::<RemoteWindowObject, _>(this, |w| w.frame)
}

impl web::CatPawRemoteWindowImpl for Web {
    fn post_message(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        target_origin: String,
        _transfer: Vec<Value>,
    ) -> Fallible<()> {
        let frame = frame_of(cx, this)?;
        post_to_frame(cx, Some(frame), message, target_origin)
    }

    fn post_message_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        options: web::WindowPostMessageOptions,
    ) -> Fallible<()> {
        let frame = frame_of(cx, this)?;
        post_to_frame(cx, Some(frame), message, options.target_origin)
    }

    fn closed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        let frame = frame_of(cx, this)?;
        Ok(!cx.page.frames.is_open(frame))
    }

    fn parent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<WindowRef>> {
        let frame = frame_of(cx, this)?;
        if !cx.page.frames.is_open(frame) {
            return Ok(None);
        }
        let parent = cx.page.frames.tree().borrow().parent_of(frame);
        Ok(Some(match parent {
            Some(parent) => window_of(cx, parent),
            None => WindowRef::Remote(this),
        }))
    }

    fn top(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<WindowRef>> {
        let frame = frame_of(cx, this)?;
        if !cx.page.frames.is_open(frame) {
            return Ok(None);
        }
        let top = cx.page.frames.tree().borrow().top_of(frame);
        Ok(Some(window_of(cx, top)))
    }

    fn self_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<WindowRef> {
        frame_of(cx, this)?;
        Ok(WindowRef::Remote(this))
    }

    fn window(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<WindowRef> {
        frame_of(cx, this)?;
        Ok(WindowRef::Remote(this))
    }

    fn frames(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<WindowRef> {
        frame_of(cx, this)?;
        Ok(WindowRef::Remote(this))
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let frame = frame_of(cx, this)?;
        Ok(cx.page.frames.tree().borrow().children_of(frame).len() as u32)
    }

    fn focus(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        frame_of(cx, this).map(|_| ())
    }

    fn blur(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        frame_of(cx, this).map(|_| ())
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        frame_of(cx, this).map(|_| ())
    }
}

impl web::HTMLIFrameElementImpl for Web {
    fn srcdoc(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(cx
            .dom()
            .attr(this, "srcdoc")
            .unwrap_or_default()
            .to_string())
    }

    fn set_srcdoc(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        crate::element::set_attr(cx, this, "srcdoc", value)
    }

    fn content_document(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        // Frames are pages of their own; their documents are not reachable
        // from here, as a cross-origin document is not.
        Ok(None)
    }

    fn content_window(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<WindowRef>> {
        node::check(cx, this)?;
        let frame = cx.page.frames.child_of(this);
        Ok(frame.map(|frame| WindowRef::Remote(remote_window(cx, frame))))
    }
}

fn message<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&Value, &str, &str, Option<web::WindowProxyOrMessagePort>) -> R,
) -> Fallible<R> {
    cx.page.with::<Event, _>(this, |e| match &e.data {
        EventData::Message {
            data,
            origin,
            last_event_id,
            source,
        } => Ok(f(data, origin, last_event_id, source.clone())),
        _ => Err(Exception::type_error("not a message event")),
    })?
}

impl web::MessageEventImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        type_: String,
        init: web::MessageEventInit,
    ) -> Fallible<ObjectId> {
        let mut event = Event::new(type_, init.bubbles, init.cancelable, cx.page.clock.peek());
        event.iface = InterfaceId::MessageEvent;
        event.composed = init.composed;
        event.data = EventData::Message {
            data: init.data,
            origin: init.origin,
            last_event_id: init.last_event_id,
            source: init.source,
        };
        Ok(cx.page.alloc(event))
    }

    fn data(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        message(cx, this, |data, _, _, _| data.clone())
    }

    fn origin(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        message(cx, this, |_, origin, _, _| origin.to_string())
    }

    fn last_event_id(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        message(cx, this, |_, _, id, _| id.to_string())
    }

    fn source(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<web::WindowProxyOrMessagePort>> {
        message(cx, this, |_, _, _, source| source)
    }
}
