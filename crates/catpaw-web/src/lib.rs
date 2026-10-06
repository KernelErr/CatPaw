//! CatPaw web platform: the DOM and Web APIs, implemented in plain Rust.
//!
//! This crate knows nothing about any JavaScript engine. It implements the
//! traits `cargo xtask bindgen` generates from Web IDL ([`generated`]) on the
//! unit type [`Web`], working on a page's DOM arena and its platform objects
//! ([`PageState`]), and reaches script only through `catpaw_js::ScriptHost`
//! ([`Cx::script`]). An engine backend (`catpaw-bindings-boa`) supplies that
//! host and the glue that calls into these traits.
//!
//! Besides the APIs themselves, the crate owns the page's event loop
//! ([`event_loop`]), event dispatch ([`events`]) and the HTML parser's
//! interleaving with script execution ([`scripting`]).

mod abort;
pub mod activation;
mod attributes;
pub mod clock;
mod collections;
mod console;
mod cors;
pub mod crypto;
pub mod custom_elements;
mod document;
pub mod element;
mod encoding;
pub mod event_loop;
pub mod events;
mod fetch;
pub mod generated;
mod history;
pub mod html_names;
mod hyperlink;
mod implementation;
mod intersection_observer;
mod media;
mod mutation_observer;
pub mod net;
mod node;
pub mod page;
mod performance;
pub mod reflect;
mod resize_observer;
pub mod scripting;
mod style;
mod stylesheets;
mod svg;
mod traversal;
mod url_api;
mod window;
mod xhr;

pub use element::interface_for_node;
pub use generated::InterfaceId;
pub use page::{
    ConsoleLevel, ConsoleMessage, Cx, DialogRecord, NativeMicrotask, NavigationRequest, PageConfig,
    PageState, PlatformObject,
};

/// The type every generated `XImpl` trait is implemented on.
pub struct Web;
