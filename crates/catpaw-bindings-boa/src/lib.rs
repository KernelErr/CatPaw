//! Boa bindings for the CatPaw web platform.
//!
//! This crate connects `catpaw-web` (the DOM and Web APIs in plain Rust) to
//! the Boa JavaScript engine:
//!
//! - [`generated`] is the glue `cargo xtask bindgen` emits from Web IDL: one
//!   native function per attribute accessor and operation, converting
//!   arguments and results and calling the `catpaw_web::generated` traits.
//! - [`rt`] is the runtime that glue is written against: wrappers,
//!   conversions, and the installation of interfaces into a realm.
//! - [`BoaPage`] owns a Boa context set up for one page.

mod crypto;
pub mod generated;
mod host;
mod modules;
pub mod rt;
mod seeds;

use std::rc::Rc;

use boa_engine::builtins::promise::{OperationType, Promise};
use boa_engine::context::HostHooks;
use boa_engine::context::time::{Clock, JsInstant};
use boa_engine::job::PromiseJob;
use boa_engine::{Context, JsObject, JsValue, Source, js_string};
use catpaw_js::Value;
use catpaw_js_boa::{Jobs, inspect};
use catpaw_web::{Cx, PageState};

pub use crate::rt::Realm;

use crate::rt::{Prelude, Runtime};

/// The locale `Intl` uses when script names none, if the page's languages
/// offer none that parses.
const FALLBACK_LOCALE: &str = "en-US";

const PRELUDE: &str = include_str!("prelude.js");

/// How deep script calls may nest. The engine's default (512) is far below
/// what real pages need.
const RECURSION_LIMIT: usize = 3_000;
const STACK_SIZE_LIMIT: usize = 1024 * 256;

/// `Date` and friends read the page clock, so virtual time is consistent
/// across `Date.now()`, `performance.now()` and timers.
struct PageClock(Rc<catpaw_web::clock::Clock>);

impl Clock for PageClock {
    fn now(&self) -> JsInstant {
        let ms = self.0.peek().max(0.0);
        JsInstant::new((ms / 1000.0) as u64, ((ms % 1000.0) * 1_000_000.0) as u32)
    }

    fn system_time_millis(&self) -> i64 {
        self.0.unix_ms() as i64
    }
}

struct Hooks {
    /// The page's time zone, as seconds east of UTC.
    utc_offset_seconds: i32,
}

/// A time zone `minutes` east of UTC as `Intl` names it (`+05:30`); `None`
/// for UTC, its default.
fn offset_time_zone(minutes: i32) -> Option<String> {
    if minutes == 0 {
        return None;
    }
    let sign = if minutes < 0 { '-' } else { '+' };
    let minutes = minutes.unsigned_abs();
    Some(format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60))
}

impl HostHooks for Hooks {
    /// `Date` shows local time in the page's configured zone, never the
    /// host's: a run prints the same dates wherever it runs, and sites
    /// learn nothing of the machine's zone.
    fn local_timezone_offset_seconds(&self, _unix_time_seconds: i64) -> i32 {
        self.utc_offset_seconds
    }

    fn promise_rejection_tracker(
        &self,
        promise: &JsObject<Promise>,
        operation: OperationType,
        context: &mut Context,
    ) {
        let Some(rt) = rt::try_runtime(context) else {
            return;
        };
        let promise: JsObject = promise.clone().upcast();
        let mut rejections = rt.rejections.borrow_mut();
        match operation {
            OperationType::Reject => rejections.push(promise),
            OperationType::Handle => rejections.retain(|p| !JsObject::equals(p, &promise)),
        }
    }
}

/// A Boa context set up as the script environment of one page.
pub struct BoaPage {
    context: Context,
    runtime: Rc<Runtime>,
}

impl BoaPage {
    /// Creates the realm for `page`: the global object becomes its
    /// `Window`, and every window interface in the binding manifest is
    /// installed. Dates show the page's time zone (`Intl`'s too), `Intl`
    /// defaults to its first language, and in a seeded run (`random_seed`)
    /// `Math.random` and `crypto` draw from sequences of the realm's own.
    pub fn new(page: Rc<PageState>) -> Result<Self, String> {
        Self::with_realm(page, Realm::Window)
    }

    /// Creates the realm of a dedicated worker whose state is `page`: the
    /// global object becomes its `DedicatedWorkerGlobalScope`, with the
    /// worker interfaces.
    pub fn new_worker(page: Rc<PageState>) -> Result<Self, String> {
        Self::with_realm(page, Realm::Worker)
    }

    fn with_realm(page: Rc<PageState>, realm: Realm) -> Result<Self, String> {
        let jobs = Jobs::new();
        let hooks = Hooks {
            utc_offset_seconds: page.config.timezone_offset_minutes.saturating_mul(60),
        };
        let mut context = Context::builder()
            .job_executor(jobs.clone())
            .clock(Rc::new(PageClock(page.clock.clone())))
            .host_hooks(Rc::new(hooks))
            .module_loader(Rc::new(modules::PageModuleLoader::new(page.clone())))
            .build()
            .map_err(|e| format!("failed to create a script context: {e}"))?;
        let limits = context.runtime_limits_mut();
        limits.set_recursion_limit(RECURSION_LIMIT);
        limits.set_stack_size_limit(STACK_SIZE_LIMIT);

        // The page's languages, not the host's, decide how numbers and
        // dates are written when script asks for no locale.
        let language = page
            .config
            .languages
            .iter()
            .map(String::as_str)
            .find(|language| !language.trim().is_empty())
            .unwrap_or(FALLBACK_LOCALE);
        if context.set_default_locale(Some(language)).is_err() {
            context
                .set_default_locale(Some(FALLBACK_LOCALE))
                .map_err(|e| format!("failed to set the default locale: {e}"))?;
        }
        // `Intl` shows dates in the page's time zone too (`Date` has it from
        // the hooks). An offset no time zone has (beyond 18 hours) leaves it
        // in UTC.
        if let Some(zone) = offset_time_zone(page.config.timezone_offset_minutes) {
            let _ = context.set_default_time_zone(Some(&zone));
        }

        // Random sequences of the realm's own (see `seeds`); `crypto`'s is
        // set up once the realm stands.
        let seed = seeds::realm_seed(&page, realm);
        context
            .realm()
            .set_random_seed(seed.map(|seed| seed.rotate_left(17)));

        // Microtasks the page queues itself run in order with script's.
        let queue = jobs.clone();
        page.set_microtask_queue(move |task| {
            queue.enqueue_microtask(PromiseJob::new(move |ctx| {
                rt::with_cx(ctx, task);
                Ok(JsValue::undefined())
            }));
        });
        let runtime = Runtime::new(page, jobs);
        runtime.attach(&mut context);
        rt::install(&mut context, &runtime, realm)
            .map_err(|e| format!("failed to install the bindings: {e}"))?;

        let exports = context
            .eval(Source::from_bytes(PRELUDE))
            .map_err(|e| format!("failed to evaluate the prelude: {e}"))?;
        let exports = exports
            .as_object()
            .ok_or("the prelude did not return its exports")?;
        let mut export = |name: &str| -> Result<JsObject, String> {
            exports
                .get(boa_engine::JsString::from(name), &mut context)
                .ok()
                .and_then(|v| v.as_object())
                .ok_or_else(|| format!("the prelude does not export `{name}`"))
        };
        *runtime.prelude.borrow_mut() = Some(Prelude {
            dom_exception: export("DOMException")?,
            structured_clone: export("structuredClone")?,
            json_parse: export("jsonParse")?,
            json_stringify: export("jsonStringify")?,
        });
        // `globalThis` must be the global object scripts see as `window`
        // or `self`.
        debug_assert!(
            context
                .global_object()
                .has_property(
                    js_string!(match realm {
                        Realm::Window => "window",
                        Realm::Worker => "self",
                    }),
                    &mut context
                )
                .unwrap_or(false)
        );
        catpaw_web::crypto::seed_realm(&runtime.page, seed);
        Ok(Self { context, runtime })
    }

    pub fn page(&self) -> &Rc<PageState> {
        &self.runtime.page
    }

    pub fn runtime(&self) -> &Rc<Runtime> {
        &self.runtime
    }

    /// The underlying engine context.
    pub fn context(&mut self) -> &mut Context {
        &mut self.context
    }

    /// Runs `f` with the page context: the entry point for everything the
    /// embedder does to the page (loading, running the event loop, ...).
    pub fn with_cx<R>(&mut self, f: impl FnOnce(&mut Cx<'_>) -> R) -> R {
        rt::with_cx(&mut self.context, f)
    }

    /// Evaluates `source` as a classic script and returns its completion
    /// value, or a description of the exception it threw.
    pub fn eval(&mut self, source: &str) -> Result<Value, String> {
        let url = self.runtime.page.url.borrow().to_string();
        self.with_cx(|cx| match cx.script.eval_script(source, &url, 1) {
            Ok(value) => Ok(value),
            Err(e) => Err(cx.script.describe_exception(&e)),
        })
    }

    /// Evaluates `source` and renders the result the way a console would.
    pub fn eval_to_string(&mut self, source: &str) -> Result<String, String> {
        let value = self.eval(source)?;
        Ok(self.with_cx(|cx| cx.script.display(std::slice::from_ref(&value))))
    }

    /// Renders a script value the way a console would.
    pub fn display(&mut self, value: &JsValue) -> String {
        inspect::display(
            std::slice::from_ref(value),
            &mut self.context,
            &rt::describe_native,
        )
    }
}

impl Drop for BoaPage {
    fn drop(&mut self) {
        // Roots released by the page's teardown are dropped with it.
        rt::release_roots();
        catpaw_web::crypto::forget_realm(&self.runtime.page);
    }
}
