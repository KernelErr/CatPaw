//! CatPaw style: Stylo (Servo's style system) running over the arena DOM.
//!
//! The DOM crate knows nothing about style. This crate keeps a side table of
//! per-element Stylo data ([`StyleTable`]), implements Stylo's DOM traits on
//! a lightweight `(&Dom, &StyleTable, NodeId)` handle ([`CatNode`]), and
//! drives restyles through [`StyleEngine`]. It is the only crate that depends
//! on Stylo, which isolates the rest of the engine from Stylo's monthly
//! breaking releases.
//!
//! The integration follows Blitz's `blitz-dom/src/stylo.rs`, the reference
//! for using Stylo outside Servo.

pub mod computed;
pub mod cssom;
pub mod engine;
pub mod inline;
pub mod media;
pub mod node;
pub mod query;
pub mod supports;
pub mod table;

pub use computed::{ComputedStyle, Pseudo};
pub use engine::{StyleEngine, StyleOptions};
pub use inline::InlineStyle;
pub use media::MediaQueryList;
pub use node::{CatNode, with_style_context};
pub use query::Selectors;
pub use table::{StyleSlot, StyleTable};

/// The user-agent stylesheet: the HTML rendering section's `display` rules
/// and the other defaults that decide what exists for an agent to see.
pub const UA_STYLESHEET: &str = include_str!("ua.css");
