//! Inline formatting contexts: the text, inline elements, atomic inline
//! boxes and pseudo-element text of a block container, shaped and broken
//! into lines by Parley.

use std::borrow::Cow;
use std::collections::HashMap;

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_style::StyleEngine;
use catpaw_text::parley::style::{
    FontFamily, FontFamilyName, FontFeatures, FontVariations, GenericFamily, LineHeight,
    OverflowWrap, StyleProperty, TextStyle, TextWrapMode, WhiteSpaceCollapse, WordBreak,
};
use catpaw_text::parley::{self, TreeBuilder};
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

impl InlineContext {
    pub(crate) fn build(
        tree: &mut LayoutTree,
        dom: &Dom,
        styles: &StyleEngine,
        container: BoxId,
        items: Vec<InlineItem>,
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
        let fonts = tree.fonts.clone();
        let mut fonts = fonts.lock().unwrap_or_else(|e| e.into_inner());
        let mut inline_styles = std::mem::take(&mut tree.inline_styles);
        let node_box = std::mem::take(&mut tree.node_box);
        let Fonts { font_cx, layout_cx } = &mut *fonts;
        let root_text_style = text_style(&root_style, brush_for(root_node));
        let mut builder = layout_cx.tree_builder(font_cx, 1.0, true, &root_text_style);
        let mut boxes = Vec::new();
        let mut state = Pusher {
            dom,
            styles,
            node_box: &node_box,
            inline_styles: &mut inline_styles,
            boxes: &mut boxes,
            transform: root_style.clone_text_transform(),
            ws: ws_mode(&root_style),
            prev_space: true,
            pending_space: false,
        };
        // White space is collapsed here, by the CSS rules; Parley gets the
        // text as it should be shown.
        builder.set_white_space_mode(WhiteSpaceCollapse::Preserve);
        for item in items {
            state.push_item(&mut builder, item);
        }
        let (layout, text) = builder.build();
        tree.inline_styles = inline_styles;
        tree.node_box = node_box;
        Self {
            layout,
            text,
            boxes,
        }
    }
}

struct Pusher<'a> {
    dom: &'a Dom,
    styles: &'a StyleEngine,
    /// The boxes made so far: an element in here met inside an inline
    /// element is an atomic box to place on the line.
    node_box: &'a HashMap<NodeId, BoxId>,
    inline_styles: &'a mut HashMap<NodeId, Arc<ComputedValues>>,
    boxes: &'a mut Vec<BoxId>,
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
    fn push_item(&mut self, builder: &mut TreeBuilder<'_, Brush>, item: InlineItem) {
        match item {
            InlineItem::Text(node) => {
                let text = self
                    .dom
                    .node(node)
                    .as_text()
                    .unwrap_or_default()
                    .to_string();
                let style = text_style_of_text(self.dom, self.styles, node);
                let brush = brush_for(node);
                match style {
                    Some(style) => {
                        self.inline_styles.insert(node, style.clone());
                        let mut span = text_style(&style, brush);
                        span.brush = brush;
                        builder.push_style_span(span);
                        self.push_text(builder, &text);
                        builder.pop_style_span();
                    }
                    None => {
                        builder.push_style_modification_span(&[StyleProperty::Brush(brush)]);
                        self.push_text(builder, &text);
                        builder.pop_style_span();
                    }
                }
            }
            InlineItem::Element(node, style) => {
                self.inline_styles.insert(node, style.clone());
                let outer_transform = self.transform;
                let outer_ws = self.ws;
                self.transform = style.clone_text_transform();
                self.ws = ws_mode(&style);
                builder.push_style_span(text_style(&style, brush_for(node)));
                if self.dom.is_html_element(node, "br") {
                    // A forced break: spaces before it are dropped, as are
                    // those after it.
                    self.pending_space = false;
                    builder.push_text("\n");
                    self.prev_space = true;
                } else if self.dom.is_html_element(node, "wbr") {
                    builder.push_text("\u{200B}");
                } else {
                    if let Some(text) = self.pseudo(node, catpaw_style::Pseudo::Before) {
                        self.push_pseudo(builder, text);
                    }
                    for child in self.dom.rendered_children(node) {
                        match self.dom.kind(child) {
                            NodeKind::Text(_) => self.push_item(builder, InlineItem::Text(child)),
                            NodeKind::Element(_) => self.push_element_child(builder, child),
                            _ => {}
                        }
                    }
                    if let Some(text) = self.pseudo(node, catpaw_style::Pseudo::After) {
                        self.push_pseudo(builder, text);
                    }
                }
                builder.pop_style_span();
                self.transform = outer_transform;
                self.ws = outer_ws;
            }
            InlineItem::Atomic(id) => {
                self.flush_space(builder);
                self.prev_space = false;
                self.boxes.push(id);
                builder.push_inline_box(parley::InlineBox {
                    id: slotmap::Key::data(&id).as_ffi(),
                    kind: parley::InlineBoxKind::InFlow,
                    index: 0,
                    width: 0.0,
                    height: 0.0,
                });
            }
            InlineItem::Pseudo(text) => self.push_pseudo(builder, text),
        }
    }

    /// An element met inside an inline element: inline boxes recurse here;
    /// anything that generates a box of its own was already given one by
    /// the constructor and is placed as an atomic box.
    fn push_element_child(&mut self, builder: &mut TreeBuilder<'_, Brush>, child: NodeId) {
        if let Some(&id) = self.node_box.get(&child) {
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
                self.push_item(builder, InlineItem::Atomic(id));
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
            for grandchild in self.dom.rendered_children(child) {
                match self.dom.kind(grandchild) {
                    NodeKind::Text(_) => self.push_item(builder, InlineItem::Text(grandchild)),
                    NodeKind::Element(_) => self.push_element_child(builder, grandchild),
                    _ => {}
                }
            }
            return;
        }
        self.push_item(builder, InlineItem::Element(child, style));
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

    fn push_pseudo(&mut self, builder: &mut TreeBuilder<'_, Brush>, pseudo: PseudoText) {
        self.inline_styles
            .entry(pseudo.owner)
            .or_insert_with(|| pseudo.style.clone());
        let outer_transform = self.transform;
        let outer_ws = self.ws;
        self.transform = pseudo.style.clone_text_transform();
        self.ws = ws_mode(&pseudo.style);
        builder.push_style_span(text_style(&pseudo.style, brush_for(pseudo.owner)));
        self.push_text(builder, &pseudo.text);
        builder.pop_style_span();
        self.transform = outer_transform;
        self.ws = outer_ws;
    }

    /// Pushes a text node's characters with the white space the
    /// `white-space-collapse` of its element leaves.
    fn push_text(&mut self, builder: &mut TreeBuilder<'_, Brush>, text: &str) {
        let transformed = transform_text(text, self.transform);
        if transformed.is_empty() {
            return;
        }
        match self.ws {
            Ws::Preserve => {
                self.flush_space(builder);
                builder.push_text(&transformed);
                self.prev_space = transformed.ends_with('\n');
            }
            Ws::Collapse | Ws::PreserveBreaks => {
                let mut out = String::with_capacity(transformed.len());
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
                if !out.is_empty() {
                    builder.push_text(&out);
                }
            }
        }
    }

    /// Pushes the space that is waiting, in the current style.
    fn flush_space(&mut self, builder: &mut TreeBuilder<'_, Brush>) {
        if self.pending_space {
            self.pending_space = false;
            builder.push_text(" ");
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
