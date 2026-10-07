//! Taffy's view of the box tree, and the layout of the boxes Taffy leaves
//! to us: inline roots (Parley) and replaced elements.

use style::Atom;
use style::properties::ComputedValues;
use stylo_taffy::TaffyStyloStyle;
use taffy::prelude::*;
use taffy::{
    AvailableSpace, BlockContext, BoxSizing, CacheTree, CoreStyle, LayoutBlockContainer,
    LayoutFlexboxContainer, LayoutGridContainer, LayoutInput, LayoutOutput, LayoutPartialTree,
    MaybeMath, MaybeResolve, NodeId, Point, RequestedAxis, ResolveOrZero, RunMode, Size,
    SizingMode, TraversePartialTree, TraverseTree, compute_block_layout, compute_cached_layout,
    compute_flexbox_layout, compute_grid_layout, compute_leaf_layout, compute_root_layout,
};

use crate::{BoxId, BoxKind, LayoutTree, resolve_calc_value};

pub(crate) fn perform_layout(tree: &mut LayoutTree) {
    let Some(root) = tree.root else {
        return;
    };
    let viewport = tree.viewport;
    compute_root_layout(
        tree,
        root.to_taffy(),
        Size {
            width: AvailableSpace::Definite(viewport.width),
            height: AvailableSpace::Definite(viewport.height),
        },
    );
}

/// Lays out the boxes positioned against the initial containing block,
/// which is the viewport.
pub(crate) fn layout_root_oof(tree: &mut LayoutTree) {
    let viewport = Size {
        width: tree.viewport.width,
        height: tree.viewport.height,
    };
    for child in tree.oof_root.clone() {
        tree.layout_absolute_child(child, viewport, taffy::Rect::ZERO, taffy::Rect::ZERO);
    }
}

type ChildIter<'a> = std::iter::Map<std::slice::Iter<'a, BoxId>, fn(&BoxId) -> NodeId>;

fn to_taffy(id: &BoxId) -> NodeId {
    id.to_taffy()
}

impl LayoutTree {
    fn style_of(&self, id: NodeId) -> TaffyStyloStyle<&ComputedValues> {
        TaffyStyloStyle(&*self.boxes[BoxId::from_taffy(id)].style)
    }

    fn dispatch(
        &mut self,
        id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        let kind = self.boxes[BoxId::from_taffy(id)].kind;
        match kind {
            BoxKind::Block => compute_block_layout(self, id, inputs, block_ctx),
            BoxKind::Flex => compute_flexbox_layout(self, id, inputs),
            BoxKind::Grid => compute_grid_layout(self, id, inputs),
            BoxKind::InlineRoot => {
                self.compute_inline_layout(BoxId::from_taffy(id), inputs, block_ctx)
            }
            BoxKind::Replaced => {
                let intrinsic = self.boxes[BoxId::from_taffy(id)].intrinsic;
                let style = self.style_of(id);
                compute_leaf_layout(inputs, &style, resolve_calc_value, |known, _available| {
                    let width = known.width.or(intrinsic.width);
                    let height = known.height.or(intrinsic.height);
                    match (width, height, intrinsic.ratio) {
                        (Some(w), Some(h), _) => Size {
                            width: w,
                            height: h,
                        },
                        (Some(w), None, Some(ratio)) => Size {
                            width: w,
                            height: w / ratio,
                        },
                        (None, Some(h), Some(ratio)) => Size {
                            width: h * ratio,
                            height: h,
                        },
                        (Some(w), None, None) => Size {
                            width: w,
                            height: intrinsic.default_height,
                        },
                        (None, Some(h), None) => Size {
                            width: intrinsic.default_width,
                            height: h,
                        },
                        (None, None, _) => Size {
                            width: intrinsic.default_width,
                            height: intrinsic.default_height,
                        },
                    }
                })
            }
        }
    }

    /// Lays out an inline root: shapes its text against the width it gets,
    /// places its atomic boxes where Parley put them.
    fn compute_inline_layout(
        &mut self,
        id: BoxId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        let (padding, border, box_sizing, size, min_size, max_size, aspect_ratio) = {
            let style = self.style_of(id.to_taffy());
            let parent_width = inputs.parent_size.width;
            (
                style
                    .padding()
                    .resolve_or_zero(parent_width, resolve_calc_value),
                style
                    .border()
                    .resolve_or_zero(parent_width, resolve_calc_value),
                style.box_sizing(),
                style.size(),
                style.min_size(),
                style.max_size(),
                style.aspect_ratio(),
            )
        };
        let pb = padding + border;
        let pb_sum = pb.sum_axes();
        let box_sizing_adjustment = if box_sizing == BoxSizing::ContentBox {
            pb_sum
        } else {
            Size::ZERO
        };
        let (node_size, node_min_size, node_max_size) = match inputs.sizing_mode {
            SizingMode::ContentSize => (inputs.known_dimensions, Size::NONE, Size::NONE),
            SizingMode::InherentSize => {
                let style_size = size
                    .maybe_resolve(inputs.parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_min = min_size
                    .maybe_resolve(inputs.parent_size, resolve_calc_value)
                    .maybe_apply_aspect_ratio(aspect_ratio)
                    .maybe_add(box_sizing_adjustment);
                let style_max = max_size
                    .maybe_resolve(inputs.parent_size, resolve_calc_value)
                    .maybe_add(box_sizing_adjustment);
                let node_size = inputs
                    .known_dimensions
                    .or(style_size.maybe_clamp(style_min, style_max));
                (node_size, style_min, style_max)
            }
        };
        let known = node_size.maybe_max(pb_sum.map(Some));

        let mut context = self.boxes[id]
            .inline
            .take()
            .expect("inline root has a context");
        let atomic = context.boxes.clone();

        // The width the lines may take.
        let available_content_width = match known.width {
            Some(w) => AvailableSpace::Definite((w - pb_sum.width).max(0.0)),
            None => inputs.available_space.width.map_definite_value(|w| {
                let w = w.maybe_clamp(node_min_size.width, node_max_size.width);
                (w - pb_sum.width).max(0.0)
            }),
        };
        let content_parent_size = Size {
            width: known.width.map(|w| (w - pb_sum.width).max(0.0)),
            height: known.height.map(|h| (h - pb_sum.height).max(0.0)),
        };
        let child_inputs = LayoutInput {
            known_dimensions: Size::NONE,
            parent_size: content_parent_size,
            available_space: Size {
                width: available_content_width,
                height: AvailableSpace::MaxContent,
            },
            known_dimensions_are_definite: Size {
                width: false,
                height: false,
            },
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Both,
            run_mode: RunMode::ComputeSize,
            vertical_margins_are_collapsible: taffy::Line::FALSE,
        };

        // Size the atomic boxes so that Parley can place them.
        let mut margins = Vec::with_capacity(atomic.len());
        for (index, child) in atomic.iter().enumerate() {
            let margin = self
                .style_of(child.to_taffy())
                .margin()
                .resolve_or_zero(content_parent_size.width, resolve_calc_value);
            let output = self.compute_child_layout(child.to_taffy(), child_inputs);
            if let Some(inline_box) = context.layout.inline_boxes_mut().get_mut(index) {
                inline_box.width = output.size.width + margin.left + margin.right;
                inline_box.height = output.size.height + margin.top + margin.bottom;
            }
            margins.push(margin);
        }

        let content_widths = context.layout.calculate_content_widths();
        let content_width = match available_content_width {
            AvailableSpace::Definite(limit) => {
                limit.min(content_widths.max).max(content_widths.min)
            }
            AvailableSpace::MinContent => content_widths.min,
            AvailableSpace::MaxContent => content_widths.max,
        }
        .ceil();
        let width = known
            .width
            .map(|w| (w - pb_sum.width).max(0.0))
            .unwrap_or_else(|| {
                (content_width + pb_sum.width).maybe_clamp(node_min_size.width, node_max_size.width)
                    - pb_sum.width
            })
            .max(0.0);

        // Lines are broken against the floats of the block formatting
        // context, when there is one to ask; a scroll container starts
        // its own.
        let is_scroll_container = {
            let style = self.style_of(id.to_taffy());
            let overflow = style.overflow();
            overflow.x.is_scroll_container() || overflow.y.is_scroll_container()
        };
        fn break_in(
            outer: &mut BlockContext<'_>,
            layout: &mut parley::Layout<catpaw_text::Brush>,
            pb: taffy::Rect<f32>,
            width: f32,
        ) -> f32 {
            if outer.is_bfc_root() {
                outer.set_width(width + pb.left + pb.right);
            }
            let ctx = outer.sub_context(pb.top, [pb.left, pb.right]);
            break_lines_around_floats(layout, &ctx, width)
        }
        context.height = match block_ctx {
            Some(ctx) if !is_scroll_container => break_in(ctx, &mut context.layout, pb, width),
            _ => {
                let mut own = taffy::BlockFormattingContext::new();
                let mut root = own.root_block_context();
                break_in(&mut root, &mut context.layout, pb, width)
            }
        };
        let (alignment, last_line) = {
            let style = &self.boxes[id].style;
            (
                text_align(style.clone_text_align()),
                text_align_last(style.clone_text_align_last()),
            )
        };
        context.layout.align(
            alignment,
            parley::AlignmentOptions {
                align_when_overflowing: false,
            },
        );
        let _ = last_line;
        let has_content = !context.text.is_empty() || !atomic.is_empty();
        let content_height = if has_content { context.height } else { 0.0 };
        let measured = Size {
            width: width + pb_sum.width,
            height: content_height + pb_sum.height,
        };
        let outer = Size {
            width: known.width.unwrap_or(measured.width),
            height: known.height.unwrap_or(measured.height),
        }
        .maybe_clamp(node_min_size, node_max_size)
        .maybe_max(pb_sum.map(Some));
        let first_baseline = context
            .layout
            .lines()
            .next()
            .map(|line| line.metrics().baseline + pb.top);
        let last_baseline = context
            .layout
            .lines()
            .last()
            .map(|line| line.metrics().baseline + pb.top);
        let overflow_width = context.layout.full_width().max(width) + pb_sum.width;
        let overflow_height = content_height + pb_sum.height;

        if inputs.run_mode == RunMode::PerformLayout {
            // Place the atomic boxes, then let them lay out their insides.
            let mut placed = Vec::new();
            for line in context.layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::InlineBox(inline_box) = item {
                        placed.push((inline_box.id, inline_box.x, inline_box.y));
                    }
                }
            }
            for (ffi, x, y) in placed {
                let child = BoxId::from_taffy(NodeId::from(ffi));
                let index = atomic.iter().position(|c| *c == child).unwrap_or(0);
                let margin = margins.get(index).copied().unwrap_or(taffy::Rect::ZERO);
                let output = self.compute_child_layout(
                    child.to_taffy(),
                    LayoutInput {
                        run_mode: RunMode::PerformLayout,
                        ..child_inputs
                    },
                );
                let location = Point {
                    x: pb.left + x + margin.left,
                    y: pb.top + y + margin.top,
                };
                self.set_child_layout(
                    child,
                    location,
                    output.size,
                    content_parent_size.width,
                    margin,
                );
            }
            // Absolutely positioned boxes whose containing block this is.
            let oof: Vec<BoxId> = self.boxes[id]
                .children
                .iter()
                .copied()
                .filter(|c| !atomic.contains(c))
                .collect();
            for child in oof {
                self.layout_absolute_child(child, outer, border, pb);
            }
        }

        self.boxes[id].inline = Some(context);
        let mut output = LayoutOutput::from_outer_size(outer);
        output.scrollable_overflow_rect = taffy::Rect {
            left: 0.0,
            top: 0.0,
            right: overflow_width.max(outer.width),
            bottom: overflow_height.max(outer.height),
        };
        output.baselines.first = first_baseline;
        output.baselines.last = last_baseline;
        output
    }

    /// Records a child's layout the way Taffy would after placing it.
    fn set_child_layout(
        &mut self,
        child: BoxId,
        location: Point<f32>,
        size: Size<f32>,
        parent_width: Option<f32>,
        margin: taffy::Rect<f32>,
    ) {
        let (padding, border) = {
            let style = self.style_of(child.to_taffy());
            (
                style
                    .padding()
                    .resolve_or_zero(parent_width, resolve_calc_value),
                style
                    .border()
                    .resolve_or_zero(parent_width, resolve_calc_value),
            )
        };
        let layout = &mut self.boxes[child].layout;
        layout.location = location;
        layout.size = size;
        layout.padding = padding;
        layout.border = border;
        layout.margin = margin;
        if layout.scrollable_overflow_rect == taffy::Rect::ZERO {
            layout.scrollable_overflow_rect = taffy::Rect {
                left: 0.0,
                top: 0.0,
                right: size.width,
                bottom: size.height,
            };
        }
    }

    /// Positions an absolutely positioned child of an inline root against
    /// the root's padding box: insets where given, the start of the content
    /// otherwise.
    fn layout_absolute_child(
        &mut self,
        child: BoxId,
        container: Size<f32>,
        border: taffy::Rect<f32>,
        pb: taffy::Rect<f32>,
    ) {
        let padding_box = Size {
            width: (container.width - border.left - border.right).max(0.0),
            height: (container.height - border.top - border.bottom).max(0.0),
        };
        let (inset, margin, size) = {
            let style = self.style_of(child.to_taffy());
            let inset = style.inset();
            let inset = taffy::Rect {
                left: inset
                    .left
                    .maybe_resolve(Some(padding_box.width), resolve_calc_value),
                right: inset
                    .right
                    .maybe_resolve(Some(padding_box.width), resolve_calc_value),
                top: inset
                    .top
                    .maybe_resolve(Some(padding_box.height), resolve_calc_value),
                bottom: inset
                    .bottom
                    .maybe_resolve(Some(padding_box.height), resolve_calc_value),
            };
            let margin = style
                .margin()
                .resolve_or_zero(Some(padding_box.width), resolve_calc_value);
            let size = style
                .size()
                .maybe_resolve(padding_box.map(Some), resolve_calc_value);
            (inset, margin, size)
        };
        let known = Size {
            width: match (inset.left, inset.right, size.width) {
                (Some(l), Some(r), None) => {
                    Some((padding_box.width - l - r - margin.left - margin.right).max(0.0))
                }
                (_, _, w) => w,
            },
            height: match (inset.top, inset.bottom, size.height) {
                (Some(t), Some(b), None) => {
                    Some((padding_box.height - t - b - margin.top - margin.bottom).max(0.0))
                }
                (_, _, h) => h,
            },
        };
        let output = self.compute_child_layout(
            child.to_taffy(),
            LayoutInput {
                known_dimensions: known,
                known_dimensions_are_definite: known.map(|k| k.is_some()),
                parent_size: padding_box.map(Some),
                available_space: padding_box.map(AvailableSpace::Definite),
                sizing_mode: SizingMode::InherentSize,
                axis: RequestedAxis::Both,
                run_mode: RunMode::PerformLayout,
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            },
        );
        let x = match (inset.left, inset.right) {
            (Some(l), _) => border.left + l + margin.left,
            (None, Some(r)) => {
                border.left + padding_box.width - r - margin.right - output.size.width
            }
            (None, None) => pb.left + margin.left,
        };
        let y = match (inset.top, inset.bottom) {
            (Some(t), _) => border.top + t + margin.top,
            (None, Some(b)) => {
                border.top + padding_box.height - b - margin.bottom - output.size.height
            }
            (None, None) => pb.top + margin.top,
        };
        self.set_child_layout(
            child,
            Point { x, y },
            output.size,
            Some(padding_box.width),
            margin,
        );
    }
}

/// Breaks the lines of an inline layout so that each takes the band free
/// of floats at its height: the band's start and width come from the
/// block context, and a band too narrow to hold anything is passed over
/// for the next one down. Returns the bottom of the lowest line.
fn break_lines_around_floats(
    layout: &mut parley::Layout<catpaw_text::Brush>,
    ctx: &BlockContext<'_>,
    width: f32,
) -> f32 {
    // A band the floats leave no room in is passed over for the space
    // below all of them.
    let usable = |ctx: &BlockContext<'_>, slot: taffy::ContentSlot| {
        if slot.segment_id.is_some() && slot.width < 1.0 {
            ctx.find_content_slot(slot.y, taffy::Clear::Both, None)
        } else {
            slot
        }
    };
    let mut breaker = layout.break_lines();
    let first = usable(ctx, ctx.find_content_slot(0.0, taffy::Clear::None, None));
    let mut beside_floats = first.segment_id.is_some();
    {
        let state = breaker.state_mut();
        state.set_layout_max_advance(width);
        state.set_line_max_advance(first.width.max(0.0));
        state.set_line_x(first.x);
        state.set_line_y(f64::from(first.y));
    }
    // The bottom of each line, for the height; a trailing empty line (from
    // a final forced break) does not count, as Parley has it.
    let mut bottoms: Vec<f32> = Vec::new();
    while let Some(yielded) = breaker.break_next() {
        match yielded {
            parley::layout::YieldData::LineBreak(data) => {
                bottoms.push(data.line_y_end as f32);
                let state = breaker.state_mut();
                if beside_floats {
                    let min_y = state.line_y() as f32;
                    let next = usable(ctx, ctx.find_content_slot(min_y, taffy::Clear::None, None));
                    beside_floats = next.segment_id.is_some();
                    state.set_line_max_advance(next.width.max(0.0));
                    state.set_line_x(next.x);
                    state.set_line_y(f64::from(next.y));
                } else {
                    state.set_line_x(0.0);
                    state.set_line_max_advance(width);
                }
            }
            parley::layout::YieldData::MaxHeightExceeded(_) => {}
            parley::layout::YieldData::InlineBoxBreak(data) => {
                // No floated inline boxes are made yet; one would sit on
                // the line as an in-flow box.
                let state = breaker.state_mut();
                state.append_inline_box_to_line(data.advance, f32::NEG_INFINITY);
            }
        }
    }
    breaker.finish();
    let last_is_empty = layout
        .lines()
        .last()
        .is_some_and(|line| line.text_range().is_empty() && line.items().next().is_none());
    if last_is_empty && layout.len() >= 2 {
        bottoms.pop();
    }
    bottoms
        .into_iter()
        .fold(0.0_f32, f32::max)
        .max(layout.height().min(0.0))
}

fn text_align(align: style::values::computed::TextAlign) -> parley::Alignment {
    use style::values::computed::TextAlign;
    match align {
        TextAlign::Start | TextAlign::MozLeft | TextAlign::MozCenter | TextAlign::MozRight => {
            parley::Alignment::Start
        }
        TextAlign::Left => parley::Alignment::Left,
        TextAlign::Right => parley::Alignment::Right,
        TextAlign::Center => parley::Alignment::Center,
        TextAlign::Justify => parley::Alignment::Justify,
        TextAlign::End => parley::Alignment::End,
    }
}

fn text_align_last(align: style::values::computed::TextAlignLast) -> Option<parley::Alignment> {
    use style::values::computed::TextAlignLast;
    match align {
        TextAlignLast::Auto => None,
        TextAlignLast::Start => Some(parley::Alignment::Start),
        TextAlignLast::End => Some(parley::Alignment::End),
        TextAlignLast::Left => Some(parley::Alignment::Left),
        TextAlignLast::Right => Some(parley::Alignment::Right),
        TextAlignLast::Center => Some(parley::Alignment::Center),
        TextAlignLast::Justify => Some(parley::Alignment::Justify),
    }
}

impl TraversePartialTree for LayoutTree {
    type ChildIter<'a> = ChildIter<'a>;

    fn child_ids(&self, parent: NodeId) -> Self::ChildIter<'_> {
        self.boxes[BoxId::from_taffy(parent)]
            .children
            .iter()
            .map(to_taffy as fn(&BoxId) -> NodeId)
    }

    fn child_count(&self, parent: NodeId) -> usize {
        self.boxes[BoxId::from_taffy(parent)].children.len()
    }

    fn get_child_id(&self, parent: NodeId, index: usize) -> NodeId {
        self.boxes[BoxId::from_taffy(parent)].children[index].to_taffy()
    }
}

impl TraverseTree for LayoutTree {}

impl LayoutPartialTree for LayoutTree {
    type CoreContainerStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;
    type CustomIdent = Atom;

    fn get_core_container_style(&self, id: NodeId) -> Self::CoreContainerStyle<'_> {
        self.style_of(id)
    }

    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 {
        resolve_calc_value(val, basis)
    }

    fn set_unrounded_layout(&mut self, id: NodeId, layout: &Layout) {
        self.boxes[BoxId::from_taffy(id)].layout = *layout;
    }

    fn compute_child_layout(&mut self, id: NodeId, inputs: LayoutInput) -> LayoutOutput {
        compute_cached_layout(self, id, inputs, |tree, id, inputs| {
            tree.dispatch(id, inputs, None)
        })
    }
}

impl CacheTree for LayoutTree {
    fn cache_get(&mut self, id: NodeId, input: &LayoutInput) -> Option<LayoutOutput> {
        self.boxes[BoxId::from_taffy(id)].cache.get(input)
    }

    fn cache_store(&mut self, id: NodeId, input: &LayoutInput, output: LayoutOutput) {
        self.boxes[BoxId::from_taffy(id)].cache.store(input, output);
    }

    fn cache_clear(&mut self, id: NodeId) {
        self.boxes[BoxId::from_taffy(id)].cache.clear();
    }
}

impl LayoutBlockContainer for LayoutTree {
    type BlockContainerStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;
    type BlockItemStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;

    fn get_block_container_style(&self, id: NodeId) -> Self::BlockContainerStyle<'_> {
        self.style_of(id)
    }

    fn get_block_child_style(&self, id: NodeId) -> Self::BlockItemStyle<'_> {
        self.style_of(id)
    }

    fn compute_block_child_layout(
        &mut self,
        id: NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        compute_cached_layout(self, id, inputs, |tree, id, inputs| {
            tree.dispatch(id, inputs, block_ctx)
        })
    }
}

impl LayoutFlexboxContainer for LayoutTree {
    type FlexboxContainerStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;
    type FlexboxItemStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;

    fn get_flexbox_container_style(&self, id: NodeId) -> Self::FlexboxContainerStyle<'_> {
        self.style_of(id)
    }

    fn get_flexbox_child_style(&self, id: NodeId) -> Self::FlexboxItemStyle<'_> {
        self.style_of(id)
    }
}

impl LayoutGridContainer for LayoutTree {
    type GridContainerStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;
    type GridItemStyle<'a>
        = TaffyStyloStyle<&'a ComputedValues>
    where
        Self: 'a;

    fn get_grid_container_style(&self, id: NodeId) -> Self::GridContainerStyle<'_> {
        self.style_of(id)
    }

    fn get_grid_child_style(&self, id: NodeId) -> Self::GridItemStyle<'_> {
        self.style_of(id)
    }
}
