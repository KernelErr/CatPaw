//! `ScriptHost` on top of a Boa context: what the web platform
//! implementation can ask of the engine.

use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::job::PromiseJob;
use boa_engine::module::Module;
use boa_engine::object::builtins::JsPromise;
use boa_engine::{Context, JsError, JsString, JsValue, NativeFunction, Source};
use catpaw_js::{
    Callback, CallbackKind, Exception, Fallible, ObjectId, PromiseRef, ScriptHost, Value,
};
use catpaw_js_boa::inspect;
use catpaw_web::ConsoleLevel;

use crate::modules::PageModuleLoader;
use crate::rt::{self, IntoJs, Runtime};

pub struct BoaHost<'a> {
    ctx: &'a mut Context,
    rt: Rc<Runtime>,
}

/// Logs an exception nothing caught.
fn report_uncaught(ctx: &mut Context, rt: &Runtime, error: JsError, prefix: &str) {
    let text = match error.into_opaque(ctx) {
        Ok(value) => inspect::describe_thrown(&value, ctx, &rt::describe_native),
        Err(_) => "an error that could not be described".to_string(),
    };
    let text = format!("{prefix}{text}");
    rt.page.errors.borrow_mut().push(text.clone());
    rt.page.log(ConsoleLevel::Error, text);
}

/// Performs a microtask checkpoint and the housekeeping that follows a task.
/// Does nothing if script is still on the stack or a checkpoint is already
/// running.
pub(crate) fn checkpoint(ctx: &mut Context, rt: &Runtime) {
    if rt.depth.get() > 0 || rt.in_checkpoint.replace(true) {
        return;
    }
    rt.jobs.checkpoint(ctx);

    // Promises rejected during this task that still have no handler.
    let rejected = std::mem::take(&mut *rt.rejections.borrow_mut());
    let mut reported = false;
    for promise in rejected {
        let Ok(promise) = JsPromise::from_object(promise) else {
            continue;
        };
        if let PromiseState::Rejected(reason) = promise.state() {
            let text = inspect::describe_thrown(&reason, ctx, &rt::describe_native);
            let text = format!("Uncaught (in promise) {text}");
            rt.page.errors.borrow_mut().push(text.clone());
            // `unhandledrejection` listeners may claim the rejection.
            let promise_value = Value::Opaque(rt::root(promise.into()));
            let reason_value = rt::value_from_js(&reason, ctx).unwrap_or_default();
            let unhandled = rt::with_cx(ctx, |cx| {
                catpaw_web::events::report_unhandled_rejection(cx, promise_value, reason_value)
            });
            if unhandled {
                rt.page.log(ConsoleLevel::Error, text);
            }
            reported = true;
        }
    }

    if reported {
        // Listeners may have queued microtasks of their own.
        rt.jobs.checkpoint(ctx);
    }

    rt.between_tasks(ctx);
    rt.in_checkpoint.set(false);
}

impl<'a> BoaHost<'a> {
    pub fn new(ctx: &'a mut Context, rt: Rc<Runtime>) -> Self {
        Self { ctx, rt }
    }

    /// Enters script. The outermost entry starts the script budget, which
    /// the microtasks run on leaving spend from as well.
    fn enter(&mut self) {
        let depth = self.rt.depth.get();
        if depth == 0
            && let Some(budget) = self.rt.page.config.script_budget
        {
            self.ctx.set_deadline(Some(Instant::now() + budget));
        }
        self.rt.depth.set(depth + 1);
    }

    /// Leaves script; when the outermost call returns, microtasks run.
    fn leave(&mut self) {
        self.rt.depth.set(self.rt.depth.get().saturating_sub(1));
        checkpoint(self.ctx, &self.rt);
        if self.rt.depth.get() == 0 {
            self.ctx.set_deadline(None);
        }
    }

    fn js_value(&mut self, value: &Value) -> JsValue {
        value.clone().into_js(self.ctx).unwrap_or_default()
    }

    fn value_of(&mut self, value: &JsValue) -> Value {
        rt::value_from_js(value, self.ctx).unwrap_or_default()
    }

    fn finish(&mut self, result: Result<JsValue, JsError>) -> Fallible<Value> {
        match result {
            Ok(value) => Ok(self.value_of(&value)),
            Err(e) => Err(rt::exception_from_js(e, self.ctx)),
        }
    }

    fn prelude_call(
        &mut self,
        pick: fn(&rt::Prelude) -> &boa_engine::JsObject,
        args: &[JsValue],
    ) -> Result<JsValue, JsError> {
        let function = self
            .rt
            .prelude
            .borrow()
            .as_ref()
            .map(|p| pick(p).clone())
            .ok_or_else(|| rt::type_error("The realm is not initialised"))?;
        function.call(&JsValue::undefined(), args, self.ctx)
    }
}

impl ScriptHost for BoaHost<'_> {
    fn call(&mut self, callback: &Callback, this: &Value, args: &[Value]) -> Fallible<Value> {
        let target = rt::rooted(&callback.root);
        let mut this_js = self.js_value(this);
        let args_js: Vec<JsValue> = args.iter().map(|a| self.js_value(a)).collect();

        let mut function = target.clone();
        if let CallbackKind::Interface(method) = callback.kind
            && !target.is_callable()
        {
            // A callback interface object: call its method.
            let Some(object) = target.as_object() else {
                return Ok(Value::Undefined);
            };
            function = object
                .get(JsString::from(method), self.ctx)
                .map_err(|e| rt::exception_from_js(e, self.ctx))?;
            this_js = target;
        }
        let Some(function) = function.as_callable() else {
            // A non-callable event handler object is silently skipped.
            return Ok(Value::Undefined);
        };

        self.enter();
        let result = function.call(&this_js, &args_js, self.ctx);
        let out = self.finish(result);
        self.leave();
        out
    }

    fn same_callback(&mut self, a: &Callback, b: &Callback) -> bool {
        rt::rooted(&a.root).strict_equals(&rt::rooted(&b.root))
    }

    fn is_constructor(&mut self, callback: &Callback) -> bool {
        rt::rooted(&callback.root)
            .as_object()
            .is_some_and(|object| object.is_constructor())
    }

    fn construct(&mut self, callback: &Callback, args: &[Value]) -> Fallible<Value> {
        let target = rt::rooted(&callback.root);
        let Some(constructor) = target.as_object().filter(|o| o.is_constructor()) else {
            return Err(Exception::type_error("The value is not a constructor"));
        };
        let args_js: Vec<JsValue> = args.iter().map(|a| self.js_value(a)).collect();
        self.enter();
        let result = constructor
            .construct(&args_js, None, self.ctx)
            .map(JsValue::from);
        let out = self.finish(result);
        self.leave();
        out
    }

    fn get_property(&mut self, object: &Value, name: &str) -> Fallible<Value> {
        let target = self.js_value(object);
        let Some(object) = target.as_object() else {
            return Ok(Value::Undefined);
        };
        self.enter();
        let result = object.get(JsString::from(name), self.ctx);
        let out = self.finish(result);
        self.leave();
        out
    }

    fn buffer_bytes(&mut self, value: &Value) -> Option<Vec<u8>> {
        let js = self.js_value(value);
        rt::buffer_from_js(&js, self.ctx).ok()
    }

    fn as_callback(&mut self, value: &Value) -> Option<Callback> {
        let target = self.js_value(value);
        target.is_callable().then(|| Callback {
            root: rt::root(target),
            kind: CallbackKind::Function,
        })
    }

    fn to_string_sequence(&mut self, value: &Value) -> Fallible<Vec<String>> {
        let target = self.js_value(value);
        let result = rt::sequence_from_js(&target, self.ctx, rt::string_from_js);
        result.map_err(|e| rt::exception_from_js(e, self.ctx))
    }

    fn react(&mut self, value: &Value, token: u64) {
        let value = self.js_value(value);
        let Ok(promise) = JsPromise::resolve(value, self.ctx) else {
            return;
        };
        // Each reaction tells the page which closure to run, and with what.
        let reaction = |fulfilled: bool, ctx: &mut Context| {
            NativeFunction::from_copy_closure_with_captures(
                |_this, args, (token, fulfilled), ctx| {
                    let settlement = rt::value_from_js(rt::arg(args, 0), ctx)?;
                    let outcome = if *fulfilled {
                        Ok(settlement)
                    } else {
                        Err(settlement)
                    };
                    let token = *token;
                    rt::with_cx(ctx, |cx| catpaw_web::promises::settled(cx, token, outcome));
                    Ok(JsValue::undefined())
                },
                (token, fulfilled),
            )
            .to_js_function(ctx.realm())
        };
        let on_fulfilled = reaction(true, self.ctx);
        let on_rejected = reaction(false, self.ctx);
        let _ = promise.then(Some(on_fulfilled), Some(on_rejected), self.ctx);
    }

    fn eval_script(&mut self, source: &str, url: &str, line: u32) -> Fallible<Value> {
        self.enter();
        // Boa numbers lines from the start of the source: a script that
        // starts further down its document (an inline one) is padded so
        // that positions in stacks and timer sites are the document's.
        let padded;
        let source = if line > 1 {
            padded = format!("{}{source}", "\n".repeat(line as usize - 1));
            padded.as_str()
        } else {
            source
        };
        let result = self
            .ctx
            .eval(Source::from_bytes(source).with_path(Path::new(url)));
        let out = self.finish(result);
        self.leave();
        out
    }

    fn eval_module(&mut self, source: &str, url: &str) -> Fallible<()> {
        self.enter();
        let parsed = Module::parse(
            Source::from_bytes(source).with_path(Path::new(url)),
            None,
            self.ctx,
        );
        let out = match parsed {
            Ok(module) => {
                if let Some(loader) = self.ctx.downcast_module_loader::<PageModuleLoader>() {
                    loader.register(url, module.clone());
                }
                let promise = module.load_link_evaluate(self.ctx);
                // Loading, linking and evaluation all advance through jobs.
                let nested = self.rt.in_checkpoint.replace(true);
                self.rt.jobs.checkpoint(self.ctx);
                self.rt.in_checkpoint.set(nested);
                match promise.state() {
                    PromiseState::Rejected(reason) => {
                        // Reported by the caller, not as an unhandled rejection.
                        let promise: boa_engine::JsObject = promise.into();
                        self.rt
                            .rejections
                            .borrow_mut()
                            .retain(|p| !boa_engine::JsObject::equals(p, &promise));
                        Err(Exception::Thrown(rt::root(reason)))
                    }
                    // Still pending means top-level await on something the
                    // event loop has yet to deliver.
                    PromiseState::Fulfilled(_) | PromiseState::Pending => Ok(()),
                }
            }
            Err(e) => Err(rt::exception_from_js(e, self.ctx)),
        };
        self.leave();
        out
    }

    fn compile_function(&mut self, params: &[&str], body: &str, url: &str) -> Fallible<Callback> {
        // Event handler content attributes see the element's and the
        // document's properties as variables.
        let source = format!(
            "(function({}) {{\nwith (this && this.ownerDocument ? this.ownerDocument : document) {{ with (this || {{}}) {{\n{body}\n}} }}\n}})",
            params.join(", ")
        );
        let result = self
            .ctx
            .eval(Source::from_bytes(&source).with_path(Path::new(url)));
        match result {
            Ok(function) => Ok(Callback {
                root: rt::root(function),
                kind: CallbackKind::Function,
            }),
            Err(e) => Err(rt::exception_from_js(e, self.ctx)),
        }
    }

    fn run_microtasks(&mut self) {
        checkpoint(self.ctx, &self.rt);
    }

    fn queue_microtask(&mut self, callback: Callback) {
        let job = PromiseJob::new(move |ctx| {
            let Some(function) = rt::rooted(&callback.root).as_callable() else {
                return Ok(JsValue::undefined());
            };
            if let Err(e) = function.call(&JsValue::undefined(), &[], ctx) {
                let rt = rt::runtime(ctx);
                report_uncaught(ctx, &rt, e, "Uncaught ");
            }
            Ok(JsValue::undefined())
        });
        self.rt.jobs.enqueue_microtask(job);
    }

    fn new_promise(&mut self) -> PromiseRef {
        let (promise, functions) = JsPromise::new_pending(self.ctx);
        let promise = PromiseRef(rt::root(promise.into()));
        rt::store_resolvers(&promise, functions);
        promise
    }

    fn resolve_promise(&mut self, promise: &PromiseRef, value: Value) {
        if let Some(functions) = rt::take_resolvers(promise) {
            let value = self.js_value(&value);
            let _ = functions
                .resolve
                .call(&JsValue::undefined(), &[value], self.ctx);
        }
    }

    fn reject_promise(&mut self, promise: &PromiseRef, error: Exception) {
        if let Some(functions) = rt::take_resolvers(promise) {
            let reason = rt::exception_to_js(error, self.ctx)
                .into_opaque(self.ctx)
                .unwrap_or_default();
            let _ = functions
                .reject
                .call(&JsValue::undefined(), &[reason], self.ctx);
        }
    }

    fn exception_value(&mut self, exception: &Exception) -> Value {
        match exception {
            Exception::Thrown(root) => Value::Opaque(root.clone()),
            Exception::Value(value) => value.clone(),
            other => {
                let thrown = rt::exception_to_js(other.clone(), self.ctx)
                    .into_opaque(self.ctx)
                    .unwrap_or_default();
                self.value_of(&thrown)
            }
        }
    }

    fn describe_exception(&mut self, exception: &Exception) -> String {
        match exception {
            Exception::Value(value) => {
                let value = self.js_value(value);
                inspect::describe_thrown(&value, self.ctx, &rt::describe_native)
            }
            Exception::Thrown(root) => {
                inspect::describe_thrown(&rt::rooted(root), self.ctx, &rt::describe_native)
            }
            other => other.to_string(),
        }
    }

    fn display(&mut self, values: &[Value]) -> String {
        let values: Vec<JsValue> = values.iter().map(|v| self.js_value(v)).collect();
        inspect::display(&values, self.ctx, &rt::describe_native)
    }

    fn to_dom_string(&mut self, value: &Value) -> Fallible<String> {
        let value = self.js_value(value);
        rt::string_from_js(&value, self.ctx).map_err(|e| rt::exception_from_js(e, self.ctx))
    }

    fn parse_json(&mut self, text: &str) -> Fallible<Value> {
        let result = self.prelude_call(|p| &p.json_parse, &[JsString::from(text).into()]);
        self.finish(result)
    }

    fn stringify_json(&mut self, value: &Value) -> Fallible<Option<String>> {
        let value = self.js_value(value);
        match self.prelude_call(|p| &p.json_stringify, &[value]) {
            Ok(result) => Ok(result.as_string().map(|s| s.to_std_string_lossy())),
            Err(e) => Err(rt::exception_from_js(e, self.ctx)),
        }
    }

    fn structured_clone(&mut self, value: &Value) -> Fallible<Value> {
        let value = self.js_value(value);
        let result = self.prelude_call(|p| &p.structured_clone, &[value]);
        self.finish(result)
    }

    fn root_object(&mut self, id: ObjectId) {
        self.rt.root_object(id, self.ctx);
    }

    fn unroot_object(&mut self, id: ObjectId) {
        self.rt.unroot_object(id, self.ctx);
    }

    fn collect_garbage(&mut self) {
        boa_gc::force_collect();
        self.rt.sweep(self.ctx);
    }

    fn caller_site(&mut self) -> Option<catpaw_js::SourceSite> {
        // Native functions push no frame: the innermost frame is the
        // script that called into the platform.
        let frame = self.ctx.stack_trace().next()?;
        let location = frame.position();
        let position = location.position?;
        let url = match &location.path {
            boa_engine::vm::SourcePath::Path(path) => path.to_string_lossy().into_owned(),
            boa_engine::vm::SourcePath::Eval => "eval".to_string(),
            _ => String::new(),
        };
        Some(catpaw_js::SourceSite {
            url,
            line: position.line_number(),
            column: position.column_number(),
            function: location.function_name.to_std_string_escaped(),
        })
    }
}
