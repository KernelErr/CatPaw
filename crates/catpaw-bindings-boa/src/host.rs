//! `ScriptHost` on top of a Boa context: what the web platform
//! implementation can ask of the engine.

use std::path::Path;
use std::rc::Rc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::job::PromiseJob;
use boa_engine::object::builtins::JsPromise;
use boa_engine::{Context, JsError, JsString, JsValue, Source, js_string};
use catpaw_js::{
    Callback, CallbackKind, Exception, Fallible, ObjectId, PromiseRef, ScriptHost, Value,
};
use catpaw_js_boa::inspect;
use catpaw_web::ConsoleLevel;

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
    for promise in rejected {
        let Ok(promise) = JsPromise::from_object(promise) else {
            continue;
        };
        if let PromiseState::Rejected(reason) = promise.state() {
            let text = inspect::describe_thrown(&reason, ctx, &rt::describe_native);
            let text = format!("Uncaught (in promise) {text}");
            rt.page.errors.borrow_mut().push(text.clone());
            rt.page.log(ConsoleLevel::Error, text);
        }
    }

    rt.between_tasks(ctx);
    rt.in_checkpoint.set(false);
}

impl<'a> BoaHost<'a> {
    pub fn new(ctx: &'a mut Context, rt: Rc<Runtime>) -> Self {
        Self { ctx, rt }
    }

    fn enter(&self) {
        self.rt.depth.set(self.rt.depth.get() + 1);
    }

    /// Leaves script; when the outermost call returns, microtasks run.
    fn leave(&mut self) {
        self.rt.depth.set(self.rt.depth.get().saturating_sub(1));
        checkpoint(self.ctx, &self.rt);
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
        if callback.kind == CallbackKind::Interface && !target.is_callable() {
            // A callback interface object: call its `handleEvent` method.
            let Some(object) = target.as_object() else {
                return Ok(Value::Undefined);
            };
            function = object
                .get(js_string!("handleEvent"), self.ctx)
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

    fn eval_script(&mut self, source: &str, url: &str, _line: u32) -> Fallible<Value> {
        self.enter();
        let result = self
            .ctx
            .eval(Source::from_bytes(source).with_path(Path::new(url)));
        let out = self.finish(result);
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

    fn describe_exception(&mut self, exception: &Exception) -> String {
        match exception {
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
}
