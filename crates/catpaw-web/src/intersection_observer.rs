//! `IntersectionObserver` (<https://w3c.github.io/IntersectionObserver/>).
//!
//! There is no layout yet (M1), so every box is taken to be empty and at
//! the origin. By the standard's own rules for empty boxes a target then
//! intersects its root exactly when it is in the root's tree, with a ratio
//! of 1. Observers therefore see every target as visible: once when it is
//! first observed, and again whenever it leaves or enters the tree.

use std::cell::{Cell, RefCell};

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::element::RectObject;
use crate::event_loop;
use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, layout, platform_object};

/// One side of a root margin.
#[derive(Clone, Copy)]
enum Margin {
    Px(f64),
    Percent(f64),
}

impl Margin {
    fn resolve(self, basis: f64) -> f64 {
        match self {
            Margin::Px(px) => px,
            Margin::Percent(percent) => basis * percent / 100.0,
        }
    }
}

/// Top, right, bottom, left.
type Margins = [Margin; 4];

/// <https://w3c.github.io/IntersectionObserver/#parse-a-margin>
fn parse_margins(text: &str, name: &str) -> Fallible<Margins> {
    let invalid = || {
        Exception::dom(
            "SyntaxError",
            format!("{name} must be specified in pixels or percent."),
        )
    };
    let mut sides = Vec::new();
    for token in text.split_ascii_whitespace() {
        let token = token.to_ascii_lowercase();
        let (number, percent) = match (token.strip_suffix("px"), token.strip_suffix('%')) {
            (Some(number), _) => (number, false),
            (_, Some(number)) => (number, true),
            _ => return Err(invalid()),
        };
        let numeric = number.contains(|c: char| c.is_ascii_digit())
            && number
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e'));
        let value = number
            .parse::<f64>()
            .ok()
            .filter(|v| numeric && v.is_finite());
        let value = value.ok_or_else(invalid)?;
        sides.push(if percent {
            Margin::Percent(value)
        } else {
            Margin::Px(value)
        });
    }
    Ok(match sides[..] {
        [] => [Margin::Px(0.0); 4],
        [all] => [all; 4],
        [vertical, horizontal] => [vertical, horizontal, vertical, horizontal],
        [top, horizontal, bottom] => [top, horizontal, bottom, horizontal],
        [top, right, bottom, left] => [top, right, bottom, left],
        _ => {
            return Err(Exception::dom(
                "SyntaxError",
                format!("{name} takes at most four values."),
            ));
        }
    })
}

fn serialize_margins(margins: &Margins) -> String {
    let sides: Vec<String> = margins
        .iter()
        .map(|side| match side {
            Margin::Px(px) => format!("{px}px"),
            Margin::Percent(percent) => format!("{percent}%"),
        })
        .collect();
    sides.join(" ")
}

#[derive(Clone, Copy)]
enum Root {
    /// The page's viewport.
    Implicit,
    Document(NodeId),
    Element(NodeId),
}

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

const EMPTY: Rect = Rect {
    x: 0.0,
    y: 0.0,
    width: 0.0,
    height: 0.0,
};

fn rect_object(page: &PageState, rect: Rect) -> ObjectId {
    page.alloc(RectObject {
        iface: InterfaceId::DOMRectReadOnly,
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    })
}

struct Target {
    node: NodeId,
    /// -1 until the target has been looked at.
    previous_threshold_index: i32,
    previous_is_intersecting: bool,
}

pub struct IntersectionObserverObject {
    callback: Callback,
    root: Root,
    root_margin: Margins,
    scroll_margin: Margins,
    thresholds: Vec<f64>,
    targets: Vec<Target>,
    /// Entries waiting to be delivered.
    queued: Vec<ObjectId>,
}
platform_object!(IntersectionObserverObject, IntersectionObserver);

impl Rect {
    fn from_layout(rect: catpaw_layout::Rect) -> Self {
        Self {
            x: f64::from(rect.x),
            y: f64::from(rect.y),
            width: f64::from(rect.width),
            height: f64::from(rect.height),
        }
    }

    fn area(&self) -> f64 {
        self.width * self.height
    }

    /// The overlap of two rectangles, or `None` when they are apart. Edges
    /// that touch count as overlapping with no area.
    fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right >= x && bottom >= y).then_some(Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

/// The root intersection rectangle of `root` with `root_margin` applied.
fn root_bounds_of(page: &PageState, root: Root, root_margin: [Margin; 4]) -> Rect {
    {
        let base = match root {
            Root::Element(root) => {
                let border = layout::bounding_client_rect(page, root);
                let client = layout::client_box(page, root);
                Rect {
                    x: f64::from(border.x + client.x),
                    y: f64::from(border.y + client.y),
                    width: f64::from(client.width),
                    height: f64::from(client.height),
                }
            }
            Root::Implicit | Root::Document(_) => Rect {
                x: 0.0,
                y: 0.0,
                width: f64::from(page.config.viewport_width),
                height: f64::from(page.config.viewport_height),
            },
        };
        let [top, right, bottom, left] = root_margin;
        let (top, bottom) = (top.resolve(base.height), bottom.resolve(base.height));
        let (left, right) = (left.resolve(base.width), right.resolve(base.width));
        Rect {
            x: base.x - left,
            y: base.y - top,
            width: (base.width + left + right).max(0.0),
            height: (base.height + top + bottom).max(0.0),
        }
    }
}

/// What one observation of a target found.
#[derive(Clone, Copy)]
struct Observation {
    bounding: Rect,
    intersection: Rect,
    ratio: f64,
    is_intersecting: bool,
}

/// Observes `target` against `root_bounds`.
fn observe(page: &PageState, root_bounds: &Rect, target: NodeId) -> Observation {
    let bounding = Rect::from_layout(layout::bounding_client_rect(page, target));
    match bounding.intersection(root_bounds) {
        Some(intersection) => {
            let ratio = if bounding.area() > 0.0 {
                (intersection.area() / bounding.area()).clamp(0.0, 1.0)
            } else {
                1.0
            };
            Observation {
                bounding,
                intersection,
                ratio,
                is_intersecting: true,
            }
        }
        None => Observation {
            bounding,
            intersection: EMPTY,
            ratio: 0.0,
            is_intersecting: false,
        },
    }
}

pub struct IntersectionObserverEntryObject {
    time: f64,
    root_bounds: Rect,
    observation: Observation,
    target: NodeId,
}
platform_object!(IntersectionObserverEntryObject, IntersectionObserverEntry);

/// The page's intersection observer state.
#[derive(Default)]
pub(crate) struct Observers {
    /// The observers that have targets, in the order they began observing.
    observing: RefCell<Vec<ObjectId>>,
    /// The observers that have entries to deliver.
    notify: RefCell<Vec<ObjectId>>,
    /// The task that delivers them is queued.
    task_queued: Cell<bool>,
    /// The DOM version the last update looked at.
    seen: Cell<Option<u64>>,
}

/// Asks for a frame if targets may have moved since the last update: the
/// document or a scroll position changed. Called by the event loop before
/// it decides what to wait for.
pub(crate) fn request_frame_if_stale(page: &PageState) {
    let state = &page.intersection;
    if state.observing.borrow().is_empty() {
        return;
    }
    if state.seen.get() != Some(layout::geometry_version(page)) {
        event_loop::request_frame(page);
    }
}

/// Whether `target` is rendered inside `root`.
fn intersects(dom: &Dom, root: Root, target: NodeId) -> bool {
    if !dom.contains(target) {
        return false;
    }
    match root {
        Root::Implicit => dom.is_connected(target),
        Root::Document(document) => dom.root_of(target) == document,
        Root::Element(root) => dom.ancestors(target).any(|ancestor| ancestor == root),
    }
}

/// <https://w3c.github.io/IntersectionObserver/#update-intersection-observations-algo>,
/// part of a frame.
pub(crate) fn update(cx: &mut Cx<'_>) {
    let page = cx.page;
    let observers = page.intersection.observing.borrow().clone();
    if observers.is_empty() {
        return;
    }
    page.intersection
        .seen
        .set(Some(layout::geometry_version(page)));
    let time = page.clock.now();
    for observer in observers {
        // Geometry is computed with the object arena free: a layout can
        // touch other objects (adopted style sheets).
        let snapshot = page.try_with::<IntersectionObserverObject, _>(observer, |o| {
            let targets: Vec<(NodeId, i32, bool)> = o
                .targets
                .iter()
                .map(|t| {
                    (
                        t.node,
                        t.previous_threshold_index,
                        t.previous_is_intersecting,
                    )
                })
                .collect();
            (o.root, o.root_margin, o.thresholds.clone(), targets)
        });
        let Some((root, root_margin, thresholds, targets)) = snapshot else {
            continue;
        };
        let root_in_document = {
            let dom = page.dom.borrow();
            match root {
                Root::Implicit => true,
                Root::Document(r) | Root::Element(r) => dom.contains(r) && dom.is_connected(r),
            }
        };
        // An observer whose root is out of the document is passed over.
        if !root_in_document {
            continue;
        }
        let root_bounds = root_bounds_of(page, root, root_margin);
        let mut changed = Vec::new();
        let mut states = Vec::with_capacity(targets.len());
        for (node, previous_index, previous_intersecting) in targets {
            let intersecting = {
                let dom = page.dom.borrow();
                intersects(&dom, root, node)
            };
            let observation = if intersecting {
                observe(page, &root_bounds, node)
            } else {
                Observation {
                    bounding: EMPTY,
                    intersection: EMPTY,
                    ratio: 0.0,
                    is_intersecting: false,
                }
            };
            let index = thresholds
                .iter()
                .position(|&threshold| threshold > observation.ratio)
                .unwrap_or(thresholds.len()) as i32;
            if index != previous_index || observation.is_intersecting != previous_intersecting {
                changed.push((node, observation));
            }
            states.push((node, index, observation.is_intersecting));
        }
        let _ = page.try_with::<IntersectionObserverObject, _>(observer, |o| {
            for (node, index, intersecting) in &states {
                if let Some(target) = o.targets.iter_mut().find(|t| t.node == *node) {
                    target.previous_threshold_index = *index;
                    target.previous_is_intersecting = *intersecting;
                }
            }
        });

        if changed.is_empty() {
            continue;
        }
        let entries: Vec<ObjectId> = changed
            .into_iter()
            .map(|(target, observation)| {
                page.alloc(IntersectionObserverEntryObject {
                    time,
                    root_bounds,
                    observation,
                    target,
                })
            })
            .collect();
        let was_empty = page.try_with::<IntersectionObserverObject, _>(observer, |o| {
            let was_empty = o.queued.is_empty();
            o.queued.extend(entries);
            was_empty
        });
        if was_empty == Some(true) {
            // Entries keep their observer alive until they are delivered.
            cx.pin(observer);
            page.intersection.notify.borrow_mut().push(observer);
        }
        if !page.intersection.task_queued.replace(true) {
            event_loop::queue_task(page, "intersection observer", notify);
        }
    }
}

/// <https://w3c.github.io/IntersectionObserver/#notify-intersection-observers-algo>
fn notify(cx: &mut Cx<'_>) {
    let page = cx.page;
    page.intersection.task_queued.set(false);
    let observers = std::mem::take(&mut *page.intersection.notify.borrow_mut());
    for observer in observers {
        let taken = page.try_with::<IntersectionObserverObject, _>(observer, |o| {
            (o.callback.clone(), std::mem::take(&mut o.queued))
        });
        let Some((callback, entries)) = taken else {
            continue;
        };
        // Taken with `takeRecords()` in the meantime.
        if entries.is_empty() {
            continue;
        }
        let entries = Value::Array(entries.into_iter().map(Value::Object).collect());
        let this = Value::Object(observer);
        if let Err(e) = cx.script.call(&callback, &this, &[entries, this.clone()]) {
            cx.report_exception(&e);
        }
        cx.unpin(observer);
    }
}

fn observer<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut IntersectionObserverObject) -> R,
) -> Fallible<R> {
    cx.page.with::<IntersectionObserverObject, _>(this, f)
}

/// Stops observing the targets `drop` selects.
fn stop_observing(cx: &mut Cx<'_>, this: ObjectId, drop: impl Fn(&Target) -> bool) -> Fallible<()> {
    let stopped = observer(cx, this, |o| {
        let had_targets = !o.targets.is_empty();
        o.targets.retain(|target| !drop(target));
        had_targets && o.targets.is_empty()
    })?;
    if stopped {
        cx.page
            .intersection
            .observing
            .borrow_mut()
            .retain(|o| *o != this);
        cx.unpin(this);
    }
    Ok(())
}

impl web::IntersectionObserverImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        callback: Callback,
        options: web::IntersectionObserverInit,
    ) -> Fallible<ObjectId> {
        let root_margin = parse_margins(&options.root_margin, "rootMargin")?;
        let scroll_margin = parse_margins(&options.scroll_margin, "scrollMargin")?;
        let mut thresholds = match options.threshold {
            web::DoubleOrDoubleSequence::Double(threshold) => vec![threshold],
            web::DoubleOrDoubleSequence::DoubleSequence(thresholds) => thresholds,
        };
        if thresholds.iter().any(|t| !(0.0..=1.0).contains(t)) {
            return Err(Exception::range_error(
                "Threshold values must be numbers between 0 and 1",
            ));
        }
        thresholds.sort_by(f64::total_cmp);
        if thresholds.is_empty() {
            thresholds.push(0.0);
        }
        let root = match options.root {
            None => Root::Implicit,
            Some(web::ElementOrDocument::Document(document)) => Root::Document(document),
            Some(web::ElementOrDocument::Element(element)) => Root::Element(element),
        };
        Ok(cx.page.alloc(IntersectionObserverObject {
            callback,
            root,
            root_margin,
            scroll_margin,
            thresholds,
            targets: Vec::new(),
            queued: Vec::new(),
        }))
    }

    fn root(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<web::ElementOrDocument>> {
        observer(cx, this, |o| match o.root {
            Root::Implicit => None,
            Root::Document(document) => Some(web::ElementOrDocument::Document(document)),
            Root::Element(element) => Some(web::ElementOrDocument::Element(element)),
        })
    }

    fn root_margin(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        observer(cx, this, |o| serialize_margins(&o.root_margin))
    }

    fn scroll_margin(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        observer(cx, this, |o| serialize_margins(&o.scroll_margin))
    }

    fn thresholds(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<f64>> {
        observer(cx, this, |o| o.thresholds.clone())
    }

    fn observe(cx: &mut Cx<'_>, this: ObjectId, target: NodeId) -> Fallible<()> {
        if !matches!(cx.dom().kind(target), NodeKind::Element(_)) {
            return Err(Exception::type_error("The target is not an element"));
        }
        let started = observer(cx, this, |o| {
            if o.targets.iter().any(|t| t.node == target) {
                return None;
            }
            o.targets.push(Target {
                node: target,
                previous_threshold_index: -1,
                previous_is_intersecting: false,
            });
            Some(o.targets.len() == 1)
        })?;
        match started {
            None => return Ok(()),
            Some(true) => {
                // An observer is kept alive by what it observes.
                cx.pin(this);
                cx.page.intersection.observing.borrow_mut().push(this);
            }
            Some(false) => {}
        }
        // The first look at the target happens in the next frame.
        event_loop::request_frame(cx.page);
        Ok(())
    }

    fn unobserve(cx: &mut Cx<'_>, this: ObjectId, target: NodeId) -> Fallible<()> {
        stop_observing(cx, this, |t| t.node == target)
    }

    fn disconnect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        stop_observing(cx, this, |_| true)
    }

    fn take_records(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        let entries = observer(cx, this, |o| std::mem::take(&mut o.queued))?;
        if !entries.is_empty() {
            cx.page
                .intersection
                .notify
                .borrow_mut()
                .retain(|o| *o != this);
            cx.unpin(this);
        }
        Ok(entries)
    }
}

fn entry<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&IntersectionObserverEntryObject) -> R,
) -> Fallible<R> {
    cx.page
        .with::<IntersectionObserverEntryObject, _>(this, |e| f(e))
}

impl web::IntersectionObserverEntryImpl for Web {
    fn time(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        entry(cx, this, |e| e.time)
    }

    fn root_bounds(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<ObjectId>> {
        let bounds = entry(cx, this, |e| e.root_bounds)?;
        Ok(Some(rect_object(cx.page, bounds)))
    }

    fn bounding_client_rect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let rect = entry(cx, this, |e| e.observation.bounding)?;
        Ok(rect_object(cx.page, rect))
    }

    fn intersection_rect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let rect = entry(cx, this, |e| e.observation.intersection)?;
        Ok(rect_object(cx.page, rect))
    }

    fn is_intersecting(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        entry(cx, this, |e| e.observation.is_intersecting)
    }

    fn intersection_ratio(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        entry(cx, this, |e| e.observation.ratio)
    }

    fn target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        entry(cx, this, |e| e.target)
    }
}
