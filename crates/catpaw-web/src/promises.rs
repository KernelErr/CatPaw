//! Reacting to promises from Rust: `when_settled` runs a closure once a
//! script value (a promise, or anything else) settles.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_js::Value;

use crate::page::Cx;

type Settled = Box<dyn FnOnce(&mut Cx<'_>, Result<Value, Value>)>;

/// The closures waiting for promises to settle, by token.
#[derive(Default)]
pub(crate) struct Reactions {
    pending: RefCell<HashMap<u64, Settled>>,
    next: Cell<u64>,
}

/// Runs `f` once `value` settles: with the fulfillment value, or with the
/// rejection reason. A value that is not a promise counts as fulfilled.
/// Calls `f` once `value` settles: at once (in a microtask) for a value
/// that is not a promise.
pub fn when_settled(
    cx: &mut Cx<'_>,
    value: Value,
    f: impl FnOnce(&mut Cx<'_>, Result<Value, Value>) + 'static,
) {
    let reactions = &cx.page.reactions;
    let token = reactions.next.get();
    reactions.next.set(token + 1);
    reactions.pending.borrow_mut().insert(token, Box::new(f));
    cx.script.react(&value, token);
}

/// Called by the script host when the promise behind `token` settled.
pub fn settled(cx: &mut Cx<'_>, token: u64, outcome: Result<Value, Value>) {
    let reaction = cx.page.reactions.pending.borrow_mut().remove(&token);
    if let Some(reaction) = reaction {
        reaction(cx, outcome);
    }
}
