//! The runtime the generated glue is written against: script wrappers for
//! nodes and platform objects, conversions between Boa values and the
//! engine-neutral types of `catpaw-js`, and the installation of interface
//! objects and prototypes into a realm.
//!
//! Wrapper lifetime (milestone 1):
//! - A node has at most one wrapper, kept alive for the life of the page.
//! - A platform object has at most one live wrapper. The runtime holds it
//!   weakly; once script drops it and nothing pins the object
//!   (`Cx::pin`), a sweep frees the object from the page's arena.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::builtins::promise::ResolvingFunctions;
use boa_engine::native_function::NativeFunctionPointer;
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::object::builtins::{
    AlignedVec, JsArray, JsArrayBuffer, JsDataView, JsPromise, JsProxy, JsTypedArray, JsUint8Array,
};
use boa_engine::property::{PropertyDescriptor, PropertyKey};
use boa_engine::{
    Context, Finalize, JsData, JsError, JsNativeError, JsObject, JsResult, JsString, JsSymbol,
    JsValue, NativeFunction, Trace, js_string,
};
use catpaw_dom::NodeId;
use catpaw_js::{
    ArrayBufferData, Callback, CallbackKind, EventTargetRef, Exception, Fallible, ObjectId,
    PromiseRef, Rooted, Uint8ArrayData, Value, WindowRef, drain_released, next_root_id,
};
use catpaw_js_boa::Jobs;
use catpaw_web::generated::InterfaceId as I;
use catpaw_web::{Cx, PageState};

use crate::host::BoaHost;

// ---------------------------------------------------------------- tables

/// A proxy trap target: which node or platform object it stands for.
#[derive(Clone, Copy, Debug)]
pub enum Handle {
    Node(NodeId),
    Object(ObjectId),
}

impl Handle {
    pub fn node(self) -> NodeId {
        match self {
            Handle::Node(id) => id,
            Handle::Object(_) => NodeId::default(),
        }
    }

    pub fn object(self) -> ObjectId {
        match self {
            Handle::Object(id) => id,
            Handle::Node(_) => ObjectId::default(),
        }
    }
}

pub type LengthFn = fn(Handle, &mut Context) -> JsResult<u32>;
pub type IndexedGetFn = fn(Handle, u32, &mut Context) -> JsResult<Option<JsValue>>;
pub type NamedGetFn = fn(Handle, &str, &mut Context) -> JsResult<Option<JsValue>>;
pub type NamedPropertiesFn = fn(Handle, &mut Context) -> JsResult<Vec<String>>;
pub type NamedSetFn = fn(Handle, &str, &JsValue, &mut Context) -> JsResult<()>;
pub type NamedDeleteFn = fn(Handle, &str, &mut Context) -> JsResult<bool>;

/// The indexed and named property access of a legacy platform object.
pub struct ExoticDef {
    pub length: Option<LengthFn>,
    pub indexed_get: Option<IndexedGetFn>,
    pub named_get: Option<NamedGetFn>,
    pub named_properties: Option<NamedPropertiesFn>,
    pub named_set: Option<NamedSetFn>,
    pub named_delete: Option<NamedDeleteFn>,
    /// `[LegacyOverrideBuiltIns]`: named properties shadow the prototype chain.
    pub override_builtins: bool,
    /// The named properties stand in for attributes the interface would
    /// otherwise define one by one: members on the prototype chain take
    /// precedence when setting, too.
    pub attribute_like: bool,
}

pub enum Iterable {
    None,
    /// `iterable<V>`: the Array.prototype iteration methods over indices.
    Values,
    /// `iterable<K, V>`: the function returns an array of `[key, value]` pairs.
    Pairs(NativeFunctionPointer),
}

pub struct AttrDef {
    pub name: &'static str,
    pub getter: NativeFunctionPointer,
    pub setter: Option<NativeFunctionPointer>,
}

pub struct OpDef {
    pub name: &'static str,
    pub func: NativeFunctionPointer,
    pub length: usize,
}

pub struct InterfaceDef {
    pub id: I,
    pub name: &'static str,
    pub parent: Option<I>,
    /// The global object implements this interface (`Window`).
    pub global: bool,
    pub constructor: Option<NativeFunctionPointer>,
    pub constructor_length: usize,
    pub attrs: &'static [AttrDef],
    pub ops: &'static [OpDef],
    pub static_attrs: &'static [AttrDef],
    pub static_ops: &'static [OpDef],
    pub consts: &'static [(&'static str, f64)],
    pub iterable: Iterable,
    pub exotic: Option<ExoticDef>,
}

pub struct NamespaceDef {
    pub name: &'static str,
    pub ops: &'static [OpDef],
}

// ---------------------------------------------------------------- runtime

/// Native data of a node's wrapper.
#[derive(Trace, Finalize, JsData)]
pub struct NodeWrapper {
    #[unsafe_ignore_trace]
    pub id: NodeId,
    #[unsafe_ignore_trace]
    pub iface: I,
}

/// Native data of a platform object's wrapper.
#[derive(Trace, Finalize, JsData)]
pub struct ObjectWrapper {
    #[unsafe_ignore_trace]
    pub id: ObjectId,
    #[unsafe_ignore_trace]
    pub iface: I,
}

type SameObjectCache = Vec<(&'static str, JsValue)>;

struct NodeSlot {
    wrapper: JsObject,
    cache: SameObjectCache,
}

struct ObjectSlot {
    /// A `WeakRef` to the wrapper.
    weak: JsObject,
    /// The wrapper itself, while the object is pinned.
    strong: Option<JsObject>,
    cache: SameObjectCache,
}

/// Functions captured from the prelude.
pub(crate) struct Prelude {
    pub dom_exception: JsObject,
    pub structured_clone: JsObject,
    pub json_parse: JsObject,
    pub json_stringify: JsObject,
}

/// Per-page binding state, shared by everything that runs in the page's
/// context.
pub struct Runtime {
    pub page: Rc<PageState>,
    pub jobs: Rc<Jobs>,
    defs: Vec<Option<&'static InterfaceDef>>,
    protos: RefCell<Vec<Option<JsObject>>>,
    nodes: RefCell<HashMap<NodeId, NodeSlot>>,
    objects: RefCell<HashMap<ObjectId, ObjectSlot>>,
    window_cache: RefCell<SameObjectCache>,
    /// `WeakRef` and `WeakRef.prototype.deref`, captured at startup.
    weak_ref: RefCell<Option<(JsObject, JsObject)>>,
    pub(crate) prelude: RefCell<Option<Prelude>>,
    /// The key under which a proxy's `get` trap hands out its target.
    target_symbol: JsSymbol,
    /// How deeply Rust is nested inside calls into script.
    pub(crate) depth: Cell<u32>,
    /// A microtask checkpoint is running (they do not nest).
    pub(crate) in_checkpoint: Cell<bool>,
    /// Number of wrapped objects at which the next sweep happens.
    sweep_at: Cell<usize>,
    /// Rejected promises nobody has handled yet.
    pub(crate) rejections: RefCell<Vec<JsObject>>,
}

struct RuntimeHandle(Rc<Runtime>);

thread_local! {
    /// Script values rooted on behalf of Rust (`catpaw_js::Rooted`). Shared
    /// by the pages of a thread, like the id counter that keys it.
    static ROOTS: RefCell<HashMap<u64, JsValue>> = RefCell::new(HashMap::new());
    static RESOLVERS: RefCell<HashMap<u64, ResolvingFunctions>> = RefCell::new(HashMap::new());
    static UNDEFINED: &'static JsValue = Box::leak(Box::new(JsValue::undefined()));
}

/// The runtime of the page `ctx` belongs to, if it has been attached yet.
pub fn try_runtime(ctx: &Context) -> Option<Rc<Runtime>> {
    ctx.get_data::<RuntimeHandle>().map(|h| h.0.clone())
}

/// The runtime of the page `ctx` belongs to.
pub fn runtime(ctx: &Context) -> Rc<Runtime> {
    ctx.get_data::<RuntimeHandle>()
        .expect("the context was not set up by catpaw-bindings-boa")
        .0
        .clone()
}

/// Roots `value` until the returned handle (and its clones) are dropped.
pub fn root(value: JsValue) -> Rooted {
    let id = next_root_id();
    ROOTS.with(|r| r.borrow_mut().insert(id, value));
    Rooted::new(id)
}

/// The value behind a root.
pub fn rooted(root: &Rooted) -> JsValue {
    ROOTS.with(|r| r.borrow().get(&root.id()).cloned().unwrap_or_default())
}

/// Drops the roots whose handles have all been dropped.
pub fn release_roots() {
    let released = drain_released();
    if released.is_empty() {
        return;
    }
    ROOTS.with(|r| {
        let mut roots = r.borrow_mut();
        for id in &released {
            roots.remove(id);
        }
    });
    RESOLVERS.with(|r| {
        let mut resolvers = r.borrow_mut();
        for id in &released {
            resolvers.remove(id);
        }
    });
}

pub(crate) fn store_resolvers(promise: &PromiseRef, functions: ResolvingFunctions) {
    RESOLVERS.with(|r| r.borrow_mut().insert(promise.0.id(), functions));
}

pub(crate) fn take_resolvers(promise: &PromiseRef) -> Option<ResolvingFunctions> {
    RESOLVERS.with(|r| r.borrow_mut().remove(&promise.0.id()))
}

/// Runs `f` with the page context of `ctx`.
pub fn with_cx<R>(ctx: &mut Context, f: impl FnOnce(&mut Cx<'_>) -> R) -> R {
    let rt = runtime(ctx);
    let mut host = BoaHost::new(ctx, rt.clone());
    let mut cx = Cx::new(&rt.page, &mut host);
    f(&mut cx)
}

/// What a script object is, from the bindings' point of view.
#[derive(Clone, Copy)]
enum Native {
    Node(NodeId, I),
    Object(ObjectId, I),
    Window,
}

impl Runtime {
    pub(crate) fn new(page: Rc<PageState>, jobs: Rc<Jobs>) -> Rc<Self> {
        let mut defs: Vec<Option<&'static InterfaceDef>> = vec![None; I::COUNT];
        for def in crate::generated::INTERFACES {
            defs[def.id as usize] = Some(def);
        }
        Rc::new(Self {
            page,
            jobs,
            defs,
            protos: RefCell::new(vec![None; I::COUNT]),
            nodes: RefCell::new(HashMap::new()),
            objects: RefCell::new(HashMap::new()),
            window_cache: RefCell::new(Vec::new()),
            weak_ref: RefCell::new(None),
            prelude: RefCell::new(None),
            target_symbol: JsSymbol::new(Some(js_string!("catpaw.target")))
                .expect("symbol allocation cannot fail this early"),
            depth: Cell::new(0),
            in_checkpoint: Cell::new(false),
            sweep_at: Cell::new(4096),
            rejections: RefCell::new(Vec::new()),
        })
    }

    pub(crate) fn attach(self: &Rc<Self>, ctx: &mut Context) {
        ctx.insert_data(RuntimeHandle(self.clone()));
    }

    fn proto(&self, iface: I) -> Option<JsObject> {
        self.protos.borrow()[iface as usize].clone()
    }

    /// The exotic behavior of `iface`, its own or inherited.
    fn exotic(&self, iface: I) -> Option<&'static ExoticDef> {
        let mut current = Some(iface);
        while let Some(i) = current {
            if let Some(def) = self.defs[i as usize]
                && let Some(exotic) = &def.exotic
            {
                return Some(exotic);
            }
            current = i.parent();
        }
        None
    }

    fn native_of(&self, obj: &JsObject, ctx: &mut Context) -> Option<Native> {
        if let Some(w) = obj.downcast_ref::<NodeWrapper>() {
            return Some(Native::Node(w.id, w.iface));
        }
        if let Some(w) = obj.downcast_ref::<ObjectWrapper>() {
            return Some(Native::Object(w.id, w.iface));
        }
        if JsObject::equals(obj, &ctx.global_object()) {
            return Some(Native::Window);
        }
        // A legacy platform object: the wrapper script sees is a proxy
        // whose `get` trap hands out the real wrapper for our symbol.
        if JsProxy::from_object(obj.clone()).is_ok() {
            let target = obj.get(self.target_symbol.clone(), ctx).ok()?.as_object()?;
            let w = target.downcast_ref::<ObjectWrapper>()?;
            return Some(Native::Object(w.id, w.iface));
        }
        None
    }

    /// The wrapper of a node, created on first use.
    pub fn wrap_node(&self, id: NodeId, ctx: &mut Context) -> JsValue {
        if let Some(slot) = self.nodes.borrow().get(&id) {
            return slot.wrapper.clone().into();
        }
        let iface = {
            let dom = self.page.dom.borrow();
            if !dom.contains(id) {
                return JsValue::null();
            }
            catpaw_web::interface_for_node(&dom, id)
        };
        let _ = ctx;
        let wrapper = JsObject::from_proto_and_data(self.proto(iface), NodeWrapper { id, iface });
        self.nodes.borrow_mut().insert(
            id,
            NodeSlot {
                wrapper: wrapper.clone(),
                cache: Vec::new(),
            },
        );
        wrapper.into()
    }

    fn weak_deref(&self, weak: &JsObject, ctx: &mut Context) -> Option<JsObject> {
        let deref = self.weak_ref.borrow().as_ref().map(|(_, d)| d.clone())?;
        deref.call(&weak.clone().into(), &[], ctx).ok()?.as_object()
    }

    fn create_object_wrapper(
        &self,
        id: ObjectId,
        iface: I,
        proto: Option<JsObject>,
        ctx: &mut Context,
    ) -> JsResult<JsObject> {
        let proto = proto.or_else(|| self.proto(iface));
        let target = JsObject::from_proto_and_data(proto, ObjectWrapper { id, iface });
        if self.exotic(iface).is_none() {
            return Ok(target);
        }
        let proxy = JsProxy::builder(target)
            .get(trap_get)
            .set(trap_set)
            .has(trap_has)
            .delete_property(trap_delete)
            .own_keys(trap_own_keys)
            .get_own_property_descriptor(trap_get_own_property_descriptor)
            .define_property(trap_define_property)
            .build(ctx)?;
        Ok(proxy.into())
    }

    fn register_object(&self, id: ObjectId, wrapper: &JsObject, ctx: &mut Context) -> JsResult<()> {
        let constructor = self.weak_ref.borrow().as_ref().map(|(c, _)| c.clone());
        let weak = match constructor {
            Some(c) => c.construct(&[wrapper.clone().into()], None, ctx)?,
            // Before the realm is fully set up: hold the wrapper strongly.
            None => wrapper.clone(),
        };
        let pinned = self.page.is_pinned(id) || self.weak_ref.borrow().is_none();
        self.objects.borrow_mut().insert(
            id,
            ObjectSlot {
                weak,
                strong: pinned.then(|| wrapper.clone()),
                cache: Vec::new(),
            },
        );
        Ok(())
    }

    fn live_wrapper(&self, id: ObjectId, ctx: &mut Context) -> Option<JsObject> {
        let (strong, weak) = {
            let objects = self.objects.borrow();
            let slot = objects.get(&id)?;
            (slot.strong.clone(), slot.weak.clone())
        };
        strong.or_else(|| self.weak_deref(&weak, ctx))
    }

    /// The wrapper of a platform object, created on first use.
    pub fn wrap_object(&self, id: ObjectId, ctx: &mut Context) -> JsResult<JsValue> {
        if let Some(wrapper) = self.live_wrapper(id, ctx) {
            return Ok(wrapper.into());
        }
        let Some(iface) = self.page.interface_of(id) else {
            return Ok(JsValue::null());
        };
        let wrapper = self.create_object_wrapper(id, iface, None, ctx)?;
        self.register_object(id, &wrapper, ctx)?;
        Ok(wrapper.into())
    }

    /// `Cx::pin` reached one: keep the wrapper (if any) alive.
    pub(crate) fn root_object(&self, id: ObjectId, ctx: &mut Context) {
        if let Some(wrapper) = self.live_wrapper(id, ctx)
            && let Some(slot) = self.objects.borrow_mut().get_mut(&id)
        {
            slot.strong = Some(wrapper);
        }
    }

    /// `Cx::unpin` reached zero: the wrapper decides the object's fate
    /// again. An object script never saw is freed right away.
    pub(crate) fn unroot_object(&self, id: ObjectId, ctx: &mut Context) {
        let known = match self.objects.borrow_mut().get_mut(&id) {
            Some(slot) => {
                slot.strong = None;
                true
            }
            None => false,
        };
        if !known || self.live_wrapper(id, ctx).is_none() {
            self.objects.borrow_mut().remove(&id);
            self.page.free_object(id);
        }
    }

    /// Frees the platform objects whose wrappers have been collected.
    pub(crate) fn sweep(&self, ctx: &mut Context) {
        let candidates: Vec<(ObjectId, JsObject)> = self
            .objects
            .borrow()
            .iter()
            .filter(|(_, slot)| slot.strong.is_none())
            .map(|(id, slot)| (*id, slot.weak.clone()))
            .collect();
        for (id, weak) in candidates {
            if self.weak_deref(&weak, ctx).is_some() || self.page.is_pinned(id) {
                continue;
            }
            self.objects.borrow_mut().remove(&id);
            self.page.free_object(id);
        }
        // `deref` keeps its results alive until the kept-objects list is
        // cleared; without this nothing would ever become collectable.
        ctx.clear_kept_objects();
        let live = self.objects.borrow().len();
        self.sweep_at.set((live * 2).max(4096));
    }

    /// Housekeeping between tasks, with no script on the stack.
    pub(crate) fn between_tasks(&self, ctx: &mut Context) {
        release_roots();
        ctx.clear_kept_objects();
        if self.objects.borrow().len() >= self.sweep_at.get() {
            self.sweep(ctx);
        }
    }

    /// Number of platform objects that currently have (or had) a wrapper.
    pub fn wrapped_object_count(&self) -> usize {
        self.objects.borrow().len()
    }

    fn cache_of<R>(
        &self,
        this: &JsValue,
        ctx: &mut Context,
        f: impl FnOnce(&mut SameObjectCache) -> R,
    ) -> Option<R> {
        let native = match this.as_object() {
            Some(obj) => self.native_of(&obj, ctx)?,
            None => Native::Window,
        };
        match native {
            Native::Node(id, _) => self
                .nodes
                .borrow_mut()
                .get_mut(&id)
                .map(|s| f(&mut s.cache)),
            Native::Object(id, _) => self
                .objects
                .borrow_mut()
                .get_mut(&id)
                .map(|s| f(&mut s.cache)),
            Native::Window => Some(f(&mut self.window_cache.borrow_mut())),
        }
    }
}

// ---------------------------------------------------------------- errors

pub fn type_error(message: &str) -> JsError {
    JsNativeError::typ()
        .with_message(message.to_string())
        .into()
}

fn illegal_invocation() -> JsError {
    type_error("Illegal invocation")
}

/// Converts an engine-neutral exception into a throwable error.
pub fn exception_to_js(exception: Exception, ctx: &mut Context) -> JsError {
    match exception {
        Exception::Dom { name, message } => {
            let constructor = runtime(ctx)
                .prelude
                .borrow()
                .as_ref()
                .map(|p| p.dom_exception.clone());
            let Some(constructor) = constructor else {
                return JsNativeError::error()
                    .with_message(format!("{name}: {message}"))
                    .into();
            };
            let args = [
                JsString::from(message.as_str()).into(),
                JsString::from(name).into(),
            ];
            match constructor.construct(&args, None, ctx) {
                Ok(object) => JsError::from_opaque(object.into()),
                Err(e) => e,
            }
        }
        Exception::Type(message) => JsNativeError::typ().with_message(message).into(),
        Exception::Range(message) => JsNativeError::range().with_message(message).into(),
        Exception::Thrown(root) => JsError::from_opaque(rooted(&root)),
        Exception::Value(value) => match value.into_js(ctx) {
            Ok(value) => JsError::from_opaque(value),
            Err(e) => e,
        },
    }
}

/// Captures a thrown error so that Rust can carry it around.
pub fn exception_from_js(error: JsError, ctx: &mut Context) -> Exception {
    match error.into_opaque(ctx) {
        Ok(value) => Exception::Thrown(root(value)),
        Err(_) => Exception::type_error("An error could not be converted"),
    }
}

// ------------------------------------------------------------ conversions

/// Conversion from a script value to an IDL type.
pub trait FromJs: Sized {
    fn from_js(v: &JsValue, ctx: &mut Context) -> JsResult<Self>;
}

/// Conversion from an IDL type to a script value.
pub trait IntoJs {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue>;
}

impl IntoJs for () {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsValue::undefined())
    }
}

impl IntoJs for bool {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsValue::new(self))
    }
}

macro_rules! number_into_js {
    ($($ty:ty),*) => {
        $(
            impl IntoJs for $ty {
                fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
                    Ok(JsValue::new(self as f64))
                }
            }
        )*
    };
}
number_into_js!(i8, u8, i16, u16, u32, i64, u64, f64);

impl IntoJs for i32 {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsValue::new(self))
    }
}

impl IntoJs for String {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsString::from(self.as_str()).into())
    }
}

impl IntoJs for &str {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsString::from(self).into())
    }
}

impl IntoJs for JsValue {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(self)
    }
}

impl IntoJs for NodeId {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(runtime(ctx).wrap_node(self, ctx))
    }
}

impl IntoJs for ObjectId {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        runtime(ctx).wrap_object(self, ctx)
    }
}

impl IntoJs for WindowRef {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(ctx.global_object().into())
    }
}

impl IntoJs for EventTargetRef {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        match self {
            EventTargetRef::Window => WindowRef.into_js(ctx),
            EventTargetRef::Node(id) => id.into_js(ctx),
            EventTargetRef::Object(id) => id.into_js(ctx),
        }
    }
}

impl IntoJs for Callback {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(rooted(&self.root))
    }
}

impl IntoJs for PromiseRef {
    fn into_js(self, _ctx: &mut Context) -> JsResult<JsValue> {
        Ok(rooted(&self.0))
    }
}

impl IntoJs for ArrayBufferData {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        let block = AlignedVec::from_iter(0, self.0);
        Ok(JsArrayBuffer::from_byte_block(block, ctx)?.into())
    }
}

impl IntoJs for Uint8ArrayData {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(JsUint8Array::from_iter(self.0, ctx)?.into())
    }
}

impl<T: IntoJs> IntoJs for Option<T> {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        match self {
            Some(v) => v.into_js(ctx),
            None => Ok(JsValue::null()),
        }
    }
}

impl<T: IntoJs> IntoJs for Vec<T> {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        let mut items = Vec::with_capacity(self.len());
        for item in self {
            items.push(item.into_js(ctx)?);
        }
        Ok(JsArray::from_iter(items, ctx).into())
    }
}

/// A key/value pair becomes a two-element array (an iterator entry).
impl<A: IntoJs, B: IntoJs> IntoJs for (A, B) {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        let items = [self.0.into_js(ctx)?, self.1.into_js(ctx)?];
        Ok(JsArray::from_iter(items, ctx).into())
    }
}

impl IntoJs for Value {
    fn into_js(self, ctx: &mut Context) -> JsResult<JsValue> {
        Ok(match self {
            Value::Undefined => JsValue::undefined(),
            Value::Null => JsValue::null(),
            Value::Bool(b) => JsValue::new(b),
            Value::Number(n) => JsValue::new(n),
            Value::String(s) => JsString::from(s.as_str()).into(),
            Value::Node(id) => id.into_js(ctx)?,
            Value::Object(id) => id.into_js(ctx)?,
            Value::Window => ctx.global_object().into(),
            Value::Array(items) => items.into_js(ctx)?,
            Value::Record(entries) => {
                let object = new_plain_object(ctx);
                for (key, value) in entries {
                    set_member(&object, &key, value, ctx)?;
                }
                object.into()
            }
            Value::ArrayBuffer(bytes) => ArrayBufferData(bytes).into_js(ctx)?,
            Value::Uint8Array(bytes) => Uint8ArrayData(bytes).into_js(ctx)?,
            Value::Promise(promise) => rooted(&promise.0),
            Value::Callback(callback) => rooted(&callback.root),
            Value::Opaque(root) => rooted(&root),
        })
    }
}

/// Converts a script value to a [`Value`]: primitives and platform objects
/// by meaning, everything else as an opaque rooted handle.
pub fn value_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<Value> {
    if v.is_undefined() {
        return Ok(Value::Undefined);
    }
    if v.is_null() {
        return Ok(Value::Null);
    }
    if let Some(b) = v.as_boolean() {
        return Ok(Value::Bool(b));
    }
    if let Some(n) = v.as_number() {
        return Ok(Value::Number(n));
    }
    if let Some(s) = v.as_string() {
        return Ok(Value::String(s.to_std_string_lossy()));
    }
    if let Some(obj) = v.as_object() {
        match runtime(ctx).native_of(&obj, ctx) {
            Some(Native::Node(id, _)) => return Ok(Value::Node(id)),
            Some(Native::Object(id, _)) => return Ok(Value::Object(id)),
            Some(Native::Window) => return Ok(Value::Window),
            None => {}
        }
    }
    Ok(Value::Opaque(root(v.clone())))
}

pub fn ret<T: IntoJs>(result: Fallible<T>, ctx: &mut Context) -> JsResult<JsValue> {
    match result {
        Ok(value) => value.into_js(ctx),
        Err(e) => Err(exception_to_js(e, ctx)),
    }
}

pub fn ret_opt<T: IntoJs>(
    result: Fallible<Option<T>>,
    ctx: &mut Context,
) -> JsResult<Option<JsValue>> {
    match result {
        Ok(Some(value)) => value.into_js(ctx).map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(exception_to_js(e, ctx)),
    }
}

pub fn ret_pairs<K: IntoJs, V: IntoJs>(
    result: Fallible<Vec<(K, V)>>,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    ret(result, ctx)
}

/// The argument at `index`, or `undefined` if it was not passed.
pub fn arg(args: &[JsValue], index: usize) -> &JsValue {
    match args.get(index) {
        Some(v) => v,
        None => UNDEFINED.with(|u| *u),
    }
}

pub fn require_args(args: &[JsValue], count: usize, label: &str) -> JsResult<()> {
    if args.len() >= count {
        return Ok(());
    }
    Err(type_error(&format!(
        "Failed to execute '{label}': {count} argument{} required, but only {} present.",
        if count == 1 { "" } else { "s" },
        args.len()
    )))
}

pub fn require_new(new_target: &JsValue, name: &str) -> JsResult<()> {
    if new_target.is_undefined() {
        return Err(type_error(&format!(
            "Failed to construct '{name}': Please use the 'new' operator."
        )));
    }
    Ok(())
}

pub fn string_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<String> {
    if let Some(s) = v.as_string() {
        return Ok(s.to_std_string_lossy());
    }
    Ok(v.to_string(ctx)?.to_std_string_lossy())
}

pub fn string_from_js_null_empty(v: &JsValue, ctx: &mut Context) -> JsResult<String> {
    if v.is_null() {
        return Ok(String::new());
    }
    string_from_js(v, ctx)
}

/// WebIDL `double` and `float`: a number that is neither NaN nor infinite.
pub fn to_finite(v: &JsValue, ctx: &mut Context) -> JsResult<f64> {
    let n = v.to_number(ctx)?;
    if n.is_finite() {
        Ok(n)
    } else {
        Err(type_error("The provided value is not a finite number"))
    }
}

const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

pub fn to_u64(v: &JsValue, ctx: &mut Context) -> JsResult<u64> {
    let n = v.to_number(ctx)?;
    if !n.is_finite() {
        return Ok(0);
    }
    Ok(n.trunc().rem_euclid(TWO_POW_64) as u64)
}

pub fn to_i64(v: &JsValue, ctx: &mut Context) -> JsResult<i64> {
    Ok(to_u64(v, ctx)? as i64)
}

fn array_buffer_bytes(buffer: &JsValue) -> Option<Vec<u8>> {
    let buffer = JsArrayBuffer::from_object(buffer.as_object()?).ok()?;
    buffer.data().map(|d| d.to_vec())
}

fn view_bytes(buffer: &JsValue, offset: usize, length: usize) -> JsResult<Vec<u8>> {
    let bytes = array_buffer_bytes(buffer)
        .ok_or_else(|| type_error("The view's buffer is detached or shared"))?;
    bytes
        .get(offset..offset.saturating_add(length))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| type_error("The view is out of bounds"))
}

pub fn is_buffer(v: &JsValue) -> bool {
    v.as_object().is_some_and(|obj| {
        JsArrayBuffer::from_object(obj.clone()).is_ok()
            || JsTypedArray::from_object(obj.clone()).is_ok()
            || JsDataView::from_object(obj).is_ok()
    })
}

/// The bytes of an `ArrayBuffer` or of the part of one a view covers.
pub fn buffer_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<Vec<u8>> {
    let not_a_buffer = || type_error("The value is not an ArrayBuffer or an ArrayBufferView");
    let obj = v.as_object().ok_or_else(not_a_buffer)?;
    if JsArrayBuffer::from_object(obj.clone()).is_ok() {
        return array_buffer_bytes(v).ok_or_else(|| type_error("The ArrayBuffer is detached"));
    }
    if let Ok(view) = JsTypedArray::from_object(obj.clone()) {
        let buffer = view.buffer(ctx)?;
        let offset = view.byte_offset(ctx)?;
        let length = view.byte_length(ctx)?;
        return view_bytes(&buffer, offset, length);
    }
    if let Ok(view) = JsDataView::from_object(obj) {
        let buffer = view.buffer(ctx)?;
        let offset = view.byte_offset(ctx)? as usize;
        let length = view.byte_length(ctx)? as usize;
        return view_bytes(&buffer, offset, length);
    }
    Err(not_a_buffer())
}

fn wrong_type(iface: I) -> JsError {
    type_error(&format!("The value is not of type '{}'.", iface.name()))
}

pub fn node_from_js(v: &JsValue, iface: I, ctx: &mut Context) -> JsResult<NodeId> {
    let native = v
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Node(id, actual)) if actual.is_a(iface) => Ok(id),
        _ => Err(wrong_type(iface)),
    }
}

pub fn object_from_js(v: &JsValue, iface: I, ctx: &mut Context) -> JsResult<ObjectId> {
    let native = v
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Object(id, actual)) if actual.is_a(iface) => Ok(id),
        _ => Err(wrong_type(iface)),
    }
}

pub fn window_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<WindowRef> {
    if is_window(v, ctx) {
        Ok(WindowRef)
    } else {
        Err(type_error("The value is not of type 'Window'."))
    }
}

pub fn event_target_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<EventTargetRef> {
    let native = v
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Window) => Ok(EventTargetRef::Window),
        Some(Native::Node(id, _)) => Ok(EventTargetRef::Node(id)),
        Some(Native::Object(id, actual)) if actual.is_a(I::EventTarget) => {
            Ok(EventTargetRef::Object(id))
        }
        _ => Err(wrong_type(I::EventTarget)),
    }
}

pub fn is_instance(v: &JsValue, iface: I, ctx: &mut Context) -> bool {
    let native = v
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Node(_, actual) | Native::Object(_, actual)) => actual.is_a(iface),
        _ => false,
    }
}

pub fn is_window(v: &JsValue, ctx: &mut Context) -> bool {
    v.as_object()
        .is_some_and(|obj| JsObject::equals(&obj, &ctx.global_object()))
}

pub fn callback_from_js(v: &JsValue, kind: CallbackKind, _ctx: &mut Context) -> JsResult<Callback> {
    match kind {
        CallbackKind::Function if !v.is_callable() => {
            return Err(type_error(
                "The callback provided as parameter is not a function.",
            ));
        }
        CallbackKind::Interface if !v.is_object() => {
            return Err(type_error(
                "The callback provided as parameter is not an object.",
            ));
        }
        _ => {}
    }
    Ok(Callback {
        root: root(v.clone()),
        kind,
    })
}

/// An event handler attribute value: any object is kept, anything else
/// means "no handler".
pub fn event_handler_from_js(v: &JsValue, _ctx: &mut Context) -> JsResult<Option<Callback>> {
    Ok(v.is_object().then(|| Callback {
        root: root(v.clone()),
        kind: CallbackKind::Function,
    }))
}

pub fn promise_from_js(v: &JsValue, ctx: &mut Context) -> JsResult<PromiseRef> {
    let promise = JsPromise::resolve(v.clone(), ctx)?;
    Ok(PromiseRef(root(promise.into())))
}

/// Runs `f` for every value the iterable `v` yields.
fn for_each_iterated(
    v: &JsValue,
    ctx: &mut Context,
    mut f: impl FnMut(&JsValue, &mut Context) -> JsResult<()>,
) -> JsResult<()> {
    let not_iterable = || type_error("The provided value is not iterable.");
    let obj = v.as_object().ok_or_else(not_iterable)?;
    let method = obj
        .get(JsSymbol::iterator(), ctx)?
        .as_callable()
        .ok_or_else(not_iterable)?;
    let iterator = method.call(v, &[], ctx)?;
    let next = iterator
        .as_object()
        .ok_or_else(not_iterable)?
        .get(js_string!("next"), ctx)?
        .as_callable()
        .ok_or_else(not_iterable)?;
    loop {
        let result = next.call(&iterator, &[], ctx)?;
        let result = result
            .as_object()
            .ok_or_else(|| type_error("The iterator result is not an object."))?;
        if result.get(js_string!("done"), ctx)?.to_boolean() {
            return Ok(());
        }
        let value = result.get(js_string!("value"), ctx)?;
        f(&value, ctx)?;
    }
}

pub fn sequence_from_js<T>(
    v: &JsValue,
    ctx: &mut Context,
    mut f: impl FnMut(&JsValue, &mut Context) -> JsResult<T>,
) -> JsResult<Vec<T>> {
    let mut out = Vec::new();
    for_each_iterated(v, ctx, |item, ctx| {
        out.push(f(item, ctx)?);
        Ok(())
    })?;
    Ok(out)
}

pub fn is_iterable(v: &JsValue, ctx: &mut Context) -> JsResult<bool> {
    let Some(obj) = v.as_object() else {
        return Ok(false);
    };
    Ok(obj.get(JsSymbol::iterator(), ctx)?.is_callable())
}

fn key_to_string(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::String(s) => Some(s.to_std_string_lossy()),
        PropertyKey::Index(i) => Some(i.get().to_string()),
        PropertyKey::Symbol(_) => None,
    }
}

/// `record<K, V>`: the own enumerable string-keyed properties, in order.
pub fn record_from_js<T>(
    v: &JsValue,
    ctx: &mut Context,
    mut f: impl FnMut(&JsValue, &mut Context) -> JsResult<T>,
) -> JsResult<Vec<(String, T)>> {
    let obj = v
        .as_object()
        .ok_or_else(|| type_error("The provided value is not an object."))?;
    let mut out = Vec::new();
    for key in obj.own_property_keys(ctx)? {
        let Some(name) = key_to_string(&key) else {
            continue;
        };
        let enumerable = own_descriptor(&obj, &key, ctx)?
            .and_then(|d| d.get(js_string!("enumerable"), ctx).ok())
            .is_some_and(|e| e.to_boolean());
        if !enumerable {
            continue;
        }
        let value = obj.get(key, ctx)?;
        out.push((name, f(&value, ctx)?));
    }
    Ok(out)
}

pub fn dictionary_object(v: &JsValue, name: &str) -> JsResult<Option<JsObject>> {
    if v.is_null_or_undefined() {
        return Ok(None);
    }
    v.as_object().map(Some).ok_or_else(|| {
        type_error(&format!(
            "The provided value is not of type '{name}' (an object is required)."
        ))
    })
}

pub fn dictionary_member(
    obj: &Option<JsObject>,
    name: &str,
    ctx: &mut Context,
) -> JsResult<Option<JsValue>> {
    let Some(obj) = obj else {
        return Ok(None);
    };
    let value = obj.get(JsString::from(name), ctx)?;
    Ok((!value.is_undefined()).then_some(value))
}

pub fn new_plain_object(ctx: &mut Context) -> JsObject {
    JsObject::with_object_proto(ctx.intrinsics())
}

pub fn set_member<T: IntoJs>(
    obj: &JsObject,
    name: &str,
    value: T,
    ctx: &mut Context,
) -> JsResult<()> {
    let value = value.into_js(ctx)?;
    obj.create_data_property_or_throw(JsString::from(name), value, ctx)?;
    Ok(())
}

// ------------------------------------------------------------ receivers

pub fn this_node(this: &JsValue, iface: I, ctx: &mut Context) -> JsResult<NodeId> {
    let native = this
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Node(id, actual)) if actual.is_a(iface) => Ok(id),
        _ => Err(illegal_invocation()),
    }
}

pub fn this_object(this: &JsValue, iface: I, ctx: &mut Context) -> JsResult<ObjectId> {
    let native = this
        .as_object()
        .and_then(|obj| runtime(ctx).native_of(&obj, ctx));
    match native {
        Some(Native::Object(id, actual)) if actual.is_a(iface) => Ok(id),
        _ => Err(illegal_invocation()),
    }
}

/// The receiver of an `EventTarget` member. A missing receiver means the
/// global object, as when calling a bare `addEventListener(...)`.
pub fn this_event_target(this: &JsValue, ctx: &mut Context) -> JsResult<EventTargetRef> {
    if this.is_null_or_undefined() {
        return Ok(EventTargetRef::Window);
    }
    event_target_from_js(this, ctx).map_err(|_| illegal_invocation())
}

/// The receiver of a `Window` member: the global object, or nothing at all.
pub fn this_window(this: &JsValue, ctx: &mut Context) -> JsResult<()> {
    if this.is_null_or_undefined() || is_window(this, ctx) {
        Ok(())
    } else {
        Err(illegal_invocation())
    }
}

fn this_or_global(this: &JsValue, ctx: &mut Context) -> JsResult<JsObject> {
    match this.as_object() {
        Some(obj) => Ok(obj),
        None if this.is_null_or_undefined() => Ok(ctx.global_object()),
        None => Err(illegal_invocation()),
    }
}

/// A `[SameObject]` attribute's cached value on its holder.
pub fn cached(this: &JsValue, name: &'static str, ctx: &mut Context) -> Option<JsValue> {
    runtime(ctx)
        .cache_of(this, ctx, |cache| {
            cache
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        })
        .flatten()
}

pub fn cache(this: &JsValue, name: &'static str, value: &JsValue, ctx: &mut Context) {
    runtime(ctx).cache_of(this, ctx, |cache| {
        if !cache.iter().any(|(n, _)| *n == name) {
            cache.push((name, value.clone()));
        }
    });
}

/// `[PutForwards=target]`: assigning to `attr` assigns to `attr.target`.
pub fn put_forwards(
    this: &JsValue,
    attr: &str,
    target: &str,
    value: &JsValue,
    ctx: &mut Context,
) -> JsResult<()> {
    let holder = this_or_global(this, ctx)?;
    let forwarded = holder.get(JsString::from(attr), ctx)?;
    let forwarded = forwarded
        .as_object()
        .ok_or_else(|| type_error("The forwarding attribute's value is not an object."))?;
    forwarded.set(JsString::from(target), value.clone(), true, ctx)?;
    Ok(())
}

/// `[Replaceable]`: assigning shadows the attribute with a data property.
pub fn replace_property(
    this: &JsValue,
    name: &str,
    value: &JsValue,
    ctx: &mut Context,
) -> JsResult<()> {
    let holder = this_or_global(this, ctx)?;
    holder.define_property_or_throw(
        JsString::from(name),
        PropertyDescriptor::builder()
            .value(value.clone())
            .writable(true)
            .enumerable(true)
            .configurable(true)
            .build(),
        ctx,
    )?;
    Ok(())
}

pub fn count_stub(ctx: &mut Context, label: &'static str) {
    runtime(ctx).page.count_stub(label);
}

/// What a constructor implementation returns for a platform object.
pub trait ConstructedObject {
    fn object_id(self) -> Option<ObjectId>;
}

impl ConstructedObject for ObjectId {
    fn object_id(self) -> Option<ObjectId> {
        Some(self)
    }
}

impl ConstructedObject for EventTargetRef {
    fn object_id(self) -> Option<ObjectId> {
        match self {
            EventTargetRef::Object(id) => Some(id),
            _ => None,
        }
    }
}

/// The prototype for an object being constructed: `new.target.prototype`,
/// so that subclasses get instances of their own.
fn prototype_from_new_target(
    new_target: &JsValue,
    ctx: &mut Context,
) -> JsResult<Option<JsObject>> {
    let Some(constructor) = new_target.as_object() else {
        return Ok(None);
    };
    Ok(constructor.get(js_string!("prototype"), ctx)?.as_object())
}

pub fn wrap_constructed_object(
    id: impl ConstructedObject,
    new_target: &JsValue,
    iface: I,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let id = id
        .object_id()
        .ok_or_else(|| type_error("The constructor did not create an object."))?;
    let rt = runtime(ctx);
    let proto = prototype_from_new_target(new_target, ctx)?;
    let iface = rt.page.interface_of(id).unwrap_or(iface);
    let wrapper = rt.create_object_wrapper(id, iface, proto, ctx)?;
    rt.register_object(id, &wrapper, ctx)?;
    Ok(wrapper.into())
}

pub fn wrap_constructed_node(
    id: NodeId,
    new_target: &JsValue,
    iface: I,
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let rt = runtime(ctx);
    let proto = prototype_from_new_target(new_target, ctx)?.or_else(|| rt.proto(iface));
    let wrapper = JsObject::from_proto_and_data(proto, NodeWrapper { id, iface });
    rt.nodes.borrow_mut().insert(
        id,
        NodeSlot {
            wrapper: wrapper.clone(),
            cache: Vec::new(),
        },
    );
    Ok(wrapper.into())
}

// ---------------------------------------------------------- proxy traps

/// A canonical array index: the decimal form of an integer below 2^32 - 1.
fn array_index(name: &str) -> Option<u32> {
    if name.is_empty() || (name.len() > 1 && name.starts_with('0')) {
        return None;
    }
    if !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

struct Trap {
    target: JsObject,
    handle: Handle,
    def: &'static ExoticDef,
}

fn trap(args: &[JsValue], ctx: &mut Context) -> JsResult<Trap> {
    let target = arg(args, 0).as_object().ok_or_else(illegal_invocation)?;
    let (handle, iface) = {
        if let Some(w) = target.downcast_ref::<ObjectWrapper>() {
            (Handle::Object(w.id), w.iface)
        } else if let Some(w) = target.downcast_ref::<NodeWrapper>() {
            (Handle::Node(w.id), w.iface)
        } else {
            return Err(illegal_invocation());
        }
    };
    let def = runtime(ctx).exotic(iface).ok_or_else(illegal_invocation)?;
    Ok(Trap {
        target,
        handle,
        def,
    })
}

/// The property name of a trap's key argument, unless it is a symbol.
fn trap_key(key: &JsValue) -> Option<String> {
    key.as_string().map(|s| s.to_std_string_lossy())
}

fn property_key(key: &JsValue, ctx: &mut Context) -> JsResult<PropertyKey> {
    key.to_property_key(ctx)
}

/// Whether the named property `name` is visible on the object.
fn named_visible(
    t: &Trap,
    name: &str,
    key: &JsValue,
    ctx: &mut Context,
) -> JsResult<Option<JsValue>> {
    let Some(named_get) = t.def.named_get else {
        return Ok(None);
    };
    if !t.def.override_builtins && t.target.has_property(property_key(key, ctx)?, ctx)? {
        return Ok(None);
    }
    named_get(t.handle, name, ctx)
}

fn trap_get(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    let Some(name) = trap_key(key) else {
        if let Some(symbol) = key.as_symbol()
            && symbol == runtime(ctx).target_symbol
        {
            return Ok(t.target.into());
        }
        return t.target.get(property_key(key, ctx)?, ctx);
    };
    if let Some(index) = array_index(&name)
        && let Some(indexed_get) = t.def.indexed_get
    {
        return Ok(indexed_get(t.handle, index, ctx)?.unwrap_or_default());
    }
    if let Some(value) = named_visible(&t, &name, key, ctx)? {
        return Ok(value);
    }
    t.target.get(property_key(key, ctx)?, ctx)
}

fn trap_set(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    let value = arg(args, 2);
    if let Some(name) = trap_key(key) {
        if array_index(&name).is_some() && t.def.indexed_get.is_some() {
            // No interface here has an indexed setter.
            return Ok(JsValue::new(false));
        }
        if let Some(named_set) = t.def.named_set {
            let shadowed =
                t.def.attribute_like && t.target.has_property(property_key(key, ctx)?, ctx)?;
            if !shadowed {
                named_set(t.handle, &name, value, ctx)?;
                return Ok(JsValue::new(true));
            }
        }
    }
    let ok = t
        .target
        .set(property_key(key, ctx)?, value.clone(), false, ctx)?;
    Ok(JsValue::new(ok))
}

fn trap_has(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    if let Some(name) = trap_key(key) {
        if let Some(index) = array_index(&name)
            && let Some(indexed_get) = t.def.indexed_get
        {
            return Ok(JsValue::new(indexed_get(t.handle, index, ctx)?.is_some()));
        }
        if named_visible(&t, &name, key, ctx)?.is_some() {
            return Ok(JsValue::new(true));
        }
    }
    let has = t.target.has_property(property_key(key, ctx)?, ctx)?;
    Ok(JsValue::new(has))
}

fn trap_delete(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    if let Some(name) = trap_key(key) {
        if let Some(index) = array_index(&name)
            && let Some(indexed_get) = t.def.indexed_get
        {
            // Indexed properties cannot be deleted while they exist.
            let exists = indexed_get(t.handle, index, ctx)?.is_some();
            return Ok(JsValue::new(!exists));
        }
        if named_visible(&t, &name, key, ctx)?.is_some() {
            return Ok(JsValue::new(match t.def.named_delete {
                Some(named_delete) => named_delete(t.handle, &name, ctx)?,
                None => false,
            }));
        }
    }
    let deleted = t
        .target
        .delete_property_or_throw(property_key(key, ctx)?, ctx)
        .unwrap_or(false);
    Ok(JsValue::new(deleted))
}

fn trap_own_keys(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let mut keys: Vec<JsValue> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(length) = t.def.length {
        for i in 0..length(t.handle, ctx)? {
            let name = i.to_string();
            keys.push(JsString::from(name.as_str()).into());
            seen.push(name);
        }
    }
    if let Some(named_properties) = t.def.named_properties {
        for name in named_properties(t.handle, ctx)? {
            if seen.contains(&name) {
                continue;
            }
            let key: JsValue = JsString::from(name.as_str()).into();
            if !t.def.override_builtins && t.target.has_property(property_key(&key, ctx)?, ctx)? {
                continue;
            }
            keys.push(key);
            seen.push(name);
        }
    }
    for key in t.target.own_property_keys(ctx)? {
        match key {
            PropertyKey::Symbol(symbol) => keys.push(symbol.into()),
            other => {
                if let Some(name) = key_to_string(&other)
                    && !seen.contains(&name)
                {
                    keys.push(JsString::from(name.as_str()).into());
                    seen.push(name);
                }
            }
        }
    }
    Ok(JsArray::from_iter(keys, ctx).into())
}

fn data_descriptor(value: JsValue, writable: bool, ctx: &mut Context) -> JsResult<JsValue> {
    let descriptor = new_plain_object(ctx);
    descriptor.create_data_property_or_throw(js_string!("value"), value, ctx)?;
    descriptor.create_data_property_or_throw(js_string!("writable"), writable, ctx)?;
    descriptor.create_data_property_or_throw(js_string!("enumerable"), true, ctx)?;
    descriptor.create_data_property_or_throw(js_string!("configurable"), true, ctx)?;
    Ok(descriptor.into())
}

/// `Object.getOwnPropertyDescriptor(obj, key)` as a descriptor object.
fn own_descriptor(
    obj: &JsObject,
    key: &PropertyKey,
    ctx: &mut Context,
) -> JsResult<Option<JsObject>> {
    let function = ctx
        .intrinsics()
        .constructors()
        .object()
        .constructor()
        .get(js_string!("getOwnPropertyDescriptor"), ctx)?;
    let Some(function) = function.as_callable() else {
        return Ok(None);
    };
    let key: JsValue = match key {
        PropertyKey::String(s) => s.clone().into(),
        PropertyKey::Symbol(s) => s.clone().into(),
        PropertyKey::Index(i) => JsString::from(i.get().to_string()).into(),
    };
    let result = function.call(&JsValue::undefined(), &[obj.clone().into(), key], ctx)?;
    Ok(result.as_object())
}

fn trap_get_own_property_descriptor(
    _this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    if let Some(name) = trap_key(key) {
        if let Some(index) = array_index(&name)
            && let Some(indexed_get) = t.def.indexed_get
        {
            return match indexed_get(t.handle, index, ctx)? {
                Some(value) => data_descriptor(value, false, ctx),
                None => Ok(JsValue::undefined()),
            };
        }
        if let Some(value) = named_visible(&t, &name, key, ctx)? {
            return data_descriptor(value, t.def.named_set.is_some(), ctx);
        }
    }
    let key = property_key(key, ctx)?;
    Ok(own_descriptor(&t.target, &key, ctx)?
        .map(JsValue::from)
        .unwrap_or_default())
}

fn trap_define_property(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let t = trap(args, ctx)?;
    let key = arg(args, 1);
    let descriptor = arg(args, 2);
    if let Some(name) = trap_key(key) {
        if array_index(&name).is_some() && t.def.indexed_get.is_some() {
            return Ok(JsValue::new(false));
        }
        if let Some(named_set) = t.def.named_set
            && let Some(descriptor) = descriptor.as_object()
        {
            let value = descriptor.get(js_string!("value"), ctx)?;
            named_set(t.handle, &name, &value, ctx)?;
            return Ok(JsValue::new(true));
        }
    }
    let descriptor = descriptor.to_property_descriptor(ctx)?;
    let ok = t
        .target
        .define_property_or_throw(property_key(key, ctx)?, descriptor, ctx)
        .is_ok();
    Ok(JsValue::new(ok))
}

// ---------------------------------------------------------- installation

fn illegal_constructor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Err(type_error("Illegal constructor"))
}

fn function(
    ctx: &Context,
    f: NativeFunction,
    name: &str,
    length: usize,
    constructor: bool,
) -> JsObject {
    FunctionObjectBuilder::new(ctx.realm(), f)
        .name(JsString::from(name))
        .length(length)
        .constructor(constructor)
        .build()
        .into()
}

fn method_descriptor(function: JsObject) -> PropertyDescriptor {
    PropertyDescriptor::builder()
        .value(function)
        .writable(true)
        .enumerable(true)
        .configurable(true)
        .build()
}

fn hidden_descriptor(value: impl Into<JsValue>) -> PropertyDescriptor {
    PropertyDescriptor::builder()
        .value(value)
        .writable(true)
        .enumerable(false)
        .configurable(true)
        .build()
}

fn define_attr(ctx: &Context, holder: &JsObject, attr: &AttrDef) {
    let getter = function(
        ctx,
        NativeFunction::from_fn_ptr(attr.getter),
        &format!("get {}", attr.name),
        0,
        false,
    );
    let mut descriptor = PropertyDescriptor::builder()
        .get(getter)
        .enumerable(true)
        .configurable(true);
    descriptor = match attr.setter {
        Some(setter) => descriptor.set(function(
            ctx,
            NativeFunction::from_fn_ptr(setter),
            &format!("set {}", attr.name),
            1,
            false,
        )),
        None => descriptor.set(JsValue::undefined()),
    };
    holder.insert_property(JsString::from(attr.name), descriptor.build());
}

fn define_op(ctx: &Context, holder: &JsObject, op: &OpDef) {
    let f = function(
        ctx,
        NativeFunction::from_fn_ptr(op.func),
        op.name,
        op.length,
        false,
    );
    holder.insert_property(JsString::from(op.name), method_descriptor(f));
}

fn iterator_of(value: &JsValue, ctx: &mut Context) -> JsResult<JsValue> {
    let obj = value
        .as_object()
        .ok_or_else(|| type_error("The value is not iterable."))?;
    let method = obj
        .get(JsSymbol::iterator(), ctx)?
        .as_callable()
        .ok_or_else(|| type_error("The value is not iterable."))?;
    method.call(value, &[], ctx)
}

/// Splits the `[key, value]` array `pair`.
fn split_pair(pair: &JsValue, ctx: &mut Context) -> JsResult<(JsValue, JsValue)> {
    let pair = pair
        .as_object()
        .ok_or_else(|| type_error("The entry is not an array."))?;
    Ok((pair.get(0, ctx)?, pair.get(1, ctx)?))
}

fn pairs_of(
    pairs: NativeFunctionPointer,
    this: &JsValue,
    ctx: &mut Context,
) -> JsResult<Vec<JsValue>> {
    let array = pairs(this, &[], ctx)?;
    let array = array
        .as_object()
        .and_then(|o| JsArray::from_object(o).ok())
        .ok_or_else(|| type_error("The entries are not an array."))?;
    let length = array.length(ctx)?;
    let mut out = Vec::with_capacity(length as usize);
    for i in 0..length {
        out.push(array.at(i as i64, ctx)?);
    }
    Ok(out)
}

/// Installs `entries`, `keys`, `values`, `forEach` and `@@iterator` for a
/// pair iterable whose entries come from `pairs`.
fn define_pair_iteration(ctx: &mut Context, proto: &JsObject, pairs: NativeFunctionPointer) {
    let entries = NativeFunction::from_copy_closure(move |this, _args, ctx| {
        let items = pairs_of(pairs, this, ctx)?;
        let array: JsValue = JsArray::from_iter(items, ctx).into();
        iterator_of(&array, ctx)
    });
    let keys = NativeFunction::from_copy_closure(move |this, _args, ctx| {
        let mut items = Vec::new();
        for pair in pairs_of(pairs, this, ctx)? {
            items.push(split_pair(&pair, ctx)?.0);
        }
        let array: JsValue = JsArray::from_iter(items, ctx).into();
        iterator_of(&array, ctx)
    });
    let values = NativeFunction::from_copy_closure(move |this, _args, ctx| {
        let mut items = Vec::new();
        for pair in pairs_of(pairs, this, ctx)? {
            items.push(split_pair(&pair, ctx)?.1);
        }
        let array: JsValue = JsArray::from_iter(items, ctx).into();
        iterator_of(&array, ctx)
    });
    let for_each = NativeFunction::from_copy_closure(move |this, args, ctx| {
        let callback = arg(args, 0)
            .as_callable()
            .ok_or_else(|| type_error("The callback provided as parameter is not a function."))?;
        let this_arg = arg(args, 1).clone();
        for pair in pairs_of(pairs, this, ctx)? {
            let (key, value) = split_pair(&pair, ctx)?;
            callback.call(&this_arg, &[value, key, this.clone()], ctx)?;
        }
        Ok(JsValue::undefined())
    });

    let entries = function(ctx, entries, "entries", 0, false);
    proto.insert_property(js_string!("entries"), method_descriptor(entries.clone()));
    proto.insert_property(JsSymbol::iterator(), hidden_descriptor(entries));
    let keys = function(ctx, keys, "keys", 0, false);
    proto.insert_property(js_string!("keys"), method_descriptor(keys));
    let values = function(ctx, values, "values", 0, false);
    proto.insert_property(js_string!("values"), method_descriptor(values));
    let for_each = function(ctx, for_each, "forEach", 1, false);
    proto.insert_property(js_string!("forEach"), method_descriptor(for_each));
}

/// Gives a value iterable the Array.prototype iteration methods, which
/// work on any object with a length and indexed properties.
fn define_value_iteration(ctx: &mut Context, proto: &JsObject) -> JsResult<()> {
    let array_proto = ctx.intrinsics().constructors().array().prototype();
    for name in ["entries", "keys", "values", "forEach"] {
        let f = array_proto.get(JsString::from(name), ctx)?;
        if let Some(f) = f.as_object() {
            proto.insert_property(JsString::from(name), method_descriptor(f));
        }
    }
    let values = array_proto.get(js_string!("values"), ctx)?;
    proto.insert_property(JsSymbol::iterator(), hidden_descriptor(values));
    Ok(())
}

fn install_interface(
    ctx: &mut Context,
    rt: &Runtime,
    def: &'static InterfaceDef,
    constructors: &mut [Option<JsObject>],
) -> JsResult<()> {
    let global = ctx.global_object();
    let proto = JsObject::with_object_proto(ctx.intrinsics());
    if let Some(parent) = def.parent
        && let Some(parent_proto) = rt.proto(parent)
    {
        proto.set_prototype(Some(parent_proto));
    }

    // A global interface exposes its members on the global object itself.
    let member_holder = if def.global { &global } else { &proto };
    for attr in def.attrs {
        define_attr(ctx, member_holder, attr);
    }
    for op in def.ops {
        define_op(ctx, member_holder, op);
    }

    let constructor = function(
        ctx,
        NativeFunction::from_fn_ptr(def.constructor.unwrap_or(illegal_constructor)),
        def.name,
        def.constructor_length,
        true,
    );
    if let Some(parent) = def.parent
        && let Some(parent_constructor) = constructors[parent as usize].clone()
    {
        constructor.set_prototype(Some(parent_constructor));
    }
    constructor.insert_property(
        js_string!("prototype"),
        PropertyDescriptor::builder()
            .value(proto.clone())
            .writable(false)
            .enumerable(false)
            .configurable(false)
            .build(),
    );
    proto.insert_property(
        js_string!("constructor"),
        hidden_descriptor(constructor.clone()),
    );
    proto.insert_property(
        JsSymbol::to_string_tag(),
        PropertyDescriptor::builder()
            .value(JsString::from(def.name))
            .writable(false)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
    for attr in def.static_attrs {
        define_attr(ctx, &constructor, attr);
    }
    for op in def.static_ops {
        define_op(ctx, &constructor, op);
    }
    for (name, value) in def.consts {
        let descriptor = PropertyDescriptor::builder()
            .value(*value)
            .writable(false)
            .enumerable(true)
            .configurable(false)
            .build();
        constructor.insert_property(JsString::from(*name), descriptor.clone());
        proto.insert_property(JsString::from(*name), descriptor);
    }

    match def.iterable {
        // Indexed properties alone make an object iterable, without the
        // other iteration methods.
        Iterable::None => {
            if def.exotic.as_ref().is_some_and(|e| e.indexed_get.is_some()) {
                let values = ctx
                    .intrinsics()
                    .constructors()
                    .array()
                    .prototype()
                    .get(js_string!("values"), ctx)?;
                proto.insert_property(JsSymbol::iterator(), hidden_descriptor(values));
            }
        }
        Iterable::Values => define_value_iteration(ctx, &proto)?,
        Iterable::Pairs(pairs) => define_pair_iteration(ctx, &proto, pairs),
    }

    global.insert_property(
        JsString::from(def.name),
        hidden_descriptor(constructor.clone()),
    );
    if def.global {
        global.set_prototype(Some(proto.clone()));
    }
    rt.protos.borrow_mut()[def.id as usize] = Some(proto);
    constructors[def.id as usize] = Some(constructor);
    Ok(())
}

fn install_namespace(ctx: &mut Context, def: &'static NamespaceDef) {
    let namespace = JsObject::with_object_proto(ctx.intrinsics());
    for op in def.ops {
        define_op(ctx, &namespace, op);
    }
    namespace.insert_property(
        JsSymbol::to_string_tag(),
        PropertyDescriptor::builder()
            .value(JsString::from(def.name))
            .writable(false)
            .enumerable(false)
            .configurable(true)
            .build(),
    );
    ctx.global_object()
        .insert_property(JsString::from(def.name), hidden_descriptor(namespace));
}

/// Installs every interface and namespace into the context's realm.
pub(crate) fn install(ctx: &mut Context, rt: &Runtime) -> JsResult<()> {
    let weak_ref = ctx.intrinsics().constructors().weak_ref();
    let (weak_constructor, weak_prototype) = (weak_ref.constructor(), weak_ref.prototype());
    if let Some(deref) = weak_prototype.get(js_string!("deref"), ctx)?.as_object() {
        *rt.weak_ref.borrow_mut() = Some((weak_constructor, deref));
    }

    let mut constructors: Vec<Option<JsObject>> = vec![None; I::COUNT];
    for def in crate::generated::INTERFACES {
        install_interface(ctx, rt, def, &mut constructors)?;
    }
    for def in crate::generated::NAMESPACES {
        install_namespace(ctx, def);
    }
    Ok(())
}

/// A short description of a platform object for the console, or `None`
/// for ordinary script objects.
pub(crate) fn describe_native(obj: &JsObject, ctx: &mut Context) -> Option<String> {
    let rt = runtime(ctx);
    match rt.native_of(obj, ctx)? {
        Native::Window => Some("[object Window]".to_string()),
        Native::Object(_, iface) => Some(format!("[object {}]", iface.name())),
        Native::Node(id, iface) => {
            let dom = rt.page.dom.borrow();
            let Some(el) = dom.element(id) else {
                return Some(match dom.get(id).and_then(|n| n.as_text()) {
                    Some(text) => format!("#text {text:?}"),
                    None => format!("[object {}]", iface.name()),
                });
            };
            let mut out = format!("<{}", el.name.local);
            for attr in el.attrs.iter().take(8) {
                out.push_str(&format!(" {}=\"{}\"", attr.name.local, attr.value));
            }
            out.push('>');
            Some(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_array_indices() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("42"), Some(42));
        assert_eq!(array_index("4294967294"), Some(4_294_967_294));
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("01"), None);
        assert_eq!(array_index("-1"), None);
        assert_eq!(array_index("1.5"), None);
        assert_eq!(array_index(""), None);
        assert_eq!(array_index("length"), None);
    }
}
