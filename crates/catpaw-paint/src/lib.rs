//! Painting for CatPaw: a laid-out box tree drawn with tiny-skia, for
//! screenshots.
//!
//! What is drawn, in order: the canvas background (the root element's, or
//! the body's), then each box's background and borders followed by its
//! content, in tree order with positioned boxes after their static
//! siblings; text comes from Parley's glyph runs through skrifa outlines.
//! Overflow that is hidden, clipped or scrolled clips its descendants to
//! the padding box. Not drawn yet: images, gradients, shadows, rounded
//! corners, transforms, opacity.
//!
//! Only what can show is drawn: backgrounds, borders, glyphs and bitmaps
//! that miss the output or the clip are skipped, as is everything inside a
//! clip that misses the output. Clips are rectangles, so one mask buffer
//! the size of the output serves them all: opaque, with a clip cut into it
//! only around a draw that crosses the clip's edge.

use std::collections::HashMap;
use std::rc::Rc;

use catpaw_dom::{Dom, NodeId};
use catpaw_layout::{BoxId, BoxKind, LayoutTree, Positioning, Rect};
use catpaw_text::parley::{self, PositionedLayoutItem};
use catpaw_text::skrifa::{self, MetadataProvider as _};
use style::color::{AbsoluteColor, ColorSpace};
use style::properties::ComputedValues;
use style::values::computed::TextDecorationLine;
use style::values::specified::box_::Overflow;
use tiny_skia::{FillRule, Mask, Paint, Path, PathBuilder, Pixmap, Transform};

pub use tiny_skia;

pub mod canvas;

/// What to paint.
#[derive(Clone, Debug)]
pub struct Options {
    /// Size of the output, in CSS pixels.
    pub width: u32,
    pub height: u32,
    /// The document coordinates at the output's top-left corner.
    pub scroll: (f32, f32),
    /// Device pixels per CSS pixel.
    pub scale: f32,
}

/// The bitmap behind a replaced element (a canvas), if it has one; asked
/// for only when the element can show.
pub type ReplacedContent<'a> = &'a dyn Fn(NodeId) -> Option<Pixmap>;

/// Paints the tree into a pixmap of `options.width × options.height` CSS
/// pixels (times the scale), white where nothing is drawn.
pub fn render(tree: &LayoutTree, dom: &Dom, options: &Options) -> Pixmap {
    render_with(tree, dom, options, &|_| None)
}

/// `render`, with the bitmaps of replaced elements drawn in their content
/// boxes.
pub fn render_with(
    tree: &LayoutTree,
    dom: &Dom,
    options: &Options,
    replaced: ReplacedContent<'_>,
) -> Pixmap {
    render_counting(tree, dom, options, replaced).0
}

/// `render`, encoded as PNG.
pub fn render_png(tree: &LayoutTree, dom: &Dom, options: &Options) -> Vec<u8> {
    render_png_with(tree, dom, options, &|_| None)
}

/// `render_with`, encoded as PNG.
pub fn render_png_with(
    tree: &LayoutTree,
    dom: &Dom,
    options: &Options,
    replaced: ReplacedContent<'_>,
) -> Vec<u8> {
    render_with(tree, dom, options, replaced)
        .encode_png()
        .expect("PNG encoding of an in-memory pixmap")
}

/// `render_with`, with counts of the work it did.
fn render_counting(
    tree: &LayoutTree,
    dom: &Dom,
    options: &Options,
    replaced: ReplacedContent<'_>,
) -> (Pixmap, Stats) {
    let width = ((options.width as f32) * options.scale).round().max(1.0) as u32;
    let height = ((options.height as f32) * options.scale).round().max(1.0) as u32;
    let mut pixmap = Pixmap::new(width, height).expect("a non-empty pixmap");
    pixmap.fill(tiny_skia::Color::WHITE);
    let mut painter = Painter {
        pixmap: &mut pixmap,
        tree,
        dom,
        scroll: options.scroll,
        scale: options.scale,
        clip: None,
        clip_mask: ClipMask::default(),
        glyphs: HashMap::new(),
        font_boxes: HashMap::new(),
        replaced,
        stats: Stats::default(),
    };
    painter.paint_canvas();
    if let Some(root) = tree.root() {
        painter.paint_box(root);
    }
    for id in tree.viewport_positioned() {
        painter.paint_box(*id);
    }
    let mut stats = painter.stats;
    stats.masks_made = painter.clip_mask.made;
    stats.mask_cuts = painter.clip_mask.cuts;
    (pixmap, stats)
}

/// Counts of what a render did, for tests to check that work stays in
/// proportion to what shows.
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct Stats {
    /// Glyphs and bitmaps drawn, and how many of them crossed a clip's edge.
    drawn: usize,
    masked: usize,
    /// Glyphs, bitmaps and rectangles skipped because they could not show.
    culled: usize,
    /// Clip masks allocated, and clips cut into them.
    masks_made: usize,
    mask_cuts: usize,
}

struct Painter<'a> {
    pixmap: &'a mut Pixmap,
    tree: &'a LayoutTree,
    dom: &'a Dom,
    scroll: (f32, f32),
    scale: f32,
    /// The clip in output coordinates, if any box above clips.
    clip: Option<Rect>,
    /// The mask draws under the clip go through.
    clip_mask: ClipMask,
    /// Glyph outlines at a size, by font, glyph and size.
    glyphs: HashMap<(u64, u32, u32, u32), Option<Rc<Path>>>,
    /// The box around every glyph of a font, by font.
    font_boxes: HashMap<(u64, u32), Option<[f32; 4]>>,
    replaced: ReplacedContent<'a>,
    stats: Stats,
}

/// Where a draw can show.
#[derive(Clone, Copy)]
enum Reach {
    /// Nowhere: it misses the output, or the clip hides all of it.
    Nowhere,
    /// Only on pixels the clip, if there is one, leaves whole.
    Whole,
    /// Across the edge of the clip's rectangle, within the given pixels
    /// (`[left, top, right, bottom)`, on the output).
    ClipEdge(tiny_skia::Rect, [usize; 4]),
}

/// The mask that draws under a clip go through. tiny-skia takes only masks
/// the size of the pixmap, and blends through a mask with arithmetic of its
/// own (coverage scaled before blending, no shortcut for opaque colours), so
/// a draw under a clip goes through a mask even where the clip hides none
/// of it, to come out as it always has. One buffer serves every clip: it is
/// opaque everywhere, except that a draw crossing the clip's edge first
/// cuts the clip into the pixels it covers, and mends them afterwards.
#[derive(Default)]
struct ClipMask {
    mask: Option<Mask>,
    /// Buffers allocated, and cuts made into them.
    made: usize,
    cuts: usize,
}

impl ClipMask {
    /// The buffer for a `width × height` output, opaque everywhere.
    fn open(&mut self, width: u32, height: u32) -> &mut Mask {
        if self.mask.is_none() {
            self.made += 1;
        }
        self.mask.get_or_insert_with(|| {
            let size = tiny_skia::IntSize::from_wh(width, height).expect("a non-empty pixmap");
            Mask::from_vec(vec![255; width as usize * height as usize], size)
                .expect("a mask the size of the pixmap")
        })
    }

    /// The buffer with `clip` cut into `region`: cleared there, then the
    /// clip's rectangle drawn as tiny-skia draws a whole clip's mask. A
    /// pixel's coverage depends only on the edges that cross it, and where
    /// the region cuts the rectangle short, its edges lie on pixel
    /// boundaries a pixel away from anything the draw touches.
    fn cut(&mut self, clip: tiny_skia::Rect, region: [usize; 4], width: u32, height: u32) -> &Mask {
        self.cuts += 1;
        let mask = self.open(width, height);
        fill_region(mask, region, 0);
        let [left, top, right, bottom] = region.map(|v| v as f32);
        if let Some(rect) = tiny_skia::Rect::from_ltrb(
            clip.left().max(left),
            clip.top().max(top),
            clip.right().min(right),
            clip.bottom().min(bottom),
        ) {
            let path = PathBuilder::from_rect(rect);
            mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        }
        mask
    }

    /// Makes `region` opaque again after a cut.
    fn mend(&mut self, region: [usize; 4]) {
        if let Some(mask) = &mut self.mask {
            fill_region(mask, region, 255);
        }
    }
}

/// Sets the pixels of `region` (`[left, top, right, bottom)`) of a mask.
fn fill_region(mask: &mut Mask, [left, top, right, bottom]: [usize; 4], value: u8) {
    let stride = mask.width() as usize;
    let data = mask.data_mut();
    for row in top..bottom {
        data[row * stride + left..row * stride + right].fill(value);
    }
}

/// An sRGB colour with alpha, components in 0..=1.
#[derive(Clone, Copy, PartialEq)]
struct Rgba([f32; 4]);

impl Rgba {
    fn from_style(color: &AbsoluteColor) -> Self {
        let srgb = color.to_color_space(ColorSpace::Srgb);
        let c = srgb.raw_components();
        Rgba([c[0], c[1], c[2], c[3]])
    }

    fn is_transparent(&self) -> bool {
        self.0[3] <= 0.0
    }

    fn paint(&self) -> Paint<'static> {
        let mut paint = Paint::default();
        let [r, g, b, a] = self.0;
        paint.set_color(
            tiny_skia::Color::from_rgba(
                r.clamp(0.0, 1.0),
                g.clamp(0.0, 1.0),
                b.clamp(0.0, 1.0),
                a.clamp(0.0, 1.0),
            )
            .expect("components in range"),
        );
        paint.anti_alias = true;
        paint
    }
}

fn background_color(style: &ComputedValues) -> Rgba {
    let current = style.clone_color();
    Rgba::from_style(
        &style
            .get_background()
            .background_color
            .resolve_to_absolute(&current),
    )
}

fn text_color(style: &ComputedValues) -> Rgba {
    Rgba::from_style(&style.clone_color())
}

/// The bounds `[left, top, right, bottom]` of a rectangle placed by
/// `transform`.
fn transformed_bounds(rect: tiny_skia::Rect, transform: Transform) -> [f32; 4] {
    let t = transform;
    let mut bounds = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for (x, y) in [
        (rect.left(), rect.top()),
        (rect.right(), rect.top()),
        (rect.left(), rect.bottom()),
        (rect.right(), rect.bottom()),
    ] {
        let (px, py) = (t.sx * x + t.kx * y + t.tx, t.ky * x + t.sy * y + t.ty);
        bounds = [
            bounds[0].min(px),
            bounds[1].min(py),
            bounds[2].max(px),
            bounds[3].max(py),
        ];
    }
    bounds
}

impl Painter<'_> {
    /// Document coordinates to output coordinates.
    fn to_output(&self, rect: Rect, fixed: bool) -> Rect {
        let (sx, sy) = if fixed { (0.0, 0.0) } else { self.scroll };
        Rect::new(
            (rect.x - sx) * self.scale,
            (rect.y - sy) * self.scale,
            rect.width * self.scale,
            rect.height * self.scale,
        )
    }

    fn intersect_clip(&self, rect: Rect) -> Option<Rect> {
        let clip = match self.clip {
            None => return Some(rect),
            Some(c) => c,
        };
        let x = rect.x.max(clip.x);
        let y = rect.y.max(clip.y);
        let right = rect.right().min(clip.right());
        let bottom = rect.bottom().min(clip.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// Whether a rectangle in output coordinates lies wholly off the
    /// output, a pixel of slack included for anti-aliasing (false for odd
    /// geometry, which is left to tiny-skia).
    fn off_output(&self, rect: Rect) -> bool {
        let (width, height) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        rect.right() < -1.0 || rect.bottom() < -1.0 || rect.x > width + 1.0 || rect.y > height + 1.0
    }

    /// Whether nothing can show through the current clip: it has no area
    /// or lies off the output.
    fn clipped_away(&self) -> bool {
        self.clip
            .is_some_and(|clip| clip.width <= 0.0 || clip.height <= 0.0 || self.off_output(clip))
    }

    /// Where a draw whose ink lies within `[left, top, right, bottom]`
    /// (output coordinates) can show. Rasterizing touches only the pixels
    /// the ink's bounds round out to, and a clip's mask is opaque on the
    /// pixels the clip covers whole and clear past those it touches; a
    /// pixel of slack on each side absorbs rounding.
    fn reach(&self, [left, top, right, bottom]: [f32; 4]) -> Reach {
        let (width, height) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        // The clip's rectangle as its mask is drawn from; without one (odd
        // geometry), the mask stays clear and hides everything.
        let clip = match self.clip {
            None => None,
            Some(clip) => match tiny_skia::Rect::from_xywh(clip.x, clip.y, clip.width, clip.height)
            {
                Some(rect) => Some(rect),
                None => return Reach::Nowhere,
            },
        };
        if ![left, top, right, bottom].iter().all(|v| v.is_finite()) {
            // Odd geometry goes through the whole clip, as it always did.
            let output = [0, 0, width as usize, height as usize];
            return clip.map_or(Reach::Whole, |clip| Reach::ClipEdge(clip, output));
        }
        let x0 = (left.floor() - 1.0).max(0.0);
        let y0 = (top.floor() - 1.0).max(0.0);
        let x1 = (right.ceil() + 1.0).min(width);
        let y1 = (bottom.ceil() + 1.0).min(height);
        if x0 >= x1 || y0 >= y1 {
            return Reach::Nowhere;
        }
        let Some(clip) = clip else {
            return Reach::Whole;
        };
        if clip.width() <= 0.0
            || clip.height() <= 0.0
            || x1 <= clip.left().floor() - 1.0
            || y1 <= clip.top().floor() - 1.0
            || x0 >= clip.right().ceil() + 1.0
            || y0 >= clip.bottom().ceil() + 1.0
        {
            return Reach::Nowhere;
        }
        if x0 >= clip.left().ceil() + 1.0
            && y0 >= clip.top().ceil() + 1.0
            && x1 <= clip.right().floor() - 1.0
            && y1 <= clip.bottom().floor() - 1.0
        {
            return Reach::Whole;
        }
        Reach::ClipEdge(clip, [x0, y0, x1, y1].map(|v| v as usize))
    }

    /// Runs `draw` for ink within `bounds` (output coordinates), under the
    /// clip: not at all where it cannot show, and with the clip cut into
    /// the mask only when it crosses the clip's edge.
    fn draw_clipped(&mut self, bounds: [f32; 4], draw: impl FnOnce(&mut Pixmap, Option<&Mask>)) {
        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        match self.reach(bounds) {
            Reach::Nowhere => self.stats.culled += 1,
            Reach::Whole => {
                self.stats.drawn += 1;
                if self.clip.is_some() {
                    let mask = self.clip_mask.open(width, height);
                    draw(self.pixmap, Some(mask));
                } else {
                    draw(self.pixmap, None);
                }
            }
            Reach::ClipEdge(clip, region) => {
                self.stats.drawn += 1;
                self.stats.masked += 1;
                let mask = self.clip_mask.cut(clip, region, width, height);
                draw(self.pixmap, Some(mask));
                self.clip_mask.mend(region);
            }
        }
    }

    fn fill_rect(&mut self, rect: Rect, color: Rgba) {
        if color.is_transparent() {
            return;
        }
        let Some(rect) = self.intersect_clip(rect) else {
            return;
        };
        if self.off_output(rect) {
            self.stats.culled += 1;
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.width, rect.height) else {
            return;
        };
        self.pixmap
            .fill_rect(rect, &color.paint(), Transform::identity(), None);
    }

    /// The canvas background: the root element's background colour, or the
    /// body's when the root has none.
    fn paint_canvas(&mut self) {
        let Some(root) = self.tree.root() else {
            return;
        };
        let root_box = self.tree.get(root);
        let mut color = background_color(&root_box.style);
        if color.is_transparent()
            && let Some(root_node) = root_box.node
            && let Some(body) = self
                .dom
                .child_elements(root_node)
                .find(|e| self.dom.is_html_element(*e, "body"))
            && let Some(body_box) = self.tree.box_of(body)
        {
            color = background_color(&self.tree.get(body_box).style);
        }
        if color.is_transparent() {
            return;
        }
        let (w, h) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        self.fill_rect(Rect::new(0.0, 0.0, w, h), color);
    }

    fn paint_box(&mut self, id: BoxId) {
        let tree = self.tree;
        let b = tree.get(id);
        let style: &ComputedValues = &b.style;
        let visible =
            style.get_inherited_box().visibility == style::computed_values::visibility::T::Visible;
        let fixed = tree.fixed_ancestor(id).is_some();
        let border_box = self.to_output(tree.border_box(id), fixed);
        let padding_box = self.to_output(tree.padding_box(id), fixed);
        // Backgrounds and borders stay within the border box.
        if visible && !self.off_output(border_box) {
            // Propagated to the canvas already.
            let is_root = Some(id) == tree.root();
            let is_body = b.node.is_some_and(|n| self.dom.is_html_element(n, "body"))
                && tree
                    .root()
                    .is_some_and(|r| background_color(&tree.get(r).style).is_transparent());
            if !is_root && !is_body {
                self.fill_rect(border_box, background_color(style));
            }
            self.paint_borders(id, border_box, padding_box, style);
        }

        let clips = {
            let overflow = style.get_box();
            overflow.overflow_x != Overflow::Visible || overflow.overflow_y != Overflow::Visible
        };
        let outer_clip = self.clip;
        if clips {
            self.clip = self.intersect_clip(padding_box).or(Some(Rect::default()));
        }
        // Nothing inside a clip that hides everything can show.
        if !self.clipped_away() {
            if visible && b.kind == BoxKind::InlineRoot {
                let content = self.to_output(tree.content_box(id), fixed);
                self.paint_inline(id, content.x, content.y);
            }
            if visible
                && b.kind == BoxKind::Replaced
                && let Some(node) = b.node
            {
                let content = self.to_output(tree.content_box(id), fixed);
                // The bitmap comes as a copy: only asked for when it can
                // show.
                let bounds = [content.x, content.y, content.right(), content.bottom()];
                if matches!(self.reach(bounds), Reach::Nowhere) {
                    self.stats.culled += 1;
                } else if let Some(bitmap) = (self.replaced)(node) {
                    self.paint_bitmap(&bitmap, content);
                }
            }
            // Static children first, then positioned ones, each in tree
            // order.
            let children = tree.children(id);
            for positioned in [false, true] {
                for &child in children {
                    if (tree.get(child).positioning != Positioning::Static) == positioned {
                        self.paint_box(child);
                    }
                }
            }
        }
        self.clip = outer_clip;
    }

    /// Draws a bitmap scaled into `rect`, clipped like everything else.
    fn paint_bitmap(&mut self, bitmap: &Pixmap, rect: Rect) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let transform = Transform::from_translate(rect.x, rect.y).pre_scale(
            rect.width / bitmap.width() as f32,
            rect.height / bitmap.height() as f32,
        );
        let bounds = [rect.x, rect.y, rect.right(), rect.bottom()];
        self.draw_clipped(bounds, |pixmap, mask| {
            pixmap.draw_pixmap(
                0,
                0,
                bitmap.as_ref(),
                &tiny_skia::PixmapPaint::default(),
                transform,
                mask,
            );
        });
    }

    fn paint_borders(&mut self, id: BoxId, outer: Rect, inner: Rect, style: &ComputedValues) {
        use style::values::computed::BorderStyle;
        let border = style.get_border();
        let current = style.clone_color();
        let _ = id;
        let sides = [
            (
                border.border_top_style,
                &border.border_top_color,
                Rect::new(outer.x, outer.y, outer.width, inner.y - outer.y),
            ),
            (
                border.border_right_style,
                &border.border_right_color,
                Rect::new(
                    inner.right(),
                    outer.y,
                    outer.right() - inner.right(),
                    outer.height,
                ),
            ),
            (
                border.border_bottom_style,
                &border.border_bottom_color,
                Rect::new(
                    outer.x,
                    inner.bottom(),
                    outer.width,
                    outer.bottom() - inner.bottom(),
                ),
            ),
            (
                border.border_left_style,
                &border.border_left_color,
                Rect::new(outer.x, outer.y, inner.x - outer.x, outer.height),
            ),
        ];
        for (side_style, color, rect) in sides {
            if matches!(side_style, BorderStyle::None | BorderStyle::Hidden)
                || rect.width <= 0.0
                || rect.height <= 0.0
            {
                continue;
            }
            let color = Rgba::from_style(&color.resolve_to_absolute(&current));
            self.fill_rect(rect, color);
        }
    }

    /// Whether `node` or an inline ancestor up to the context's root asks
    /// for an underline.
    fn underlined(&self, node: NodeId, root: NodeId) -> bool {
        let mut current = Some(node);
        while let Some(n) = current {
            if let Some(style) = self.tree.inline_style(n)
                && style
                    .get_text()
                    .text_decoration_line
                    .contains(TextDecorationLine::UNDERLINE)
            {
                return true;
            }
            if n == root {
                break;
            }
            current = self.dom.parent_element(n);
        }
        let root_style = self.tree.box_of(root).map(|b| &self.tree.get(b).style);
        root_style.is_some_and(|s| {
            s.get_text()
                .text_decoration_line
                .contains(TextDecorationLine::UNDERLINE)
        })
    }

    fn paint_inline(&mut self, id: BoxId, origin_x: f32, origin_y: f32) {
        let tree = self.tree;
        let Some(layout) = tree.inline_layout(id) else {
            return;
        };
        let root_node = tree.element_of(id);
        let root_style: &ComputedValues = &tree.get(id).style;
        let scale = self.scale;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let font = run.run().font();
                let size = run.run().font_size();
                let glyphs_show =
                    match self.run_ink(font, size, run.positioned_glyphs(), origin_x, origin_y) {
                        Some(ink) => !matches!(self.reach(ink), Reach::Nowhere),
                        None => true,
                    };
                let underline = root_node.map(|root| {
                    let metrics = run.run().metrics();
                    let thickness = (size / 14.0).max(1.0) * scale;
                    let rect = Rect::new(
                        origin_x + run.offset() * scale,
                        origin_y + (run.baseline() + metrics.descent * 0.35) * scale,
                        run.advance() * scale,
                        thickness,
                    );
                    (root, rect)
                });
                let underline = underline.filter(|(_, rect)| !self.off_output(*rect));
                if !glyphs_show && underline.is_none() {
                    self.stats.culled += 1;
                    continue;
                }
                let node = LayoutTree::node_of_brush(run.style().brush);
                let style = tree.inline_style(node).map_or(root_style, |s| &**s);
                if style.get_inherited_box().visibility
                    != style::computed_values::visibility::T::Visible
                {
                    continue;
                }
                let color = text_color(style);
                if color.is_transparent() {
                    continue;
                }
                if glyphs_show {
                    let coords = run.run().normalized_coords();
                    let paint = color.paint();
                    for glyph in run.positioned_glyphs() {
                        let Some(path) = self.glyph_path(font, glyph.id, size, coords) else {
                            continue;
                        };
                        let transform = Transform::from_translate(
                            origin_x + glyph.x * scale,
                            origin_y + glyph.y * scale,
                        )
                        .pre_scale(scale, scale);
                        let bounds = transformed_bounds(path.bounds(), transform);
                        self.draw_clipped(bounds, |pixmap, mask| {
                            pixmap.fill_path(&path, &paint, FillRule::Winding, transform, mask);
                        });
                    }
                }
                if let Some((root, rect)) = underline
                    && self.underlined(node, root)
                {
                    self.fill_rect(rect, color);
                }
            }
        }
    }

    /// Bounds (output coordinates) around the ink of a run's glyphs, from
    /// where they sit and the box around every glyph of their font, with an
    /// em to spare on each side for outlines that stray past it (variations,
    /// careless fonts); `None` when the font gives no box. Cheaper than the
    /// outlines, so that runs far from the output need none.
    fn run_ink(
        &mut self,
        font: &parley::FontData,
        size: f32,
        glyphs: impl Iterator<Item = parley::Glyph>,
        origin_x: f32,
        origin_y: f32,
    ) -> Option<[f32; 4]> {
        let [x_min, y_min, x_max, y_max] = self.font_box(font)?;
        let mut at = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for glyph in glyphs {
            at = [
                at[0].min(glyph.x),
                at[1].min(glyph.y),
                at[2].max(glyph.x),
                at[3].max(glyph.y),
            ];
        }
        if at[0] > at[2] {
            // No glyphs: an empty box, which reaches nowhere.
            return Some([f32::MAX, f32::MAX, f32::MIN, f32::MIN]);
        }
        let (scale, em) = (self.scale, size * self.scale);
        // Outlines grow upwards in the font and downwards on the page.
        Some([
            origin_x + at[0] * scale + (x_min - 1.0) * em,
            origin_y + at[1] * scale - (y_max + 1.0) * em,
            origin_x + at[2] * scale + (x_max + 1.0) * em,
            origin_y + at[3] * scale - (y_min - 1.0) * em,
        ])
    }

    /// The box around every glyph of a font (the head table's, which font
    /// tools keep up to date), in ems.
    fn font_box(&mut self, font: &parley::FontData) -> Option<[f32; 4]> {
        *self
            .font_boxes
            .entry((font.data.id(), font.index))
            .or_insert_with(|| {
                use skrifa::raw::TableProvider as _;
                let font_ref = skrifa::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
                let head = font_ref.head().ok()?;
                let em = f32::from(head.units_per_em());
                (em > 0.0).then(|| {
                    [head.x_min(), head.y_min(), head.x_max(), head.y_max()]
                        .map(|v| f32::from(v) / em)
                })
            })
    }

    fn glyph_path(
        &mut self,
        font: &parley::FontData,
        glyph: u32,
        size: f32,
        coords: &[i16],
    ) -> Option<Rc<Path>> {
        let key = (font.data.id(), font.index, glyph, size.to_bits());
        if let Some(cached) = self.glyphs.get(&key) {
            return cached.clone();
        }
        let path = (|| {
            let font_ref = skrifa::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
            let outline = font_ref.outline_glyphs().get(skrifa::GlyphId::new(glyph))?;
            let coords: Vec<skrifa::instance::NormalizedCoord> = coords
                .iter()
                .map(|c| skrifa::instance::NormalizedCoord::from_bits(*c))
                .collect();
            let mut pen = PathPen {
                builder: PathBuilder::new(),
            };
            outline
                .draw(
                    skrifa::outline::DrawSettings::unhinted(
                        skrifa::instance::Size::new(size),
                        skrifa::instance::LocationRef::new(&coords),
                    ),
                    &mut pen,
                )
                .ok()?;
            pen.builder.finish()
        })()
        .map(Rc::new);
        self.glyphs.insert(key, path.clone());
        path
    }
}

/// Builds a tiny-skia path from a glyph outline, flipping y so that the
/// font's upwards y grows downwards on the page.
pub(crate) struct PathPen {
    pub(crate) builder: PathBuilder,
}

impl skrifa::outline::OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(x, -y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(x, -y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder.quad_to(cx0, -cy0, x, -y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_to(cx0, -cy0, cx1, -cy1, x, -y);
    }
    fn close(&mut self) {
        self.builder.close();
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    use catpaw_dom::html::HtmlParseOptions;
    use catpaw_dom::parse_html;
    use catpaw_layout::{BuildInput, Viewport};
    use catpaw_style::{StyleEngine, StyleOptions};

    use super::*;

    /// The system allocator, counting on each thread the allocations of at
    /// least a given size.
    struct Counting;

    thread_local! {
        /// The size counted from, and the count.
        static LARGE: Cell<(usize, usize)> = const { Cell::new((usize::MAX, 0)) };
    }

    fn note(size: usize) {
        let _ = LARGE.try_with(|large| {
            let (from, count) = large.get();
            if size >= from {
                large.set((from, count + 1));
            }
        });
    }

    // SAFETY: every call goes straight to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            note(size);
            unsafe { System.realloc(ptr, layout, size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// What `f` returns, and how many allocations of `bytes` or more it
    /// made.
    fn allocations_of<T>(bytes: usize, f: impl FnOnce() -> T) -> (T, usize) {
        LARGE.with(|large| large.set((bytes, 0)));
        let out = f();
        let (_, count) = LARGE.with(|large| large.replace((usize::MAX, 0)));
        (out, count)
    }

    struct Page {
        dom: Dom,
        tree: LayoutTree,
    }

    fn lay_out(html: &str, width: u32, height: u32) -> Page {
        let result = parse_html(html, &HtmlParseOptions::default());
        let mut engine = StyleEngine::new(&StyleOptions {
            viewport_width: width as f32,
            viewport_height: height as f32,
            ..StyleOptions::default()
        });
        engine.set_quirks_mode(result.dom.quirks_mode());
        engine.restyle(&result.dom);
        let fonts = catpaw_text::shared_fonts();
        let tree = LayoutTree::build(BuildInput {
            dom: &result.dom,
            styles: &engine,
            fonts: &fonts,
            viewport: Viewport {
                width: width as f32,
                height: height as f32,
            },
            scroll_offsets: &HashMap::new(),
        });
        Page {
            dom: result.dom,
            tree,
        }
    }

    fn render_page(page: &Page, width: u32, height: u32, scroll_y: f32) -> (Pixmap, Stats) {
        let options = Options {
            width,
            height,
            scroll: (0.0, scroll_y),
            scale: 1.0,
        };
        render_counting(&page.tree, &page.dom, &options, &|_| None)
    }

    fn words(n: usize) -> String {
        let words = ["glyph", "clip", "edge", "mask", "gjpqy"];
        (0..n)
            .map(|i| words[i % words.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Pixels inside the rectangle that are not near white.
    fn ink(pixmap: &Pixmap, x: std::ops::Range<u32>, y: std::ops::Range<u32>) -> usize {
        y.flat_map(|y| x.clone().map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = pixmap.pixel(x, y).expect("inside the pixmap");
                p.red() < 128 || p.green() < 128 || p.blue() < 128
            })
            .count()
    }

    #[test]
    fn text_under_a_clip_shares_one_mask() {
        // Thousands of glyphs in a clip with fractional edges; the text
        // overflows it, so some glyphs cross its edge.
        let html = format!(
            r#"<!doctype html><body style="margin:0;font:14px/18px sans-serif"><div style="margin:10.25px;width:301.5px;height:200.5px;overflow:hidden">{}</div>"#,
            words(1500)
        );
        let page = lay_out(&html, 400, 300);
        let ((pixmap, stats), large) =
            allocations_of(400 * 300, || render_page(&page, 400, 300, 0.0));
        assert!(stats.drawn > 100, "{stats:?}");
        assert!(stats.culled > 0, "{stats:?}");
        // Only glyphs on the clip's edge have it cut into the mask; the
        // rest go through the same opaque buffer.
        assert!(
            stats.masked > 0 && stats.masked * 4 < stats.drawn,
            "{stats:?}"
        );
        assert_eq!(stats.mask_cuts, stats.masked, "{stats:?}");
        assert_eq!(stats.masks_made, 1, "{stats:?}");
        // The pixmap and the one mask: a mask copied per glyph, or made per
        // clip, would show here.
        assert!(
            (1..=2).contains(&large),
            "{large} allocations as big as a mask"
        );
        assert!(ink(&pixmap, 10..312, 10..211) > 1000);
        assert_eq!(ink(&pixmap, 313..400, 0..300), 0, "clipped on the right");
        assert_eq!(ink(&pixmap, 0..400, 212..300), 0, "clipped below");
    }

    #[test]
    fn only_what_reaches_the_output_is_drawn() {
        let html = format!(
            r#"<!doctype html><body style="margin:0;font:14px/18px sans-serif">{}"#,
            (0..150)
                .map(|_| format!("<p>{}</p>", words(30)))
                .collect::<String>()
        );
        let page = lay_out(&html, 400, 300);
        let height = page
            .tree
            .root()
            .map(|r| page.tree.scroll_metrics(r).scroll_height)
            .expect("a root box")
            .ceil() as u32;
        let (_, whole) = render_page(&page, 400, height, 0.0);
        let (pixmap, view) = render_page(&page, 400, 300, 2000.0);
        assert!(whole.drawn > 10_000, "{whole:?}");
        assert!(
            view.drawn > 100 && view.drawn * 20 < whole.drawn,
            "{view:?} of {whole:?}"
        );
        assert!(ink(&pixmap, 0..400, 0..300) > 1000);
    }

    #[test]
    fn only_canvases_that_can_show_are_asked_for() {
        let page = lay_out(
            r#"<!doctype html><body style="margin:0"><canvas width="40" height="30"></canvas><div style="height:2000px"></div><canvas width="40" height="30"></canvas>"#,
            400,
            300,
        );
        let asked = Cell::new(0);
        let replaced = |_| {
            asked.set(asked.get() + 1);
            let mut bitmap = Pixmap::new(40, 30).expect("a bitmap");
            bitmap.fill(tiny_skia::Color::BLACK);
            Some(bitmap)
        };
        let options = Options {
            width: 400,
            height: 300,
            scroll: (0.0, 0.0),
            scale: 1.0,
        };
        let pixmap = render_with(&page.tree, &page.dom, &options, &replaced);
        assert_eq!(asked.get(), 1);
        assert!(ink(&pixmap, 0..40, 0..30) > 1000);
    }

    #[test]
    fn nothing_is_visited_inside_a_clip_that_hides_everything() {
        let html = format!(
            r#"<!doctype html><body style="margin:0"><div style="height:0;overflow:hidden">{0}</div><div style="width:0;overflow:hidden">{0}</div><div style="position:absolute;top:5000px;height:50px;overflow:hidden">{0}</div>"#,
            words(2000)
        );
        let page = lay_out(&html, 400, 300);
        let (pixmap, stats) = render_page(&page, 400, 300, 0.0);
        assert_eq!(stats.drawn, 0, "{stats:?}");
        assert!(stats.culled < 10, "{stats:?}");
        assert_eq!(ink(&pixmap, 0..400, 0..300), 0);
    }

    #[test]
    fn a_cut_clip_is_mended() {
        // White text crossing a small clip's edge leaves no ink but has
        // that clip cut into the mask; text drawn later under another clip
        // must not be cut by it.
        let later = r#"<div style="position:absolute;left:0;top:0;width:300px;height:100px;overflow:hidden;font:20px sans-serif">MMMMMMMMMMMM MMMMMMMMMMMM</div>"#;
        let first = r#"<div style="position:absolute;left:0;top:0;width:60.5px;height:30.5px;overflow:hidden;white-space:nowrap;font:20px sans-serif;color:#fff">MMMMMMMMMMMM MMMMMMMMMMMM</div>"#;
        let both = lay_out(
            &format!(r#"<!doctype html><body style="margin:0">{first}{later}"#),
            300,
            100,
        );
        let alone = lay_out(
            &format!(r#"<!doctype html><body style="margin:0">{later}"#),
            300,
            100,
        );
        let (with_cut, stats) = render_page(&both, 300, 100, 0.0);
        let (without, _) = render_page(&alone, 300, 100, 0.0);
        assert!(stats.mask_cuts > 0, "{stats:?}");
        assert!(ink(&without, 0..300, 0..100) > 500);
        assert!(with_cut.data() == without.data());
    }
}
