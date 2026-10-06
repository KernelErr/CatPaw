//! `AbortController` and `AbortSignal`
//! (<https://dom.spec.whatwg.org/#aborting-ongoing-activities>).

use std::rc::Rc;

use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value};

use crate::event_loop::{self, TimerAction};
use crate::generated as web;
use crate::page::{Cx, PageState};
use crate::{Web, events, platform_object};

/// Something the platform does when a signal aborts (cancel a request,
/// drop a listener), given the abort reason.
pub type AbortAlgorithm = Rc<dyn Fn(&mut Cx<'_>, &Value)>;

pub struct AbortSignalObject {
    aborted: bool,
    reason: Value,
    algorithms: Vec<AbortAlgorithm>,
}
platform_object!(AbortSignalObject, AbortSignal);

pub struct AbortControllerObject {
    signal: ObjectId,
}
platform_object!(AbortControllerObject, AbortController);

/// Creates a signal that has not been aborted.
pub fn new_signal(page: &PageState) -> ObjectId {
    page.alloc(AbortSignalObject {
        aborted: false,
        reason: Value::Undefined,
        algorithms: Vec::new(),
    })
}

/// The reason `signal` was aborted with, if it was. A signal that no
/// longer exists can never abort.
pub fn abort_reason(page: &PageState, signal: ObjectId) -> Option<Value> {
    page.try_with::<AbortSignalObject, _>(signal, |s| s.aborted.then(|| s.reason.clone()))
        .flatten()
}

/// Runs `algorithm` when `signal` aborts (it must not be aborted yet).
pub fn add_algorithm(page: &PageState, signal: ObjectId, algorithm: AbortAlgorithm) {
    let _ = page.with::<AbortSignalObject, _>(signal, |s| {
        if !s.aborted {
            s.algorithms.push(algorithm);
        }
    });
}

/// <https://dom.spec.whatwg.org/#abortsignal-signal-abort>
pub fn signal_abort(cx: &mut Cx<'_>, signal: ObjectId, reason: Value) {
    let reason = if reason.is_undefined() {
        cx.script
            .exception_value(&Exception::abort("signal is aborted without reason"))
    } else {
        reason
    };
    let algorithms = cx.page.with::<AbortSignalObject, _>(signal, |s| {
        if s.aborted {
            return None;
        }
        s.aborted = true;
        s.reason = reason.clone();
        Some(std::mem::take(&mut s.algorithms))
    });
    let Ok(Some(algorithms)) = algorithms else {
        return;
    };
    for algorithm in algorithms {
        algorithm(cx, &reason);
    }
    events::fire(cx, EventTargetRef::Object(signal), "abort", false, false);
}

impl web::AbortControllerImpl for Web {
    fn signal(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page.with::<AbortControllerObject, _>(this, |c| c.signal)
    }

    fn abort(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<()> {
        let signal = cx
            .page
            .with::<AbortControllerObject, _>(this, |c| c.signal)?;
        signal_abort(cx, signal, reason);
        Ok(())
    }

    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        let signal = new_signal(cx.page);
        Ok(cx.page.alloc(AbortControllerObject { signal }))
    }
}

impl web::AbortSignalImpl for Web {
    fn abort(cx: &mut Cx<'_>, reason: Value) -> Fallible<ObjectId> {
        let signal = new_signal(cx.page);
        signal_abort(cx, signal, reason);
        Ok(signal)
    }

    fn timeout(cx: &mut Cx<'_>, milliseconds: u64) -> Fallible<ObjectId> {
        let signal = new_signal(cx.page);
        let delay = i32::try_from(milliseconds).unwrap_or(i32::MAX);
        event_loop::set_timer(
            cx.page,
            TimerAction::Native(Rc::new(move |cx| {
                let reason = cx
                    .script
                    .exception_value(&Exception::timeout("signal timed out"));
                signal_abort(cx, signal, reason);
            })),
            delay,
            false,
        );
        Ok(signal)
    }

    fn any(cx: &mut Cx<'_>, signals: Vec<ObjectId>) -> Fallible<ObjectId> {
        let result = new_signal(cx.page);
        // Already aborted: the result starts out aborted with that reason.
        for &source in &signals {
            if let Some(reason) = abort_reason(cx.page, source) {
                signal_abort(cx, result, reason);
                return Ok(result);
            }
        }
        for source in signals {
            add_algorithm(
                cx.page,
                source,
                Rc::new(move |cx, reason| signal_abort(cx, result, reason.clone())),
            );
        }
        Ok(result)
    }

    fn aborted(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        cx.page.with::<AbortSignalObject, _>(this, |s| s.aborted)
    }

    fn reason(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        cx.page
            .with::<AbortSignalObject, _>(this, |s| s.reason.clone())
    }

    fn throw_if_aborted(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        match abort_reason(cx.page, this) {
            Some(reason) => Err(Exception::Value(reason)),
            None => Ok(()),
        }
    }
}
