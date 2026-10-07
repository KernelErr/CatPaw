//! Geometry questions about a laid-out tree: where boxes and inline
//! fragments are, what scrolls how far, and what is under a point.

use catpaw_dom::{Dom, NodeId};
use catpaw_text::parley;
use style::values::specified::box_::Overflow;

use crate::inline::node_of_brush;
use crate::{BoxId, BoxKind, LayoutTree, Positioning, Rect};

/// What a point hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HitTarget {
    /// The element under the point (text hits its parent element).
    pub element: NodeId,
}

/// The scroll geometry of a box.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollMetrics {
    /// The padding box, in document coordinates.
    pub client: Rect,
    pub scroll_width: f32,
    pub scroll_height: f32,
}

/// Fills in the document coordinates of every box.
pub(crate) fn place(tree: &mut LayoutTree) {
    let Some(root) = tree.root else {
        return;
    };
    let mut stack = vec![(root, 0.0f32, 0.0f32)];
    stack.extend(tree.oof_root.iter().map(|id| (*id, 0.0f32, 0.0f32)));
    while let Some((id, parent_x, parent_y)) = stack.pop() {
        let (x, y) = {
            let b = &tree.boxes[id];
            // Fixed boxes keep viewport coordinates; the page adds the
            // window's scroll position when it needs document ones.
            if b.positioning == Positioning::Fixed {
                (b.layout.location.x, b.layout.location.y)
            } else {
                (
                    parent_x + b.layout.location.x,
                    parent_y + b.layout.location.y,
                )
            }
        };
        tree.boxes[id].origin = (x, y);
        let scroll = tree.boxes[id]
            .node
            .and_then(|n| tree.scroll_offsets.get(&n).copied())
            .unwrap_or((0.0, 0.0));
        for child in tree.boxes[id].children.clone() {
            stack.push((child, x - scroll.0, y - scroll.1));
        }
    }
}

impl LayoutTree {
    /// The border box of a box, in document coordinates (viewport
    /// coordinates for a fixed box).
    pub fn border_box(&self, id: BoxId) -> Rect {
        let b = &self.boxes[id];
        Rect::new(
            b.origin.0,
            b.origin.1,
            b.layout.size.width,
            b.layout.size.height,
        )
    }

    /// The padding box: the border box without its borders.
    pub fn padding_box(&self, id: BoxId) -> Rect {
        let b = &self.boxes[id];
        let border = b.layout.border;
        Rect::new(
            b.origin.0 + border.left,
            b.origin.1 + border.top,
            (b.layout.size.width - border.left - border.right).max(0.0),
            (b.layout.size.height - border.top - border.bottom).max(0.0),
        )
    }

    pub fn content_box(&self, id: BoxId) -> Rect {
        let b = &self.boxes[id];
        let inset = b.layout.border + b.layout.padding;
        Rect::new(
            b.origin.0 + inset.left,
            b.origin.1 + inset.top,
            (b.layout.size.width - inset.left - inset.right).max(0.0),
            (b.layout.size.height - inset.top - inset.bottom).max(0.0),
        )
    }

    /// Whether the box is positioned against the viewport.
    pub fn is_fixed(&self, id: BoxId) -> bool {
        self.boxes[id].positioning == Positioning::Fixed
    }

    /// The nearest ancestor box (the box itself included) that is fixed.
    pub fn fixed_ancestor(&self, id: BoxId) -> Option<BoxId> {
        let mut current = Some(id);
        while let Some(b) = current {
            if self.boxes[b].positioning == Positioning::Fixed {
                return Some(b);
            }
            current = self.boxes[b].parent;
        }
        None
    }

    /// The rectangles a node is rendered in: one per line for text and
    /// inline elements, the border box for anything with a box, none for
    /// what is not rendered.
    pub fn node_rects(&self, dom: &Dom, node: NodeId) -> Vec<Rect> {
        if let Some(id) = self.node_box.get(&node) {
            return vec![self.border_box(*id)];
        }
        let Some(&owner) = self.inline_owner.get(&node) else {
            return Vec::new();
        };
        self.inline_fragments(dom, owner, node)
    }

    /// The smallest rectangle around everything a node is rendered in.
    pub fn bounding_rect(&self, dom: &Dom, node: NodeId) -> Option<Rect> {
        self.node_rects(dom, node)
            .into_iter()
            .reduce(|a, b| a.union(&b))
    }

    /// Whether `node` is `ancestor` or inside it.
    fn within(dom: &Dom, node: NodeId, ancestor: NodeId) -> bool {
        node == ancestor || dom.ancestors(node).any(|a| a == ancestor)
    }

    /// The line fragments of an inline node in the context `owner`.
    fn inline_fragments(&self, dom: &Dom, owner: BoxId, node: NodeId) -> Vec<Rect> {
        let b = &self.boxes[owner];
        let Some(context) = &b.inline else {
            return Vec::new();
        };
        let origin_x = b.origin.0 + b.layout.border.left + b.layout.padding.left;
        let origin_y = b.origin.1 + b.layout.border.top + b.layout.padding.top;
        let mut rects = Vec::new();
        for line in context.layout.lines() {
            let mut line_rect: Option<Rect> = None;
            for item in line.items() {
                match item {
                    parley::PositionedLayoutItem::GlyphRun(run) => {
                        let brush = run.style().brush;
                        if !Self::within(dom, node_of_brush(brush), node) {
                            continue;
                        }
                        let metrics = run.run().metrics();
                        let rect = Rect::new(
                            origin_x + run.offset(),
                            origin_y + run.baseline() - metrics.ascent,
                            run.advance(),
                            metrics.ascent + metrics.descent,
                        );
                        line_rect = Some(line_rect.map_or(rect, |r| r.union(&rect)));
                    }
                    parley::PositionedLayoutItem::InlineBox(inline_box) => {
                        let child = BoxId::from_taffy(taffy::NodeId::from(inline_box.id));
                        let Some(child_node) = self.boxes.get(child).and_then(|c| c.node) else {
                            continue;
                        };
                        if child_node != node && Self::within(dom, child_node, node) {
                            let rect = self.border_box(child);
                            line_rect = Some(line_rect.map_or(rect, |r| r.union(&rect)));
                        }
                    }
                }
            }
            if let Some(rect) = line_rect {
                rects.push(rect);
            }
        }
        if rects.is_empty() && dom.is_element(node) {
            // An inline element with nothing in it still has a place: a
            // point at the start of the line it would be on.
            let y = context
                .layout
                .lines()
                .next()
                .map_or(origin_y, |line| origin_y + line.metrics().block_min_coord);
            rects.push(Rect::new(origin_x, y, 0.0, 0.0));
        }
        rects
    }

    /// The scroll geometry of a box: its padding box and how far its
    /// content reaches.
    pub fn scroll_metrics(&self, id: BoxId) -> ScrollMetrics {
        let client = self.padding_box(id);
        let b = &self.boxes[id];
        let inset = b.layout.border + b.layout.padding;
        let scroll = b
            .node
            .and_then(|n| self.scroll_offsets.get(&n).copied())
            .unwrap_or((0.0, 0.0));
        // Content extent relative to the padding box origin, as if unscrolled.
        let mut right = client.width;
        let mut bottom = client.height;
        if let Some(context) = &b.inline {
            right = right.max(context.layout.full_width() + inset.right);
            bottom = bottom.max(context.height + inset.bottom);
        }
        for child in &b.children {
            let c = &self.boxes[*child];
            if c.positioning == Positioning::Fixed {
                continue;
            }
            let rel_x = c.origin.0 + scroll.0 - client.x;
            let rel_y = c.origin.1 + scroll.1 - client.y;
            right = right.max(rel_x + c.layout.size.width + c.layout.margin.right);
            bottom = bottom.max(rel_y + c.layout.size.height + c.layout.margin.bottom);
        }
        ScrollMetrics {
            client,
            scroll_width: right.max(0.0),
            scroll_height: bottom.max(0.0),
        }
    }

    /// Whether the box clips or scrolls its overflow.
    pub fn clips_overflow(&self, id: BoxId) -> bool {
        let overflow = &self.boxes[id].style.get_box();
        overflow.overflow_x != Overflow::Visible || overflow.overflow_y != Overflow::Visible
    }

    /// Whether the box is a scroll container.
    pub fn is_scroll_container(&self, id: BoxId) -> bool {
        let overflow = &self.boxes[id].style.get_box();
        matches!(overflow.overflow_x, Overflow::Scroll | Overflow::Auto)
            || matches!(overflow.overflow_y, Overflow::Scroll | Overflow::Auto)
    }

    /// The element under a point given in document coordinates, with the
    /// window's scroll position for fixed boxes.
    pub fn hit_test(
        &self,
        dom: &Dom,
        x: f32,
        y: f32,
        window_scroll: (f32, f32),
    ) -> Option<HitTarget> {
        let root = self.root?;
        self.oof_root
            .iter()
            .rev()
            .find_map(|id| self.hit_box(dom, *id, x, y, window_scroll))
            .or_else(|| self.hit_box(dom, root, x, y, window_scroll))
            .or_else(|| {
                // The root element's background covers the viewport, so a
                // point on bare canvas still hits it.
                let viewport = Rect::new(
                    window_scroll.0,
                    window_scroll.1,
                    self.viewport.width,
                    self.viewport.height,
                );
                viewport
                    .contains(x, y)
                    .then_some(())
                    .and(self.boxes[root].node)
            })
            .map(|element| HitTarget { element })
    }

    fn hit_box(
        &self,
        dom: &Dom,
        id: BoxId,
        x: f32,
        y: f32,
        window_scroll: (f32, f32),
    ) -> Option<NodeId> {
        let b = &self.boxes[id];
        let (px, py) = if b.positioning == Positioning::Fixed {
            (x - window_scroll.0, y - window_scroll.1)
        } else {
            (x, y)
        };
        let rect = self.border_box(id);
        let inside = rect.contains(px, py);
        if !inside && self.clips_overflow(id) {
            return None;
        }
        // Later siblings paint over earlier ones; positioned boxes over
        // static ones.
        let mut order: Vec<BoxId> = b.children.clone();
        order.sort_by_key(|c| self.boxes[*c].positioning != Positioning::Static);
        for child in order.into_iter().rev() {
            if let Some(hit) = self.hit_box(dom, child, px, py, (0.0, 0.0)) {
                return Some(hit);
            }
        }
        if !inside {
            return None;
        }
        if b.kind == BoxKind::InlineRoot
            && let Some(context) = &b.inline
        {
            let origin_x = b.origin.0 + b.layout.border.left + b.layout.padding.left;
            let origin_y = b.origin.1 + b.layout.border.top + b.layout.padding.top;
            for line in context.layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                        let metrics = run.run().metrics();
                        let rect = Rect::new(
                            origin_x + run.offset(),
                            origin_y + run.baseline() - metrics.ascent,
                            run.advance(),
                            metrics.ascent + metrics.descent,
                        );
                        if rect.contains(px, py) {
                            let node = node_of_brush(run.style().brush);
                            if dom.is_element(node) {
                                return Some(node);
                            }
                            if let Some(parent) = dom.parent_element(node) {
                                return Some(parent);
                            }
                        }
                    }
                }
            }
        }
        self.element_of(id)
    }

    /// The element a box stands for: its own, or for an anonymous box the
    /// nearest ancestor's.
    pub fn element_of(&self, id: BoxId) -> Option<NodeId> {
        let mut current = Some(id);
        while let Some(b) = current {
            if let Some(node) = self.boxes[b].node {
                return Some(node);
            }
            current = self.boxes[b].parent;
        }
        None
    }

    /// `offsetParent`: the nearest positioned ancestor element with a box,
    /// else `body`.
    pub fn offset_parent(&self, dom: &Dom, node: NodeId) -> Option<NodeId> {
        let id = self
            .node_box
            .get(&node)
            .copied()
            .or_else(|| self.inline_owner.get(&node).copied())?;
        if self.boxes[id].positioning == Positioning::Fixed {
            return None;
        }
        let body = dom.child_elements(dom.document()).next().and_then(|html| {
            dom.child_elements(html)
                .find(|e| dom.is_html_element(*e, "body"))
        });
        if Some(node) == body {
            return None;
        }
        let mut current = self.boxes[id].parent;
        // An inline element's own box is the context it is shaped in; its
        // offset parent search starts from that box.
        if !self.node_box.contains_key(&node) {
            current = Some(id);
        }
        while let Some(b) = current {
            let bx = &self.boxes[b];
            if let Some(el) = bx.node {
                if bx.positioning != Positioning::Static
                    || dom.is_html_element(el, "td")
                    || dom.is_html_element(el, "th")
                    || dom.is_html_element(el, "table")
                {
                    return Some(el);
                }
                if Some(el) == body {
                    return Some(el);
                }
            }
            current = bx.parent;
        }
        body
    }
}
