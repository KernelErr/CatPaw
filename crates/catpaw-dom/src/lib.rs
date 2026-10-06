//! CatPaw DOM: the node arena, tree operations, and the html5ever `TreeSink`.
//!
//! Design: every DOM node of an engine thread lives in one generational
//! `SlotMap`; the rest of the engine addresses nodes through [`NodeId`]
//! handles (see `docs/adr/0002-dom-arena-and-wrapper-liveness.md`). This crate
//! knows nothing about JavaScript, style, or layout; those crates layer their
//! own per-node data on top of the ids this crate hands out.

pub mod arena;
pub mod html;
pub mod serialize;

pub use arena::{
    Attr, DoctypeData, DocumentData, Dom, ElementData, FragmentKind, Node, NodeId, NodeKind,
};
pub use html::{HtmlParseOptions, ParseResult, parse_html, parse_html_bytes, parse_html_fragment};
pub use markup5ever::interface::QuirksMode;
pub use markup5ever::{LocalName, Namespace, Prefix, QualName, local_name, namespace_url, ns};
pub use serialize::{html5lib_dump, to_html};
