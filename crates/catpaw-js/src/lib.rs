//! Engine-neutral vocabulary between the web platform implementation and a
//! JavaScript engine.
//!
//! `catpaw-web` implements DOM and Web APIs in plain Rust. It never names an
//! engine type: values cross the boundary as [`Value`], script callbacks as
//! [`Callback`], promises as [`PromiseRef`], failures as [`Exception`], and
//! everything the implementation needs *from* the engine goes through the
//! [`ScriptHost`] trait. A backend (`catpaw-bindings-boa` today, V8 later)
//! implements `ScriptHost` and the generated glue that converts between its
//! own values and these types. See ADR 0001.

pub mod exception;
pub mod host;
pub mod value;

pub use catpaw_dom::NodeId;
pub use exception::{Exception, Fallible};
pub use host::ScriptHost;
pub use value::{
    ArrayBufferData, Callback, CallbackKind, EventTargetRef, ObjectId, PromiseRef, Rooted,
    Uint8ArrayData, Value, WindowRef, drain_released, next_root_id,
};
