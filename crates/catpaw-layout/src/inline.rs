//! Inline formatting contexts: the text, inline elements, atomic inline
//! boxes and pseudo-element text of a block container, shaped and broken
//! into lines by Parley.
//!
//! What goes into Parley is recorded first ([`Ops`]). Shaping is the
//! costly part of a layout, so a context whose record equals one of the
//! last layout's takes that one's shaped text ([`Shaped`]) instead of
//! shaping it again.

use std::borrow::Cow;
use std::collections::HashMap;

use slotmap::SecondaryMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_style::StyleEngine;
use catpaw_text::parley;
use catpaw_text::parley::style::{
    FontFamily, FontFamilyName, FontFeatures, FontVariations, GenericFamily, LineHeight,
    OverflowWrap, StyleProperty, TextStyle, TextWrapMode, WhiteSpaceCollapse, WordBreak,
};
use catpaw_text::{Brush, Fonts};
use style::computed_values::text_wrap_mode::T as StyloTextWrapMode;
use style::computed_values::white_space_collapse::T as StyloWhiteSpaceCollapse;
use style::properties::ComputedValues;
use style::servo_arc::Arc;
use style::values::computed::font::{GenericFontFamily, SingleFontFamily};
use style::values::computed::{
    LineBreak, OverflowWrap as StyloOverflowWrap, TextTransform, WordBreak as StyloWordBreak,
};

use crate::{BoxId, LayoutTree};

/// Text a `::before` or `::after` contributes.
pub(crate) struct PseudoText {
    pub owner: NodeId,
    pub text: String,
    pub style: Arc<ComputedValues>,
}

/// One inline-level thing in a block container.
pub(crate) enum InlineItem {
    Text(NodeId),
    /// An inline box (a `span`): its content is shaped in this context.
    Element(NodeId, Arc<ComputedValues>),
    /// An atomic inline-level box (an `inline-block`, an image): laid out
    /// by Taffy, placed by Parley.
    Atomic(BoxId),
    Pseudo(PseudoText),
}

/// The shaped content of an inline root.
pub(crate) struct InlineContext {
    pub layout: parley::Layout<Brush>,
    pub text: String,
    /// The atomic boxes, in the order Parley knows them.
    pub boxes: Vec<BoxId>,
    /// From the top of the content box to the bottom of the lowest line,
    /// once the lines are broken (lines beside floats sit lower than
    /// Parley's own height, a sum of line heights, says).
    pub height: f32,
    /// What was pushed to Parley for it.
    pub ops: Ops,
    /// The box of the last layout whose shaped text it took, if it did.
    pub reused_from: Option<BoxId>,
}

/// The brush of a run of text: the node whose text it is.
pub(crate) fn brush_for(node: NodeId) -> Brush {
    Brush {
        node: slotmap::Key::data(&node).as_ffi(),
    }
}

pub(crate) fn node_of_brush(brush: Brush) -> NodeId {
    slotmap::KeyData::from_ffi(brush.node).into()
}

/// One thing pushed to Parley's tree builder.
#[derive(Clone)]
enum Op {
    /// A span in these computed values (compared by identity: the same
    /// values make the same text style) and brush.
    Span(Arc<ComputedValues>, Brush),
    /// A span that only changes the brush.
    BrushSpan(Brush),
    Pop,
    /// Text: what `Ops::text` holds up to this offset since the last text.
    Text(usize),
    /// An atomic inline box for this element. Parley only places it, so
    /// what is inside does not matter to the shaping.
    InlineBox(Option<NodeId>),
}

impl PartialEq for Op {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Op::Span(a, x), Op::Span(b, y)) => Arc::ptr_eq(a, b) && x == y,
            (Op::BrushSpan(x), Op::BrushSpan(y)) => x == y,
            (Op::Pop, Op::Pop) => true,
            (Op::Text(a), Op::Text(b)) => a == b,
            (Op::InlineBox(a), Op::InlineBox(b)) => a == b,
            _ => false,
        }
    }
}

/// What an inline formatting context pushes to Parley, in order: equal
/// records shape to equal layouts (the fonts do not change).
#[derive(Clone)]
pub(crate) struct Ops {
    /// The style and brush of the root.
    root: (Arc<ComputedValues>, Brush),
    items: Vec<Op>,
    /// The text of the `Text` items, end to end.
    text: String,
    /// A hash of all of it, filled in by `finish`.
    pub hash: u64,
}

impl PartialEq for Ops {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
            && Arc::ptr_eq(&self.root.0, &other.root.0)
            && self.root.1 == other.root.1
            && self.items == other.items
            && self.text == other.text
    }
}

impl Ops {
    fn new(style: Arc<ComputedValues>, brush: Brush) -> Self {
        Self {
            root: (style, brush),
            items: Vec::new(),
            text: String::new(),
            hash: 0,
        }
    }

    fn push_span(&mut self, style: &Arc<ComputedValues>, brush: Brush) {
        self.items.push(Op::Span(style.clone(), brush));
    }

    fn push_brush_span(&mut self, brush: Brush) {
        self.items.push(Op::BrushSpan(brush));
    }

    fn pop(&mut self) {
        self.items.push(Op::Pop);
    }

    fn push_text(&mut self, text: &str) {
        self.text.push_str(text);
        self.items.push(Op::Text(self.text.len()));
    }

    /// Ends text written straight into `self.text` since `start`, if any
    /// was.
    fn end_text(&mut self, start: usize) {
        if self.text.len() > start {
            self.items.push(Op::Text(self.text.len()));
        }
    }

    fn push_inline_box(&mut self, node: Option<NodeId>) {
        self.items.push(Op::InlineBox(node));
    }

    fn finish(&mut self) {
        let mut hasher = DefaultHasher::new();
        (self.root.0.heap_ptr() as usize).hash(&mut hasher);
        self.root.1.hash(&mut hasher);
        for op in &self.items {
            match op {
                Op::Span(style, brush) => {
                    0u8.hash(&mut hasher);
                    (style.heap_ptr() as usize).hash(&mut hasher);
                    brush.hash(&mut hasher);
                }
                Op::BrushSpan(brush) => {
                    1u8.hash(&mut hasher);
                    brush.hash(&mut hasher);
                }
                Op::Pop => 2u8.hash(&mut hasher),
                Op::Text(end) => {
                    3u8.hash(&mut hasher);
                    end.hash(&mut hasher);
                }
                Op::InlineBox(node) => {
                    4u8.hash(&mut hasher);
                    node.hash(&mut hasher);
                }
            }
        }
        self.text.hash(&mut hasher);
        self.hash = hasher.finish();
    }

    /// Replays the record into Parley and shapes it. `boxes` are the
    /// atomic boxes, in order.
    fn shape(&self, fonts: &mut Fonts, boxes: &[BoxId]) -> (parley::Layout<Brush>, String) {
        let Fonts { font_cx, layout_cx } = fonts;
        let root = text_style(&self.root.0, self.root.1);
        let mut builder = layout_cx.tree_builder(font_cx, 1.0, true, &root);
        // White space is collapsed before, by the CSS rules; Parley gets
        // the text as it should be shown.
        builder.set_white_space_mode(WhiteSpaceCollapse::Preserve);
        let mut text_start = 0;
        let mut boxes = boxes.iter();
        for op in &self.items {
            match op {
                Op::Span(style, brush) => {
                    let mut span = text_style(style, *brush);
                    span.brush = *brush;
                    builder.push_style_span(span);
                }
                Op::BrushSpan(brush) => {
                    builder.push_style_modification_span(&[StyleProperty::Brush(*brush)]);
                }
                Op::Pop => builder.pop_style_span(),
                Op::Text(end) => {
                    builder.push_text(&self.text[text_start..*end]);
                    text_start = *end;
                }
                Op::InlineBox(_) => {
                    let id = boxes.next().expect("one box per inline box");
                    builder.push_inline_box(parley::InlineBox {
                        id: slotmap::Key::data(id).as_ffi(),
                        kind: parley::InlineBoxKind::InFlow,
                        index: 0,
                        width: 0.0,
                        height: 0.0,
                    });
                }
            }
        }
        builder.build()
    }
}

/// A context's shaped text, kept from one layout to the next: its layout
/// (with the lines last broken), the text Parley saw and the height of
/// those lines, and the box it was in.
struct Shaped {
    layout: parley::Layout<Brush>,
    text: String,
    height: f32,
    from: BoxId,
}

/// The shaped text of the last layout's inline contexts, by what was
/// pushed to Parley for them.
#[derive(Default)]
pub(crate) struct ShapedCache {
    /// By hash of the record; `None` stands for records that several
    /// contexts had.
    entries: HashMap<u64, Vec<(Ops, Option<Shaped>)>>,
}

impl ShapedCache {
    /// Keeps the shaped text of the context of box `from`. Contexts with
    /// equal records (which only identical content can make) are all
    /// passed over, so that none takes another's lines.
    pub(crate) fn insert(&mut self, from: BoxId, context: InlineContext) {
        let list = self.entries.entry(context.ops.hash).or_default();
        if let Some((_, shaped)) = list.iter_mut().find(|(ops, _)| *ops == context.ops) {
            *shaped = None;
            return;
        }
        let shaped = Shaped {
            layout: context.layout,
            text: context.text,
            height: context.height,
            from,
        };
        list.push((context.ops, Some(shaped)));
    }

    fn take(&mut self, ops: &Ops) -> Option<Shaped> {
        let list = self.entries.get_mut(&ops.hash)?;
        list.iter_mut().find(|(o, _)| o == ops)?.1.take()
    }
}

impl InlineContext {
    /// Records what `items` push to Parley and shapes it, or takes the
    /// shaped text of an equal record from `shaped`.
    pub(crate) fn build(
        tree: &mut LayoutTree,
        dom: &Dom,
        styles: &StyleEngine,
        container: BoxId,
        items: Vec<InlineItem>,
        shaped: Option<&mut ShapedCache>,
    ) -> Self {
        let root_style = tree.boxes[container].style.clone();
        let root_node = tree.boxes[container]
            .node
            .or_else(|| {
                tree.boxes[container]
                    .parent
                    .and_then(|p| tree.boxes[p].node)
            })
            .unwrap_or_else(|| dom.document());
        let mut inline_styles = std::mem::take(&mut tree.inline_styles);
        let node_box = std::mem::take(&mut tree.node_box);
        let mut boxes = Vec::new();
        let mut state = Pusher {
            dom,
            styles,
            node_box: &node_box,
            inline_styles: &mut inline_styles,
            boxes: &mut boxes,
            ops: Ops::new(root_style.clone(), brush_for(root_node)),
            transform: root_style.clone_text_transform(),
            ws: ws_mode(&root_style),
            prev_space: true,
            pending_space: false,
        };
        for item in items {
            state.push_item(item);
        }
        let mut ops = state.ops;
        tree.inline_styles = inline_styles;
        tree.node_box = node_box;
        // The atomic boxes go into the record as the elements they are.
        let mut atomic = boxes.iter().map(|b| tree.boxes[*b].node);
        for op in &mut ops.items {
            if let Op::InlineBox(node) = op {
                *node = atomic.next().flatten();
            }
        }
        ops.finish();
        if let Some(found) = shaped.and_then(|cache| cache.take(&ops)) {
            let mut layout = found.layout;
            // The same elements, in new boxes.
            for (inline_box, id) in layout.inline_boxes_mut().iter_mut().zip(&boxes) {
                inline_box.id = slotmap::Key::data(id).as_ffi();
            }
            return Self {
                layout,
                text: found.text,
                boxes,
                height: found.height,
                ops,
                reused_from: Some(found.from),
            };
        }
        let fonts = tree.fonts.clone();
        let mut fonts = fonts.lock().unwrap_or_else(|e| e.into_inner());
        let (layout, text) = ops.shape(&mut fonts, &boxes);
        Self {
            layout,
            text,
            boxes,
            height: 0.0,
            ops,
            reused_from: None,
        }
    }
}

struct Pusher<'a> {
    dom: &'a Dom,
    styles: &'a StyleEngine,
    /// The boxes made so far: an element in here met inside an inline
    /// element is an atomic box to place on the line.
    node_box: &'a SecondaryMap<NodeId, BoxId>,
    inline_styles: &'a mut SecondaryMap<NodeId, Arc<ComputedValues>>,
    boxes: &'a mut Vec<BoxId>,
    /// What is pushed to Parley.
    ops: Ops,
    transform: TextTransform,
    ws: Ws,
    /// The text so far ends in white space (or nothing yet): collapsible
    /// white space coming next is dropped.
    prev_space: bool,
    /// A collapsible space was seen and not yet pushed; it goes in before
    /// the next text or inline box, in that item's style.
    pending_space: bool,
}

/// How a run of text treats its white space.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ws {
    /// Runs of white space become one space; leading ones go.
    Collapse,
    /// `pre-line`: like `Collapse`, but newlines stay and break lines.
    PreserveBreaks,
    /// `pre`, `pre-wrap`, `break-spaces`: as written.
    Preserve,
}

fn ws_mode(style: &ComputedValues) -> Ws {
    match style.clone_white_space_collapse() {
        StyloWhiteSpaceCollapse::Collapse => Ws::Collapse,
        StyloWhiteSpaceCollapse::PreserveBreaks => Ws::PreserveBreaks,
        _ => Ws::Preserve,
    }
}

fn is_collapsible(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

impl Pusher<'_> {
    fn push_item(&mut self, item: InlineItem) {
        match item {
            InlineItem::Text(node) => {
                let dom = self.dom;
                let text = dom.node(node).as_text().unwrap_or_default();
                let style = text_style_of_text(self.dom, self.styles, node);
                let brush = brush_for(node);
                match style {
                    Some(style) => {
                        self.inline_styles.insert(node, style.clone());
                        self.ops.push_span(&style, brush);
                        self.push_text(text);
                        self.ops.pop();
                    }
                    None => {
                        self.ops.push_brush_span(brush);
                        self.push_text(text);
                        self.ops.pop();
                    }
                }
            }
            InlineItem::Element(node, style) => {
                self.inline_styles.insert(node, style.clone());
                let outer_transform = self.transform;
                let outer_ws = self.ws;
                self.transform = style.clone_text_transform();
                self.ws = ws_mode(&style);
                self.ops.push_span(&style, brush_for(node));
                if self.dom.is_html_element(node, "br") {
                    // A forced break: spaces before it are dropped, as are
                    // those after it.
                    self.pending_space = false;
                    self.ops.push_text("\n");
                    self.prev_space = true;
                } else if self.dom.is_html_element(node, "wbr") {
                    self.ops.push_text("\u{200B}");
                } else {
                    if let Some(text) = self.pseudo(node, catpaw_style::Pseudo::Before) {
                        self.push_pseudo(text);
                    }
                    let dom = self.dom;
                    for child in dom.iter_rendered_children(node) {
                        match self.dom.kind(child) {
                            NodeKind::Text(_) => self.push_item(InlineItem::Text(child)),
                            NodeKind::Element(_) => self.push_element_child(child),
                            _ => {}
                        }
                    }
                    if let Some(text) = self.pseudo(node, catpaw_style::Pseudo::After) {
                        self.push_pseudo(text);
                    }
                }
                self.ops.pop();
                self.transform = outer_transform;
                self.ws = outer_ws;
            }
            InlineItem::Atomic(id) => {
                self.flush_space();
                self.prev_space = false;
                self.boxes.push(id);
                // Which element it is, is filled in when the record is done.
                self.ops.push_inline_box(None);
            }
            InlineItem::Pseudo(text) => self.push_pseudo(text),
        }
    }

    /// An element met inside an inline element: inline boxes recurse here;
    /// anything that generates a box of its own was already given one by
    /// the constructor and is placed as an atomic box.
    fn push_element_child(&mut self, child: NodeId) {
        if let Some(&id) = self.node_box.get(child) {
            // Out-of-flow boxes were hung off their containing block; the
            // rest are atomic inline boxes.
            let positioning = self
                .styles
                .primary_style(child)
                .map(|s| s.get_box().position);
            let out_of_flow = matches!(
                positioning,
                Some(style::computed_values::position::T::Absolute)
                    | Some(style::computed_values::position::T::Fixed)
            );
            if !out_of_flow {
                self.push_item(InlineItem::Atomic(id));
            }
            return;
        }
        let Some(style) = self.styles.primary_style(child) else {
            return;
        };
        let display = style.get_box().display;
        if display.is_none() {
            return;
        }
        if display.is_contents() {
            let dom = self.dom;
            for grandchild in dom.iter_rendered_children(child) {
                match self.dom.kind(grandchild) {
                    NodeKind::Text(_) => self.push_item(InlineItem::Text(grandchild)),
                    NodeKind::Element(_) => self.push_element_child(grandchild),
                    _ => {}
                }
            }
            return;
        }
        self.push_item(InlineItem::Element(child, style));
    }

    fn pseudo(&self, el: NodeId, pseudo: catpaw_style::Pseudo) -> Option<PseudoText> {
        use style::values::computed::counters::{Content, ContentItem};
        let style = self.styles.pseudo_style(el, pseudo)?;
        if style.get_box().display.is_none() {
            return None;
        }
        let Content::Items(items) = &style.get_counters().content else {
            return None;
        };
        let mut text = String::new();
        for item in items.items.iter() {
            if let ContentItem::String(s) = item {
                text.push_str(s);
            }
        }
        Some(PseudoText {
            owner: el,
            text,
            style,
        })
    }

    fn push_pseudo(&mut self, pseudo: PseudoText) {
        if !self.inline_styles.contains_key(pseudo.owner) {
            self.inline_styles
                .insert(pseudo.owner, pseudo.style.clone());
        }
        let outer_transform = self.transform;
        let outer_ws = self.ws;
        self.transform = pseudo.style.clone_text_transform();
        self.ws = ws_mode(&pseudo.style);
        self.ops.push_span(&pseudo.style, brush_for(pseudo.owner));
        self.push_text(&pseudo.text);
        self.ops.pop();
        self.transform = outer_transform;
        self.ws = outer_ws;
    }

    /// Pushes a text node's characters with the white space the
    /// `white-space-collapse` of its element leaves.
    fn push_text(&mut self, text: &str) {
        let transformed = transform_text(text, self.transform);
        if transformed.is_empty() {
            return;
        }
        match self.ws {
            Ws::Preserve => {
                self.flush_space();
                self.ops.push_text(&transformed);
                self.prev_space = transformed.ends_with('\n');
            }
            Ws::Collapse | Ws::PreserveBreaks => {
                // Written straight into the record.
                let start = self.ops.text.len();
                let out = &mut self.ops.text;
                for c in transformed.chars() {
                    if self.ws == Ws::PreserveBreaks && c == '\n' {
                        self.pending_space = false;
                        out.push('\n');
                        self.prev_space = true;
                    } else if is_collapsible(c) {
                        if !self.prev_space {
                            self.pending_space = true;
                            self.prev_space = true;
                        }
                    } else {
                        if self.pending_space {
                            out.push(' ');
                            self.pending_space = false;
                        }
                        out.push(c);
                        self.prev_space = false;
                    }
                }
                self.ops.end_text(start);
            }
        }
    }

    /// Pushes the space that is waiting, in the current style.
    fn flush_space(&mut self) {
        if self.pending_space {
            self.pending_space = false;
            self.ops.push_text(" ");
        }
    }
}

/// The style a text node is shaped with: its parent element's.
fn text_style_of_text(
    dom: &Dom,
    styles: &StyleEngine,
    node: NodeId,
) -> Option<Arc<ComputedValues>> {
    let parent = dom.parent_element(node)?;
    styles.primary_style(parent)
}

fn transform_text(text: &str, transform: TextTransform) -> Cow<'_, str> {
    if transform.contains(TextTransform::UPPERCASE) {
        Cow::Owned(text.to_uppercase())
    } else if transform.contains(TextTransform::LOWERCASE) {
        Cow::Owned(text.to_lowercase())
    } else if transform.contains(TextTransform::CAPITALIZE) {
        let mut out = String::with_capacity(text.len());
        let mut at_word_start = true;
        for c in text.chars() {
            if at_word_start && c.is_alphanumeric() {
                out.extend(c.to_uppercase());
            } else {
                out.push(c);
            }
            at_word_start = c.is_whitespace() || c == '-' || c == '\u{A0}';
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

fn generic_family(generic: GenericFontFamily) -> GenericFamily {
    match generic {
        GenericFontFamily::Serif => GenericFamily::Serif,
        GenericFontFamily::Monospace => GenericFamily::Monospace,
        GenericFontFamily::Cursive => GenericFamily::Cursive,
        GenericFontFamily::Fantasy => GenericFamily::Fantasy,
        GenericFontFamily::SystemUi => GenericFamily::SystemUi,
        GenericFontFamily::SansSerif | GenericFontFamily::None => GenericFamily::SansSerif,
    }
}

/// The Parley style of text with these computed values.
pub(crate) fn text_style(
    style: &ComputedValues,
    brush: Brush,
) -> TextStyle<'static, 'static, Brush> {
    use style::values::computed::font::LineHeight as StyloLineHeight;
    let font = style.get_font();
    let text = style.get_inherited_text();
    let font_size = font.font_size.used_size.0.px();
    let families: Vec<FontFamilyName<'static>> = font
        .font_family
        .families
        .list
        .iter()
        .map(|family| match family {
            SingleFontFamily::FamilyName(name) => {
                let name: &str = &name.name;
                if matches!(name, "-apple-system" | "BlinkMacSystemFont") {
                    FontFamilyName::Generic(GenericFamily::SystemUi)
                } else {
                    FontFamilyName::Named(Cow::Owned(name.to_string()))
                }
            }
            SingleFontFamily::Generic(generic) => FontFamilyName::Generic(generic_family(*generic)),
        })
        .collect();
    let line_height = match font.line_height {
        StyloLineHeight::Normal => LineHeight::MetricsRelative(1.0),
        StyloLineHeight::Number(n) => LineHeight::FontSizeRelative(n.0),
        StyloLineHeight::Length(l) => LineHeight::Absolute(l.0.px()),
    };
    let letter_spacing = text
        .letter_spacing
        .0
        .resolve(style::values::computed::Length::new(font_size))
        .px();
    let word_spacing = text
        .word_spacing
        .resolve(style::values::computed::Length::new(font_size))
        .px();
    let attributes = catpaw_style::fonts::query_attributes(font);
    let word_break = match text.word_break {
        StyloWordBreak::Normal => WordBreak::Normal,
        StyloWordBreak::BreakAll => WordBreak::BreakAll,
        StyloWordBreak::KeepAll => WordBreak::KeepAll,
    };
    let overflow_wrap = match text.overflow_wrap {
        StyloOverflowWrap::Normal => OverflowWrap::Normal,
        StyloOverflowWrap::BreakWord => OverflowWrap::BreakWord,
        StyloOverflowWrap::Anywhere => OverflowWrap::Anywhere,
    };
    let _ = LineBreak::Normal;
    let text_wrap_mode = match text.text_wrap_mode {
        StyloTextWrapMode::Wrap => TextWrapMode::Wrap,
        StyloTextWrapMode::Nowrap => TextWrapMode::NoWrap,
    };
    TextStyle {
        font_family: FontFamily::List(Cow::Owned(families)),
        font_size,
        font_width: attributes.width,
        font_style: attributes.style,
        font_weight: attributes.weight,
        font_variations: FontVariations::List(Cow::Borrowed(&[])),
        font_features: FontFeatures::List(Cow::Borrowed(&[])),
        locale: None,
        brush,
        line_height,
        word_spacing,
        letter_spacing,
        word_break,
        overflow_wrap,
        text_wrap_mode,
        ..TextStyle::default()
    }
}
