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

    /// Whether `callback` can be constructed with `new`.
    fn is_constructor(&mut self, callback: &Callback) -> bool;

    /// Constructs an object: `new callback(...args)`.
    fn construct(&mut self, callback: &Callback, args: &[Value]) -> Fallible<Value>;

    /// Reads a property of a script object (`object[name]`). Non-objects
    /// read as `undefined`.
    fn get_property(&mut self, object: &Value, name: &str) -> Fallible<Value>;

    /// The value as a callback, if it is callable.
    fn as_callback(&mut self, value: &Value) -> Option<Callback>;

    /// WebIDL `sequence<DOMString>` conversion.
    fn to_string_sequence(&mut self, value: &Value) -> Fallible<Vec<String>>;

    /// Arranges for the page's settlement handler to be called with
    /// `token` once `value` settles: at once (as a microtask) for a value
    /// that is not a promise, with the value as the fulfillment.
    fn react(&mut self, value: &Value, token: u64);

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

    /// The script value an exception is thrown as: the `DOMException` or
    /// error object for exceptions raised by Rust, the thrown value itself
    /// otherwise.
    fn exception_value(&mut self, exception: &Exception) -> Value;

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
