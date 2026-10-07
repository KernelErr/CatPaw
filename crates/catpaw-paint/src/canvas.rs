//! A Canvas 2D raster backend over tiny-skia: the state, paths, text and
//! pixels behind `CanvasRenderingContext2D`. The Web API layer maps calls
//! onto this; nothing here knows about the DOM.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use catpaw_text::parley::style::{
    FontFamily, FontFamilyName, FontStyle, FontWeight, GenericFamily, StyleProperty,
};
use catpaw_text::parley::{self, Alignment, AlignmentOptions, PositionedLayoutItem};
use catpaw_text::{Brush, Fonts, shared_fonts, skrifa};
use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, IntRect, LineCap, LineJoin, LinearGradient, Mask,
    Paint, Path, PathBuilder, Pixmap, PixmapPaint, Point, RadialGradient, Shader, SpreadMode,
    Stroke, StrokeDash, Transform,
};

/// Parses a CSS color as canvas styles take them; `None` for what is not
/// a color.
pub fn parse_color(css: &str) -> Option<Color> {
    let c = csscolorparser::parse(css.trim()).ok()?;
    Color::from_rgba(
        c.r.clamp(0.0, 1.0),
        c.g.clamp(0.0, 1.0),
        c.b.clamp(0.0, 1.0),
        c.a.clamp(0.0, 1.0),
    )
}

/// A color as `fillStyle` reports it: `#rrggbb`, or `rgba(r, g, b, a)`
/// when not opaque.
pub fn serialize_color(color: Color) -> String {
    let c = color.to_color_u8();
    if c.alpha() == 255 {
        format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
    } else {
        let alpha = (f32::from(c.alpha()) / 255.0 * 1000.0).round() / 1000.0;
        format!("rgba({}, {}, {}, {})", c.red(), c.green(), c.blue(), alpha)
    }
}

/// A fill or stroke style.
#[derive(Clone, Debug)]
pub enum Style {
    Color(Color),
    Linear {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        stops: Vec<(f32, Color)>,
    },
    Radial {
        x0: f32,
        y0: f32,
        r0: f32,
        x1: f32,
        y1: f32,
        r1: f32,
        stops: Vec<(f32, Color)>,
    },
    /// Drawn as its last stop: tiny-skia has no conic gradient.
    Conic {
        angle: f32,
        x: f32,
        y: f32,
        stops: Vec<(f32, Color)>,
    },
}

/// The font a context draws text with, from the `font` shorthand.
#[derive(Clone, Debug)]
pub struct Font {
    pub families: Vec<FontFamilyName<'static>>,
    pub size: f32,
    pub weight: f32,
    pub italic: bool,
    /// The shorthand as `font` reports it.
    pub css: String,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            families: vec![FontFamilyName::Generic(GenericFamily::SansSerif)],
            size: 10.0,
            weight: 400.0,
            italic: false,
            css: "10px sans-serif".to_string(),
        }
    }
}

impl Font {
    /// Reads `[style] [variant] [weight] size[/line-height] family, ...`;
    /// `None` when the shorthand is not understood (the context keeps its
    /// font then).
    pub fn parse(css: &str) -> Option<Self> {
        let mut italic = false;
        let mut weight = 400.0f32;
        let mut size = None;
        let mut rest = String::new();
        let mut tokens = css.split_whitespace();
        for token in tokens.by_ref() {
            let lower = token.to_ascii_lowercase();
            match lower.as_str() {
                "normal" | "small-caps" => {}
                "italic" | "oblique" => italic = true,
                "bold" | "bolder" => weight = 700.0,
                "lighter" => weight = 300.0,
                _ if lower.chars().all(|c| c.is_ascii_digit()) && lower.len() == 3 => {
                    weight = lower.parse().ok()?;
                }
                _ => {
                    let size_part = lower.split('/').next().unwrap_or(&lower);
                    size = Some(parse_length(size_part)?);
                    break;
                }
            }
        }
        let size = size?;
        for token in tokens {
            if !rest.is_empty() {
                rest.push(' ');
            }
            rest.push_str(token);
        }
        if rest.is_empty() {
            return None;
        }
        let families: Vec<FontFamilyName<'static>> = rest
            .split(',')
            .map(|f| f.trim().trim_matches(|c| c == '"' || c == '\''))
            .filter(|f| !f.is_empty())
            .map(|f| match f.to_ascii_lowercase().as_str() {
                "sans-serif" => FontFamilyName::Generic(GenericFamily::SansSerif),
                "serif" => FontFamilyName::Generic(GenericFamily::Serif),
                "monospace" => FontFamilyName::Generic(GenericFamily::Monospace),
                "cursive" => FontFamilyName::Generic(GenericFamily::Cursive),
                "fantasy" => FontFamilyName::Generic(GenericFamily::Fantasy),
                "system-ui" | "-apple-system" | "blinkmacsystemfont" => {
                    FontFamilyName::Generic(GenericFamily::SystemUi)
                }
                _ => FontFamilyName::Named(std::borrow::Cow::Owned(f.to_string())),
            })
            .collect();
        if families.is_empty() {
            return None;
        }
        let mut canonical = String::new();
        if italic {
            canonical.push_str("italic ");
        }
        if weight != 400.0 {
            if weight == 700.0 {
                canonical.push_str("bold ");
            } else {
                canonical.push_str(&format!("{weight} "));
            }
        }
        canonical.push_str(&format!("{}px ", trim_float(size)));
        let names: Vec<String> = families
            .iter()
            .map(|f| match f {
                FontFamilyName::Named(n) => {
                    if n.contains(' ') {
                        format!("\"{n}\"")
                    } else {
                        n.to_string()
                    }
                }
                FontFamilyName::Generic(g) => match g {
                    GenericFamily::SansSerif => "sans-serif",
                    GenericFamily::Serif => "serif",
                    GenericFamily::Monospace => "monospace",
                    GenericFamily::Cursive => "cursive",
                    GenericFamily::Fantasy => "fantasy",
                    _ => "system-ui",
                }
                .to_string(),
            })
            .collect();
        canonical.push_str(&names.join(", "));
        Some(Self {
            families,
            size,
            weight,
            italic,
            css: canonical,
        })
    }
}

fn trim_float(v: f32) -> String {
    let s = format!("{v}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

fn parse_length(s: &str) -> Option<f32> {
    let (number, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic() || c == '%')?);
    let value: f32 = number.parse().ok()?;
    let px = match unit {
        "px" => value,
        "pt" => value * 4.0 / 3.0,
        "pc" => value * 16.0,
        "em" | "rem" => value * 16.0,
        "%" => value / 100.0 * 16.0,
        "in" => value * 96.0,
        "cm" => value * 96.0 / 2.54,
        "mm" => value * 96.0 / 25.4,
        _ => return None,
    };
    (px >= 0.0).then_some(px)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlign {
    Start,
    End,
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextBaseline {
    Top,
    Hanging,
    Middle,
    Alphabetic,
    Ideographic,
    Bottom,
}

/// The drawing state `save()` and `restore()` keep.
#[derive(Clone)]
pub struct State {
    pub transform: Transform,
    pub fill: Style,
    pub stroke: Style,
    pub line_width: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub miter_limit: f32,
    pub dash: Vec<f32>,
    pub dash_offset: f32,
    pub global_alpha: f32,
    pub blend: BlendMode,
    /// The composite operation as script set it.
    pub composite: String,
    pub clip: Option<Arc<Mask>>,
    pub font: Font,
    pub text_align: TextAlign,
    pub text_baseline: TextBaseline,
    pub direction_rtl: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            transform: Transform::identity(),
            fill: Style::Color(Color::BLACK),
            stroke: Style::Color(Color::BLACK),
            line_width: 1.0,
            line_cap: LineCap::Butt,
            line_join: LineJoin::Miter,
            miter_limit: 10.0,
            dash: Vec::new(),
            dash_offset: 0.0,
            global_alpha: 1.0,
            blend: BlendMode::SourceOver,
            composite: "source-over".to_string(),
            clip: None,
            font: Font::default(),
            text_align: TextAlign::Start,
            text_baseline: TextBaseline::Alphabetic,
            direction_rtl: false,
        }
    }
}

/// `globalCompositeOperation` names tiny-skia can blend.
pub fn blend_mode(name: &str) -> Option<BlendMode> {
    Some(match name {
        "source-over" => BlendMode::SourceOver,
        "source-in" => BlendMode::SourceIn,
        "source-out" => BlendMode::SourceOut,
        "source-atop" => BlendMode::SourceAtop,
        "destination-over" => BlendMode::DestinationOver,
        "destination-in" => BlendMode::DestinationIn,
        "destination-out" => BlendMode::DestinationOut,
        "destination-atop" => BlendMode::DestinationAtop,
        "lighter" => BlendMode::Plus,
        "copy" => BlendMode::Source,
        "xor" => BlendMode::Xor,
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => return None,
    })
}

/// A path in user space, as `Path2D` and the context's current path.
#[derive(Clone, Default)]
pub struct Path2d {
    builder: PathBuilder,
    /// The subpath's first point, for `closePath` and `arcTo`.
    start: Option<(f32, f32)>,
    current: Option<(f32, f32)>,
}

impl Path2d {
    pub fn is_empty(&self) -> bool {
        self.current.is_none()
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        self.builder.move_to(x, y);
        self.start = Some((x, y));
        self.current = Some((x, y));
    }

    fn ensure_start(&mut self, x: f32, y: f32) {
        if self.current.is_none() {
            self.move_to(x, y);
        }
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        if !(x.is_finite() && y.is_finite()) {
            return;
        }
        if self.current.is_none() {
            self.move_to(x, y);
            return;
        }
        self.builder.line_to(x, y);
        self.current = Some((x, y));
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        if ![cx, cy, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        self.ensure_start(cx, cy);
        self.builder.quad_to(cx, cy, x, y);
        self.current = Some((x, y));
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        if ![c1x, c1y, c2x, c2y, x, y].iter().all(|v| v.is_finite()) {
            return;
        }
        self.ensure_start(c1x, c1y);
        self.builder.cubic_to(c1x, c1y, c2x, c2y, x, y);
        self.current = Some((x, y));
    }

    pub fn close(&mut self) {
        if let Some(start) = self.start
            && self.current.is_some()
        {
            self.builder.close();
            self.builder.move_to(start.0, start.1);
            self.current = Some(start);
        }
    }

    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) {
            return;
        }
        self.move_to(x, y);
        self.line_to(x + w, y);
        self.line_to(x + w, y + h);
        self.line_to(x, y + h);
        self.close();
    }

    /// Corner radii in order: top-left, top-right, bottom-right,
    /// bottom-left.
    pub fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radii: [f32; 4]) {
        if ![x, y, w, h].iter().all(|v| v.is_finite()) || radii.iter().any(|r| !r.is_finite()) {
            return;
        }
        let max = (w.abs() / 2.0).min(h.abs() / 2.0);
        let [tl, tr, br, bl] = radii.map(|r| r.max(0.0).min(max));
        let (sx, sy) = (w.signum(), h.signum());
        self.move_to(x + tl * sx, y);
        self.line_to(x + w - tr * sx, y);
        self.arc_corner(x + w - tr * sx, y + tr * sy, tr, -0.5, 0.0, sx, sy);
        self.line_to(x + w, y + h - br * sy);
        self.arc_corner(x + w - br * sx, y + h - br * sy, br, 0.0, 0.5, sx, sy);
        self.line_to(x + bl * sx, y + h);
        self.arc_corner(x + bl * sx, y + h - bl * sy, bl, 0.5, 1.0, sx, sy);
        self.line_to(x, y + tl * sy);
        self.arc_corner(x + tl * sx, y + tl * sy, tl, 1.0, 1.5, sx, sy);
        self.close();
    }

    #[allow(clippy::too_many_arguments)]
    fn arc_corner(&mut self, cx: f32, cy: f32, r: f32, from: f32, to: f32, sx: f32, sy: f32) {
        if r <= 0.0 {
            return;
        }
        let pi = std::f32::consts::PI;
        self.ellipse(cx, cy, r, r, 0.0, from * pi, to * pi, (sx * sy) < 0.0);
    }

    /// `arc()`: a circular arc, continuing the subpath from a line to
    /// its start.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(&mut self, x: f32, y: f32, r: f32, a0: f32, a1: f32, ccw: bool) {
        self.ellipse(x, y, r, r, 0.0, a0, a1, ccw);
    }

    /// `ellipse()`.
    #[allow(clippy::too_many_arguments)]
    pub fn ellipse(
        &mut self,
        x: f32,
        y: f32,
        rx: f32,
        ry: f32,
        rotation: f32,
        a0: f32,
        a1: f32,
        ccw: bool,
    ) {
        if ![x, y, rx, ry, rotation, a0, a1]
            .iter()
            .all(|v| v.is_finite())
            || rx < 0.0
            || ry < 0.0
        {
            return;
        }
        let tau = std::f32::consts::TAU;
        let mut sweep = a1 - a0;
        if ccw {
            if sweep <= -tau {
                sweep = -tau;
            } else {
                sweep = sweep.rem_euclid(tau);
                if sweep > 0.0 {
                    sweep -= tau;
                }
                if sweep == 0.0 && a1 != a0 {
                    sweep = -tau;
                }
            }
        } else if sweep >= tau {
            sweep = tau;
        } else {
            sweep = sweep.rem_euclid(tau);
            if sweep == 0.0 && a1 != a0 {
                sweep = tau;
            }
        }
        let (sin_r, cos_r) = rotation.sin_cos();
        let point = |a: f32| {
            let (px, py) = (rx * a.cos(), ry * a.sin());
            (x + px * cos_r - py * sin_r, y + px * sin_r + py * cos_r)
        };
        let (sx, sy) = point(a0);
        if self.current.is_none() {
            self.move_to(sx, sy);
        } else {
            self.line_to(sx, sy);
        }
        // Cubic segments of at most a quarter turn.
        let segments = ((sweep.abs() / (std::f32::consts::FRAC_PI_2)).ceil() as usize).max(1);
        let step = sweep / segments as f32;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let mut a = a0;
        for _ in 0..segments {
            let b = a + step;
            let (cos_a, sin_a) = (a.cos(), a.sin());
            let (cos_b, sin_b) = (b.cos(), b.sin());
            // Control points on the unit circle, then scaled and rotated.
            let c1 = (cos_a - k * sin_a, sin_a + k * cos_a);
            let c2 = (cos_b + k * sin_b, sin_b - k * cos_b);
            let map = |(ux, uy): (f32, f32)| {
                let (px, py) = (rx * ux, ry * uy);
                (x + px * cos_r - py * sin_r, y + px * sin_r + py * cos_r)
            };
            let (c1x, c1y) = map(c1);
            let (c2x, c2y) = map(c2);
            let (ex, ey) = map((cos_b, sin_b));
            self.builder.cubic_to(c1x, c1y, c2x, c2y, ex, ey);
            self.current = Some((ex, ey));
            a = b;
        }
    }

    /// `arcTo()`.
    pub fn arc_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, r: f32) {
        if ![x1, y1, x2, y2, r].iter().all(|v| v.is_finite()) || r < 0.0 {
            return;
        }
        let Some((x0, y0)) = self.current else {
            self.move_to(x1, y1);
            return;
        };
        let (ax, ay) = (x0 - x1, y0 - y1);
        let (bx, by) = (x2 - x1, y2 - y1);
        let cross = ax * by - ay * bx;
        if (x0 == x1 && y0 == y1) || (x1 == x2 && y1 == y2) || r == 0.0 || cross.abs() < 1e-6 {
            self.line_to(x1, y1);
            return;
        }
        let (la, lb) = (ax.hypot(ay), bx.hypot(by));
        let (uax, uay) = (ax / la, ay / la);
        let (ubx, uby) = (bx / lb, by / lb);
        let cos_theta = uax * ubx + uay * uby;
        let theta = cos_theta.clamp(-1.0, 1.0).acos();
        let tangent = r / (theta / 2.0).tan();
        let (t1x, t1y) = (x1 + uax * tangent, y1 + uay * tangent);
        let (t2x, t2y) = (x1 + ubx * tangent, y1 + uby * tangent);
        // The center lies along the bisector, r away from the tangent
        // points.
        let (nx, ny) = if cross > 0.0 {
            (-uay, uax)
        } else {
            (uay, -uax)
        };
        let (cx, cy) = (t1x + nx * r, t1y + ny * r);
        let a0 = (t1y - cy).atan2(t1x - cx);
        let a1 = (t2y - cy).atan2(t2x - cx);
        self.line_to(t1x, t1y);
        self.arc(cx, cy, r, a0, a1, cross > 0.0);
    }

    /// Appends `other`, transformed.
    pub fn add_path(&mut self, other: &Path2d, transform: Transform) {
        if let Some(path) = other.builder.clone().finish()
            && let Some(path) = path.transform(transform)
        {
            self.builder.push_path(&path);
            if let Some((x, y)) = other.current {
                let mut p = Point::from_xy(x, y);
                transform.map_point(&mut p);
                self.current = Some((p.x, p.y));
            }
        }
    }

    pub fn finish(&self) -> Option<Path> {
        self.builder.clone().finish()
    }
}

/// What `measureText` reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextMetrics {
    pub width: f32,
    pub ascent: f32,
    pub descent: f32,
    pub font_ascent: f32,
    pub font_descent: f32,
}

/// A shaped line of text: glyphs with their font, placed from the
/// origin on the alphabetic baseline.
struct Shaped {
    glyphs: Vec<ShapedGlyph>,
    metrics: TextMetrics,
}

struct ShapedGlyph {
    font: parley::FontData,
    id: u32,
    x: f32,
    y: f32,
    size: f32,
    coords: Vec<skrifa::instance::NormalizedCoord>,
}

/// A canvas bitmap with its drawing state.
pub struct Canvas2d {
    pixmap: Pixmap,
    state: State,
    stack: Vec<State>,
    path: Path2d,
    fonts: Arc<Mutex<Fonts>>,
    glyphs: HashMap<(u64, u32, u32, u32), Option<Path>>,
}

impl Canvas2d {
    /// A transparent black bitmap of `width × height` pixels (at least 1
    /// each).
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixmap: Pixmap::new(width.max(1), height.max(1)).expect("a non-empty pixmap"),
            state: State::default(),
            stack: Vec::new(),
            path: Path2d::default(),
            fonts: shared_fonts(),
            glyphs: HashMap::new(),
        }
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    pub fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn path_mut(&mut self) -> &mut Path2d {
        &mut self.path
    }

    /// Clears everything: the bitmap, the state stack and the path (a
    /// size change, or `reset()`).
    pub fn reset(&mut self, width: u32, height: u32) {
        self.pixmap = Pixmap::new(width.max(1), height.max(1)).expect("a non-empty pixmap");
        self.state = State::default();
        self.stack.clear();
        self.path = Path2d::default();
    }

    pub fn save(&mut self) {
        self.stack.push(self.state.clone());
    }

    pub fn restore(&mut self) {
        if let Some(state) = self.stack.pop() {
            self.state = state;
        }
    }

    pub fn begin_path(&mut self) {
        self.path = Path2d::default();
    }

    // ---- transforms -----------------------------------------------------

    pub fn scale(&mut self, x: f32, y: f32) {
        self.concat(Transform::from_scale(x, y));
    }

    pub fn rotate(&mut self, angle: f32) {
        self.concat(Transform::from_rotate(angle.to_degrees()));
    }

    pub fn translate(&mut self, x: f32, y: f32) {
        self.concat(Transform::from_translate(x, y));
    }

    /// `transform(a, b, c, d, e, f)`.
    pub fn concat(&mut self, other: Transform) {
        if !transform_finite(&other) {
            return;
        }
        self.state.transform = self.state.transform.pre_concat(other);
    }

    pub fn set_transform(&mut self, t: Transform) {
        if transform_finite(&t) {
            self.state.transform = t;
        }
    }

    pub fn transform(&self) -> Transform {
        self.state.transform
    }

    // ---- painting -------------------------------------------------------

    fn paint_for(&self, style: &Style) -> Paint<'static> {
        let mut paint = Paint {
            anti_alias: true,
            blend_mode: self.state.blend,
            ..Paint::default()
        };
        let alpha = self.state.global_alpha.clamp(0.0, 1.0);
        let stops = |stops: &[(f32, Color)]| -> Vec<GradientStop> {
            let mut out: Vec<GradientStop> = stops
                .iter()
                .map(|(offset, color)| {
                    let mut c = *color;
                    c.set_alpha(c.alpha() * alpha);
                    GradientStop::new(*offset, c)
                })
                .collect();
            if out.is_empty() {
                out.push(GradientStop::new(0.0, Color::TRANSPARENT));
            }
            if out.len() == 1 {
                let only = stops.first().map(|(_, c)| *c).unwrap_or(Color::TRANSPARENT);
                out.push(GradientStop::new(1.0, only));
            }
            out
        };
        match style {
            Style::Color(color) => {
                let mut c = *color;
                c.set_alpha(c.alpha() * alpha);
                paint.set_color(c);
            }
            Style::Linear {
                x0,
                y0,
                x1,
                y1,
                stops: s,
            } => {
                paint.shader = LinearGradient::new(
                    Point::from_xy(*x0, *y0),
                    Point::from_xy(*x1, *y1),
                    stops(s),
                    SpreadMode::Pad,
                    self.state.transform,
                )
                .unwrap_or(Shader::SolidColor(Color::TRANSPARENT));
            }
            Style::Radial {
                x0,
                y0,
                r0,
                x1,
                y1,
                r1,
                stops: s,
            } => {
                paint.shader = RadialGradient::new(
                    Point::from_xy(*x0, *y0),
                    r0.max(0.0),
                    Point::from_xy(*x1, *y1),
                    r1.max(0.001),
                    stops(s),
                    SpreadMode::Pad,
                    self.state.transform,
                )
                .unwrap_or(Shader::SolidColor(Color::TRANSPARENT));
            }
            Style::Conic { stops: s, .. } => {
                let mut c = s.last().map(|(_, c)| *c).unwrap_or(Color::TRANSPARENT);
                c.set_alpha(c.alpha() * alpha);
                paint.set_color(c);
            }
        }
        paint
    }

    fn stroke(&self) -> Stroke {
        let mut stroke = Stroke {
            width: self.state.line_width.max(0.0),
            miter_limit: self.state.miter_limit,
            line_cap: self.state.line_cap,
            line_join: self.state.line_join,
            dash: None,
        };
        if !self.state.dash.is_empty() {
            let mut dash = self.state.dash.clone();
            if dash.len() % 2 == 1 {
                let copy = dash.clone();
                dash.extend(copy);
            }
            stroke.dash = StrokeDash::new(dash, self.state.dash_offset);
        }
        stroke
    }

    pub fn fill_path(&mut self, path: &Path2d, even_odd: bool) {
        let Some(path) = path.finish() else {
            return;
        };
        let rule = if even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        let paint = self.paint_for(&self.state.fill.clone());
        let transform = self.state.transform;
        let mask = self.state.clip.clone();
        self.pixmap
            .fill_path(&path, &paint, rule, transform, mask.as_deref());
    }

    pub fn stroke_path(&mut self, path: &Path2d) {
        let Some(path) = path.finish() else {
            return;
        };
        if self.state.line_width <= 0.0 {
            return;
        }
        let paint = self.paint_for(&self.state.stroke.clone());
        let stroke = self.stroke();
        let transform = self.state.transform;
        let mask = self.state.clip.clone();
        self.pixmap
            .stroke_path(&path, &paint, &stroke, transform, mask.as_deref());
    }

    /// Fills the current path.
    pub fn fill(&mut self, even_odd: bool) {
        let path = self.path.clone();
        self.fill_path(&path, even_odd);
    }

    pub fn stroke_current(&mut self) {
        let path = self.path.clone();
        self.stroke_path(&path);
    }

    /// Intersects the clip with `path` (the current path when `None`).
    pub fn clip(&mut self, path: Option<&Path2d>, even_odd: bool) {
        let path = match path {
            Some(p) => p.clone(),
            None => self.path.clone(),
        };
        let rule = if even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        let mut mask = match &self.state.clip {
            Some(existing) => (**existing).clone(),
            None => {
                Mask::new(self.pixmap.width(), self.pixmap.height()).expect("a mask of the bitmap")
            }
        };
        match path.finish() {
            Some(device) if self.state.clip.is_some() => {
                mask.intersect_path(&device, rule, true, self.state.transform);
            }
            Some(device) => mask.fill_path(&device, rule, true, self.state.transform),
            // An empty path clips everything away.
            None => mask.clear(),
        }
        self.state.clip = Some(Arc::new(mask));
    }

    pub fn clear_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let mut path = Path2d::default();
        path.rect(x, y, w, h);
        let Some(path) = path.finish() else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(Color::TRANSPARENT);
        paint.blend_mode = BlendMode::Source;
        paint.anti_alias = false;
        let transform = self.state.transform;
        let mask = self.state.clip.clone();
        self.pixmap
            .fill_path(&path, &paint, FillRule::Winding, transform, mask.as_deref());
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let mut path = Path2d::default();
        path.rect(x, y, w, h);
        self.fill_path(&path, false);
    }

    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let mut path = Path2d::default();
        path.rect(x, y, w, h);
        self.stroke_path(&path);
    }

    /// Whether the device point is inside the path (the current one when
    /// `None`) under the current transform.
    pub fn is_point_in_path(&self, path: Option<&Path2d>, x: f32, y: f32, even_odd: bool) -> bool {
        let path = match path {
            Some(p) => p.finish(),
            None => self.path.finish(),
        };
        let Some(path) = path.and_then(|p| p.transform(self.state.transform)) else {
            return false;
        };
        point_in_path(&path, x, y, even_odd)
    }

    pub fn is_point_in_stroke(&self, path: Option<&Path2d>, x: f32, y: f32) -> bool {
        let path = match path {
            Some(p) => p.finish(),
            None => self.path.finish(),
        };
        let Some(path) = path else {
            return false;
        };
        let stroke = self.stroke();
        let Some(outline) = path.stroke(&stroke, 1.0) else {
            return false;
        };
        let Some(outline) = outline.transform(self.state.transform) else {
            return false;
        };
        point_in_path(&outline, x, y, false)
    }

    // ---- text -----------------------------------------------------------

    fn shape(&mut self, text: &str) -> Shaped {
        let font = self.state.font.clone();
        let fonts = self.fonts.clone();
        let mut fonts = fonts.lock().unwrap_or_else(|e| e.into_inner());
        let Fonts { font_cx, layout_cx } = &mut *fonts;
        // Canvas text is one line: breaks and tabs become spaces.
        let text: String = text
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        let mut builder = layout_cx.ranged_builder(font_cx, &text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::List(
            std::borrow::Cow::Owned(font.families.clone()),
        )));
        builder.push_default(StyleProperty::FontSize(font.size));
        builder.push_default(StyleProperty::FontWeight(FontWeight::new(font.weight)));
        builder.push_default(StyleProperty::FontStyle(if font.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        }));
        builder.push_default(StyleProperty::Brush(Brush::default()));
        let mut layout: parley::Layout<Brush> = builder.build(&text);
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());
        let mut glyphs = Vec::new();
        let mut metrics = TextMetrics::default();
        if let Some(line) = layout.lines().next() {
            let line_metrics = line.metrics();
            metrics.font_ascent = line_metrics.ascent;
            metrics.font_descent = line_metrics.descent;
            metrics.ascent = line_metrics.ascent;
            metrics.descent = line_metrics.descent;
            let baseline = line_metrics.baseline;
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let font = run.run().font().clone();
                let size = run.run().font_size();
                let coords: Vec<skrifa::instance::NormalizedCoord> = run
                    .run()
                    .normalized_coords()
                    .iter()
                    .map(|c| skrifa::instance::NormalizedCoord::from_bits(*c))
                    .collect();
                for glyph in run.positioned_glyphs() {
                    glyphs.push(ShapedGlyph {
                        font: font.clone(),
                        id: glyph.id,
                        x: glyph.x,
                        y: glyph.y - baseline,
                        size,
                        coords: coords.clone(),
                    });
                }
            }
            metrics.width = layout.width();
        }
        Shaped { glyphs, metrics }
    }

    fn glyph_path(
        &mut self,
        font: &parley::FontData,
        glyph: u32,
        size: f32,
        coords: &[skrifa::instance::NormalizedCoord],
    ) -> Option<Path> {
        use skrifa::MetadataProvider as _;
        let key = (font.data.id(), font.index, glyph, size.to_bits());
        if let Some(cached) = self.glyphs.get(&key) {
            return cached.clone();
        }
        let path = (|| {
            let font_ref = skrifa::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
            let outline = font_ref.outline_glyphs().get(skrifa::GlyphId::new(glyph))?;
            let mut pen = crate::PathPen {
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

    /// Where a line of `width` starts and sits for the text alignment and
    /// baseline.
    fn text_offsets(&self, metrics: &TextMetrics) -> (f32, f32) {
        let rtl = self.state.direction_rtl;
        let dx = match self.state.text_align {
            TextAlign::Left => 0.0,
            TextAlign::Right => -metrics.width,
            TextAlign::Center => -metrics.width / 2.0,
            TextAlign::Start => {
                if rtl {
                    -metrics.width
                } else {
                    0.0
                }
            }
            TextAlign::End => {
                if rtl {
                    0.0
                } else {
                    -metrics.width
                }
            }
        };
        let dy = match self.state.text_baseline {
            TextBaseline::Alphabetic => 0.0,
            TextBaseline::Top | TextBaseline::Hanging => metrics.font_ascent,
            TextBaseline::Middle => (metrics.font_ascent - metrics.font_descent) / 2.0,
            TextBaseline::Bottom | TextBaseline::Ideographic => -metrics.font_descent,
        };
        (dx, dy)
    }

    /// Draws `text` with its anchor at `(x, y)`, filled or stroked.
    pub fn draw_text(&mut self, text: &str, x: f32, y: f32, max_width: Option<f32>, stroke: bool) {
        if text.is_empty() || !(x.is_finite() && y.is_finite()) {
            return;
        }
        let shaped = self.shape(text);
        let (dx, dy) = self.text_offsets(&shaped.metrics);
        let squeeze = match max_width {
            Some(max) if max.is_finite() && max > 0.0 && shaped.metrics.width > max => {
                max / shaped.metrics.width
            }
            Some(max) if !(max.is_finite() && max > 0.0) => return,
            _ => 1.0,
        };
        let base = self
            .state
            .transform
            .pre_concat(Transform::from_translate(x, y))
            .pre_concat(Transform::from_scale(squeeze, 1.0))
            .pre_concat(Transform::from_translate(dx, dy));
        let paint = self.paint_for(&if stroke {
            self.state.stroke.clone()
        } else {
            self.state.fill.clone()
        });
        let stroke_style = self.stroke();
        let mask = self.state.clip.clone();
        for glyph in &shaped.glyphs {
            let Some(path) = self.glyph_path(&glyph.font, glyph.id, glyph.size, &glyph.coords)
            else {
                continue;
            };
            let transform = base.pre_concat(Transform::from_translate(glyph.x, glyph.y));
            if stroke {
                self.pixmap
                    .stroke_path(&path, &paint, &stroke_style, transform, mask.as_deref());
            } else {
                self.pixmap
                    .fill_path(&path, &paint, FillRule::Winding, transform, mask.as_deref());
            }
        }
    }

    pub fn measure_text(&mut self, text: &str) -> TextMetrics {
        let shaped = self.shape(text);
        let mut metrics = shaped.metrics;
        // Bounds from the glyph outlines, when there are any.
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for glyph in &shaped.glyphs {
            if let Some(path) = self.glyph_path(&glyph.font, glyph.id, glyph.size, &glyph.coords) {
                let b = path.bounds();
                left = left.min(glyph.x + b.left());
                right = right.max(glyph.x + b.right());
                top = top.min(glyph.y + b.top());
                bottom = bottom.max(glyph.y + b.bottom());
            }
        }
        if left.is_finite() {
            metrics.ascent = (-top).max(0.0);
            metrics.descent = bottom.max(0.0);
            let (dx, _) = self.text_offsets(&metrics);
            // Reported relative to the alignment point.
            let _ = (dx, left, right);
        }
        metrics
    }

    // ---- images ---------------------------------------------------------

    /// Draws the `(sx, sy, sw, sh)` part of `source` into `(dx, dy, dw, dh)`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_image(
        &mut self,
        source: &Pixmap,
        sx: f32,
        sy: f32,
        sw: f32,
        sh: f32,
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
    ) {
        if ![sx, sy, sw, sh, dx, dy, dw, dh]
            .iter()
            .all(|v| v.is_finite())
            || sw <= 0.0
            || sh <= 0.0
            || dw == 0.0
            || dh == 0.0
        {
            return;
        }
        let Some(rect) = IntRect::from_xywh(
            sx.floor() as i32,
            sy.floor() as i32,
            sw.ceil().max(1.0) as u32,
            sh.ceil().max(1.0) as u32,
        ) else {
            return;
        };
        let Some(part) = source.clone_rect(rect) else {
            return;
        };
        let paint = PixmapPaint {
            opacity: self.state.global_alpha.clamp(0.0, 1.0),
            blend_mode: self.state.blend,
            quality: tiny_skia::FilterQuality::Bilinear,
        };
        let transform = self
            .state
            .transform
            .pre_concat(Transform::from_translate(dx, dy))
            .pre_concat(Transform::from_scale(
                dw / part.width() as f32,
                dh / part.height() as f32,
            ));
        let mask = self.state.clip.clone();
        self.pixmap
            .draw_pixmap(0, 0, part.as_ref(), &paint, transform, mask.as_deref());
    }

    /// The pixels of a rectangle as straight-alpha RGBA, row by row;
    /// transparent black outside the bitmap.
    pub fn image_data(&self, x: i32, y: i32, w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        for row in 0..h {
            for col in 0..w {
                let (px, py) = (x + col as i32, y + row as i32);
                if px < 0 || py < 0 {
                    continue;
                }
                let Some(pixel) = self.pixmap.pixel(px as u32, py as u32) else {
                    continue;
                };
                let c = pixel.demultiply();
                let i = ((row * w + col) * 4) as usize;
                out[i] = c.red();
                out[i + 1] = c.green();
                out[i + 2] = c.blue();
                out[i + 3] = c.alpha();
            }
        }
        out
    }

    /// Writes straight-alpha RGBA pixels at `(x, y)`, replacing what was
    /// there (no blending, transform or clip, as `putImageData` does).
    pub fn put_image_data(&mut self, data: &[u8], w: u32, h: u32, x: i32, y: i32) {
        if w == 0 || h == 0 || data.len() < (w as usize) * (h as usize) * 4 {
            return;
        }
        let Some(mut part) = Pixmap::new(w, h) else {
            return;
        };
        let pixels = part.pixels_mut();
        for (i, pixel) in pixels.iter_mut().enumerate() {
            let c = tiny_skia::ColorU8::from_rgba(
                data[i * 4],
                data[i * 4 + 1],
                data[i * 4 + 2],
                data[i * 4 + 3],
            );
            *pixel = c.premultiply();
        }
        let paint = PixmapPaint {
            opacity: 1.0,
            blend_mode: BlendMode::Source,
            quality: tiny_skia::FilterQuality::Nearest,
        };
        self.pixmap
            .draw_pixmap(x, y, part.as_ref(), &paint, Transform::identity(), None);
    }

    pub fn to_png(&self) -> Vec<u8> {
        self.pixmap
            .encode_png()
            .expect("PNG encoding of an in-memory pixmap")
    }
}

fn transform_finite(t: &Transform) -> bool {
    [t.sx, t.kx, t.ky, t.sy, t.tx, t.ty]
        .iter()
        .all(|v| v.is_finite())
}

/// Point-in-path by crossing count over the flattened path.
fn point_in_path(path: &Path, x: f32, y: f32, even_odd: bool) -> bool {
    let mut winding = 0i32;
    let mut crossings = 0u32;
    let mut start = Point::zero();
    let mut last = Point::zero();
    let mut edge = |a: Point, b: Point| {
        if (a.y > y) != (b.y > y) {
            let t = (y - a.y) / (b.y - a.y);
            let ix = a.x + t * (b.x - a.x);
            if ix > x {
                crossings += 1;
                winding += if b.y > a.y { 1 } else { -1 };
            }
        }
    };
    for segment in path.segments() {
        use tiny_skia::PathSegment;
        match segment {
            PathSegment::MoveTo(p) => {
                if last != start {
                    edge(last, start);
                }
                start = p;
                last = p;
            }
            PathSegment::LineTo(p) => {
                edge(last, p);
                last = p;
            }
            PathSegment::QuadTo(c, p) => {
                let mut prev = last;
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    let mt = 1.0 - t;
                    let q = Point::from_xy(
                        mt * mt * last.x + 2.0 * mt * t * c.x + t * t * p.x,
                        mt * mt * last.y + 2.0 * mt * t * c.y + t * t * p.y,
                    );
                    edge(prev, q);
                    prev = q;
                }
                last = p;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                let mut prev = last;
                for i in 1..=12 {
                    let t = i as f32 / 12.0;
                    let mt = 1.0 - t;
                    let q = Point::from_xy(
                        mt * mt * mt * last.x
                            + 3.0 * mt * mt * t * c1.x
                            + 3.0 * mt * t * t * c2.x
                            + t * t * t * p.x,
                        mt * mt * mt * last.y
                            + 3.0 * mt * mt * t * c1.y
                            + 3.0 * mt * t * t * c2.y
                            + t * t * t * p.y,
                    );
                    edge(prev, q);
                    prev = q;
                }
                last = p;
            }
            PathSegment::Close => {
                edge(last, start);
                last = start;
            }
        }
    }
    if last != start {
        edge(last, start);
    }
    if even_odd {
        crossings % 2 == 1
    } else {
        winding != 0
    }
}
