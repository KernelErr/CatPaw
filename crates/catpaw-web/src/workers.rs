//! Dedicated workers: the `Worker` object a page holds, the global scope
//! a worker script runs in, and the commands that cross between them.
//!
//! A worker is a page state and script realm of its own, run by the
//! embedder (on the page's thread, in turns with the page and its frames).
//! The page and its workers only ever exchange messages, which cross as
//! JSON (see `frames`); the embedder spawns and terminates workers and
//! routes what they post.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value, WindowRef};
use url::Url;

use crate::events::{self, Event, EventData};
use crate::frames::{self, MessageData};
use crate::generated::{self as web, InterfaceId};
use crate::net::{self, NetRequest, RequestKind};
use crate::page::{Cx, PageState};
use crate::{Web, event_loop, platform_object};

/// A worker of a page, numbered by that page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerId(pub u32);

/// Something the embedder is asked to do with workers.
#[derive(Clone, Debug)]
pub enum WorkerCommand {
    /// Start a worker with the script at `url`.
    Spawn {
        worker: WorkerId,
        url: Url,
        name: String,
        module: bool,
    },
    /// A message for a worker of this page.
    PostMessage { worker: WorkerId, data: MessageData },
    /// `worker.terminate()`.
    Terminate { worker: WorkerId },
    /// From a worker: a message for the page that owns it.
    ToOwner { data: MessageData },
    /// From a worker: `self.close()`.
    Close,
}

/// What a page state is when it is a worker's global scope.
#[derive(Clone, Debug)]
pub struct WorkerRole {
    pub id: WorkerId,
    pub name: String,
}

/// A page's workers, and its role when it is one itself.
#[derive(Default)]
pub struct WorkerState {
    next: Cell<u32>,
    /// The `Worker` objects by id, pinned while the worker runs.
    objects: RefCell<HashMap<WorkerId, ObjectId>>,
    commands: RefCell<Vec<WorkerCommand>>,
    role: RefCell<Option<WorkerRole>>,
    location: Cell<Option<ObjectId>>,
    navigator: Cell<Option<ObjectId>>,
    closed: Cell<bool>,
}

impl WorkerState {
    /// Makes this page state a worker's global scope.
    pub fn set_role(&self, id: WorkerId, name: String) {
        *self.role.borrow_mut() = Some(WorkerRole { id, name });
    }

    pub fn role(&self) -> Option<WorkerRole> {
        self.role.borrow().clone()
    }

    /// Whether the worker called `close()`.
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Takes the commands queued since the last call.
    pub fn take_commands(&self) -> Vec<WorkerCommand> {
        std::mem::take(&mut *self.commands.borrow_mut())
    }

    pub fn has_commands(&self) -> bool {
        !self.commands.borrow().is_empty()
    }

    fn push(&self, command: WorkerCommand) {
        self.commands.borrow_mut().push(command);
    }
}

// --------------------------------------------------------------- the page

/// A `Worker` object.
pub struct WorkerObject {
    id: WorkerId,
    terminated: Cell<bool>,
}
platform_object!(WorkerObject, Worker);

fn worker_of(cx: &Cx<'_>, this: ObjectId) -> Fallible<(WorkerId, bool)> {
    cx.page
        .with::<WorkerObject, _>(this, |w| (w.id, w.terminated.get()))
}

/// Forgets a worker that ended (terminated, closed, or failed to start):
/// its object is no longer kept alive by the page.
fn release(cx: &mut Cx<'_>, worker: WorkerId) {
    let object = cx.page.workers.objects.borrow_mut().remove(&worker);
    if let Some(object) = object {
        let _ = cx
            .page
            .with::<WorkerObject, _>(object, |w| w.terminated.set(true));
        cx.unpin(object);
    }
}

/// The embedder delivers a message a worker posted to its owner.
pub fn deliver_to_owner(page: &PageState, worker: WorkerId, data: MessageData) {
    event_loop::queue_task(page, "worker message", move |cx| {
        let object = cx.page.workers.objects.borrow().get(&worker).copied();
        if let Some(object) = object {
            frames::dispatch_message(
                cx,
                EventTargetRef::Object(object),
                data,
                String::new(),
                None,
            );
        }
    });
}

/// The embedder reports an error in a worker (a script that failed to
/// load or threw uncaught): `error` fires on the `Worker` object.
pub fn worker_error(page: &PageState, worker: WorkerId, message: String, ended: bool) {
    event_loop::queue_task(page, "worker error", move |cx| {
        let object = cx.page.workers.objects.borrow().get(&worker).copied();
        if let Some(object) = object {
            let mut event = Event::new("error", false, true, cx.page.clock.peek());
            event.iface = InterfaceId::ErrorEvent;
            event.trusted = true;
            event.data = EventData::Error {
                message,
                filename: String::new(),
                lineno: 0,
                colno: 0,
                error: Value::Undefined,
            };
            let event = cx.page.alloc(event);
            cx.pin(event);
            events::dispatch(cx, EventTargetRef::Object(object), event);
            cx.unpin(event);
        }
        if ended {
            release(cx, worker);
        }
    });
}

/// The embedder says a worker is gone (it closed itself).
pub fn worker_ended(page: &PageState, worker: WorkerId) {
    event_loop::queue_task(page, "worker ended", move |cx| release(cx, worker));
}

impl web::WorkerImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        script_url: String,
        options: web::WorkerOptions,
    ) -> Fallible<ObjectId> {
        let url = cx.page.resolve_url(&script_url).ok_or_else(|| {
            Exception::dom(
                "SyntaxError",
                format!("Failed to construct 'Worker': '{script_url}' is not a valid URL"),
            )
        })?;
        if !matches!(url.scheme(), "http" | "https" | "blob" | "data") {
            return Err(Exception::dom(
                "SecurityError",
                format!(
                    "Failed to construct 'Worker': scheme '{}' is not supported",
                    url.scheme()
                ),
            ));
        }
        let page_origin = frames::origin_of(&cx.page.url.borrow());
        if matches!(url.scheme(), "http" | "https") && frames::origin_of(&url) != page_origin {
            return Err(Exception::dom(
                "SecurityError",
                format!("Failed to construct 'Worker': {url} is not same-origin"),
            ));
        }
        let id = WorkerId(cx.page.workers.next.get());
        cx.page.workers.next.set(id.0 + 1);
        let object = cx.page.alloc(WorkerObject {
            id,
            terminated: Cell::new(false),
        });
        cx.pin(object);
        cx.page.workers.objects.borrow_mut().insert(id, object);
        cx.page.workers.push(WorkerCommand::Spawn {
            worker: id,
            url,
            name: options.name,
            module: matches!(options.type_, web::WorkerType::Module),
        });
        Ok(object)
    }

    fn terminate(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let (id, terminated) = worker_of(cx, this)?;
        if !terminated {
            cx.page
                .workers
                .push(WorkerCommand::Terminate { worker: id });
            release(cx, id);
        }
        Ok(())
    }

    fn post_message(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        _transfer: Vec<Value>,
    ) -> Fallible<()> {
        let (id, terminated) = worker_of(cx, this)?;
        let data = frames::portable(cx, &message)?;
        if !terminated {
            cx.page
                .workers
                .push(WorkerCommand::PostMessage { worker: id, data });
        }
        Ok(())
    }

    fn post_message_overload2(
        cx: &mut Cx<'_>,
        this: ObjectId,
        message: Value,
        _options: web::StructuredSerializeOptions,
    ) -> Fallible<()> {
        Self::post_message(cx, this, message, Vec::new())
    }
}

// ------------------------------------------------------------- the worker

pub struct WorkerLocationObject;
platform_object!(WorkerLocationObject, WorkerLocation);

pub struct WorkerNavigatorObject;
platform_object!(WorkerNavigatorObject, WorkerNavigator);

/// Runs the worker's script in its realm: the first thing after the
/// embedder set the realm up. Uncaught exceptions are reported in the
/// scope (its `error` event, then `page.errors`).
pub fn run_script(cx: &mut Cx<'_>, source: &str, url: &Url, module: bool) {
    let result = if module {
        cx.script.eval_module(source, url.as_str())
    } else {
        cx.script.eval_script(source, url.as_str(), 1).map(|_| ())
    };
    if let Err(e) = result {
        cx.report_exception(&e);
    }
}

/// The embedder delivers a message the owner posted to this worker.
pub fn deliver_to_worker(page: &PageState, data: MessageData) {
    event_loop::queue_task(page, "message", move |cx| {
        frames::dispatch_message(cx, EventTargetRef::Window, data, String::new(), None);
    });
}

fn decode_script(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl web::WorkerGlobalScopeImpl for Web {
    fn self_(_cx: &mut Cx<'_>) -> Fallible<WindowRef> {
        Ok(WindowRef::Local)
    }

    fn location(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        if let Some(id) = cx.page.workers.location.get() {
            return Ok(id);
        }
        let id = cx.page.alloc(WorkerLocationObject);
        cx.pin(id);
        cx.page.workers.location.set(Some(id));
        Ok(id)
    }

    fn navigator(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        if let Some(id) = cx.page.workers.navigator.get() {
            return Ok(id);
        }
        let id = cx.page.alloc(WorkerNavigatorObject);
        cx.pin(id);
        cx.page.workers.navigator.set(Some(id));
        Ok(id)
    }

    /// Fetches and runs each script in turn, synchronously.
    fn import_scripts(cx: &mut Cx<'_>, urls: Vec<String>) -> Fallible<()> {
        let mut resolved = Vec::with_capacity(urls.len());
        for input in &urls {
            let url = cx.page.resolve_url(input).ok_or_else(|| {
                Exception::dom("SyntaxError", format!("'{input}' is not a valid URL"))
            })?;
            resolved.push(url);
        }
        for url in resolved {
            let request = NetRequest::get(url.clone(), RequestKind::Script);
            let response = net::fetch_blocking(cx.page, request).map_err(|e| {
                Exception::dom(
                    "NetworkError",
                    format!("importScripts failed for {url}: {e}"),
                )
            })?;
            if !(200..300).contains(&response.status) {
                return Err(Exception::dom(
                    "NetworkError",
                    format!("importScripts failed for {url}: HTTP {}", response.status),
                ));
            }
            let source = decode_script(&response.body);
            cx.script.eval_script(&source, response.url.as_str(), 1)?;
        }
        Ok(())
    }
}

impl web::DedicatedWorkerGlobalScopeImpl for Web {
    fn name(cx: &mut Cx<'_>) -> Fallible<String> {
        Ok(cx.page.workers.role().map(|r| r.name).unwrap_or_default())
    }

    fn post_message(cx: &mut Cx<'_>, message: Value, _transfer: Vec<Value>) -> Fallible<()> {
        let data = frames::portable(cx, &message)?;
        if !cx.page.workers.closed.get() {
            cx.page.workers.push(WorkerCommand::ToOwner { data });
        }
        Ok(())
    }

    fn post_message_overload2(
        cx: &mut Cx<'_>,
        message: Value,
        _options: web::StructuredSerializeOptions,
    ) -> Fallible<()> {
        Self::post_message(cx, message, Vec::new())
    }

    fn close(cx: &mut Cx<'_>) -> Fallible<()> {
        if !cx.page.workers.closed.replace(true) {
            cx.page.workers.push(WorkerCommand::Close);
        }
        Ok(())
    }
}

fn location_part(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&Url) -> String) -> Fallible<String> {
    cx.page.with::<WorkerLocationObject, _>(this, |_| ())?;
    Ok(f(&cx.page.url.borrow()))
}

impl web::WorkerLocationImpl for Web {
    fn href(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| u.to_string())
    }

    fn origin(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, frames::origin_of)
    }

    fn protocol(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| format!("{}:", u.scheme()))
    }

    fn host(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        })
    }

    fn hostname(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| u.host_str().unwrap_or_default().to_string())
    }

    fn port(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| {
            u.port().map(|p| p.to_string()).unwrap_or_default()
        })
    }

    fn pathname(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| u.path().to_string())
    }

    fn search(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| match u.query() {
            Some(q) if !q.is_empty() => format!("?{q}"),
            _ => String::new(),
        })
    }

    fn hash(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        location_part(cx, this, |u| match u.fragment() {
            Some(f) if !f.is_empty() => format!("#{f}"),
            _ => String::new(),
        })
    }
}
