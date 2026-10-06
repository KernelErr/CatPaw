//! Per-page state and the context handed to every Web API implementation.
//!
//! A page owns one DOM arena, one arena of platform objects (everything
//! script can hold that is not a node: events, collections, URL objects, ...)
//! and the bookkeeping of its event loop. All of it lives on the page's
//! thread and is reached through [`Cx`], which pairs the state with the
//! script engine.
//!
//! Borrowing discipline: the state sits behind `RefCell`s, and calling into
//! script can re-enter any API. Never hold a borrow of page state across a
//! call that may run script (`cx.script.call`, event dispatch, script
//! execution, promise settlement).

use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use catpaw_dom::{Dom, NodeId};
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, ScriptHost};
use indexmap::IndexMap;
use slotmap::SlotMap;
use url::Url;

use crate::clock::Clock;
use crate::event_loop::{RafState, Task, Timers};
use crate::events::Listener;
use crate::generated::{DocumentReadyState, InterfaceId};
use crate::net::{NetCallback, NetHost};
use crate::scripting::ScriptState;

/// Anything stored in the page's object arena.
pub trait PlatformObject: Any {
    /// The most derived interface this object implements.
    fn interface(&self) -> InterfaceId;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Implements [`PlatformObject`] for a type with a fixed interface, or one
/// read from a field (`platform_object!(Event, |e| e.iface)`).
#[macro_export]
macro_rules! platform_object {
    ($ty:ty, |$this:ident| $iface:expr) => {
        impl $crate::page::PlatformObject for $ty {
            fn interface(&self) -> $crate::generated::InterfaceId {
                let $this = self;
                $iface
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }
    };
    ($ty:ty, $iface:ident) => {
        $crate::platform_object!($ty, |_this| $crate::generated::InterfaceId::$iface);
    };
}

/// What the page tells sites and scripts about itself.
#[derive(Clone, Debug)]
pub struct PageConfig {
    pub user_agent: String,
    /// Preferred languages, most preferred first (`navigator.languages`).
    pub languages: Vec<String>,
    /// `navigator.platform`.
    pub platform: String,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub device_pixel_ratio: f64,
    pub hardware_concurrency: u32,
    /// Run timers on a virtual clock that jumps ahead whenever the page is idle.
    pub virtual_time: bool,
    /// A fixed time origin (Unix milliseconds) for repeatable runs.
    pub time_origin_unix_ms: Option<f64>,
}

impl Default for PageConfig {
    fn default() -> Self {
        let platform = if cfg!(target_os = "windows") {
            "Win32"
        } else if cfg!(target_os = "macos") {
            "MacIntel"
        } else {
            "Linux x86_64"
        };
        Self {
            user_agent: concat!("CatPaw/", env!("CARGO_PKG_VERSION")).to_string(),
            languages: vec!["en-US".to_string(), "en".to_string()],
            platform: platform.to_string(),
            viewport_width: 1280,
            viewport_height: 720,
            device_pixel_ratio: 1.0,
            hardware_concurrency: 4,
            virtual_time: true,
            time_origin_unix_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleLevel {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

impl ConsoleLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ConsoleLevel::Debug => "debug",
            ConsoleLevel::Log => "log",
            ConsoleLevel::Info => "info",
            ConsoleLevel::Warn => "warn",
            ConsoleLevel::Error => "error",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConsoleMessage {
    pub level: ConsoleLevel,
    pub text: String,
    /// Milliseconds since the page's time origin.
    pub time_ms: f64,
}

/// A navigation requested by script (`location.href = ...`). The embedder
/// decides whether to follow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationRequest {
    pub url: Url,
    pub replace: bool,
    pub reload: bool,
}

/// A dialog (`alert`, `confirm`, `prompt`) the page opened. Dialogs never
/// block; they are answered immediately and recorded here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogRecord {
    pub kind: &'static str,
    pub message: String,
}

/// Document state that is not part of the node tree.
#[derive(Debug)]
pub struct DocumentState {
    pub ready_state: DocumentReadyState,
    pub current_script: Option<NodeId>,
    pub referrer: String,
    pub charset: String,
    pub content_type: String,
    pub focused: Option<NodeId>,
    /// Fallback cookie store when no network host is attached.
    pub cookies: IndexMap<String, String>,
    pub scroll_x: f64,
    pub scroll_y: f64,
}

impl Default for DocumentState {
    fn default() -> Self {
        Self {
            ready_state: DocumentReadyState::Loading,
            current_script: None,
            referrer: String::new(),
            charset: "UTF-8".to_string(),
            content_type: "text/html".to_string(),
            focused: None,
            cookies: IndexMap::new(),
            scroll_x: 0.0,
            scroll_y: 0.0,
        }
    }
}

/// Objects that exist once per page and are created on first use.
#[derive(Default, Debug)]
pub struct Singletons {
    pub location: Option<ObjectId>,
    pub history: Option<ObjectId>,
    pub navigator: Option<ObjectId>,
    pub performance: Option<ObjectId>,
    pub local_storage: Option<ObjectId>,
    pub session_storage: Option<ObjectId>,
}

/// Receives console messages as they are logged.
type ConsoleSink = Box<dyn Fn(&ConsoleMessage)>;

/// A microtask implemented by the page rather than by script.
pub type NativeMicrotask = Box<dyn FnOnce(&mut Cx<'_>)>;

/// Puts a native microtask on the script engine's job queue.
type MicrotaskQueue = Rc<dyn Fn(NativeMicrotask)>;

struct ObjectEntry {
    object: Box<dyn PlatformObject>,
    /// Number of reasons Rust has to keep the object (and its script
    /// wrapper) alive regardless of script reachability.
    pins: u32,
}

/// The state of one page.
pub struct PageState {
    pub dom: Rc<RefCell<Dom>>,
    pub config: PageConfig,
    pub clock: Rc<Clock>,
    pub url: RefCell<Url>,
    pub document_state: RefCell<DocumentState>,
    pub singletons: RefCell<Singletons>,
    objects: RefCell<SlotMap<ObjectId, ObjectEntry>>,

    pub(crate) listeners: RefCell<HashMap<EventTargetRef, Vec<Listener>>>,
    /// The event currently being dispatched (`window.event`).
    pub(crate) current_event: Cell<Option<ObjectId>>,

    pub(crate) tasks: RefCell<VecDeque<Task>>,
    pub(crate) timers: RefCell<Timers>,
    pub(crate) timer_nesting: Cell<u32>,
    pub(crate) raf: RefCell<RafState>,

    pub(crate) net: RefCell<Option<Rc<dyn NetHost>>>,
    pub(crate) net_callbacks: RefCell<HashMap<u64, NetCallback>>,

    pub(crate) scripts: ScriptState,

    /// `localStorage` and `sessionStorage` contents.
    pub(crate) storage: [RefCell<IndexMap<String, String>>; 2],
    /// Element ids of the document tree, valid for one arena version.
    pub(crate) id_index: RefCell<(u64, HashMap<String, NodeId>)>,
    /// Current value and checkedness of form controls whose state has
    /// diverged from their content attributes.
    pub(crate) form_state: RefCell<HashMap<NodeId, FormControlState>>,

    console: RefCell<Vec<ConsoleMessage>>,
    console_sink: RefCell<Option<ConsoleSink>>,
    pub(crate) console_state: RefCell<ConsoleState>,
    pub dialogs: RefCell<Vec<DialogRecord>>,
    pub navigation: RefCell<Option<NavigationRequest>>,
    /// The session history of this document (same-document entries).
    pub(crate) history: RefCell<crate::history::HistoryState>,
    pub(crate) mutation: crate::mutation_observer::Observers,
    pub(crate) intersection: crate::intersection_observer::Observers,
    pub(crate) styles: crate::stylesheets::Styles,
    pub(crate) attrs: crate::attributes::AttrObjects,
    pub(crate) timeline: crate::performance::Timeline,
    microtask_queue: RefCell<Option<MicrotaskQueue>>,
    /// An uncaught exception is being reported (reports do not nest).
    pub(crate) reporting_error: Cell<bool>,
    /// Uncaught exceptions, as reported to the console.
    pub errors: RefCell<Vec<String>>,
    /// Calls to members that exist but are not implemented, by name.
    pub stub_calls: RefCell<IndexMap<&'static str, u64>>,
}

#[derive(Default, Debug, Clone)]
pub struct FormControlState {
    pub value: Option<String>,
    pub checked: Option<bool>,
}

#[derive(Default, Debug)]
pub struct ConsoleState {
    pub counts: HashMap<String, u64>,
    pub timers: HashMap<String, f64>,
    pub group_depth: usize,
}

impl PageState {
    pub fn new(url: Url, config: PageConfig) -> Self {
        let clock = match config.time_origin_unix_ms {
            Some(origin) => Clock::with_origin(config.virtual_time, origin),
            None => Clock::new(config.virtual_time),
        };
        let mut dom = Dom::new();
        dom.document_data_mut().url = Some(url.clone());
        Self {
            dom: Rc::new(RefCell::new(dom)),
            config,
            clock: Rc::new(clock),
            url: RefCell::new(url.clone()),
            document_state: RefCell::new(DocumentState::default()),
            singletons: RefCell::new(Singletons::default()),
            objects: RefCell::new(SlotMap::with_key()),
            listeners: RefCell::new(HashMap::new()),
            current_event: Cell::new(None),
            tasks: RefCell::new(VecDeque::new()),
            timers: RefCell::new(Timers::default()),
            timer_nesting: Cell::new(0),
            raf: RefCell::new(RafState::default()),
            net: RefCell::new(None),
            net_callbacks: RefCell::new(HashMap::new()),
            scripts: ScriptState::default(),
            storage: [RefCell::new(IndexMap::new()), RefCell::new(IndexMap::new())],
            id_index: RefCell::new((u64::MAX, HashMap::new())),
            form_state: RefCell::new(HashMap::new()),
            console: RefCell::new(Vec::new()),
            console_sink: RefCell::new(None),
            console_state: RefCell::new(ConsoleState::default()),
            dialogs: RefCell::new(Vec::new()),
            navigation: RefCell::new(None),
            history: RefCell::new(crate::history::HistoryState::new(url.clone())),
            mutation: Default::default(),
            intersection: Default::default(),
            styles: Default::default(),
            attrs: Default::default(),
            timeline: Default::default(),
            microtask_queue: RefCell::new(None),
            reporting_error: Cell::new(false),
            errors: RefCell::new(Vec::new()),
            stub_calls: RefCell::new(IndexMap::new()),
        }
    }

    /// Attaches the network implementation used for scripts and requests.
    pub fn set_net(&self, net: Rc<dyn NetHost>) {
        *self.net.borrow_mut() = Some(net);
    }

    pub fn net(&self) -> Option<Rc<dyn NetHost>> {
        self.net.borrow().clone()
    }

    /// Calls `sink` for every console message as it is logged.
    pub fn set_console_sink(&self, sink: impl Fn(&ConsoleMessage) + 'static) {
        *self.console_sink.borrow_mut() = Some(Box::new(sink));
    }

    /// Sets how native microtasks reach the script engine's job queue.
    pub fn set_microtask_queue(&self, queue: impl Fn(NativeMicrotask) + 'static) {
        *self.microtask_queue.borrow_mut() = Some(Rc::new(queue));
    }

    /// Queues `task` as a microtask, in order with those script queues.
    /// Without a script engine there is no such queue and the task is
    /// dropped.
    pub(crate) fn queue_microtask(&self, task: impl FnOnce(&mut Cx<'_>) + 'static) {
        let queue = self.microtask_queue.borrow().clone();
        if let Some(queue) = queue {
            queue(Box::new(task));
        }
    }

    pub fn document(&self) -> NodeId {
        self.dom.borrow().document()
    }

    // ---- console --------------------------------------------------------

    pub fn log(&self, level: ConsoleLevel, text: impl Into<String>) {
        let message = ConsoleMessage {
            level,
            text: text.into(),
            time_ms: self.clock.peek(),
        };
        if let Some(sink) = &*self.console_sink.borrow() {
            sink(&message);
        }
        self.console.borrow_mut().push(message);
    }

    pub fn console_messages(&self) -> Vec<ConsoleMessage> {
        self.console.borrow().clone()
    }

    pub fn take_console_messages(&self) -> Vec<ConsoleMessage> {
        std::mem::take(&mut *self.console.borrow_mut())
    }

    /// Records a call to a member that is defined but not implemented.
    pub fn count_stub(&self, name: &'static str) {
        *self.stub_calls.borrow_mut().entry(name).or_insert(0) += 1;
    }

    // ---- object arena ---------------------------------------------------

    /// Moves `object` into the arena.
    pub fn alloc(&self, object: impl PlatformObject) -> ObjectId {
        self.objects.borrow_mut().insert(ObjectEntry {
            object: Box::new(object),
            pins: 0,
        })
    }

    /// The most derived interface of an object, or `None` for a stale id.
    pub fn interface_of(&self, id: ObjectId) -> Option<InterfaceId> {
        self.objects.borrow().get(id).map(|e| e.object.interface())
    }

    pub fn object_exists(&self, id: ObjectId) -> bool {
        self.objects.borrow().contains_key(id)
    }

    pub fn object_count(&self) -> usize {
        self.objects.borrow().len()
    }

    /// Runs `f` on the object `id`, which must be a `T`.
    ///
    /// `f` runs with the object arena borrowed: it must not allocate or
    /// look up other objects, and must not call into script.
    pub fn with<T: 'static, R>(&self, id: ObjectId, f: impl FnOnce(&mut T) -> R) -> Fallible<R> {
        let mut objects = self.objects.borrow_mut();
        let object = objects
            .get_mut(id)
            .and_then(|e| e.object.as_any_mut().downcast_mut::<T>())
            .ok_or_else(|| Exception::type_error("Illegal invocation"))?;
        Ok(f(object))
    }

    /// Like [`PageState::with`], for callers that treat a stale or
    /// mismatched id as absence.
    pub fn try_with<T: 'static, R>(&self, id: ObjectId, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        self.with(id, f).ok()
    }

    /// Frees an object. Called by the script backend once the object's
    /// wrapper is unreachable and nothing pins it.
    pub fn free_object(&self, id: ObjectId) {
        let removed = self.objects.borrow_mut().remove(id);
        if removed.is_some() {
            self.listeners
                .borrow_mut()
                .remove(&EventTargetRef::Object(id));
        }
    }

    pub fn is_pinned(&self, id: ObjectId) -> bool {
        self.objects.borrow().get(id).is_some_and(|e| e.pins > 0)
    }

    /// Returns the new pin count, or `None` for a stale id.
    fn adjust_pins(&self, id: ObjectId, delta: i32) -> Option<u32> {
        let mut objects = self.objects.borrow_mut();
        let entry = objects.get_mut(id)?;
        entry.pins = entry.pins.saturating_add_signed(delta);
        Some(entry.pins)
    }

    // ---- urls -----------------------------------------------------------

    /// The document base URL: the first `<base href>`, else the document URL.
    pub fn base_url(&self) -> Url {
        let document_url = self.url.borrow().clone();
        let dom = self.dom.borrow();
        let base = dom
            .descendants(dom.document())
            .find(|&n| dom.is_html_element(n, "base") && dom.attr(n, "href").is_some())
            .and_then(|n| dom.attr(n, "href"))
            .and_then(|href| document_url.join(href).ok());
        base.unwrap_or(document_url)
    }

    /// Resolves `input` against the document base URL.
    pub fn resolve_url(&self, input: &str) -> Option<Url> {
        self.base_url().join(input).ok()
    }
}

/// The context of a Web API call: the page plus the script engine.
pub struct Cx<'a> {
    pub page: &'a PageState,
    pub script: &'a mut dyn ScriptHost,
}

impl<'a> Cx<'a> {
    pub fn new(page: &'a PageState, script: &'a mut dyn ScriptHost) -> Self {
        Self { page, script }
    }

    pub fn dom(&self) -> Ref<'a, Dom> {
        self.page.dom.borrow()
    }

    pub fn dom_mut(&self) -> RefMut<'a, Dom> {
        self.page.dom.borrow_mut()
    }

    pub fn document(&self) -> NodeId {
        self.page.document()
    }

    /// Keeps `id` (and the identity of its script wrapper) alive until the
    /// matching [`Cx::unpin`], whatever script does with it. Used while Rust
    /// has pending work for an object: an event being dispatched, a request
    /// in flight, a page singleton.
    pub fn pin(&mut self, id: ObjectId) {
        if self.page.adjust_pins(id, 1) == Some(1) {
            self.script.root_object(id);
        }
    }

    pub fn unpin(&mut self, id: ObjectId) {
        if self.page.adjust_pins(id, -1) == Some(0) {
            self.script.unroot_object(id);
        }
    }

    /// Reports an uncaught exception: an `error` event at the window, and
    /// a console message unless a handler cancelled it.
    pub fn report_exception(&mut self, exception: &Exception) {
        crate::events::report_exception(self, exception);
    }

    /// Performs a microtask checkpoint.
    pub fn checkpoint(&mut self) {
        self.script.run_microtasks();
    }
}
