//! `ResizeObserver` (<https://drafts.csswg.org/resize-observer/>).
//!
//! There is no layout yet (M1): every box is empty, which is also the size
//! an observation starts out with, so there is never a change to report.
//! Observers can be created and pointed at elements; their callbacks do not
//! run.

use catpaw_dom::{NodeId, NodeKind};
use catpaw_js::{Callback, Exception, Fallible, ObjectId};

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct ResizeObserverObject {
    /// Not called until there is layout (see the module documentation).
    #[allow(dead_code)]
    callback: Callback,
    /// The observed elements, each with the box whose size is watched.
    targets: Vec<(NodeId, web::ResizeObserverBoxOptions)>,
}
platform_object!(ResizeObserverObject, ResizeObserver);

fn observer<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ResizeObserverObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ResizeObserverObject, _>(this, f)
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
            o.targets.retain(|(node, _)| *node != target);
            o.targets.push((target, options.box_));
        })
    }

    fn unobserve(cx: &mut Cx<'_>, this: ObjectId, target: NodeId) -> Fallible<()> {
        observer(cx, this, |o| o.targets.retain(|(node, _)| *node != target))
    }

    fn disconnect(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        observer(cx, this, |o| o.targets.clear())
    }
}
