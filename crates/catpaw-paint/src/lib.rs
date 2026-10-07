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

use std::collections::HashMap;

use catpaw_dom::{Dom, NodeId};
use catpaw_layout::{BoxId, BoxKind, LayoutTree, Positioning, Rect};
use catpaw_text::parley::{self, PositionedLayoutItem};
use catpaw_text::skrifa::{self, MetadataProvider as _};
use style::color::{AbsoluteColor, ColorSpace};
use style::properties::ComputedValues;
use style::values::computed::TextDecorationLine;
use style::values::specified::box_::Overflow;
use tiny_skia::{FillRule, Mask, Paint, PathBuilder, Pixmap, Transform};

pub use tiny_skia;

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

/// Paints the tree into a pixmap of `options.width × options.height` CSS
/// pixels (times the scale), white where nothing is drawn.
pub fn render(tree: &LayoutTree, dom: &Dom, options: &Options) -> Pixmap {
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
        masks: HashMap::new(),
        glyphs: HashMap::new(),
    };
    painter.paint_canvas();
    if let Some(root) = tree.root() {
        painter.paint_box(root);
    }
    for id in tree.viewport_positioned() {
        painter.paint_box(*id);
    }
    pixmap
}

/// `render`, encoded as PNG.
pub fn render_png(tree: &LayoutTree, dom: &Dom, options: &Options) -> Vec<u8> {
    render(tree, dom, options)
        .encode_png()
        .expect("PNG encoding of an in-memory pixmap")
}

struct Painter<'a> {
    pixmap: &'a mut Pixmap,
    tree: &'a LayoutTree,
    dom: &'a Dom,
    scroll: (f32, f32),
    scale: f32,
    /// The clip in output coordinates, if any box above clips.
    clip: Option<Rect>,
    masks: HashMap<[i32; 4], Mask>,
    /// Glyph outlines at a size, by font, glyph and size.
    glyphs: HashMap<(u64, u32, u32, u32), Option<tiny_skia::Path>>,
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

    fn fill_rect(&mut self, rect: Rect, color: Rgba) {
        if color.is_transparent() {
            return;
        }
        let Some(rect) = self.intersect_clip(rect) else {
            return;
        };
        let Some(rect) = tiny_skia::Rect::from_xywh(rect.x, rect.y, rect.width, rect.height) else {
            return;
        };
        self.pixmap
            .fill_rect(rect, &color.paint(), Transform::identity(), None);
    }

    /// A mask for the current clip, made once per distinct clip.
    fn clip_mask(&mut self) -> Option<&Mask> {
        let clip = self.clip?;
        let key = [
            clip.x.floor() as i32,
            clip.y.floor() as i32,
            clip.right().ceil() as i32,
            clip.bottom().ceil() as i32,
        ];
        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        Some(self.masks.entry(key).or_insert_with(|| {
            let mut mask = Mask::new(width, height).expect("a mask the size of the pixmap");
            if let Some(rect) = tiny_skia::Rect::from_xywh(clip.x, clip.y, clip.width, clip.height)
            {
                let path = PathBuilder::from_rect(rect);
                mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
            }
            mask
        }))
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
        let b = self.tree.get(id);
        let style = b.style.clone();
        let visible =
            style.get_inherited_box().visibility == style::computed_values::visibility::T::Visible;
        let fixed = self.tree.fixed_ancestor(id).is_some();
        let border_box = self.to_output(self.tree.border_box(id), fixed);
        let padding_box = self.to_output(self.tree.padding_box(id), fixed);
        // Propagated to the canvas already.
        let is_root = Some(id) == self.tree.root();
        let is_body = b.node.is_some_and(|n| self.dom.is_html_element(n, "body"))
            && self
                .tree
                .root()
                .is_some_and(|r| background_color(&self.tree.get(r).style).is_transparent());
        if visible && !is_root && !is_body {
            self.fill_rect(border_box, background_color(&style));
        }
        if visible {
            self.paint_borders(id, border_box, padding_box, &style);
        }

        let clips = {
            let overflow = style.get_box();
            overflow.overflow_x != Overflow::Visible || overflow.overflow_y != Overflow::Visible
        };
        let outer_clip = self.clip;
        if clips {
            self.clip = self.intersect_clip(padding_box).or(Some(Rect::default()));
        }

        if visible && b.kind == BoxKind::InlineRoot {
            let content = self.to_output(self.tree.content_box(id), fixed);
            self.paint_inline(id, content.x, content.y);
        }
        let mut children: Vec<BoxId> = self.tree.children(id).to_vec();
        children.sort_by_key(|c| self.tree.get(*c).positioning != Positioning::Static);
        for child in children {
            self.paint_box(child);
        }
        self.clip = outer_clip;
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
        let Some(layout) = self.tree.inline_layout(id) else {
            return;
        };
        let root_node = self.tree.element_of(id);
        let root_style = self.tree.get(id).style.clone();
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let node = LayoutTree::node_of_brush(run.style().brush);
                let style = self
                    .tree
                    .inline_style(node)
                    .cloned()
                    .unwrap_or_else(|| root_style.clone());
                if style.get_inherited_box().visibility
                    != style::computed_values::visibility::T::Visible
                {
                    continue;
                }
                let color = text_color(&style);
                if color.is_transparent() {
                    continue;
                }
                let font = run.run().font().clone();
                let size = run.run().font_size();
                let coords: Vec<skrifa::instance::NormalizedCoord> = run
                    .run()
                    .normalized_coords()
                    .iter()
                    .map(|c| skrifa::instance::NormalizedCoord::from_bits(*c))
                    .collect();
                let paint = color.paint();
                let scale = self.scale;
                let glyphs: Vec<parley::Glyph> = run.positioned_glyphs().collect();
                for glyph in glyphs {
                    let Some(path) = self.glyph_path(&font, glyph.id, size, &coords) else {
                        continue;
                    };
                    let transform = Transform::from_translate(
                        origin_x + glyph.x * scale,
                        origin_y + glyph.y * scale,
                    )
                    .pre_scale(scale, scale);
                    let mask = self.clip_mask().cloned();
                    self.pixmap.fill_path(
                        &path,
                        &paint,
                        FillRule::Winding,
                        transform,
                        mask.as_ref(),
                    );
                }
                if let Some(root) = root_node
                    && self.underlined(node, root)
                {
                    let metrics = run.run().metrics();
                    let thickness = (size / 14.0).max(1.0) * scale;
                    let underline = Rect::new(
                        origin_x + run.offset() * scale,
                        origin_y + (run.baseline() + metrics.descent * 0.35) * scale,
                        run.advance() * scale,
                        thickness,
                    );
                    self.fill_rect(underline, color);
                }
            }
        }
    }

    fn glyph_path(
        &mut self,
        font: &parley::FontData,
        glyph: u32,
        size: f32,
        coords: &[skrifa::instance::NormalizedCoord],
    ) -> Option<tiny_skia::Path> {
        let key = (font.data.id(), font.index, glyph, size.to_bits());
        if let Some(cached) = self.glyphs.get(&key) {
            return cached.clone();
        }
        let path = (|| {
            let font_ref = skrifa::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
            let outline = font_ref.outline_glyphs().get(skrifa::GlyphId::new(glyph))?;
            let mut pen = PathPen {
                builder: PathBuilder::new(),
            };
            outline
                .draw(
                    skrifa::outline::DrawSettings::unhinted(
                        skrifa::instance::Size::new(size),
                        skrifa::instance::LocationRef::new(coords),
                    ),
                    &mut pen,
                )
                .ok()?;
            pen.builder.finish()
        })();
        self.glyphs.insert(key, path.clone());
        path
    }
}

/// Builds a tiny-skia path from a glyph outline, flipping y so that the
/// font's upwards y grows downwards on the page.
struct PathPen {
    builder: PathBuilder,
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
