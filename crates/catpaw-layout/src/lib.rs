//! Layout for CatPaw: a box tree built from the arena DOM and Stylo's
//! computed styles, laid out by Taffy (block, flex, grid) and Parley
//! (inline formatting contexts), and the geometry questions the page asks
//! of it (`getBoundingClientRect`, `offsetWidth`, scroll sizes, hit tests).
//!
//! Layout is lazy and whole: the page builds a tree when something observes
//! geometry and throws it away when the document changes. The tree keeps
//! the computed styles it was built with, so it stays valid after the style
//! engine moves on.
//!
//! Structure follows Blitz's layout (MIT OR Apache-2.0), with the box tree
//! kept apart from the DOM.

mod construct;
mod inline;
mod query;
mod tree;

use std::collections::HashMap;
use std::sync::{Arc as StdArc, Mutex};

use catpaw_dom::{Dom, NodeId};
use catpaw_style::StyleEngine;
use catpaw_text::Fonts;
use catpaw_text::parley;
use slotmap::{SlotMap, new_key_type};
use style::properties::ComputedValues;
use style::servo_arc::Arc;

pub use construct::is_replaced;
pub use query::{HitTarget, ScrollMetrics};

new_key_type! {
    /// A box in a [`LayoutTree`].
    pub struct BoxId;
}

impl BoxId {
    fn to_taffy(self) -> taffy::NodeId {
        taffy::NodeId::from(slotmap::Key::data(&self).as_ffi())
    }

    fn from_taffy(id: taffy::NodeId) -> Self {
        slotmap::KeyData::from_ffi(u64::from(id)).into()
    }
}

/// A rectangle in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// The smallest rectangle holding both.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    pub fn translate(&self, dx: f32, dy: f32) -> Rect {
        Rect {
            x: self.x + dx,
            y: self.y + dy,
            ..*self
        }
    }
}

/// What a box is laid out as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    /// A block container whose children are block-level boxes.
    Block,
    /// A block container whose children are inline-level: text and inline
    /// boxes shaped by Parley.
    InlineRoot,
    Flex,
    Grid,
    /// A leaf sized by what it shows (an image, an iframe, a form control).
    Replaced,
}

/// How an element is positioned, for hoisting and hit-testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Positioning {
    Static,
    Relative,
    Absolute,
    Fixed,
}

/// What a replaced element is sized by.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Intrinsic {
    pub width: Option<f32>,
    pub height: Option<f32>,
    /// Width over height, when the element has one.
    pub ratio: Option<f32>,
    /// The size used when nothing else decides one.
    pub default_width: f32,
    pub default_height: f32,
}

/// One box of the tree.
pub struct LayoutBox {
    /// The element that generated the box; `None` for an anonymous box.
    pub node: Option<NodeId>,
    pub kind: BoxKind,
    pub style: Arc<ComputedValues>,
    pub positioning: Positioning,
    pub(crate) children: Vec<BoxId>,
    pub(crate) parent: Option<BoxId>,
    /// Position and size from Taffy, relative to the parent's border box.
    pub(crate) layout: taffy::Layout,
    pub(crate) cache: taffy::Cache,
    pub(crate) inline: Option<inline::InlineContext>,
    pub(crate) intrinsic: Intrinsic,
    /// Document coordinates of the border box, filled after layout.
    pub(crate) origin: (f32, f32),
}

/// The size of the viewport the tree is laid out in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

/// Everything a layout is built from.
pub struct BuildInput<'a> {
    pub dom: &'a Dom,
    pub styles: &'a StyleEngine,
    pub fonts: &'a StdArc<Mutex<Fonts>>,
    pub viewport: Viewport,
    /// Scroll positions of scroll containers, by element.
    pub scroll_offsets: &'a HashMap<NodeId, (f32, f32)>,
}

/// A laid-out box tree.
pub struct LayoutTree {
    pub(crate) boxes: SlotMap<BoxId, LayoutBox>,
    pub(crate) root: Option<BoxId>,
    /// Boxes positioned against the initial containing block: absolutely
    /// positioned ones without a positioned ancestor, and fixed ones. They
    /// are not children of the root box.
    pub(crate) oof_root: Vec<BoxId>,
    /// The principal box of each element that has one.
    pub(crate) node_box: HashMap<NodeId, BoxId>,
    /// The inline formatting context each inline element and text node
    /// takes part in.
    pub(crate) inline_owner: HashMap<NodeId, BoxId>,
    /// The computed style each inline element and text node was shaped
    /// with (a text node's is its parent's), for painting.
    pub(crate) inline_styles: HashMap<NodeId, Arc<ComputedValues>>,
    pub(crate) viewport: Viewport,
    pub(crate) fonts: StdArc<Mutex<Fonts>>,
    pub(crate) scroll_offsets: HashMap<NodeId, (f32, f32)>,
}

impl LayoutTree {
    /// Builds and lays out the tree for the document.
    pub fn build(input: BuildInput<'_>) -> Self {
        let mut tree = Self {
            boxes: SlotMap::with_key(),
            root: None,
            oof_root: Vec::new(),
            node_box: HashMap::new(),
            inline_owner: HashMap::new(),
            inline_styles: HashMap::new(),
            viewport: input.viewport,
            fonts: input.fonts.clone(),
            scroll_offsets: input.scroll_offsets.clone(),
        };
        construct::build(&mut tree, input.dom, input.styles);
        tree::perform_layout(&mut tree);
        tree::layout_root_oof(&mut tree);
        query::place(&mut tree);
        tree
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn root(&self) -> Option<BoxId> {
        self.root
    }

    pub fn get(&self, id: BoxId) -> &LayoutBox {
        &self.boxes[id]
    }

    pub fn children(&self, id: BoxId) -> &[BoxId] {
        &self.boxes[id].children
    }

    /// The principal box of an element, if it generates one.
    pub fn box_of(&self, node: NodeId) -> Option<BoxId> {
        self.node_box.get(&node).copied()
    }

    /// The boxes positioned against the viewport, painted last.
    pub fn viewport_positioned(&self) -> &[BoxId] {
        &self.oof_root
    }

    /// The shaped lines of an inline root.
    pub fn inline_layout(&self, id: BoxId) -> Option<&parley::Layout<catpaw_text::Brush>> {
        self.boxes[id].inline.as_ref().map(|c| &c.layout)
    }

    /// The style a text run's node was shaped with.
    pub fn inline_style(&self, node: NodeId) -> Option<&Arc<ComputedValues>> {
        self.inline_styles.get(&node)
    }

    /// The node a run's brush names.
    pub fn node_of_brush(brush: catpaw_text::Brush) -> NodeId {
        inline::node_of_brush(brush)
    }

    /// The number of boxes in the tree.
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }
}

/// `calc()` values Taffy hands back for resolution.
pub(crate) fn resolve_calc_value(calc_ptr: *const (), parent_size: f32) -> f32 {
    use style::values::computed::CSSPixelLength;
    use style::values::computed::length_percentage::CalcLengthPercentage;
    // SAFETY: Taffy passes back the pointer `stylo_taffy` gave it for a
    // `calc()` length, which lives in the box's computed values for as long
    // as the box does.
    let calc = unsafe { &*(calc_ptr as *const CalcLengthPercentage) };
    calc.resolve(CSSPixelLength::new(parent_size)).px()
}
