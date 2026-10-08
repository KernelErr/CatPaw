//! Box tree construction: which elements generate boxes, how inline and
//! block children mix (anonymous blocks), where out-of-flow boxes attach,
//! and what replaced elements are sized by.

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_style::{Pseudo, StyleEngine};
use markup5ever::ns;
use style::computed_values::position::T as Position;
use style::properties::ComputedValues;
use style::servo_arc::Arc;
use style::values::computed::Display;
use style::values::computed::counters::{Content, ContentItem};
use style::values::specified::box_::{DisplayInside, DisplayOutside};

use crate::inline::{InlineContext, InlineItem, PseudoText, ShapedCache};
use crate::{BoxId, BoxKind, Intrinsic, LayoutBox, LayoutTree, Positioning};

/// Elements laid out as a leaf sized by their content rather than by their
/// children.
pub fn is_replaced(dom: &Dom, el: NodeId) -> bool {
    let Some(data) = dom.element(el) else {
        return false;
    };
    if *data.name.ns == *ns!(svg) {
        return &*data.name.local == "svg";
    }
    data.is_html()
        && matches!(
            &*data.name.local,
            "img"
                | "iframe"
                | "frame"
                | "video"
                | "audio"
                | "canvas"
                | "embed"
                | "object"
                | "input"
                | "select"
                | "textarea"
                | "meter"
                | "progress"
        )
}

/// Builds the boxes of the document. Inline contexts take their shaped
/// text from `shaped` where an equal one was shaped before.
pub(crate) fn build(
    tree: &mut LayoutTree,
    dom: &Dom,
    styles: &StyleEngine,
    shaped: Option<&mut ShapedCache>,
) {
    let Some(root) = dom.child_elements(dom.document()).next() else {
        return;
    };
    let Some(style) = styles.primary_style(root) else {
        return;
    };
    let mut builder = Builder {
        tree,
        dom,
        styles,
        shaped,
    };
    let root_box = builder.make_box(root, style, None);
    builder.tree.root = Some(root_box);
    builder.fill(root_box, root);
}

struct Builder<'a> {
    tree: &'a mut LayoutTree,
    dom: &'a Dom,
    styles: &'a StyleEngine,
    shaped: Option<&'a mut ShapedCache>,
}

/// A child of a block container, before anonymous boxes are made.
enum FlowItem {
    /// Inline-level content: text, an inline element, or an atomic box.
    Inline(InlineItem),
    /// A block-level box.
    Block(BoxId),
}

impl Builder<'_> {
    fn positioning(style: &ComputedValues) -> Positioning {
        match style.get_box().position {
            Position::Static => Positioning::Static,
            Position::Relative | Position::Sticky => Positioning::Relative,
            Position::Absolute => Positioning::Absolute,
            Position::Fixed => Positioning::Fixed,
        }
    }

    /// The kind of box an element's `display` and name call for.
    fn kind_for(&self, el: Option<NodeId>, display: Display) -> BoxKind {
        if let Some(el) = el
            && is_replaced(self.dom, el)
        {
            return BoxKind::Replaced;
        }
        match display.inside() {
            DisplayInside::Flex => BoxKind::Flex,
            DisplayInside::Grid => BoxKind::Grid,
            // Rows lay their cells side by side; the rest of the table
            // model stacks like blocks until tables are mapped to a grid.
            DisplayInside::TableRow => BoxKind::Flex,
            _ => BoxKind::Block,
        }
    }

    fn make_box(&mut self, el: NodeId, style: Arc<ComputedValues>, parent: Option<BoxId>) -> BoxId {
        let display = style.get_box().display;
        let kind = self.kind_for(Some(el), display);
        let positioning = Self::positioning(&style);
        let intrinsic = if kind == BoxKind::Replaced {
            intrinsic_size(self.dom, el, &style)
        } else {
            Intrinsic::default()
        };
        let id = self.tree.boxes.insert(LayoutBox {
            node: Some(el),
            kind,
            style,
            positioning,
            children: Vec::new(),
            parent,
            layout: taffy::Layout::new(),
            cache: taffy::Cache::new(),
            inline: None,
            intrinsic,
            origin: (0.0, 0.0),
            fingerprint: 0,
            transplanted: false,
        });
        self.tree.node_box.insert(el, id);
        id
    }

    fn make_anonymous(&mut self, parent: BoxId, style: Arc<ComputedValues>) -> BoxId {
        self.tree.boxes.insert(LayoutBox {
            node: None,
            kind: BoxKind::Block,
            style,
            positioning: Positioning::Static,
            children: Vec::new(),
            parent: Some(parent),
            layout: taffy::Layout::new(),
            cache: taffy::Cache::new(),
            inline: None,
            intrinsic: Intrinsic::default(),
            origin: (0.0, 0.0),
            fingerprint: 0,
            transplanted: false,
        })
    }

    /// Builds the children of `container`, the box of `el`.
    fn fill(&mut self, container: BoxId, el: NodeId) {
        if self.tree.boxes[container].kind == BoxKind::Replaced {
            return;
        }
        let mut items = Vec::new();
        self.collect_pseudo(container, el, Pseudo::Before, &mut items);
        let dom = self.dom;
        for child in dom.iter_rendered_children(el) {
            self.collect(container, child, &mut items);
        }
        self.collect_pseudo(container, el, Pseudo::After, &mut items);
        self.attach(container, items);
    }

    /// A `::before` or `::after` with content: block-level ones (the
    /// clearfix `::after { display: table; clear: both }` above all) get
    /// a box of their own holding their text; inline ones join the line.
    fn collect_pseudo(
        &mut self,
        container: BoxId,
        el: NodeId,
        pseudo: Pseudo,
        items: &mut Vec<FlowItem>,
    ) {
        let Some(text) = self.pseudo_text(el, pseudo) else {
            return;
        };
        let display = text.style.get_box().display;
        let block_level = display.outside() == DisplayOutside::Block
            || text.style.get_box().float.is_floating()
            || matches!(
                Self::positioning(&text.style),
                Positioning::Absolute | Positioning::Fixed
            );
        if !block_level {
            items.push(FlowItem::Inline(InlineItem::Pseudo(text)));
            return;
        }
        let positioning = Self::positioning(&text.style);
        let id = self.tree.boxes.insert(LayoutBox {
            node: None,
            kind: self.kind_for(None, display),
            style: text.style.clone(),
            positioning,
            children: Vec::new(),
            parent: Some(container),
            layout: taffy::Layout::new(),
            cache: taffy::Cache::new(),
            inline: None,
            intrinsic: Intrinsic::default(),
            origin: (0.0, 0.0),
            fingerprint: 0,
            transplanted: false,
        });
        let has_text = !text.text.trim().is_empty();
        if has_text {
            self.make_inline_root(id, vec![InlineItem::Pseudo(text)]);
        }
        if matches!(positioning, Positioning::Absolute | Positioning::Fixed) {
            match self.containing_block(el, positioning) {
                Some(cb) => {
                    self.tree.boxes[id].parent = Some(cb);
                    self.tree.boxes[cb].children.push(id);
                }
                None => {
                    self.tree.boxes[id].parent = self.tree.root;
                    self.tree.oof_root.push(id);
                }
            }
            return;
        }
        items.push(FlowItem::Block(id));
    }

    /// The text a `::before` or `::after` adds, if its `content` is text.
    fn pseudo_text(&self, el: NodeId, pseudo: Pseudo) -> Option<PseudoText> {
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

    /// Sorts one DOM child of `container` into inline or block items.
    fn collect(&mut self, container: BoxId, child: NodeId, items: &mut Vec<FlowItem>) {
        match self.dom.kind(child) {
            NodeKind::Text(_) => {
                self.tree.inline_owner.insert(child, container);
                items.push(FlowItem::Inline(InlineItem::Text(child)));
            }
            NodeKind::Element(_) => {
                let Some(style) = self.styles.primary_style(child) else {
                    // Unstyled: inside a `display: none` subtree.
                    return;
                };
                let display = style.get_box().display;
                if display.is_none() {
                    return;
                }
                if display.is_contents() {
                    let dom = self.dom;
                    for grandchild in dom.iter_rendered_children(child) {
                        self.collect(container, grandchild, items);
                    }
                    return;
                }
                let positioning = Self::positioning(&style);
                if matches!(positioning, Positioning::Absolute | Positioning::Fixed) {
                    let id = self.make_box(child, style, None);
                    self.fill(id, child);
                    match self.containing_block(child, positioning) {
                        Some(cb) => {
                            self.tree.boxes[id].parent = Some(cb);
                            self.tree.boxes[cb].children.push(id);
                        }
                        None => {
                            // Against the initial containing block (the
                            // viewport), laid out after the tree.
                            self.tree.boxes[id].parent = self.tree.root;
                            self.tree.oof_root.push(id);
                        }
                    }
                    return;
                }
                let floating = style.get_box().float.is_floating();
                if display.outside() == DisplayOutside::Inline && !floating {
                    if display.inside() == DisplayInside::Flow && !is_replaced(self.dom, child) {
                        // An inline box: its content joins the parent's
                        // inline formatting context, and the atomic boxes
                        // inside it get boxes of their own now.
                        self.tree.inline_owner.insert(child, container);
                        self.prepare_inline_descendants(child, container);
                        items.push(FlowItem::Inline(InlineItem::Element(child, style)));
                    } else {
                        let id = self.make_box(child, style, Some(container));
                        self.fill(id, child);
                        items.push(FlowItem::Inline(InlineItem::Atomic(id)));
                    }
                    return;
                }
                let id = self.make_box(child, style, Some(container));
                self.fill(id, child);
                items.push(FlowItem::Block(id));
            }
            _ => {}
        }
    }

    /// The box an absolutely positioned element is laid out in: the nearest
    /// positioned ancestor with a box; `None` for the initial containing
    /// block (always so for a fixed box).
    fn containing_block(&self, el: NodeId, positioning: Positioning) -> Option<BoxId> {
        if positioning == Positioning::Fixed {
            return None;
        }
        self.dom.ancestors(el).find_map(|ancestor| {
            let id = *self.tree.node_box.get(ancestor)?;
            (self.tree.boxes[id].positioning != Positioning::Static).then_some(id)
        })
    }

    /// Turns the items into the container's children: all inline makes the
    /// container an inline root; a mix wraps each run of inline items in an
    /// anonymous block.
    fn attach(&mut self, container: BoxId, items: Vec<FlowItem>) {
        let kind = self.tree.boxes[container].kind;
        let has_block = items.iter().any(|i| matches!(i, FlowItem::Block(_)));
        let item_container = matches!(kind, BoxKind::Flex | BoxKind::Grid);
        if !has_block && !item_container {
            let inline_items: Vec<InlineItem> = items
                .into_iter()
                .map(|i| match i {
                    FlowItem::Inline(item) => item,
                    FlowItem::Block(_) => unreachable!(),
                })
                .collect();
            if inline_items.is_empty() {
                return;
            }
            self.make_inline_root(container, inline_items);
            return;
        }
        let mut run: Vec<InlineItem> = Vec::new();
        let mut children = Vec::new();
        for item in items {
            match item {
                FlowItem::Inline(inline) => run.push(inline),
                FlowItem::Block(id) => {
                    self.flush_run(container, &mut run, &mut children);
                    children.push(id);
                }
            }
        }
        self.flush_run(container, &mut run, &mut children);
        // Out-of-flow boxes attached while the children were built stay
        // after the in-flow ones.
        let container_box = &mut self.tree.boxes[container];
        let oof = std::mem::take(&mut container_box.children);
        container_box.children = children;
        container_box.children.extend(oof);
    }

    /// Wraps a run of inline items in an anonymous inline root, unless it is
    /// only white space between blocks.
    fn flush_run(
        &mut self,
        container: BoxId,
        run: &mut Vec<InlineItem>,
        children: &mut Vec<BoxId>,
    ) {
        if run.is_empty() {
            return;
        }
        let items = std::mem::take(run);
        let only_space = items.iter().all(|item| match item {
            InlineItem::Text(node) => self.dom.node(*node).as_text().is_some_and(|t| {
                t.chars()
                    .all(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C'))
            }),
            _ => false,
        });
        if only_space {
            return;
        }
        let parent_style = self.tree.boxes[container].style.clone();
        let style = self.styles.anonymous_block_style(&parent_style);
        let anon = self.make_anonymous(container, style);
        for item in &items {
            match item {
                InlineItem::Text(node) => {
                    self.tree.inline_owner.insert(*node, anon);
                }
                InlineItem::Element(node, _) => {
                    self.tree.inline_owner.insert(*node, anon);
                    self.reown_inline_descendants(*node, anon);
                }
                InlineItem::Atomic(id) => self.tree.boxes[*id].parent = Some(anon),
                InlineItem::Pseudo(_) => {}
            }
        }
        self.make_inline_root(anon, items);
        children.push(anon);
    }

    fn make_inline_root(&mut self, container: BoxId, items: Vec<InlineItem>) {
        let context = InlineContext::build(
            self.tree,
            self.dom,
            self.styles,
            container,
            items,
            self.shaped.as_deref_mut(),
        );
        // The atomic boxes the builder met, nested ones included, are the
        // root's Taffy children; out-of-flow boxes hung off it come after.
        let atomic = context.boxes.clone();
        for id in &atomic {
            self.tree.boxes[*id].parent = Some(container);
        }
        let container_box = &mut self.tree.boxes[container];
        container_box.kind = BoxKind::InlineRoot;
        container_box.inline = Some(context);
        let oof: Vec<BoxId> = std::mem::take(&mut container_box.children)
            .into_iter()
            .filter(|c| !atomic.contains(c))
            .collect();
        container_box.children = atomic;
        container_box.children.extend(oof);
    }

    /// Moves the inline content of `el` (text and inline elements without
    /// boxes of their own) to the context `owner`, after an anonymous block
    /// took over from the container they were collected for.
    fn reown_inline_descendants(&mut self, el: NodeId, owner: BoxId) {
        let dom = self.dom;
        for child in dom.iter_rendered_children(el) {
            match self.dom.kind(child) {
                NodeKind::Text(_) => {
                    self.tree.inline_owner.insert(child, owner);
                }
                NodeKind::Element(_) => {
                    if self.tree.node_box.contains_key(child) {
                        continue;
                    }
                    if self.tree.inline_owner.contains_key(child) {
                        self.tree.inline_owner.insert(child, owner);
                    }
                    self.reown_inline_descendants(child, owner);
                }
                _ => {}
            }
        }
    }

    /// Walks the content of an inline element: text and inline boxes join
    /// the context `owner`; atomic inline-level elements (images, form
    /// controls, inline-blocks), floats and positioned elements get boxes
    /// of their own, which the inline builder places or which hang off
    /// their containing block.
    fn prepare_inline_descendants(&mut self, el: NodeId, owner: BoxId) {
        let dom = self.dom;
        for child in dom.iter_rendered_children(el) {
            match self.dom.kind(child) {
                NodeKind::Text(_) => {
                    self.tree.inline_owner.insert(child, owner);
                }
                NodeKind::Element(_) => {
                    let Some(style) = self.styles.primary_style(child) else {
                        continue;
                    };
                    let display = style.get_box().display;
                    if display.is_none() {
                        continue;
                    }
                    if display.is_contents() {
                        self.prepare_inline_descendants(child, owner);
                        continue;
                    }
                    let positioning = Self::positioning(&style);
                    if matches!(positioning, Positioning::Absolute | Positioning::Fixed) {
                        let id = self.make_box(child, style, None);
                        self.fill(id, child);
                        match self.containing_block(child, positioning) {
                            Some(cb) => {
                                self.tree.boxes[id].parent = Some(cb);
                                self.tree.boxes[cb].children.push(id);
                            }
                            None => {
                                self.tree.boxes[id].parent = self.tree.root;
                                self.tree.oof_root.push(id);
                            }
                        }
                        continue;
                    }
                    let floating = style.get_box().float.is_floating();
                    let inline_flow = display.outside() == DisplayOutside::Inline
                        && display.inside() == DisplayInside::Flow
                        && !is_replaced(self.dom, child);
                    if inline_flow && !floating {
                        self.tree.inline_owner.insert(child, owner);
                        self.prepare_inline_descendants(child, owner);
                    } else {
                        // Atomic: laid out by Taffy, placed on the line.
                        let id = self.make_box(child, style, Some(owner));
                        self.fill(id, child);
                    }
                }
                _ => {}
            }
        }
    }
}

fn attr_px(dom: &Dom, el: NodeId, name: &str) -> Option<f32> {
    let value = dom.attr(el, name)?.trim();
    let digits: String = value
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse::<f32>().ok().filter(|v| *v >= 0.0)
}

fn attr_count(dom: &Dom, el: NodeId, name: &str, default: f32) -> f32 {
    attr_px(dom, el, name)
        .filter(|v| *v > 0.0)
        .unwrap_or(default)
}

/// `line-height` as a length, with `normal` taken as 1.2 times the font size.
pub(crate) fn line_height_px(style: &ComputedValues) -> f32 {
    use style::values::computed::font::LineHeight;
    let font_size = style.clone_font_size().used_size().px();
    match style.get_font().line_height {
        LineHeight::Normal => font_size * 1.2,
        LineHeight::Number(n) => font_size * n.0,
        LineHeight::Length(l) => l.0.px(),
    }
}

/// What a replaced element is sized by when CSS does not say.
fn intrinsic_size(dom: &Dom, el: NodeId, style: &ComputedValues) -> Intrinsic {
    let data = dom.element(el).expect("replaced elements are elements");
    let font_size = style.clone_font_size().used_size().px();
    let line = line_height_px(style);
    let char_width = font_size * 0.55;
    let with_attrs = |default_width: f32, default_height: f32| {
        let width = attr_px(dom, el, "width");
        let height = attr_px(dom, el, "height");
        Intrinsic {
            width,
            height,
            ratio: match (width, height) {
                (Some(w), Some(h)) if h > 0.0 => Some(w / h),
                _ => None,
            },
            default_width,
            default_height,
        }
    };
    let fixed = |width: f32, height: f32| Intrinsic {
        width: Some(width),
        height: Some(height),
        ratio: None,
        default_width: width,
        default_height: height,
    };
    if *data.name.ns == *ns!(svg) {
        let mut size = with_attrs(300.0, 150.0);
        if size.ratio.is_none()
            && let Some(view_box) = dom.attr(el, "viewBox")
        {
            let parts: Vec<f32> = view_box
                .split([' ', ','])
                .filter(|p| !p.is_empty())
                .filter_map(|p| p.parse().ok())
                .collect();
            if parts.len() == 4 && parts[3] > 0.0 {
                size.ratio = Some(parts[2] / parts[3]);
            }
        }
        return size;
    }
    match &*data.name.local {
        "img" => {
            // No decoded image yet: nothing but the attributes gives a size.
            let mut size = with_attrs(0.0, 0.0);
            if size.width.is_none() && size.height.is_none() {
                size.width = Some(0.0);
                size.height = Some(0.0);
            }
            size
        }
        "canvas" => {
            let width = attr_px(dom, el, "width").unwrap_or(300.0);
            let height = attr_px(dom, el, "height").unwrap_or(150.0);
            Intrinsic {
                width: Some(width),
                height: Some(height),
                ratio: (height > 0.0).then(|| width / height),
                default_width: 300.0,
                default_height: 150.0,
            }
        }
        "audio" => {
            if dom.attr(el, "controls").is_some() {
                fixed(300.0, 54.0)
            } else {
                fixed(0.0, 0.0)
            }
        }
        "video" | "iframe" | "frame" | "embed" | "object" => with_attrs(300.0, 150.0),
        "textarea" => fixed(
            attr_count(dom, el, "cols", 20.0) * char_width,
            attr_count(dom, el, "rows", 2.0) * line,
        ),
        "select" => {
            let longest = dom
                .descendants(el)
                .filter(|n| dom.is_html_element(*n, "option"))
                .map(|n| dom.text_content(n).trim().chars().count())
                .max()
                .unwrap_or(0) as f32;
            let rows = if dom.attr(el, "multiple").is_some() {
                attr_count(dom, el, "size", 4.0)
            } else {
                attr_count(dom, el, "size", 1.0)
            };
            fixed(longest * char_width + 24.0, rows * line)
        }
        "meter" | "progress" => fixed(font_size * 10.0, font_size),
        "input" => {
            let kind = dom
                .attr(el, "type")
                .map(|t| t.trim().to_ascii_lowercase())
                .unwrap_or_default();
            match kind.as_str() {
                "checkbox" | "radio" => fixed(13.0, 13.0),
                "range" => fixed(129.0, 21.0),
                "color" => fixed(44.0, 23.0),
                "file" => fixed(250.0, line),
                "image" => with_attrs(0.0, 0.0),
                "submit" | "reset" | "button" => {
                    let label = dom
                        .attr(el, "value")
                        .map(str::to_string)
                        .unwrap_or_else(|| match kind.as_str() {
                            "submit" => "Submit".to_string(),
                            "reset" => "Reset".to_string(),
                            _ => String::new(),
                        });
                    fixed(label.chars().count() as f32 * char_width + 12.0, line)
                }
                _ => fixed(attr_count(dom, el, "size", 20.0) * char_width, line),
            }
        }
        _ => with_attrs(300.0, 150.0),
    }
}
