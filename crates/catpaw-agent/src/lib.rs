//! CatPaw agent layer: the semantic (accessibility-tree-like) view of a
//! document that LLM agents read, with stable element references, plus the
//! alternative "read" views (markdown, text, links, forms).
//!
//! See `docs/adr/0005-cst-snapshot-format.md` for the CST format.
//!
//! This crate depends only on `catpaw-dom`. Anything that needs computed
//! style reaches it through the [`StyleOracle`] trait, which the style crate
//! implements; the built-in [`AttributeOracle`] uses attributes and inline
//! styles alone and is what a parse-only pipeline uses.

pub mod a11y;
pub mod read;
pub mod snapshot;
pub mod visibility;

pub use read::{
    FieldInfo, FormInfo, LinkInfo, LinkStyle, ReadOptions, forms, links, markdown, text,
};
pub use snapshot::{Filter, RefTable, Snapshot, SnapshotOptions, Snapshotter};
pub use visibility::{AttributeOracle, StyleOracle};
