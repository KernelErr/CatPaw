//! What the web platform implementation needs from a JavaScript engine.

use crate::exception::{Exception, Fallible};
use crate::value::{Callback, ObjectId, PromiseRef, Value};

/// The engine, as seen from engine-neutral code. One instance exists per
/// page; it is handed to implementations inside `catpaw_web::Cx`.
///
/// Calling into script can run arbitrary code, including code that re-enters
/// the platform implementation. Callers must not hold borrows of page state
/// across any method that may run script (`call`, `eval_script`,
/// `run_microtasks`, promise settlement).
pub trait ScriptHost {
    /// Invokes a callback. For callback interfaces the `handleEvent` method
    /// is looked up now. A thrown exception comes back as `Err`.
    fn call(&mut self, callback: &Callback, this: &Value, args: &[Value]) -> Fallible<Value>;

    /// Whether two callbacks are the same script object.
    fn same_callback(&mut self, a: &Callback, b: &Callback) -> bool;

    /// Evaluates a classic script. `url` and `line` label the source in
    /// stack traces.
    fn eval_script(&mut self, source: &str, url: &str, line: u32) -> Fallible<Value>;

    /// Evaluates a module script: parses `source` as the module at `url`,
    /// loads its imports and evaluates the graph. An error in loading,
    /// linking or evaluation comes back as `Err`.
    fn eval_module(&mut self, source: &str, url: &str) -> Fallible<()>;

    /// Compiles `body` as the body of a function with the given parameter
    /// names (used for `onclick="..."` content attributes and string timer
    /// handlers).
    fn compile_function(&mut self, params: &[&str], body: &str, url: &str) -> Fallible<Callback>;

    /// Performs a microtask checkpoint.
    fn run_microtasks(&mut self);

    /// Queues `callback` as a microtask.
    fn queue_microtask(&mut self, callback: Callback);

    fn new_promise(&mut self) -> PromiseRef;
    fn resolve_promise(&mut self, promise: &PromiseRef, value: Value);
    fn reject_promise(&mut self, promise: &PromiseRef, error: Exception);

    /// A human-readable rendering of an exception (message and, when the
    /// engine has one, a stack).
    fn describe_exception(&mut self, exception: &Exception) -> String;

    /// Formats values the way `console.log` would.
    fn display(&mut self, values: &[Value]) -> String;

    /// WebIDL `DOMString` conversion of an arbitrary value.
    fn to_dom_string(&mut self, value: &Value) -> Fallible<String>;

    /// `JSON.parse`.
    fn parse_json(&mut self, text: &str) -> Fallible<Value>;

    /// `JSON.stringify`; `None` when the value is not serializable.
    fn stringify_json(&mut self, value: &Value) -> Fallible<Option<String>>;

    /// The structured clone algorithm, within one realm.
    fn structured_clone(&mut self, value: &Value) -> Fallible<Value>;

    /// Keeps the script wrapper of a platform object alive (and its identity
    /// stable) until [`ScriptHost::unroot_object`]. Used while Rust has
    /// pending work for the object: an event being dispatched, a request in
    /// flight.
    fn root_object(&mut self, id: ObjectId);
    fn unroot_object(&mut self, id: ObjectId);

    /// Requests a garbage collection (tests and leak checks).
    fn collect_garbage(&mut self);
}
