//! Form controls: what they hold, drawn inside their boxes.
//!
//! A control's value lives in the page, not in the tree, so the page says
//! what each one shows ([`ControlFace`]); the box around it comes from the
//! style sheets like any other. Text is shaped in the control's font at
//! paint time. The focused text field, list or chooser gets a ring and a
//! caret, as a browser shows one (the page's own `:focus` styles do not
//! apply yet), so that someone driving the page from a screenshot sees
//! where their typing goes.

use catpaw_layout::{BoxId, Rect};
use catpaw_text::Brush;
use catpaw_text::parley::{self, PositionedLayoutItem};
use style::properties::ComputedValues;
use tiny_skia::{FillRule, LineCap, LineJoin, PathBuilder, Stroke, Transform};

use crate::{Painter, Rgba, text_color};

/// What a form control shows inside its box.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlFace {
    /// A one-line field: its value (a password already masked), or its
    /// placeholder when it is empty.
    Field { text: String, placeholder: bool },
    /// A `textarea`: its value, or its placeholder, broken into lines.
    Area { text: String, placeholder: bool },
    /// A button made from an `input` (`submit`, `reset`, `button`): its
    /// label.
    Button { label: String },
    /// A checkbox or a radio button.
    Check { radio: bool, checked: bool },
    /// A drop-down list: the label of the option it shows.
    DropDown { label: String },
    /// A list box (`multiple`, or more than one row): each option's label
    /// and whether it is selected.
    ListBox { options: Vec<(String, bool)> },
    /// A file chooser: what it says beside its button (the chosen files'
    /// names, or that none is chosen).
    File { label: String },
    /// A `progress`, `meter` or range input: how full it is, `None` for a
    /// progress bar with no value.
    Gauge { kind: Gauge, fraction: Option<f32> },
    /// A colour input: its colour.
    Color { rgb: [u8; 3] },
}

/// Which kind of gauge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gauge {
    Progress,
    Meter,
    Range,
}

/// What form controls show, by element: answered by the page.
pub type ControlFaces<'a> = &'a dyn Fn(catpaw_dom::NodeId) -> Option<ControlFace>;

const BORDER: Rgba = Rgba([0.463, 0.463, 0.463, 1.0]);
const ACCENT: Rgba = Rgba([0.0, 0.459, 1.0, 1.0]);
const FOCUS_RING: Rgba = Rgba([0.063, 0.388, 0.847, 1.0]);
const PLACEHOLDER: Rgba = Rgba([0.459, 0.459, 0.459, 1.0]);
const BUTTON_FACE: Rgba = Rgba([0.937, 0.937, 0.937, 1.0]);
const WHITE: Rgba = Rgba([1.0, 1.0, 1.0, 1.0]);
const SELECTED_ROW: Rgba = Rgba([0.808, 0.808, 0.808, 1.0]);
const METER_FILL: Rgba = Rgba([0.063, 0.486, 0.063, 1.0]);

/// Room kept at the right of a drop-down for its arrow, in CSS pixels.
const ARROW_ROOM: f32 = 18.0;

/// The text to shape for a field: an empty one still has a line, for the
/// caret to take its height from.
fn visible_text(text: &str) -> &str {
    if text.is_empty() { "\u{200B}" } else { text }
}

impl Painter<'_> {
    /// Draws what the control `id` shows inside `content` (its content box)
    /// and `padding` (its padding box), both in output coordinates.
    pub(crate) fn paint_control(
        &mut self,
        id: BoxId,
        face: &ControlFace,
        content: Rect,
        padding: Rect,
        focused: bool,
    ) {
        let style = self.tree.get(id).style.clone();
        let outer_clip = self.clip;
        // What a control holds stays inside it.
        self.clip = self.intersect_clip(padding).or(Some(Rect::default()));
        match face {
            ControlFace::Field { text, placeholder } => {
                self.paint_field_text(&style, text, *placeholder, content, focused);
            }
            ControlFace::Area { text, placeholder } => {
                let color = if *placeholder {
                    PLACEHOLDER
                } else {
                    text_color(&style)
                };
                let layout = catpaw_layout::shape_control_text(
                    &style,
                    visible_text(text),
                    Some((content.width / self.scale).max(1.0)),
                );
                self.paint_layout(&layout, content.x, content.y, color);
                if focused {
                    let (x, y, height) =
                        self.caret_after(&layout, content.x, content.y, *placeholder);
                    self.paint_caret(x, y, height, text_color(&style));
                }
            }
            ControlFace::Button { label } => {
                let layout = catpaw_layout::shape_control_text(&style, label, None);
                let (x, y) = self.centered(&layout, content);
                self.paint_layout(&layout, x, y, text_color(&style));
            }
            ControlFace::Check { radio, checked } => {
                // The box is the content box, or the border box when the
                // page gives it no room inside.
                let area = if content.width >= 2.0 && content.height >= 2.0 {
                    content
                } else {
                    padding
                };
                self.paint_check(area, *radio, *checked);
            }
            ControlFace::DropDown { label } => {
                let room = ARROW_ROOM * self.scale;
                let text_box = Rect::new(
                    content.x,
                    content.y,
                    (content.width - room).max(0.0),
                    content.height,
                );
                let layout = catpaw_layout::shape_control_text(&style, label, None);
                let y = text_box.y + (text_box.height - layout.height() * self.scale) / 2.0;
                let label_clip = self.clip;
                self.clip = self.intersect_clip(text_box).or(Some(Rect::default()));
                self.paint_layout(&layout, text_box.x, y, text_color(&style));
                self.clip = label_clip;
                self.paint_arrow(content, text_color(&style));
            }
            ControlFace::ListBox { options } => {
                let mut y = content.y;
                for (label, selected) in options {
                    let layout = catpaw_layout::shape_control_text(&style, label, None);
                    let row = layout.height() * self.scale;
                    if *selected {
                        self.fill_rect(Rect::new(content.x, y, content.width, row), SELECTED_ROW);
                    }
                    self.paint_layout(&layout, content.x + 2.0 * self.scale, y, text_color(&style));
                    y += row;
                    if y > content.bottom() {
                        break;
                    }
                }
            }
            ControlFace::File { label } => {
                let button = catpaw_layout::shape_control_text(&style, "Choose File", None);
                let width = (button.width() + 12.0) * self.scale;
                let face = Rect::new(content.x, content.y, width, content.height);
                self.fill_rect(face, BUTTON_FACE);
                self.frame(face, self.scale, BORDER);
                let (x, y) = self.centered(&button, face);
                self.paint_layout(&button, x, y, text_color(&style));
                let layout = catpaw_layout::shape_control_text(&style, label, None);
                let y = content.y + (content.height - layout.height() * self.scale) / 2.0;
                self.paint_layout(
                    &layout,
                    face.right() + 4.0 * self.scale,
                    y,
                    text_color(&style),
                );
            }
            ControlFace::Gauge { kind, fraction } => self.paint_gauge(content, *kind, *fraction),
            ControlFace::Color { rgb } => {
                let inset = 4.0 * self.scale;
                let swatch = Rect::new(
                    content.x + inset,
                    content.y + inset,
                    (content.width - 2.0 * inset).max(0.0),
                    (content.height - 2.0 * inset).max(0.0),
                );
                let [r, g, b] = rgb.map(|c| f32::from(c) / 255.0);
                self.fill_rect(swatch, Rgba([r, g, b, 1.0]));
                self.frame(swatch, self.scale, BORDER);
            }
        }
        self.clip = outer_clip;
    }

    /// A field's text on one line, centred vertically; once it outgrows the
    /// field, a focused field shows its end, where the caret is.
    fn paint_field_text(
        &mut self,
        style: &ComputedValues,
        text: &str,
        placeholder: bool,
        content: Rect,
        focused: bool,
    ) {
        let layout = catpaw_layout::shape_control_text(style, visible_text(text), None);
        let width = layout.width() * self.scale;
        let caret_room = 2.0 * self.scale;
        let x = if focused && !placeholder && width + caret_room > content.width {
            content.right() - width - caret_room
        } else {
            content.x
        };
        let y = content.y + (content.height - layout.height() * self.scale) / 2.0;
        let color = if placeholder {
            PLACEHOLDER
        } else {
            text_color(style)
        };
        self.paint_layout(&layout, x, y, color);
        if focused {
            let (cx, cy, height) = self.caret_after(&layout, x, y, placeholder);
            self.paint_caret(cx, cy, height, text_color(style));
        }
    }

    /// Where the caret goes after the text of `layout` drawn at `(x, y)`:
    /// its position and height, in output coordinates. Before a
    /// placeholder, it sits at the start.
    fn caret_after(
        &self,
        layout: &parley::Layout<Brush>,
        x: f32,
        y: f32,
        placeholder: bool,
    ) -> (f32, f32, f32) {
        let scale = self.scale;
        let mut at = (x, y, layout.height() * scale);
        if placeholder {
            if let Some(line) = layout.lines().next() {
                at.2 = line.metrics().line_height * scale;
            }
            return at;
        }
        if let Some(line) = layout.lines().last() {
            let metrics = line.metrics();
            let advance: f32 = line
                .items()
                .map(|item| match item {
                    PositionedLayoutItem::GlyphRun(run) => run.offset() + run.advance(),
                    PositionedLayoutItem::InlineBox(b) => b.x + b.width,
                })
                .fold(0.0, f32::max);
            at = (
                x + advance * scale,
                y + metrics.block_min_coord * scale,
                metrics.line_height * scale,
            );
        }
        at
    }

    fn paint_caret(&mut self, x: f32, y: f32, height: f32, color: Rgba) {
        let width = self.scale.max(1.0);
        self.fill_rect(Rect::new(x, y, width, height), color);
    }

    /// Where to put `layout` to centre it in `area`.
    fn centered(&self, layout: &parley::Layout<Brush>, area: Rect) -> (f32, f32) {
        (
            area.x + (area.width - layout.width() * self.scale) / 2.0,
            area.y + (area.height - layout.height() * self.scale) / 2.0,
        )
    }

    /// Draws the glyphs of a shaped layout whose top-left corner is at
    /// `(x, y)` (output coordinates), in one colour.
    pub(crate) fn paint_layout(
        &mut self,
        layout: &parley::Layout<Brush>,
        x: f32,
        y: f32,
        color: Rgba,
    ) {
        if color.is_transparent() {
            return;
        }
        let scale = self.scale;
        let paint = color.paint();
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let font = run.run().font();
                let size = run.run().font_size();
                let coords = run.run().normalized_coords();
                for glyph in run.positioned_glyphs() {
                    let Some(path) = self.glyph_path(font, glyph.id, size, coords) else {
                        continue;
                    };
                    let transform =
                        Transform::from_translate(x + glyph.x * scale, y + glyph.y * scale)
                            .pre_scale(scale, scale);
                    let bounds = crate::transformed_bounds(path.bounds(), transform);
                    self.draw_clipped(bounds, |pixmap, mask| {
                        pixmap.fill_path(&path, &paint, FillRule::Winding, transform, mask);
                    });
                }
            }
        }
    }

    /// A rectangle's outline, `width` output pixels thick, inside it.
    fn frame(&mut self, rect: Rect, width: f32, color: Rgba) {
        let w = width.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
        if w <= 0.0 {
            return;
        }
        self.fill_rect(Rect::new(rect.x, rect.y, rect.width, w), color);
        self.fill_rect(Rect::new(rect.x, rect.bottom() - w, rect.width, w), color);
        self.fill_rect(
            Rect::new(rect.x, rect.y + w, w, rect.height - 2.0 * w),
            color,
        );
        self.fill_rect(
            Rect::new(rect.right() - w, rect.y + w, w, rect.height - 2.0 * w),
            color,
        );
    }

    /// The ring a focused control shows around its border box.
    pub(crate) fn paint_focus_ring(&mut self, border_box: Rect) {
        let width = 2.0 * self.scale;
        let ring = Rect::new(
            border_box.x - width,
            border_box.y - width,
            border_box.width + 2.0 * width,
            border_box.height + 2.0 * width,
        );
        self.frame(ring, width, FOCUS_RING);
    }

    fn paint_check(&mut self, area: Rect, radio: bool, checked: bool) {
        let size = area.width.min(area.height);
        if size <= 0.0 {
            return;
        }
        let x = area.x + (area.width - size) / 2.0;
        let y = area.y + (area.height - size) / 2.0;
        let line = self.scale.max(1.0);
        let bounds = [x - line, y - line, x + size + line, y + size + line];
        let (fill, edge) = if checked && !radio {
            (ACCENT, ACCENT)
        } else if checked {
            (WHITE, ACCENT)
        } else {
            (WHITE, BORDER)
        };
        let outline = if radio {
            PathBuilder::from_circle(x + size / 2.0, y + size / 2.0, (size - line) / 2.0)
        } else {
            tiny_skia::Rect::from_xywh(x + line / 2.0, y + line / 2.0, size - line, size - line)
                .map(PathBuilder::from_rect)
        };
        let Some(outline) = outline else {
            return;
        };
        let stroke = Stroke {
            width: line,
            ..Stroke::default()
        };
        self.draw_clipped(bounds, |pixmap, mask| {
            pixmap.fill_path(
                &outline,
                &fill.paint(),
                FillRule::Winding,
                Transform::identity(),
                mask,
            );
            pixmap.stroke_path(
                &outline,
                &edge.paint(),
                &stroke,
                Transform::identity(),
                mask,
            );
        });
        if !checked {
            return;
        }
        if radio {
            if let Some(dot) = PathBuilder::from_circle(x + size / 2.0, y + size / 2.0, size * 0.27)
            {
                self.draw_clipped(bounds, |pixmap, mask| {
                    pixmap.fill_path(
                        &dot,
                        &ACCENT.paint(),
                        FillRule::Winding,
                        Transform::identity(),
                        mask,
                    );
                });
            }
            return;
        }
        let mut tick = PathBuilder::new();
        tick.move_to(x + size * 0.22, y + size * 0.52);
        tick.line_to(x + size * 0.42, y + size * 0.72);
        tick.line_to(x + size * 0.78, y + size * 0.3);
        let Some(tick) = tick.finish() else {
            return;
        };
        let stroke = Stroke {
            width: (size * 0.14).max(line),
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        self.draw_clipped(bounds, |pixmap, mask| {
            pixmap.stroke_path(&tick, &WHITE.paint(), &stroke, Transform::identity(), mask);
        });
    }

    /// The arrow at the right of a drop-down.
    fn paint_arrow(&mut self, content: Rect, color: Rgba) {
        let s = self.scale;
        let cx = content.right() - (ARROW_ROOM / 2.0) * s;
        let cy = content.y + content.height / 2.0;
        let mut arrow = PathBuilder::new();
        arrow.move_to(cx - 4.0 * s, cy - 2.0 * s);
        arrow.line_to(cx + 4.0 * s, cy - 2.0 * s);
        arrow.line_to(cx, cy + 3.0 * s);
        arrow.close();
        let Some(arrow) = arrow.finish() else {
            return;
        };
        let bounds = [cx - 5.0 * s, cy - 3.0 * s, cx + 5.0 * s, cy + 4.0 * s];
        self.draw_clipped(bounds, |pixmap, mask| {
            pixmap.fill_path(
                &arrow,
                &color.paint(),
                FillRule::Winding,
                Transform::identity(),
                mask,
            );
        });
    }

    fn paint_gauge(&mut self, content: Rect, kind: Gauge, fraction: Option<f32>) {
        let s = self.scale;
        match kind {
            Gauge::Range => {
                let track_height = 4.0 * s;
                let cy = content.y + content.height / 2.0;
                let track = Rect::new(
                    content.x,
                    cy - track_height / 2.0,
                    content.width,
                    track_height,
                );
                self.fill_rect(track, BUTTON_FACE);
                self.frame(track, s, BORDER);
                let fraction = fraction.unwrap_or(0.5).clamp(0.0, 1.0);
                let radius = (content.height / 2.0).min(8.0 * s);
                let cx = content.x + radius + (content.width - 2.0 * radius) * fraction;
                self.fill_rect(
                    Rect::new(track.x, track.y, (cx - track.x).max(0.0), track.height),
                    ACCENT,
                );
                if let Some(thumb) = PathBuilder::from_circle(cx, cy, radius) {
                    let bounds = [cx - radius, cy - radius, cx + radius, cy + radius];
                    self.draw_clipped(bounds, |pixmap, mask| {
                        pixmap.fill_path(
                            &thumb,
                            &ACCENT.paint(),
                            FillRule::Winding,
                            Transform::identity(),
                            mask,
                        );
                    });
                }
            }
            Gauge::Progress | Gauge::Meter => {
                self.fill_rect(content, BUTTON_FACE);
                self.frame(content, s, BORDER);
                if let Some(fraction) = fraction {
                    let fill = if kind == Gauge::Meter {
                        METER_FILL
                    } else {
                        ACCENT
                    };
                    let inner = Rect::new(
                        content.x + s,
                        content.y + s,
                        ((content.width - 2.0 * s) * fraction.clamp(0.0, 1.0)).max(0.0),
                        (content.height - 2.0 * s).max(0.0),
                    );
                    self.fill_rect(inner, fill);
                }
            }
        }
    }
}
