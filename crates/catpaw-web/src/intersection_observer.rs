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
use crate::{Web, platform_object};

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

impl IntersectionObserverObject {
    /// The root's box grown by the root margin. An element has an empty
    /// box; the viewport does not.
    fn root_bounds(&self, page: &PageState) -> Rect {
        let (width, height) = match self.root {
            Root::Element(_) => (0.0, 0.0),
            Root::Implicit | Root::Document(_) => (
                f64::from(page.config.viewport_width),
                f64::from(page.config.viewport_height),
            ),
        };
        let [top, right, bottom, left] = self.root_margin;
        let (top, bottom) = (top.resolve(height), bottom.resolve(height));
        let (left, right) = (left.resolve(width), right.resolve(width));
        Rect {
            x: -left,
            y: -top,
            width: (width + left + right).max(0.0),
            height: (height + top + bottom).max(0.0),
        }
    }
}

pub struct IntersectionObserverEntryObject {
    time: f64,
    root_bounds: Rect,
    is_intersecting: bool,
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

/// Asks for a frame if targets may have entered or left the tree since the
/// last update. Called by the event loop before it decides what to wait for.
pub(crate) fn request_frame_if_stale(page: &PageState) {
    let state = &page.intersection;
    if state.observing.borrow().is_empty() {
        return;
    }
    if state.seen.get() != Some(page.dom.borrow().version()) {
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
        .set(Some(page.dom.borrow().version()));
    let time = page.clock.now();
    for observer in observers {
        // The targets whose state changed, with their new state.
        let changed = page.try_with::<IntersectionObserverObject, _>(observer, |o| {
            let dom = page.dom.borrow();
            // An observer whose root is out of the document is passed over.
            let root_in_document = match o.root {
                Root::Implicit => true,
                Root::Document(root) | Root::Element(root) => {
                    dom.contains(root) && dom.is_connected(root)
                }
            };
            if !root_in_document {
                return None;
            }
            let (root, thresholds) = (o.root, &o.thresholds);
            let mut changed = Vec::new();
            for target in &mut o.targets {
                let is_intersecting = intersects(&dom, root, target.node);
                let ratio = if is_intersecting { 1.0 } else { 0.0 };
                let index = thresholds
                    .iter()
                    .position(|&threshold| threshold > ratio)
                    .unwrap_or(thresholds.len()) as i32;
                if index != target.previous_threshold_index
                    || is_intersecting != target.previous_is_intersecting
                {
                    changed.push((target.node, is_intersecting));
                }
                target.previous_threshold_index = index;
                target.previous_is_intersecting = is_intersecting;
            }
            Some((o.root_bounds(page), changed))
        });
        let Some(Some((root_bounds, changed))) = changed else {
            continue;
        };
        if changed.is_empty() {
            continue;
        }
        let entries: Vec<ObjectId> = changed
            .into_iter()
            .map(|(target, is_intersecting)| {
                page.alloc(IntersectionObserverEntryObject {
                    time,
                    root_bounds,
                    is_intersecting,
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
        entry(cx, this, |_| ())?;
        Ok(rect_object(cx.page, EMPTY))
    }

    fn intersection_rect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        entry(cx, this, |_| ())?;
        Ok(rect_object(cx.page, EMPTY))
    }

    fn is_intersecting(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        entry(cx, this, |e| e.is_intersecting)
    }

    fn intersection_ratio(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        entry(cx, this, |e| if e.is_intersecting { 1.0 } else { 0.0 })
    }

    fn target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        entry(cx, this, |e| e.target)
    }
}
