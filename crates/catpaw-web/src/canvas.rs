//! `<canvas>` and its 2D context: the Web API over the raster backend in
//! `catpaw_paint::canvas`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use catpaw_dom::NodeId;
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Uint8ArrayData, Value};
use catpaw_paint::canvas::{self, Canvas2d, Path2d, Style, TextAlign, TextBaseline};
use catpaw_paint::tiny_skia::{LineCap, LineJoin, Pixmap, Transform};

use crate::generated::{self as web};
use crate::page::{Cx, PageState};
use crate::{Web, event_loop, node, platform_object};

const DEFAULT_WIDTH: u32 = 300;
const DEFAULT_HEIGHT: u32 = 150;

/// The bitmaps and contexts of the page's canvases.
#[derive(Default)]
pub struct Canvases {
    bitmaps: RefCell<HashMap<NodeId, Rc<RefCell<Canvas2d>>>>,
    contexts: RefCell<HashMap<NodeId, ObjectId>>,
}

impl Canvases {
    /// The bitmap of a canvas element, if a context was ever made.
    pub fn pixmap(&self, node: NodeId) -> Option<Pixmap> {
        self.bitmaps
            .borrow()
            .get(&node)
            .map(|c| c.borrow().pixmap().clone())
    }
}

type StyleObject = Option<web::StringOrCanvasGradientOrCanvasPattern>;

pub struct ContextObject {
    canvas: NodeId,
    bitmap: Rc<RefCell<Canvas2d>>,
    /// The gradient or pattern object `fillStyle`/`strokeStyle` were set
    /// to, when not a color (colors are read back from the bitmap state).
    fill_object: RefCell<StyleObject>,
    stroke_object: RefCell<StyleObject>,
    /// Those objects as `save()` stacked them.
    style_stack: RefCell<Vec<(StyleObject, StyleObject)>>,
    shadow: RefCell<(f64, f64, f64, String)>,
    filter: RefCell<String>,
    smoothing: Cell<(bool, web::ImageSmoothingQuality)>,
    spacing: RefCell<(String, String)>,
    direction: Cell<web::CanvasDirection>,
}
platform_object!(ContextObject, CanvasRenderingContext2D);

pub struct GradientObject {
    style: RefCell<Style>,
}
platform_object!(GradientObject, CanvasGradient);

pub struct PatternObject;
platform_object!(PatternObject, CanvasPattern);

pub struct Path2DObject {
    path: RefCell<Path2d>,
}
platform_object!(Path2DObject, Path2D);

pub struct TextMetricsObject {
    metrics: canvas::TextMetrics,
}
platform_object!(TextMetricsObject, TextMetrics);

pub struct ImageDataObject {
    width: u32,
    height: u32,
    data: Vec<u8>,
}
platform_object!(ImageDataObject, ImageData);

fn size_attr(page: &PageState, el: NodeId, name: &str, default: u32) -> u32 {
    page.dom
        .borrow()
        .attr(el, name)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

/// The canvas's bitmap, made at the element's size on first use.
fn bitmap_of(page: &PageState, el: NodeId) -> Rc<RefCell<Canvas2d>> {
    if let Some(b) = page.canvases.bitmaps.borrow().get(&el) {
        return b.clone();
    }
    let width = size_attr(page, el, "width", DEFAULT_WIDTH);
    let height = size_attr(page, el, "height", DEFAULT_HEIGHT);
    let bitmap = Rc::new(RefCell::new(Canvas2d::new(width, height)));
    page.canvases
        .bitmaps
        .borrow_mut()
        .insert(el, bitmap.clone());
    bitmap
}

/// A `width` or `height` attribute changed: the bitmap starts over at
/// the new size, as the spec says.
pub(crate) fn size_changed(page: &PageState, el: NodeId) {
    let bitmap = page.canvases.bitmaps.borrow().get(&el).cloned();
    if let Some(bitmap) = bitmap {
        let width = size_attr(page, el, "width", DEFAULT_WIDTH);
        let height = size_attr(page, el, "height", DEFAULT_HEIGHT);
        bitmap.borrow_mut().reset(width, height);
    }
    let context = page.canvases.contexts.borrow().get(&el).copied();
    if let Some(context) = context {
        page.try_with::<ContextObject, _>(context, reset_styles);
    }
}

fn reset_styles(c: &mut ContextObject) {
    *c.fill_object.borrow_mut() = None;
    *c.stroke_object.borrow_mut() = None;
    c.style_stack.borrow_mut().clear();
}

/// What a style attribute reports: the color the bitmap holds, or the
/// object it was set to.
fn style_report(c: &ContextObject, fill: bool) -> web::StringOrCanvasGradientOrCanvasPattern {
    let object = if fill {
        c.fill_object.borrow().clone()
    } else {
        c.stroke_object.borrow().clone()
    };
    let state = c.bitmap.borrow();
    let style = if fill {
        &state.state().fill
    } else {
        &state.state().stroke
    };
    match (style, object) {
        (Style::Color(color), _) => {
            web::StringOrCanvasGradientOrCanvasPattern::String(canvas::serialize_color(*color))
        }
        (_, Some(object)) => object,
        // A gradient without its object: not reachable through the API.
        (_, None) => web::StringOrCanvasGradientOrCanvasPattern::String("#000000".to_string()),
    }
}

fn ctx<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&ContextObject) -> R) -> Fallible<R> {
    cx.page.with::<ContextObject, _>(this, |c| f(c))
}

fn with_canvas<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut Canvas2d) -> R) -> Fallible<R> {
    let bitmap = ctx(cx, this, |c| c.bitmap.clone())?;
    let mut canvas = bitmap.borrow_mut();
    Ok(f(&mut canvas))
}

fn path_of(cx: &Cx<'_>, path: ObjectId) -> Fallible<Path2d> {
    cx.page
        .with::<Path2DObject, _>(path, |p| p.path.borrow().clone())
}

fn f(v: f64) -> f32 {
    v as f32
}

fn matrix(init: &web::DOMMatrix2DInit) -> Option<Transform> {
    let pick = |a: Option<f64>, m: Option<f64>, default: f64| -> Option<f64> {
        match (a, m) {
            (Some(a), Some(m)) if a != m => None,
            (Some(a), _) => Some(a),
            (None, Some(m)) => Some(m),
            (None, None) => Some(default),
        }
    };
    Some(Transform::from_row(
        f(pick(init.a, init.m11, 1.0)?),
        f(pick(init.b, init.m12, 0.0)?),
        f(pick(init.c, init.m21, 0.0)?),
        f(pick(init.d, init.m22, 1.0)?),
        f(pick(init.e, init.m41, 0.0)?),
        f(pick(init.f, init.m42, 0.0)?),
    ))
}

fn style_from(cx: &Cx<'_>, value: &web::StringOrCanvasGradientOrCanvasPattern) -> Option<Style> {
    match value {
        web::StringOrCanvasGradientOrCanvasPattern::String(s) => {
            canvas::parse_color(s).map(Style::Color)
        }
        web::StringOrCanvasGradientOrCanvasPattern::CanvasGradient(id) => cx
            .page
            .try_with::<GradientObject, _>(*id, |g| g.style.borrow().clone()),
        web::StringOrCanvasGradientOrCanvasPattern::CanvasPattern(_) => None,
    }
}

fn canvas_source(
    cx: &mut Cx<'_>,
    image: &web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement,
) -> Fallible<Option<Pixmap>> {
    use web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement as Source;
    match image {
        Source::HTMLCanvasElement(el) => {
            node::check(cx, *el)?;
            Ok(Some(bitmap_of(cx.page, *el).borrow().pixmap().clone()))
        }
        // Images are not decoded yet: nothing to draw.
        _ => Ok(None),
    }
}

// ----------------------------------------------------------------- element

impl web::HTMLCanvasElementImpl for Web {
    fn width(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        crate::reflect::get_unsigned_long(cx, this, "width", DEFAULT_WIDTH, "none")
    }

    fn set_width(cx: &mut Cx<'_>, this: NodeId, value: u32) -> Fallible<()> {
        crate::reflect::set_unsigned_long(cx, this, "width", value, DEFAULT_WIDTH, "none")
    }

    fn height(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        crate::reflect::get_unsigned_long(cx, this, "height", DEFAULT_HEIGHT, "none")
    }

    fn set_height(cx: &mut Cx<'_>, this: NodeId, value: u32) -> Fallible<()> {
        crate::reflect::set_unsigned_long(cx, this, "height", value, DEFAULT_HEIGHT, "none")
    }

    fn get_context(
        cx: &mut Cx<'_>,
        this: NodeId,
        context_id: String,
        _options: Value,
    ) -> Fallible<Option<ObjectId>> {
        node::check(cx, this)?;
        if context_id != "2d" {
            return Ok(None);
        }
        if let Some(id) = cx.page.canvases.contexts.borrow().get(&this) {
            return Ok(Some(*id));
        }
        let bitmap = bitmap_of(cx.page, this);
        let id = cx.page.alloc(ContextObject {
            canvas: this,
            bitmap,
            fill_object: RefCell::new(None),
            stroke_object: RefCell::new(None),
            style_stack: RefCell::new(Vec::new()),
            shadow: RefCell::new((0.0, 0.0, 0.0, "rgba(0, 0, 0, 0)".to_string())),
            filter: RefCell::new("none".to_string()),
            smoothing: Cell::new((true, web::ImageSmoothingQuality::Low)),
            spacing: RefCell::new(("0px".to_string(), "0px".to_string())),
            direction: Cell::new(web::CanvasDirection::Inherit),
        });
        cx.pin(id);
        cx.page.canvases.contexts.borrow_mut().insert(this, id);
        Ok(Some(id))
    }

    fn to_data_url(
        cx: &mut Cx<'_>,
        this: NodeId,
        _type: String,
        _quality: Value,
    ) -> Fallible<String> {
        node::check(cx, this)?;
        use base64::Engine as _;
        let png = bitmap_of(cx.page, this).borrow().to_png();
        Ok(format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        ))
    }

    fn to_blob(
        cx: &mut Cx<'_>,
        this: NodeId,
        callback: Callback,
        _type: String,
        _quality: Value,
    ) -> Fallible<()> {
        node::check(cx, this)?;
        let png = bitmap_of(cx.page, this).borrow().to_png();
        event_loop::queue_task(cx.page, "canvas toBlob", move |cx| {
            let blob = crate::file_api::new_blob(cx, png, "image/png");
            let _ = cx
                .script
                .call(&callback, &Value::Undefined, &[Value::Object(blob)]);
        });
        Ok(())
    }
}

// ----------------------------------------------------------------- context

impl web::CanvasRenderingContext2DImpl for Web {
    fn canvas(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        ctx(cx, this, |c| c.canvas)
    }
}

impl web::CanvasStateImpl for Web {
    fn save(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        ctx(cx, this, |c| {
            c.style_stack.borrow_mut().push((
                c.fill_object.borrow().clone(),
                c.stroke_object.borrow().clone(),
            ));
            c.bitmap.borrow_mut().save();
        })
    }

    fn restore(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        ctx(cx, this, |c| {
            if let Some((fill, stroke)) = c.style_stack.borrow_mut().pop() {
                *c.fill_object.borrow_mut() = fill;
                *c.stroke_object.borrow_mut() = stroke;
            }
            c.bitmap.borrow_mut().restore();
        })
    }

    fn reset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        cx.page.with::<ContextObject, _>(this, |c| {
            reset_styles(c);
            let mut bitmap = c.bitmap.borrow_mut();
            let (w, h) = (bitmap.width(), bitmap.height());
            bitmap.reset(w, h);
        })
    }

    fn is_context_lost(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ctx(cx, this, |_| false)
    }
}

impl web::CanvasTransformImpl for Web {
    fn scale(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| c.scale(f(x), f(y)))
    }

    fn rotate(cx: &mut Cx<'_>, this: ObjectId, angle: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| c.rotate(f(angle)))
    }

    fn translate(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| c.translate(f(x), f(y)))
    }

    #[allow(clippy::many_single_char_names)]
    fn transform(
        cx: &mut Cx<'_>,
        this: ObjectId,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        e: f64,
        f_: f64,
    ) -> Fallible<()> {
        with_canvas(cx, this, |cv| {
            cv.concat(Transform::from_row(f(a), f(b), f(c), f(d), f(e), f(f_)))
        })
    }

    #[allow(clippy::many_single_char_names)]
    fn set_transform(
        cx: &mut Cx<'_>,
        this: ObjectId,
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        e: f64,
        f_: f64,
    ) -> Fallible<()> {
        with_canvas(cx, this, |cv| {
            cv.set_transform(Transform::from_row(f(a), f(b), f(c), f(d), f(e), f(f_)))
        })
    }

    fn set_transform_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        transform: web::DOMMatrix2DInit,
    ) -> Fallible<()> {
        let t = matrix(&transform).ok_or_else(|| {
            Exception::type_error(
                "Failed to execute 'setTransform': the matrix init is inconsistent",
            )
        })?;
        with_canvas(cx, this, |c| c.set_transform(t))
    }

    fn reset_transform(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_canvas(cx, this, |c| c.set_transform(Transform::identity()))
    }
}

impl web::CanvasCompositingImpl for Web {
    fn global_alpha(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        with_canvas(cx, this, |c| f64::from(c.state().global_alpha))
    }

    fn set_global_alpha(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if value.is_finite() && (0.0..=1.0).contains(&value) {
                c.state_mut().global_alpha = f(value);
            }
        })
    }

    fn global_composite_operation(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        with_canvas(cx, this, |c| c.state().composite.clone())
    }

    fn set_global_composite_operation(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: String,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if let Some(mode) = canvas::blend_mode(&value) {
                c.state_mut().blend = mode;
                c.state_mut().composite = value;
            }
        })
    }
}

impl web::CanvasImageSmoothingImpl for Web {
    fn image_smoothing_enabled(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        ctx(cx, this, |c| c.smoothing.get().0)
    }

    fn set_image_smoothing_enabled(cx: &mut Cx<'_>, this: ObjectId, value: bool) -> Fallible<()> {
        ctx(cx, this, |c| c.smoothing.set((value, c.smoothing.get().1)))
    }

    fn image_smoothing_quality(
        cx: &mut Cx<'_>,
        this: ObjectId,
    ) -> Fallible<web::ImageSmoothingQuality> {
        ctx(cx, this, |c| c.smoothing.get().1)
    }

    fn set_image_smoothing_quality(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: web::ImageSmoothingQuality,
    ) -> Fallible<()> {
        ctx(cx, this, |c| c.smoothing.set((c.smoothing.get().0, value)))
    }
}

impl web::CanvasFillStrokeStylesImpl for Web {
    fn stroke_style(
        cx: &mut Cx<'_>,
        this: ObjectId,
    ) -> Fallible<web::StringOrCanvasGradientOrCanvasPattern> {
        ctx(cx, this, |c| style_report(c, false))
    }

    fn set_stroke_style(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: web::StringOrCanvasGradientOrCanvasPattern,
    ) -> Fallible<()> {
        let Some(style) = style_from(cx, &value) else {
            return Ok(());
        };
        let object = match &style {
            Style::Color(_) => None,
            _ => Some(value),
        };
        ctx(cx, this, |c| {
            *c.stroke_object.borrow_mut() = object;
            c.bitmap.borrow_mut().state_mut().stroke = style;
        })
    }

    fn fill_style(
        cx: &mut Cx<'_>,
        this: ObjectId,
    ) -> Fallible<web::StringOrCanvasGradientOrCanvasPattern> {
        ctx(cx, this, |c| style_report(c, true))
    }

    fn set_fill_style(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: web::StringOrCanvasGradientOrCanvasPattern,
    ) -> Fallible<()> {
        let Some(style) = style_from(cx, &value) else {
            return Ok(());
        };
        let object = match &style {
            Style::Color(_) => None,
            _ => Some(value),
        };
        ctx(cx, this, |c| {
            *c.fill_object.borrow_mut() = object;
            c.bitmap.borrow_mut().state_mut().fill = style;
        })
    }

    fn create_linear_gradient(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    ) -> Fallible<ObjectId> {
        ctx(cx, this, |_| ())?;
        if ![x0, y0, x1, y1].iter().all(|v| v.is_finite()) {
            return Err(Exception::type_error(
                "the gradient coordinates must be finite",
            ));
        }
        Ok(cx.page.alloc(GradientObject {
            style: RefCell::new(Style::Linear {
                x0: f(x0),
                y0: f(y0),
                x1: f(x1),
                y1: f(y1),
                stops: Vec::new(),
            }),
        }))
    }

    fn create_radial_gradient(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x0: f64,
        y0: f64,
        r0: f64,
        x1: f64,
        y1: f64,
        r1: f64,
    ) -> Fallible<ObjectId> {
        ctx(cx, this, |_| ())?;
        if ![x0, y0, r0, x1, y1, r1].iter().all(|v| v.is_finite()) {
            return Err(Exception::type_error(
                "the gradient coordinates must be finite",
            ));
        }
        if r0 < 0.0 || r1 < 0.0 {
            return Err(Exception::dom(
                "IndexSizeError",
                "the radii must not be negative",
            ));
        }
        Ok(cx.page.alloc(GradientObject {
            style: RefCell::new(Style::Radial {
                x0: f(x0),
                y0: f(y0),
                r0: f(r0),
                x1: f(x1),
                y1: f(y1),
                r1: f(r1),
                stops: Vec::new(),
            }),
        }))
    }

    fn create_conic_gradient(
        cx: &mut Cx<'_>,
        this: ObjectId,
        start_angle: f64,
        x: f64,
        y: f64,
    ) -> Fallible<ObjectId> {
        ctx(cx, this, |_| ())?;
        Ok(cx.page.alloc(GradientObject {
            style: RefCell::new(Style::Conic {
                angle: f(start_angle),
                x: f(x),
                y: f(y),
                stops: Vec::new(),
            }),
        }))
    }

    fn create_pattern(
        cx: &mut Cx<'_>,
        this: ObjectId,
        _image: web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement,
        _repetition: String,
    ) -> Fallible<Option<ObjectId>> {
        ctx(cx, this, |_| ())?;
        // Patterns are not drawn yet; a pattern style leaves the current
        // style in place.
        Ok(Some(cx.page.alloc(PatternObject)))
    }
}

impl web::CanvasGradientImpl for Web {
    fn add_color_stop(cx: &mut Cx<'_>, this: ObjectId, offset: f64, color: String) -> Fallible<()> {
        if !(0.0..=1.0).contains(&offset) || !offset.is_finite() {
            return Err(Exception::dom(
                "IndexSizeError",
                "the offset must be between 0 and 1",
            ));
        }
        let color = canvas::parse_color(&color)
            .ok_or_else(|| Exception::dom("SyntaxError", format!("'{color}' is not a color")))?;
        cx.page.with::<GradientObject, _>(this, |g| {
            let mut style = g.style.borrow_mut();
            let stops = match &mut *style {
                Style::Linear { stops, .. }
                | Style::Radial { stops, .. }
                | Style::Conic { stops, .. } => stops,
                Style::Color(_) => return,
            };
            let at = stops.partition_point(|(o, _)| *o <= f(offset));
            stops.insert(at, (f(offset), color));
        })
    }
}

impl web::CanvasPatternImpl for Web {
    fn set_transform(
        cx: &mut Cx<'_>,
        this: ObjectId,
        _transform: web::DOMMatrix2DInit,
    ) -> Fallible<()> {
        cx.page.with::<PatternObject, _>(this, |_| ())
    }
}

impl web::CanvasShadowStylesImpl for Web {
    fn shadow_offset_x(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ctx(cx, this, |c| c.shadow.borrow().0)
    }
    fn set_shadow_offset_x(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        ctx(cx, this, |c| {
            if value.is_finite() {
                c.shadow.borrow_mut().0 = value
            }
        })
    }
    fn shadow_offset_y(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ctx(cx, this, |c| c.shadow.borrow().1)
    }
    fn set_shadow_offset_y(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        ctx(cx, this, |c| {
            if value.is_finite() {
                c.shadow.borrow_mut().1 = value
            }
        })
    }
    fn shadow_blur(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        ctx(cx, this, |c| c.shadow.borrow().2)
    }
    fn set_shadow_blur(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        ctx(cx, this, |c| {
            if value.is_finite() && value >= 0.0 {
                c.shadow.borrow_mut().2 = value
            }
        })
    }
    fn shadow_color(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ctx(cx, this, |c| c.shadow.borrow().3.clone())
    }
    fn set_shadow_color(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        ctx(cx, this, |c| {
            if let Some(color) = canvas::parse_color(&value) {
                c.shadow.borrow_mut().3 = canvas::serialize_color(color);
            }
        })
    }
}

impl web::CanvasFiltersImpl for Web {
    fn filter(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ctx(cx, this, |c| c.filter.borrow().clone())
    }
    fn set_filter(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        ctx(cx, this, |c| *c.filter.borrow_mut() = value)
    }
}

impl web::CanvasRectImpl for Web {
    fn clear_rect(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64, w: f64, h: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| c.clear_rect(f(x), f(y), f(w), f(h)))
    }
    fn fill_rect(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64, w: f64, h: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| c.fill_rect(f(x), f(y), f(w), f(h)))
    }
    fn stroke_rect(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| c.stroke_rect(f(x), f(y), f(w), f(h)))
    }
}

fn even_odd(rule: web::CanvasFillRule) -> bool {
    matches!(rule, web::CanvasFillRule::Evenodd)
}

impl web::CanvasDrawPathImpl for Web {
    fn begin_path(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_canvas(cx, this, |c| c.begin_path())
    }
    fn fill(cx: &mut Cx<'_>, this: ObjectId, fill_rule: web::CanvasFillRule) -> Fallible<()> {
        with_canvas(cx, this, |c| c.fill(even_odd(fill_rule)))
    }
    fn fill_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        path: ObjectId,
        fill_rule: web::CanvasFillRule,
    ) -> Fallible<()> {
        let path = path_of(cx, path)?;
        with_canvas(cx, this, |c| c.fill_path(&path, even_odd(fill_rule)))
    }
    fn stroke(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_canvas(cx, this, |c| c.stroke_current())
    }
    fn stroke_overload2(cx: &mut Cx<'_>, this: ObjectId, path: ObjectId) -> Fallible<()> {
        let path = path_of(cx, path)?;
        with_canvas(cx, this, |c| c.stroke_path(&path))
    }
    fn clip(cx: &mut Cx<'_>, this: ObjectId, fill_rule: web::CanvasFillRule) -> Fallible<()> {
        with_canvas(cx, this, |c| c.clip(None, even_odd(fill_rule)))
    }
    fn clip_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        path: ObjectId,
        fill_rule: web::CanvasFillRule,
    ) -> Fallible<()> {
        let path = path_of(cx, path)?;
        with_canvas(cx, this, |c| c.clip(Some(&path), even_odd(fill_rule)))
    }
    fn is_point_in_path(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x: f64,
        y: f64,
        fill_rule: web::CanvasFillRule,
    ) -> Fallible<bool> {
        with_canvas(cx, this, |c| {
            c.is_point_in_path(None, f(x), f(y), even_odd(fill_rule))
        })
    }
    fn is_point_in_path_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        path: ObjectId,
        x: f64,
        y: f64,
        fill_rule: web::CanvasFillRule,
    ) -> Fallible<bool> {
        let path = path_of(cx, path)?;
        with_canvas(cx, this, |c| {
            c.is_point_in_path(Some(&path), f(x), f(y), even_odd(fill_rule))
        })
    }
    fn is_point_in_stroke(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64) -> Fallible<bool> {
        with_canvas(cx, this, |c| c.is_point_in_stroke(None, f(x), f(y)))
    }
    fn is_point_in_stroke_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        path: ObjectId,
        x: f64,
        y: f64,
    ) -> Fallible<bool> {
        let path = path_of(cx, path)?;
        with_canvas(cx, this, |c| c.is_point_in_stroke(Some(&path), f(x), f(y)))
    }
}

impl web::CanvasTextImpl for Web {
    fn fill_text(
        cx: &mut Cx<'_>,
        this: ObjectId,
        text: String,
        x: f64,
        y: f64,
        max_width: Option<f64>,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.draw_text(&text, f(x), f(y), max_width.map(f), false)
        })
    }
    fn stroke_text(
        cx: &mut Cx<'_>,
        this: ObjectId,
        text: String,
        x: f64,
        y: f64,
        max_width: Option<f64>,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.draw_text(&text, f(x), f(y), max_width.map(f), true)
        })
    }
    fn measure_text(cx: &mut Cx<'_>, this: ObjectId, text: String) -> Fallible<ObjectId> {
        let metrics = with_canvas(cx, this, |c| c.measure_text(&text))?;
        Ok(cx.page.alloc(TextMetricsObject { metrics }))
    }
}

impl web::CanvasDrawImageImpl for Web {
    fn draw_image(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image: web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement,
        dx: f64,
        dy: f64,
    ) -> Fallible<()> {
        let Some(source) = canvas_source(cx, &image)? else {
            return Ok(());
        };
        let (w, h) = (source.width() as f32, source.height() as f32);
        with_canvas(cx, this, |c| {
            c.draw_image(&source, 0.0, 0.0, w, h, f(dx), f(dy), w, h)
        })
    }
    fn draw_image_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image: web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) -> Fallible<()> {
        let Some(source) = canvas_source(cx, &image)? else {
            return Ok(());
        };
        let (w, h) = (source.width() as f32, source.height() as f32);
        with_canvas(cx, this, |c| {
            c.draw_image(&source, 0.0, 0.0, w, h, f(dx), f(dy), f(dw), f(dh))
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn draw_image_overload3(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image: web::HTMLImageElementOrSVGImageElementOrHTMLVideoElementOrHTMLCanvasElement,
        sx: f64,
        sy: f64,
        sw: f64,
        sh: f64,
        dx: f64,
        dy: f64,
        dw: f64,
        dh: f64,
    ) -> Fallible<()> {
        let Some(source) = canvas_source(cx, &image)? else {
            return Ok(());
        };
        with_canvas(cx, this, |c| {
            c.draw_image(
                &source,
                f(sx),
                f(sy),
                f(sw),
                f(sh),
                f(dx),
                f(dy),
                f(dw),
                f(dh),
            )
        })
    }
}

fn new_image_data(cx: &Cx<'_>, width: u32, height: u32, data: Vec<u8>) -> Fallible<ObjectId> {
    if width == 0 || height == 0 {
        return Err(Exception::dom(
            "IndexSizeError",
            "the ImageData size must not be zero",
        ));
    }
    Ok(cx.page.alloc(ImageDataObject {
        width,
        height,
        data,
    }))
}

/// The bytes of an `ImageData` as script sees them now: its `data`
/// array, which script may have written to, else what it was made with.
fn image_data_bytes(cx: &mut Cx<'_>, image_data: ObjectId) -> Fallible<(u32, u32, Vec<u8>)> {
    let (width, height, own) = cx
        .page
        .with::<ImageDataObject, _>(image_data, |d| (d.width, d.height, d.data.clone()))?;
    let live = cx
        .script
        .get_property(&Value::Object(image_data), "data")
        .ok()
        .and_then(|v| cx.script.buffer_bytes(&v))
        .filter(|b| b.len() == own.len());
    Ok((width, height, live.unwrap_or(own)))
}

impl web::CanvasImageDataImpl for Web {
    fn create_image_data(
        cx: &mut Cx<'_>,
        this: ObjectId,
        sw: i32,
        sh: i32,
        _settings: web::ImageDataSettings,
    ) -> Fallible<ObjectId> {
        ctx(cx, this, |_| ())?;
        let (w, h) = (sw.unsigned_abs(), sh.unsigned_abs());
        new_image_data(cx, w, h, vec![0; (w as usize) * (h as usize) * 4])
    }
    fn create_image_data_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image_data: ObjectId,
    ) -> Fallible<ObjectId> {
        ctx(cx, this, |_| ())?;
        let (w, h) = cx
            .page
            .with::<ImageDataObject, _>(image_data, |d| (d.width, d.height))?;
        new_image_data(cx, w, h, vec![0; (w as usize) * (h as usize) * 4])
    }
    fn get_image_data(
        cx: &mut Cx<'_>,
        this: ObjectId,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        _settings: web::ImageDataSettings,
    ) -> Fallible<ObjectId> {
        let (x, w) = if sw < 0 {
            (sx + sw, sw.unsigned_abs())
        } else {
            (sx, sw as u32)
        };
        let (y, h) = if sh < 0 {
            (sy + sh, sh.unsigned_abs())
        } else {
            (sy, sh as u32)
        };
        if w == 0 || h == 0 {
            return Err(Exception::dom(
                "IndexSizeError",
                "the source rectangle is empty",
            ));
        }
        let data = with_canvas(cx, this, |c| c.image_data(x, y, w, h))?;
        new_image_data(cx, w, h, data)
    }
    fn put_image_data(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image_data: ObjectId,
        dx: i32,
        dy: i32,
    ) -> Fallible<()> {
        let (w, h, data) = image_data_bytes(cx, image_data)?;
        with_canvas(cx, this, |c| c.put_image_data(&data, w, h, dx, dy))
    }
    #[allow(clippy::too_many_arguments)]
    fn put_image_data_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        image_data: ObjectId,
        dx: i32,
        dy: i32,
        dirty_x: i32,
        dirty_y: i32,
        dirty_width: i32,
        dirty_height: i32,
    ) -> Fallible<()> {
        let (w, h, data) = image_data_bytes(cx, image_data)?;
        let (mut x0, mut dw) = (dirty_x, dirty_width);
        let (mut y0, mut dh) = (dirty_y, dirty_height);
        if dw < 0 {
            x0 += dw;
            dw = -dw;
        }
        if dh < 0 {
            y0 += dh;
            dh = -dh;
        }
        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = (dirty_x.max(x0) + dw).min(w as i32);
        let y1 = (dirty_y.max(y0) + dh).min(h as i32);
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let (cw, ch) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let mut part = Vec::with_capacity((cw * ch * 4) as usize);
        for row in y0..y1 {
            let start = ((row as u32 * w + x0 as u32) * 4) as usize;
            part.extend_from_slice(&data[start..start + (cw * 4) as usize]);
        }
        with_canvas(cx, this, |c| {
            c.put_image_data(&part, cw, ch, dx + x0, dy + y0)
        })
    }
}

impl web::CanvasPathDrawingStylesImpl for Web {
    fn line_width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        with_canvas(cx, this, |c| f64::from(c.state().line_width))
    }
    fn set_line_width(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if value.is_finite() && value > 0.0 {
                c.state_mut().line_width = f(value)
            }
        })
    }
    fn line_cap(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::CanvasLineCap> {
        with_canvas(cx, this, |c| match c.state().line_cap {
            LineCap::Butt => web::CanvasLineCap::Butt,
            LineCap::Round => web::CanvasLineCap::Round,
            LineCap::Square => web::CanvasLineCap::Square,
        })
    }
    fn set_line_cap(cx: &mut Cx<'_>, this: ObjectId, value: web::CanvasLineCap) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.state_mut().line_cap = match value {
                web::CanvasLineCap::Butt => LineCap::Butt,
                web::CanvasLineCap::Round => LineCap::Round,
                web::CanvasLineCap::Square => LineCap::Square,
            }
        })
    }
    fn line_join(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::CanvasLineJoin> {
        with_canvas(cx, this, |c| match c.state().line_join {
            LineJoin::Round => web::CanvasLineJoin::Round,
            LineJoin::Bevel => web::CanvasLineJoin::Bevel,
            LineJoin::Miter | LineJoin::MiterClip => web::CanvasLineJoin::Miter,
        })
    }
    fn set_line_join(cx: &mut Cx<'_>, this: ObjectId, value: web::CanvasLineJoin) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.state_mut().line_join = match value {
                web::CanvasLineJoin::Round => LineJoin::Round,
                web::CanvasLineJoin::Bevel => LineJoin::Bevel,
                web::CanvasLineJoin::Miter => LineJoin::Miter,
            }
        })
    }
    fn miter_limit(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        with_canvas(cx, this, |c| f64::from(c.state().miter_limit))
    }
    fn set_miter_limit(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if value.is_finite() && value > 0.0 {
                c.state_mut().miter_limit = f(value)
            }
        })
    }
    fn set_line_dash(cx: &mut Cx<'_>, this: ObjectId, segments: Vec<f64>) -> Fallible<()> {
        if segments.iter().any(|v| !v.is_finite() || *v < 0.0) {
            return Ok(());
        }
        with_canvas(cx, this, |c| {
            c.state_mut().dash = segments.iter().map(|v| f(*v)).collect()
        })
    }
    fn get_line_dash(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<f64>> {
        with_canvas(cx, this, |c| {
            let dash = &c.state().dash;
            let mut out: Vec<f64> = dash.iter().map(|v| f64::from(*v)).collect();
            if out.len() % 2 == 1 {
                let copy = out.clone();
                out.extend(copy);
            }
            out
        })
    }
    fn line_dash_offset(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        with_canvas(cx, this, |c| f64::from(c.state().dash_offset))
    }
    fn set_line_dash_offset(cx: &mut Cx<'_>, this: ObjectId, value: f64) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if value.is_finite() {
                c.state_mut().dash_offset = f(value)
            }
        })
    }
}

impl web::CanvasTextDrawingStylesImpl for Web {
    fn font(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        with_canvas(cx, this, |c| c.state().font.css.clone())
    }
    fn set_font(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            if let Some(font) = canvas::Font::parse(&value) {
                c.state_mut().font = font;
            }
        })
    }
    fn text_align(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::CanvasTextAlign> {
        with_canvas(cx, this, |c| match c.state().text_align {
            TextAlign::Start => web::CanvasTextAlign::Start,
            TextAlign::End => web::CanvasTextAlign::End,
            TextAlign::Left => web::CanvasTextAlign::Left,
            TextAlign::Right => web::CanvasTextAlign::Right,
            TextAlign::Center => web::CanvasTextAlign::Center,
        })
    }
    fn set_text_align(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: web::CanvasTextAlign,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.state_mut().text_align = match value {
                web::CanvasTextAlign::Start => TextAlign::Start,
                web::CanvasTextAlign::End => TextAlign::End,
                web::CanvasTextAlign::Left => TextAlign::Left,
                web::CanvasTextAlign::Right => TextAlign::Right,
                web::CanvasTextAlign::Center => TextAlign::Center,
            }
        })
    }
    fn text_baseline(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::CanvasTextBaseline> {
        with_canvas(cx, this, |c| match c.state().text_baseline {
            TextBaseline::Top => web::CanvasTextBaseline::Top,
            TextBaseline::Hanging => web::CanvasTextBaseline::Hanging,
            TextBaseline::Middle => web::CanvasTextBaseline::Middle,
            TextBaseline::Alphabetic => web::CanvasTextBaseline::Alphabetic,
            TextBaseline::Ideographic => web::CanvasTextBaseline::Ideographic,
            TextBaseline::Bottom => web::CanvasTextBaseline::Bottom,
        })
    }
    fn set_text_baseline(
        cx: &mut Cx<'_>,
        this: ObjectId,
        value: web::CanvasTextBaseline,
    ) -> Fallible<()> {
        with_canvas(cx, this, |c| {
            c.state_mut().text_baseline = match value {
                web::CanvasTextBaseline::Top => TextBaseline::Top,
                web::CanvasTextBaseline::Hanging => TextBaseline::Hanging,
                web::CanvasTextBaseline::Middle => TextBaseline::Middle,
                web::CanvasTextBaseline::Alphabetic => TextBaseline::Alphabetic,
                web::CanvasTextBaseline::Ideographic => TextBaseline::Ideographic,
                web::CanvasTextBaseline::Bottom => TextBaseline::Bottom,
            }
        })
    }
    fn direction(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::CanvasDirection> {
        ctx(cx, this, |c| c.direction.get())
    }
    fn set_direction(cx: &mut Cx<'_>, this: ObjectId, value: web::CanvasDirection) -> Fallible<()> {
        ctx(cx, this, |c| {
            c.direction.set(value);
            c.bitmap.borrow_mut().state_mut().direction_rtl =
                matches!(value, web::CanvasDirection::Rtl);
        })
    }
    fn letter_spacing(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ctx(cx, this, |c| c.spacing.borrow().0.clone())
    }
    fn set_letter_spacing(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        ctx(cx, this, |c| c.spacing.borrow_mut().0 = value)
    }
    fn word_spacing(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        ctx(cx, this, |c| c.spacing.borrow().1.clone())
    }
    fn set_word_spacing(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        ctx(cx, this, |c| c.spacing.borrow_mut().1 = value)
    }
}

/// The path a `CanvasPath` member works on: the context's current path,
/// or a `Path2D`'s own.
fn with_path<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut Path2d) -> R) -> Fallible<R> {
    if let Some(bitmap) = cx
        .page
        .try_with::<ContextObject, _>(this, |c| c.bitmap.clone())
    {
        let mut canvas = bitmap.borrow_mut();
        return Ok(f(canvas.path_mut()));
    }
    cx.page
        .with::<Path2DObject, _>(this, |p| f(&mut p.path.borrow_mut()))
}

fn radii_of(radii: web::DoubleOrDOMPointInitOrDoubleOrDOMPointInitSequence) -> Fallible<[f32; 4]> {
    let one = |v: &web::DoubleOrDOMPointInit| -> Fallible<f32> {
        let r = match v {
            web::DoubleOrDOMPointInit::Double(d) => *d,
            web::DoubleOrDOMPointInit::DOMPointInit(p) => p.x.max(p.y),
        };
        if r < 0.0 {
            return Err(Exception::dom("RangeError", "radii must not be negative"));
        }
        Ok(f(r))
    };
    use web::DoubleOrDOMPointInitOrDoubleOrDOMPointInitSequence as R;
    Ok(match radii {
        R::Double(d) => [one(&web::DoubleOrDOMPointInit::Double(d))?; 4],
        R::DOMPointInit(p) => [one(&web::DoubleOrDOMPointInit::DOMPointInit(p))?; 4],
        R::DoubleOrDOMPointInitSequence(list) => {
            let v: Vec<f32> = list.iter().map(one).collect::<Fallible<_>>()?;
            match v.as_slice() {
                [] => [0.0; 4],
                [a] => [*a; 4],
                [a, b] => [*a, *b, *a, *b],
                [a, b, c] => [*a, *b, *c, *b],
                [a, b, c, d, ..] => [*a, *b, *c, *d],
            }
        }
    })
}

impl web::CanvasPathImpl for Web {
    fn close_path(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        with_path(cx, this, |p| p.close())
    }
    fn move_to(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64) -> Fallible<()> {
        with_path(cx, this, |p| p.move_to(f(x), f(y)))
    }
    fn line_to(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64) -> Fallible<()> {
        with_path(cx, this, |p| p.line_to(f(x), f(y)))
    }
    fn quadratic_curve_to(
        cx: &mut Cx<'_>,
        this: ObjectId,
        cpx: f64,
        cpy: f64,
        x: f64,
        y: f64,
    ) -> Fallible<()> {
        with_path(cx, this, |p| p.quad_to(f(cpx), f(cpy), f(x), f(y)))
    }
    #[allow(clippy::too_many_arguments)]
    fn bezier_curve_to(
        cx: &mut Cx<'_>,
        this: ObjectId,
        cp1x: f64,
        cp1y: f64,
        cp2x: f64,
        cp2y: f64,
        x: f64,
        y: f64,
    ) -> Fallible<()> {
        with_path(cx, this, |p| {
            p.cubic_to(f(cp1x), f(cp1y), f(cp2x), f(cp2y), f(x), f(y))
        })
    }
    fn arc_to(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        radius: f64,
    ) -> Fallible<()> {
        if radius < 0.0 {
            return Err(Exception::dom(
                "IndexSizeError",
                "the radius must not be negative",
            ));
        }
        with_path(cx, this, |p| {
            p.arc_to(f(x1), f(y1), f(x2), f(y2), f(radius))
        })
    }
    fn rect(cx: &mut Cx<'_>, this: ObjectId, x: f64, y: f64, w: f64, h: f64) -> Fallible<()> {
        with_path(cx, this, |p| p.rect(f(x), f(y), f(w), f(h)))
    }
    #[allow(clippy::too_many_arguments)]
    fn round_rect(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        radii: web::DoubleOrDOMPointInitOrDoubleOrDOMPointInitSequence,
    ) -> Fallible<()> {
        let radii = radii_of(radii)?;
        with_path(cx, this, |p| p.round_rect(f(x), f(y), f(w), f(h), radii))
    }
    #[allow(clippy::too_many_arguments)]
    fn arc(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x: f64,
        y: f64,
        radius: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> Fallible<()> {
        if radius < 0.0 {
            return Err(Exception::dom(
                "IndexSizeError",
                "the radius must not be negative",
            ));
        }
        with_path(cx, this, |p| {
            p.arc(
                f(x),
                f(y),
                f(radius),
                f(start_angle),
                f(end_angle),
                counterclockwise,
            )
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn ellipse(
        cx: &mut Cx<'_>,
        this: ObjectId,
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        rotation: f64,
        start_angle: f64,
        end_angle: f64,
        counterclockwise: bool,
    ) -> Fallible<()> {
        if radius_x < 0.0 || radius_y < 0.0 {
            return Err(Exception::dom(
                "IndexSizeError",
                "the radii must not be negative",
            ));
        }
        with_path(cx, this, |p| {
            p.ellipse(
                f(x),
                f(y),
                f(radius_x),
                f(radius_y),
                f(rotation),
                f(start_angle),
                f(end_angle),
                counterclockwise,
            )
        })
    }
}

impl web::Path2DImpl for Web {
    fn constructor(cx: &mut Cx<'_>, path: Option<web::Path2DOrString>) -> Fallible<ObjectId> {
        let initial = match path {
            Some(web::Path2DOrString::Path2D(other)) => path_of(cx, other)?,
            // SVG path data is not parsed yet: the path starts empty.
            Some(web::Path2DOrString::String(_)) | None => Path2d::default(),
        };
        Ok(cx.page.alloc(Path2DObject {
            path: RefCell::new(initial),
        }))
    }

    fn add_path(
        cx: &mut Cx<'_>,
        this: ObjectId,
        path: ObjectId,
        transform: web::DOMMatrix2DInit,
    ) -> Fallible<()> {
        let t = matrix(&transform)
            .ok_or_else(|| Exception::type_error("the matrix init is inconsistent"))?;
        let other = path_of(cx, path)?;
        cx.page
            .with::<Path2DObject, _>(this, |p| p.path.borrow_mut().add_path(&other, t))
    }
}

fn metric<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&canvas::TextMetrics) -> R,
) -> Fallible<R> {
    cx.page
        .with::<TextMetricsObject, _>(this, |m| f(&m.metrics))
}

impl web::TextMetricsImpl for Web {
    fn width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.width))
    }
    fn actual_bounding_box_left(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |_| 0.0)
    }
    fn actual_bounding_box_right(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.width))
    }
    fn font_bounding_box_ascent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.font_ascent))
    }
    fn font_bounding_box_descent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.font_descent))
    }
    fn actual_bounding_box_ascent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.ascent))
    }
    fn actual_bounding_box_descent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.descent))
    }
    fn em_height_ascent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.font_ascent))
    }
    fn em_height_descent(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.font_descent))
    }
    fn hanging_baseline(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(m.font_ascent * 0.8))
    }
    fn alphabetic_baseline(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |_| 0.0)
    }
    fn ideographic_baseline(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        metric(cx, this, |m| f64::from(-m.font_descent))
    }
}

impl web::ImageDataImpl for Web {
    fn width(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        cx.page.with::<ImageDataObject, _>(this, |d| d.width)
    }
    fn height(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        cx.page.with::<ImageDataObject, _>(this, |d| d.height)
    }
    fn data(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Uint8ArrayData> {
        cx.page
            .with::<ImageDataObject, _>(this, |d| Uint8ArrayData(d.data.clone()))
    }
    fn color_space(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<web::PredefinedColorSpace> {
        cx.page
            .with::<ImageDataObject, _>(this, |_| web::PredefinedColorSpace::Srgb)
    }
    fn constructor(
        cx: &mut Cx<'_>,
        sw: u32,
        sh: u32,
        _settings: web::ImageDataSettings,
    ) -> Fallible<ObjectId> {
        new_image_data(cx, sw, sh, vec![0; (sw as usize) * (sh as usize) * 4])
    }
    fn constructor_overload2(
        cx: &mut Cx<'_>,
        data: Vec<u8>,
        sw: u32,
        sh: Option<u32>,
        _settings: web::ImageDataSettings,
    ) -> Fallible<ObjectId> {
        if sw == 0 || !data.len().is_multiple_of(4) || data.is_empty() {
            return Err(Exception::dom(
                "InvalidStateError",
                "the data length must be a non-zero multiple of 4",
            ));
        }
        let pixels = (data.len() / 4) as u32;
        if !pixels.is_multiple_of(sw) {
            return Err(Exception::dom(
                "IndexSizeError",
                "the data length is not a multiple of the width",
            ));
        }
        let height = pixels / sw;
        if let Some(sh) = sh
            && sh != height
        {
            return Err(Exception::dom(
                "IndexSizeError",
                "the height does not match the data",
            ));
        }
        new_image_data(cx, sw, height, data)
    }
}
