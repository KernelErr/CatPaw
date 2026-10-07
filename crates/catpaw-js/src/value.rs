//! Values and handles that cross the script boundary.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use catpaw_dom::NodeId;
use slotmap::new_key_type;

new_key_type! {
    /// Handle to a platform object (anything that is not a DOM node: events,
    /// XHR objects, URL objects, ...) in a page's object arena.
    pub struct ObjectId;
}

thread_local! {
    static NEXT_ROOT_ID: Cell<u64> = const { Cell::new(1) };
    static RELEASED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// Allocates an id for a value the backend is about to root.
pub fn next_root_id() -> u64 {
    NEXT_ROOT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

/// Root ids whose last [`Rooted`] handle was dropped since the previous call.
/// The backend drains this between tasks and unroots the values.
pub fn drain_released() -> Vec<u64> {
    RELEASED
        .try_with(|r| std::mem::take(&mut *r.borrow_mut()))
        .unwrap_or_default()
}

struct RootGuard(u64);

impl Drop for RootGuard {
    fn drop(&mut self) {
        // The thread-local may already be gone during thread teardown.
        let _ = RELEASED.try_with(|r| r.borrow_mut().push(self.0));
    }
}

/// A script value kept alive by the backend on behalf of Rust code. Cloning
/// shares the root; dropping the last clone releases it.
#[derive(Clone)]
pub struct Rooted(Rc<RootGuard>);

impl Rooted {
    /// Wraps an id obtained from [`next_root_id`] after the backend stored
    /// the value under it.
    pub fn new(id: u64) -> Self {
        Self(Rc::new(RootGuard(id)))
    }

    pub fn id(&self) -> u64 {
        self.0.0
    }
}

impl fmt::Debug for Rooted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rooted#{}", self.id())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackKind {
    /// A callback function.
    Function,
    /// A callback interface such as `EventListener`: either a function or an
    /// object whose method of this name (`handleEvent`) is looked up at
    /// call time.
    Interface(&'static str),
}

/// A script function (or callback-interface object) held by Rust.
#[derive(Clone, Debug)]
pub struct Callback {
    pub root: Rooted,
    pub kind: CallbackKind,
}

/// A promise created through [`crate::ScriptHost::new_promise`].
#[derive(Clone, Debug)]
pub struct PromiseRef(pub Rooted);

/// Anything events can be dispatched at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventTargetRef {
    Window,
    Node(NodeId),
    Object(ObjectId),
}

/// A dynamically typed value crossing the script boundary. Statically typed
/// IDL members use Rust types directly; `Value` is for `any`, callback
/// arguments and results, and promise settlements.
#[derive(Clone, Debug, Default)]
pub enum Value {
    #[default]
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Node(NodeId),
    Object(ObjectId),
    Window,
    Array(Vec<Value>),
    /// Becomes a plain object with these properties, in order.
    Record(Vec<(String, Value)>),
    /// Becomes an `ArrayBuffer`.
    ArrayBuffer(Vec<u8>),
    /// Becomes a `Uint8Array`.
    Uint8Array(Vec<u8>),
    Promise(PromiseRef),
    Callback(Callback),
    /// A script value Rust does not interpret.
    Opaque(Rooted),
}

impl Value {
    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Number(v)
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Number(v as f64)
    }
}

impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Number(v as f64)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_string())
    }
}

impl From<NodeId> for Value {
    fn from(v: NodeId) -> Self {
        Value::Node(v)
    }
}

impl From<ObjectId> for Value {
    fn from(v: ObjectId) -> Self {
        Value::Object(v)
    }
}

impl From<EventTargetRef> for Value {
    fn from(v: EventTargetRef) -> Self {
        match v {
            EventTargetRef::Window => Value::Window,
            EventTargetRef::Node(n) => Value::Node(n),
            EventTargetRef::Object(o) => Value::Object(o),
        }
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Value::Null,
        }
    }
}

/// A window as script sees it: this page's own, or another frame's,
/// reached through a platform object that stands for it (`contentWindow`,
/// `parent`, `event.source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowRef {
    Local,
    Remote(ObjectId),
}

/// Bytes returned to script as an `ArrayBuffer`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ArrayBufferData(pub Vec<u8>);

/// Bytes returned to script as a `Uint8Array`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Uint8ArrayData(pub Vec<u8>);
