//! `ResizeObserver` (<https://drafts.csswg.org/resize-observer/>).
//!
//! Observations are gathered in a frame, after animation callbacks: each
//! target's box is measured against the size last reported for it, and the
//! observers with changes are called with their entries, at once. An
//! element that is not rendered has no size, which it reports once.

use std::cell::{Cell, RefCell};

use catpaw_dom::{NodeId, NodeKind};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::element::RectObject;
use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, PageState};
use crate::{Web, event_loop, layout, platform_object};

/// One observed element.
struct Target {
    node: NodeId,
    box_: web::ResizeObserverBoxOptions,
    /// The size last reported for the watched box, `(inline, block)`.
    /// Starts out at zero, so a first observation of anything with a size
    /// is reported.
    last: (f32, f32),
}

pub struct ResizeObserverObject {
    callback: Callback,
    targets: Vec<Target>,
}
platform_object!(ResizeObserverObject, ResizeObserver);

/// The sizes of one element's boxes.
#[derive(Clone, Copy, Default)]
struct Sizes {
    /// Padding-edge offsets and content size, the entry's `contentRect`.
    content_x: f32,
    content_y: f32,
    content: (f32, f32),
    border: (f32, f32),
    device: (f32, f32),
}

pub struct ResizeObserverEntryObject {
    target: NodeId,
    sizes: Sizes,
}
platform_object!(ResizeObserverEntryObject, ResizeObserverEntry);

pub struct ResizeObserverSizeObject {
    inline: f32,
    block: f32,
}
platform_object!(ResizeObserverSizeObject, ResizeObserverSize);

/// The page's resize observer state.
#[derive(Default)]
pub(crate) struct Observers {
    /// The observers with targets, in the order they began observing.
    observing: RefCell<Vec<ObjectId>>,
    /// The geometry version the last gathering looked at.
    seen: Cell<Option<u64>>,
}

/// Asks for a frame if sizes may have changed since the last gathering.
pub(crate) fn request_frame_if_stale(page: &PageState) {
    let state = &page.resize;
    if state.observing.borrow().is_empty() {
        return;
    }
    if state.seen.get() != Some(layout::geometry_version(page)) {
        event_loop::request_frame(page);
    }
}

fn measure(page: &PageState, node: NodeId) -> Sizes {
    let connected = {
        let dom = page.dom.borrow();
        dom.contains(node) && dom.is_connected(node)
    };
    if !connected {
        return Sizes::default();
    }
    layout::with_layout(page, |tree, _| {
        let Some(id) = tree.box_of(node) else {
            return Sizes::default();
        };
        let border = tree.border_box(id);
        let content = tree.content_box(id);
        let dpr = page.config.device_pixel_ratio as f32;
        Sizes {
            content_x: content.x - border.x,
            content_y: content.y - border.y,
            content: (content.width, content.height),
            border: (border.width, border.height),
            device: (
                (content.width * dpr).round(),
                (content.height * dpr).round(),
            ),
        }
    })
}

/// Gathers and broadcasts observations: part of a frame, after animation
/// callbacks.
pub(crate) fn update(cx: &mut Cx<'_>) {
    let page = cx.page;
    let observers = page.resize.observing.borrow().clone();
    if observers.is_empty() {
        return;
    }
    page.resize.seen.set(Some(layout::geometry_version(page)));
    for observer in observers {
        let (callback, entries) = {
            // Measured with the object arena free: a layout can touch
            // other objects (adopted style sheets).
            let Some((callback, nodes)) = page.try_with::<ResizeObserverObject, _>(observer, |o| {
                let nodes: Vec<NodeId> = o.targets.iter().map(|t| t.node).collect();
                (o.callback.clone(), nodes)
            }) else {
                continue;
            };
            let measured: Vec<(usize, Sizes)> = nodes
                .iter()
                .enumerate()
                .map(|(i, node)| (i, measure(page, *node)))
                .collect();
            let mut changed = Vec::new();
            page.try_with::<ResizeObserverObject, _>(observer, |o| {
                for (index, sizes) in measured {
                    let Some(target) = o.targets.get_mut(index) else {
                        continue;
                    };
                    let watched = match target.box_ {
                        web::ResizeObserverBoxOptions::BorderBox => sizes.border,
                        web::ResizeObserverBoxOptions::ContentBox => sizes.content,
                        web::ResizeObserverBoxOptions::DevicePixelContentBox => sizes.device,
                    };
                    if watched == target.last {
                        continue;
                    }
                    target.last = watched;
                    changed.push((target.node, sizes));
                }
            });
            // Allocated once the observer is no longer borrowed.
            let entries: Vec<ObjectId> = changed
                .into_iter()
                .map(|(target, sizes)| page.alloc(ResizeObserverEntryObject { target, sizes }))
                .collect();
            (callback, entries)
        };
        if entries.is_empty() {
            continue;
        }
        for entry in &entries {
            cx.pin(*entry);
        }
        let list = Value::Array(entries.iter().copied().map(Value::Object).collect());
        let this = Value::Object(observer);
        if let Err(e) = cx.script.call(&callback, &this, &[list, this.clone()]) {
            cx.report_exception(&e);
        }
        for entry in entries {
            cx.unpin(entry);
        }
    }
}

fn observer<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ResizeObserverObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ResizeObserverObject, _>(this, f)
}

/// Keeps the page's list of observing observers in step with whether this
/// one has targets.
fn sync_observing(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
    let has_targets = observer(cx, this, |o| !o.targets.is_empty())?;
    let listed = cx.page.resize.observing.borrow().contains(&this);
    if has_targets && !listed {
        // An observer is kept alive by what it observes.
        cx.pin(this);
        cx.page.resize.observing.borrow_mut().push(this);
        event_loop::request_frame(cx.page);
    } else if !has_targets && listed {
        cx.page.resize.observing.borrow_mut().retain(|o| *o != this);
        cx.unpin(this);
    }
    Ok(())
}

impl web::ResizeObserverImpl for Web {
    fn constructor(cx: &mut Cx<'_>, callback: Callback) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(ResizeObserverObject {
            callback,
            targets: Vec::new(),
        }))
    }

    fn observe(
        cx: &mut Cx<'_>,
        this: ObjectId,
        target: NodeId,
        options: web::ResizeObserverOptions,
    ) -> Fallible<()> {
        if !matches!(cx.dom().kind(target), NodeKind::Element(_)) {
            return Err(Exception::type_error("The target is not an element"));
        }
        // Observing a target again starts its observation over.
        observer(cx, this, |o| {
            o.targets.retain(|t| t.node != target);
            o.targets.push(Target {
                node: target,
                box_: options.box_,
                last: (0.0, 0.0),
            });
        })?;
        sync_observing(cx, this)
    }

    fn unobserve(cx: &mut Cx<'_>, this: ObjectId, target: NodeId) -> Fallible<()> {
        observer(cx, this, |o| o.targets.retain(|t| t.node != target))?;
        sync_observing(cx, this)
    }

    fn disconnect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        observer(cx, this, |o| o.targets.clear())?;
        sync_observing(cx, this)
    }
}

fn entry<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&ResizeObserverEntryObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ResizeObserverEntryObject, _>(this, |e| f(e))
}

fn size_list(page: &PageState, (inline, block): (f32, f32)) -> Vec<ObjectId> {
    vec![page.alloc(ResizeObserverSizeObject { inline, block })]
}

impl web::ResizeObserverEntryImpl for Web {
    fn target(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<NodeId> {
        entry(cx, this, |e| e.target)
    }

    fn content_rect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let sizes = entry(cx, this, |e| e.sizes)?;
        Ok(cx.page.alloc(RectObject {
            iface: InterfaceId::DOMRectReadOnly,
            x: f64::from(sizes.content_x),
            y: f64::from(sizes.content_y),
            width: f64::from(sizes.content.0),
            height: f64::from(sizes.content.1),
        }))
    }

    fn border_box_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        let sizes = entry(cx, this, |e| e.sizes)?;
        Ok(size_list(cx.page, sizes.border))
    }

    fn content_box_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        let sizes = entry(cx, this, |e| e.sizes)?;
        Ok(size_list(cx.page, sizes.content))
    }

    fn device_pixel_content_box_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        let sizes = entry(cx, this, |e| e.sizes)?;
        Ok(size_list(cx.page, sizes.device))
    }
}

impl web::ResizeObserverSizeImpl for Web {
    fn inline_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        cx.page
            .with::<ResizeObserverSizeObject, _>(this, |s| f64::from(s.inline))
    }

    fn block_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        cx.page
            .with::<ResizeObserverSizeObject, _>(this, |s| f64::from(s.block))
    }
}
