//! Streams (<https://streams.spec.whatwg.org/>): `ReadableStream`,
//! `WritableStream` and `TransformStream` with their default readers,
//! writers and controllers.
//!
//! Readable streams follow the standard's algorithms, queues and
//! backpressure included. Writable and transform streams are simpler than
//! the standard: writes are handed to the sink one at a time and
//! backpressure between the two sides of a transform is not modelled.
//! Byte streams (`type: "bytes"`, BYOB readers) are read as default
//! streams, and `ReadableStream.from()` is absent.

use std::collections::VecDeque;

use catpaw_js::{Callback, Exception, Fallible, ObjectId, PromiseRef, Value};

use crate::generated as web;
use crate::page::Cx;
use crate::promises::when_settled;
use crate::{Web, platform_object};

/// The functions of an underlying source, sink or transformer, called with
/// the object they came from as `this`.
#[derive(Clone)]
struct Algorithms {
    this: Value,
    start: Option<Callback>,
    /// `pull`, `write` or `transform`.
    main: Option<Callback>,
    /// `cancel`, `abort` or `cancel`.
    cancel: Option<Callback>,
    /// `close` of a sink, `flush` of a transformer.
    close: Option<Callback>,
}

impl Algorithms {
    fn read(cx: &mut Cx<'_>, object: &Value, names: [&str; 4]) -> Fallible<Self> {
        let mut callbacks = Vec::with_capacity(4);
        for name in names {
            let value = cx.script.get_property(object, name)?;
            let callback = match value {
                Value::Undefined | Value::Null => None,
                other => Some(cx.script.as_callback(&other).ok_or_else(|| {
                    Exception::type_error(format!("The '{name}' member is not a function"))
                })?),
            };
            callbacks.push(callback);
        }
        let mut callbacks = callbacks.into_iter();
        Ok(Self {
            this: object.clone(),
            start: callbacks.next().flatten(),
            main: callbacks.next().flatten(),
            cancel: callbacks.next().flatten(),
            close: callbacks.next().flatten(),
        })
    }

    fn call(
        &self,
        cx: &mut Cx<'_>,
        callback: &Option<Callback>,
        args: &[Value],
    ) -> Fallible<Value> {
        match callback {
            Some(callback) => cx.script.call(callback, &self.this, args),
            None => Ok(Value::Undefined),
        }
    }
}

/// A queuing strategy: how much to buffer and how to measure chunks.
#[derive(Clone)]
struct Strategy {
    high_water_mark: f64,
    size: Option<Callback>,
}

impl Strategy {
    fn new(strategy: &web::QueuingStrategy, default_hwm: f64) -> Fallible<Self> {
        let high_water_mark = strategy.high_water_mark.unwrap_or(default_hwm);
        if high_water_mark.is_nan() || high_water_mark < 0.0 {
            return Err(Exception::range_error(
                "The high water mark must be a non-negative number",
            ));
        }
        Ok(Self {
            high_water_mark,
            size: strategy.size.clone(),
        })
    }

    /// The size of `chunk`: 1 without a size function.
    fn measure(&self, cx: &mut Cx<'_>, chunk: &Value) -> Fallible<f64> {
        let Some(size) = &self.size else {
            return Ok(1.0);
        };
        let size = cx
            .script
            .call(size, &Value::Undefined, std::slice::from_ref(chunk))?;
        let size = match size {
            Value::Number(n) => n,
            Value::Bool(b) => f64::from(u8::from(b)),
            Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            _ => f64::NAN,
        };
        if !size.is_finite() || size < 0.0 {
            return Err(Exception::range_error(
                "The size of a chunk must be a finite, non-negative number",
            ));
        }
        Ok(size)
    }
}

fn read_result(value: Value, done: bool) -> Value {
    Value::Record(vec![
        ("value".to_string(), value),
        ("done".to_string(), Value::Bool(done)),
    ])
}

fn resolved(cx: &mut Cx<'_>, value: Value) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script.resolve_promise(&promise, value);
    promise
}

fn rejected(cx: &mut Cx<'_>, reason: Value) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script.reject_promise(&promise, Exception::Value(reason));
    promise
}

/// A `TypeError` as a script value, to error streams with.
fn type_error_value(cx: &mut Cx<'_>, message: &str) -> Value {
    cx.script.exception_value(&Exception::type_error(message))
}

/// A promise rejected with a `TypeError`.
fn rejected_type_error(cx: &mut Cx<'_>, message: &str) -> PromiseRef {
    let promise = cx.script.new_promise();
    cx.script
        .reject_promise(&promise, Exception::type_error(message));
    promise
}

/// The `value` and `done` of a read result, which may be a script object.
fn read_outcome(cx: &mut Cx<'_>, result: &Value) -> (Value, bool) {
    match result {
        Value::Record(fields) => {
            let value = fields
                .iter()
                .find(|(k, _)| k == "value")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let done = fields
                .iter()
                .any(|(k, v)| k == "done" && matches!(v, Value::Bool(true)));
            (value, done)
        }
        Value::Opaque(_) => {
            let value = cx.script.get_property(result, "value").unwrap_or_default();
            let done = matches!(
                cx.script.get_property(result, "done"),
                Ok(Value::Bool(true))
            );
            (value, done)
        }
        _ => (Value::Undefined, true),
    }
}

// ---- readable streams -------------------------------------------------------

#[derive(Clone)]
enum ReadableState {
    Readable,
    Closed,
    Errored(Value),
}

pub struct ReadableStreamObject {
    state: ReadableState,
    reader: Option<ObjectId>,
    disturbed: bool,
    controller: ObjectId,
}
platform_object!(ReadableStreamObject, ReadableStream);

/// A source implemented here rather than in script.
#[derive(Clone, Copy)]
enum NativeSource {
    /// One branch of a tee, fed from the reader both branches share.
    Tee { shared: ObjectId, branch: usize },
}

/// The state of a default controller, kept with the controller object.
pub struct ReadableControllerObject {
    stream: Option<ObjectId>,
    algorithms: Option<Algorithms>,
    native: Option<NativeSource>,
    strategy: Strategy,
    queue: VecDeque<(Value, f64)>,
    queue_total_size: f64,
    started: bool,
    pulling: bool,
    pull_again: bool,
    close_requested: bool,
}
platform_object!(ReadableControllerObject, ReadableStreamDefaultController);

pub struct ReadableReaderObject {
    stream: Option<ObjectId>,
    read_requests: VecDeque<PromiseRef>,
    closed: PromiseRef,
}
platform_object!(ReadableReaderObject, ReadableStreamDefaultReader);

fn stream<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ReadableStreamObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ReadableStreamObject, _>(this, f)
}

fn controller<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ReadableControllerObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ReadableControllerObject, _>(this, f)
}

fn reader<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut ReadableReaderObject) -> R,
) -> Fallible<R> {
    cx.page.with::<ReadableReaderObject, _>(this, f)
}

/// Creates a readable stream and its controller around `algorithms`,
/// starting it.
fn create_readable(
    cx: &mut Cx<'_>,
    algorithms: Algorithms,
    strategy: Strategy,
) -> Fallible<ObjectId> {
    let controller_id = cx.page.alloc(ReadableControllerObject {
        stream: None,
        algorithms: Some(algorithms.clone()),
        native: None,
        strategy,
        queue: VecDeque::new(),
        queue_total_size: 0.0,
        started: false,
        pulling: false,
        pull_again: false,
        close_requested: false,
    });
    let stream_id = cx.page.alloc(ReadableStreamObject {
        state: ReadableState::Readable,
        reader: None,
        disturbed: false,
        controller: controller_id,
    });
    controller(cx, controller_id, |c| c.stream = Some(stream_id))?;
    // The controller lives as long as its stream is reachable from script
    // and vice versa.
    cx.pin(controller_id);
    cx.pin(stream_id);

    let start = algorithms.call(cx, &algorithms.start, &[Value::Object(controller_id)]);
    match start {
        Ok(value) => when_settled(cx, value, move |cx, outcome| match outcome {
            Ok(_) => {
                let _ = controller(cx, controller_id, |c| c.started = true);
                pull_if_needed(cx, controller_id);
            }
            Err(reason) => error_readable(cx, controller_id, reason),
        }),
        Err(e) => {
            let reason = cx.script.exception_value(&e);
            error_readable(cx, controller_id, reason);
        }
    }
    Ok(stream_id)
}

fn can_close_or_enqueue(cx: &Cx<'_>, controller_id: ObjectId) -> bool {
    let (stream_id, close_requested) =
        match controller(cx, controller_id, |c| (c.stream, c.close_requested)) {
            Ok(x) => x,
            Err(_) => return false,
        };
    let Some(stream_id) = stream_id else {
        return false;
    };
    let readable = stream(cx, stream_id, |s| {
        matches!(s.state, ReadableState::Readable)
    })
    .unwrap_or(false);
    !close_requested && readable
}

fn desired_size(cx: &Cx<'_>, controller_id: ObjectId) -> Option<f64> {
    let (stream_id, hwm, total) = controller(cx, controller_id, |c| {
        (c.stream, c.strategy.high_water_mark, c.queue_total_size)
    })
    .ok()?;
    let state = stream(cx, stream_id?, |s| s.state.clone()).ok()?;
    match state {
        ReadableState::Errored(_) => None,
        ReadableState::Closed => Some(0.0),
        ReadableState::Readable => Some(hwm - total),
    }
}

fn read_request_count(cx: &Cx<'_>, stream_id: ObjectId) -> usize {
    stream(cx, stream_id, |s| s.reader)
        .ok()
        .flatten()
        .and_then(|r| reader(cx, r, |r| r.read_requests.len()).ok())
        .unwrap_or(0)
}

fn should_pull(cx: &Cx<'_>, controller_id: ObjectId) -> bool {
    if !can_close_or_enqueue(cx, controller_id) {
        return false;
    }
    let (stream_id, started) = match controller(cx, controller_id, |c| (c.stream, c.started)) {
        Ok(x) => x,
        Err(_) => return false,
    };
    if !started {
        return false;
    }
    if let Some(stream_id) = stream_id
        && read_request_count(cx, stream_id) > 0
    {
        return true;
    }
    desired_size(cx, controller_id).is_some_and(|size| size > 0.0)
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-call-pull-if-needed>
fn pull_if_needed(cx: &mut Cx<'_>, controller_id: ObjectId) {
    if !should_pull(cx, controller_id) {
        return;
    }
    // A tee branch is fed by the shared reader, which keeps its own count.
    if let Ok(Some(NativeSource::Tee { shared, .. })) = controller(cx, controller_id, |c| c.native)
    {
        tee_pull(cx, shared);
        return;
    }
    let pulling = controller(cx, controller_id, |c| {
        if c.pulling {
            c.pull_again = true;
            false
        } else {
            c.pulling = true;
            true
        }
    })
    .unwrap_or(false);
    if !pulling {
        return;
    }
    let algorithms = controller(cx, controller_id, |c| c.algorithms.clone())
        .ok()
        .flatten();
    let Some(algorithms) = algorithms else {
        return;
    };
    let result = algorithms.call(cx, &algorithms.main, &[Value::Object(controller_id)]);
    match result {
        Ok(value) => when_settled(cx, value, move |cx, outcome| match outcome {
            Ok(_) => {
                let again = controller(cx, controller_id, |c| {
                    c.pulling = false;
                    std::mem::take(&mut c.pull_again)
                })
                .unwrap_or(false);
                if again {
                    pull_if_needed(cx, controller_id);
                }
            }
            Err(reason) => error_readable(cx, controller_id, reason),
        }),
        Err(e) => {
            let reason = cx.script.exception_value(&e);
            error_readable(cx, controller_id, reason);
        }
    }
}

fn clear_algorithms(cx: &Cx<'_>, controller_id: ObjectId) {
    let _ = controller(cx, controller_id, |c| {
        c.algorithms = None;
        c.strategy.size = None;
    });
}

/// <https://streams.spec.whatwg.org/#readable-stream-close>
fn close_stream(cx: &mut Cx<'_>, stream_id: ObjectId) {
    let reader_id = match stream(cx, stream_id, |s| {
        if !matches!(s.state, ReadableState::Readable) {
            return None;
        }
        s.state = ReadableState::Closed;
        Some(s.reader)
    }) {
        Ok(Some(reader)) => reader,
        _ => return,
    };
    let Some(reader_id) = reader_id else {
        return;
    };
    let (closed, requests) = match reader(cx, reader_id, |r| {
        (r.closed.clone(), std::mem::take(&mut r.read_requests))
    }) {
        Ok(x) => x,
        Err(_) => return,
    };
    cx.script.resolve_promise(&closed, Value::Undefined);
    for request in requests {
        cx.script
            .resolve_promise(&request, read_result(Value::Undefined, true));
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-error>
fn error_stream(cx: &mut Cx<'_>, stream_id: ObjectId, reason: Value) {
    let reader_id = match stream(cx, stream_id, |s| {
        if !matches!(s.state, ReadableState::Readable) {
            return None;
        }
        s.state = ReadableState::Errored(reason.clone());
        Some(s.reader)
    }) {
        Ok(Some(reader)) => reader,
        _ => return,
    };
    let Some(reader_id) = reader_id else {
        return;
    };
    let (closed, requests) = match reader(cx, reader_id, |r| {
        (r.closed.clone(), std::mem::take(&mut r.read_requests))
    }) {
        Ok(x) => x,
        Err(_) => return,
    };
    cx.script
        .reject_promise(&closed, Exception::Value(reason.clone()));
    for request in requests {
        cx.script
            .reject_promise(&request, Exception::Value(reason.clone()));
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-error>
fn error_readable(cx: &mut Cx<'_>, controller_id: ObjectId, reason: Value) {
    let stream_id = controller(cx, controller_id, |c| {
        c.queue.clear();
        c.queue_total_size = 0.0;
        c.stream
    })
    .ok()
    .flatten();
    clear_algorithms(cx, controller_id);
    if let Some(stream_id) = stream_id {
        error_stream(cx, stream_id, reason);
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-close>
fn close_readable(cx: &mut Cx<'_>, controller_id: ObjectId) {
    if !can_close_or_enqueue(cx, controller_id) {
        return;
    }
    let (stream_id, empty) = match controller(cx, controller_id, |c| {
        c.close_requested = true;
        (c.stream, c.queue.is_empty())
    }) {
        Ok(x) => x,
        Err(_) => return,
    };
    if empty {
        clear_algorithms(cx, controller_id);
        if let Some(stream_id) = stream_id {
            close_stream(cx, stream_id);
        }
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-controller-enqueue>
fn enqueue_readable(cx: &mut Cx<'_>, controller_id: ObjectId, chunk: Value) -> Fallible<()> {
    if !can_close_or_enqueue(cx, controller_id) {
        return Ok(());
    }
    let stream_id = controller(cx, controller_id, |c| c.stream)?;
    let waiting = stream_id
        .and_then(|s| stream(cx, s, |s| s.reader).ok().flatten())
        .and_then(|r| {
            reader(cx, r, |r| r.read_requests.pop_front())
                .ok()
                .flatten()
        });
    if let Some(request) = waiting {
        cx.script
            .resolve_promise(&request, read_result(chunk, false));
    } else {
        let strategy = controller(cx, controller_id, |c| c.strategy.clone())?;
        let size = match strategy.measure(cx, &chunk) {
            Ok(size) => size,
            Err(e) => {
                let reason = cx.script.exception_value(&e);
                error_readable(cx, controller_id, reason);
                return Err(e);
            }
        };
        controller(cx, controller_id, |c| {
            c.queue.push_back((chunk, size));
            c.queue_total_size += size;
        })?;
    }
    pull_if_needed(cx, controller_id);
    Ok(())
}

/// <https://streams.spec.whatwg.org/#readable-stream-default-reader-read>:
/// answers `request` from the queue, or keeps it until there is a chunk.
fn read_into(cx: &mut Cx<'_>, stream_id: ObjectId, reader_id: ObjectId, request: PromiseRef) {
    let (state, controller_id) = match stream(cx, stream_id, |s| {
        s.disturbed = true;
        (s.state.clone(), s.controller)
    }) {
        Ok(x) => x,
        Err(_) => return,
    };
    match state {
        ReadableState::Closed => {
            cx.script
                .resolve_promise(&request, read_result(Value::Undefined, true));
        }
        ReadableState::Errored(reason) => {
            cx.script.reject_promise(&request, Exception::Value(reason));
        }
        ReadableState::Readable => {
            let chunk = controller(cx, controller_id, |c| {
                let (chunk, size) = c.queue.pop_front()?;
                c.queue_total_size = (c.queue_total_size - size).max(0.0);
                Some((chunk, c.close_requested && c.queue.is_empty()))
            })
            .ok()
            .flatten();
            match chunk {
                Some((chunk, last)) => {
                    if last {
                        clear_algorithms(cx, controller_id);
                        close_stream(cx, stream_id);
                    } else {
                        pull_if_needed(cx, controller_id);
                    }
                    cx.script
                        .resolve_promise(&request, read_result(chunk, false));
                }
                None => {
                    let _ = reader(cx, reader_id, |r| r.read_requests.push_back(request));
                    pull_if_needed(cx, controller_id);
                }
            }
        }
    }
}

/// <https://streams.spec.whatwg.org/#readable-stream-cancel>
fn cancel_stream(cx: &mut Cx<'_>, stream_id: ObjectId, reason: Value) -> Fallible<PromiseRef> {
    let (state, controller_id) = stream(cx, stream_id, |s| {
        s.disturbed = true;
        (s.state.clone(), s.controller)
    })?;
    match state {
        ReadableState::Closed => return Ok(resolved(cx, Value::Undefined)),
        ReadableState::Errored(reason) => return Ok(rejected(cx, reason)),
        ReadableState::Readable => {}
    }
    close_stream(cx, stream_id);
    let (algorithms, native) = controller(cx, controller_id, |c| {
        c.queue.clear();
        c.queue_total_size = 0.0;
        (c.algorithms.take(), c.native)
    })?;
    clear_algorithms(cx, controller_id);
    if let Some(NativeSource::Tee { shared, branch }) = native {
        return tee_cancel(cx, shared, branch, reason);
    }
    let result = match algorithms {
        Some(algorithms) => algorithms.call(cx, &algorithms.cancel, &[reason]),
        None => Ok(Value::Undefined),
    };
    let promise = cx.script.new_promise();
    match result {
        Ok(value) => {
            let settled = promise.clone();
            when_settled(cx, value, move |cx, outcome| match outcome {
                Ok(_) => cx.script.resolve_promise(&settled, Value::Undefined),
                Err(reason) => cx.script.reject_promise(&settled, Exception::Value(reason)),
            });
        }
        Err(e) => cx.script.reject_promise(&promise, e),
    }
    Ok(promise)
}

/// <https://streams.spec.whatwg.org/#acquire-readable-stream-reader>
fn acquire_reader(cx: &mut Cx<'_>, stream_id: ObjectId) -> Fallible<ObjectId> {
    let (locked, state) = stream(cx, stream_id, |s| (s.reader.is_some(), s.state.clone()))?;
    if locked {
        return Err(Exception::type_error(
            "The stream is already locked to a reader",
        ));
    }
    let closed = match state {
        ReadableState::Readable => cx.script.new_promise(),
        ReadableState::Closed => resolved(cx, Value::Undefined),
        ReadableState::Errored(reason) => rejected(cx, reason),
    };
    let reader_id = cx.page.alloc(ReadableReaderObject {
        stream: Some(stream_id),
        read_requests: VecDeque::new(),
        closed,
    });
    stream(cx, stream_id, |s| s.reader = Some(reader_id))?;
    // A reader with requests outstanding is kept until it is released.
    cx.pin(reader_id);
    Ok(reader_id)
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-readablestreamdefaultreaderrelease>
fn release_reader(cx: &mut Cx<'_>, reader_id: ObjectId) -> Fallible<()> {
    let (stream_id, requests, closed) = reader(cx, reader_id, |r| {
        (
            r.stream.take(),
            std::mem::take(&mut r.read_requests),
            r.closed.clone(),
        )
    })?;
    let Some(stream_id) = stream_id else {
        return Ok(());
    };
    let state = stream(cx, stream_id, |s| {
        s.reader = None;
        s.state.clone()
    })?;
    let released = type_error_value(cx, "The reader was released");
    if matches!(state, ReadableState::Readable) {
        cx.script
            .reject_promise(&closed, Exception::Value(released.clone()));
    } else {
        let fresh = rejected(cx, released.clone());
        reader(cx, reader_id, |r| r.closed = fresh)?;
    }
    for request in requests {
        cx.script
            .reject_promise(&request, Exception::Value(released.clone()));
    }
    cx.unpin(reader_id);
    Ok(())
}

/// Whether a stream has been read from or is locked to a reader: what
/// makes a body "used".
pub(crate) fn is_disturbed_or_locked(cx: &Cx<'_>, stream_id: ObjectId) -> bool {
    stream(cx, stream_id, |s| s.disturbed || s.reader.is_some()).unwrap_or(false)
}

/// Marks a stream as read from.
pub(crate) fn mark_disturbed(cx: &Cx<'_>, stream_id: ObjectId) {
    let _ = stream(cx, stream_id, |s| s.disturbed = true);
}

/// A readable stream over the bytes of `body`, for `Response.body`.
pub(crate) fn readable_from_bytes(cx: &mut Cx<'_>, body: Vec<u8>) -> Fallible<ObjectId> {
    let algorithms = Algorithms {
        this: Value::Undefined,
        start: None,
        main: None,
        cancel: None,
        close: None,
    };
    let strategy = Strategy {
        high_water_mark: 0.0,
        size: None,
    };
    let stream_id = create_readable(cx, algorithms, strategy)?;
    let controller_id = stream(cx, stream_id, |s| s.controller)?;
    if !body.is_empty() {
        enqueue_readable(cx, controller_id, Value::Uint8Array(body))?;
    }
    close_readable(cx, controller_id);
    Ok(stream_id)
}

impl web::ReadableStreamImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        underlying_source: Value,
        strategy: web::QueuingStrategy,
    ) -> Fallible<ObjectId> {
        let strategy = Strategy::new(&strategy, 1.0)?;
        let algorithms = match underlying_source {
            Value::Undefined | Value::Null => Algorithms {
                this: Value::Undefined,
                start: None,
                main: None,
                cancel: None,
                close: None,
            },
            source => {
                // A byte stream is served as a default stream; BYOB readers
                // are what is missing.
                Algorithms::read(cx, &source, ["start", "pull", "cancel", "close"])?
            }
        };
        create_readable(cx, algorithms, strategy)
    }

    fn locked(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        stream(cx, this, |s| s.reader.is_some())
    }

    fn cancel(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<PromiseRef> {
        if stream(cx, this, |s| s.reader.is_some())? {
            return Ok(rejected_type_error(cx, "The stream is locked to a reader"));
        }
        cancel_stream(cx, this, reason)
    }

    fn get_reader(
        cx: &mut Cx<'_>,
        this: ObjectId,
        options: web::ReadableStreamGetReaderOptions,
    ) -> Fallible<ObjectId> {
        if options.mode.is_some() {
            return Err(Exception::type_error("BYOB readers are not supported yet"));
        }
        acquire_reader(cx, this)
    }

    fn pipe_through(
        cx: &mut Cx<'_>,
        this: ObjectId,
        transform: web::ReadableWritablePair,
        options: web::StreamPipeOptions,
    ) -> Fallible<ObjectId> {
        if stream(cx, this, |s| s.reader.is_some())? {
            return Err(Exception::type_error("The stream is locked to a reader"));
        }
        if writable(cx, transform.writable, |w| w.writer.is_some())? {
            return Err(Exception::type_error(
                "The destination is locked to a writer",
            ));
        }
        pipe(cx, this, transform.writable, options)?;
        Ok(transform.readable)
    }

    fn pipe_to(
        cx: &mut Cx<'_>,
        this: ObjectId,
        destination: ObjectId,
        options: web::StreamPipeOptions,
    ) -> Fallible<PromiseRef> {
        if stream(cx, this, |s| s.reader.is_some())? {
            return Ok(rejected_type_error(cx, "The stream is locked to a reader"));
        }
        if writable(cx, destination, |w| w.writer.is_some())? {
            return Ok(rejected_type_error(
                cx,
                "The destination is locked to a writer",
            ));
        }
        pipe(cx, this, destination, options)
    }

    fn tee(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        tee(cx, this)
    }
}

impl web::ReadableStreamGenericReaderImpl for Web {
    fn closed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        reader(cx, this, |r| r.closed.clone())
    }

    fn cancel(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<PromiseRef> {
        let Some(stream_id) = reader(cx, this, |r| r.stream)? else {
            return Ok(rejected_type_error(cx, "The reader has been released"));
        };
        cancel_stream(cx, stream_id, reason)
    }
}

impl web::ReadableStreamDefaultReaderImpl for Web {
    fn constructor(cx: &mut Cx<'_>, stream: ObjectId) -> Fallible<ObjectId> {
        acquire_reader(cx, stream)
    }

    fn read(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let Some(stream_id) = reader(cx, this, |r| r.stream)? else {
            return Ok(rejected_type_error(cx, "The reader has been released"));
        };
        let request = cx.script.new_promise();
        read_into(cx, stream_id, this, request.clone());
        Ok(request)
    }

    fn release_lock(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        release_reader(cx, this)
    }
}

impl web::ReadableStreamDefaultControllerImpl for Web {
    fn desired_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<f64>> {
        controller(cx, this, |_| ())?;
        Ok(desired_size(cx, this))
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        if !can_close_or_enqueue(cx, this) {
            return Err(Exception::type_error(
                "The stream is not in a state that permits close",
            ));
        }
        close_readable(cx, this);
        Ok(())
    }

    fn enqueue(cx: &mut Cx<'_>, this: ObjectId, chunk: Value) -> Fallible<()> {
        if !can_close_or_enqueue(cx, this) {
            return Err(Exception::type_error(
                "The stream is not in a state that permits enqueue",
            ));
        }
        enqueue_readable(cx, this, chunk)
    }

    fn error(cx: &mut Cx<'_>, this: ObjectId, e: Value) -> Fallible<()> {
        controller(cx, this, |_| ())?;
        error_readable(cx, this, e);
        Ok(())
    }
}

/// <https://streams.spec.whatwg.org/#abstract-opdef-readablestreamdefaulttee>
fn tee(cx: &mut Cx<'_>, stream_id: ObjectId) -> Fallible<Vec<ObjectId>> {
    let reader_id = acquire_reader(cx, stream_id)?;
    // What both branches share: the reader and the cancellations so far.
    let shared = cx.page.alloc(TeeObject {
        reader: reader_id,
        source: stream_id,
        reading: false,
        read_again: false,
        branches: [None, None],
        canceled: [None, None],
        cancel_promise: None,
    });
    cx.pin(shared);
    let mut branches = Vec::new();
    for branch in 0..2 {
        let algorithms = Algorithms {
            this: Value::Undefined,
            start: None,
            main: None,
            cancel: None,
            close: None,
        };
        let strategy = Strategy {
            high_water_mark: 1.0,
            size: None,
        };
        let branch_id = create_readable(cx, algorithms, strategy)?;
        // The branch pulls from the shared reader, and cancels through it.
        let controller_id = stream(cx, branch_id, |s| s.controller)?;
        controller(cx, controller_id, |c| {
            c.native = Some(NativeSource::Tee { shared, branch });
        })?;
        cx.page
            .with::<TeeObject, _>(shared, |t| t.branches[branch] = Some(branch_id))?;
        branches.push(branch_id);
    }
    // The source's end or failure reaches both branches.
    let closed = reader(cx, reader_id, |r| r.closed.clone())?;
    when_settled(cx, Value::Promise(closed), move |cx, outcome| {
        if let Err(reason) = outcome {
            let branches = cx
                .page
                .with::<TeeObject, _>(shared, |t| t.branches)
                .unwrap_or_default();
            for branch in branches.into_iter().flatten() {
                if let Ok(controller_id) = stream(cx, branch, |s| s.controller) {
                    error_readable(cx, controller_id, reason.clone());
                }
            }
        }
    });
    Ok(branches)
}

/// What the two branches of a tee share.
pub struct TeeObject {
    reader: ObjectId,
    source: ObjectId,
    reading: bool,
    read_again: bool,
    branches: [Option<ObjectId>; 2],
    canceled: [Option<Value>; 2],
    cancel_promise: Option<PromiseRef>,
}
platform_object!(TeeObject, ReadableStream);

fn tee_state<R>(cx: &Cx<'_>, shared: ObjectId, f: impl FnOnce(&mut TeeObject) -> R) -> Option<R> {
    cx.page.with::<TeeObject, _>(shared, f).ok()
}

/// A branch asks for a chunk: one read on the shared reader serves both.
fn tee_pull(cx: &mut Cx<'_>, shared: ObjectId) {
    let reading = tee_state(cx, shared, |t| {
        if t.reading {
            t.read_again = true;
            false
        } else {
            t.reading = true;
            true
        }
    });
    if reading != Some(true) {
        return;
    }
    let Some((reader_id, source)) = tee_state(cx, shared, |t| (t.reader, t.source)) else {
        return;
    };
    let request = cx.script.new_promise();
    read_into(cx, source, reader_id, request.clone());
    when_settled(cx, Value::Promise(request), move |cx, outcome| {
        let Some((branches, canceled)) = tee_state(cx, shared, |t| {
            t.read_again = false;
            (
                t.branches,
                [t.canceled[0].is_some(), t.canceled[1].is_some()],
            )
        }) else {
            return;
        };
        let controllers: Vec<Option<ObjectId>> = branches
            .iter()
            .map(|b| b.and_then(|b| stream(cx, b, |s| s.controller).ok()))
            .collect();
        let Ok(result) = outcome else {
            // The reader's `closed` promise carries the failure to both.
            let _ = tee_state(cx, shared, |t| t.reading = false);
            return;
        };
        let (value, done) = read_outcome(cx, &result);
        if done {
            let _ = tee_state(cx, shared, |t| t.reading = false);
            for (i, controller_id) in controllers.iter().enumerate() {
                if let (false, Some(controller_id)) = (canceled[i], controller_id) {
                    close_readable(cx, *controller_id);
                }
            }
            if !(canceled[0] && canceled[1])
                && let Some(Some(promise)) = tee_state(cx, shared, |t| t.cancel_promise.clone())
            {
                cx.script.resolve_promise(&promise, Value::Undefined);
            }
            return;
        }
        for (i, controller_id) in controllers.iter().enumerate() {
            if let (false, Some(controller_id)) = (canceled[i], controller_id) {
                let _ = enqueue_readable(cx, *controller_id, value.clone());
            }
        }
        let again = tee_state(cx, shared, |t| {
            t.reading = false;
            std::mem::take(&mut t.read_again)
        });
        if again == Some(true) {
            tee_pull(cx, shared);
        }
    });
}

/// A branch is cancelled: the source is cancelled once both are.
fn tee_cancel(
    cx: &mut Cx<'_>,
    shared: ObjectId,
    branch: usize,
    reason: Value,
) -> Fallible<PromiseRef> {
    let promise = cx.script.new_promise();
    let both = cx.page.with::<TeeObject, _>(shared, |t| {
        t.canceled[branch] = Some(reason);
        if t.cancel_promise.is_none() {
            t.cancel_promise = Some(promise.clone());
        }
        (t.canceled[0].is_some() && t.canceled[1].is_some())
            .then(|| (t.source, t.canceled.clone(), t.cancel_promise.clone()))
    })?;
    if let Some((source, reasons, settled)) = both {
        let composite = Value::Array(reasons.into_iter().map(Option::unwrap_or_default).collect());
        let cancelled = cancel_stream(cx, source, composite)?;
        when_settled(cx, Value::Promise(cancelled), move |cx, outcome| {
            let Some(settled) = settled else {
                return;
            };
            match outcome {
                Ok(_) => cx.script.resolve_promise(&settled, Value::Undefined),
                Err(reason) => cx.script.reject_promise(&settled, Exception::Value(reason)),
            }
        });
    }
    Ok(cx
        .page
        .with::<TeeObject, _>(shared, |t| t.cancel_promise.clone())?
        .unwrap_or(promise))
}

// ---- writable streams -------------------------------------------------------

#[derive(Clone)]
enum WritableState {
    Writable,
    Closed,
    Errored(Value),
}

pub struct WritableStreamObject {
    state: WritableState,
    writer: Option<ObjectId>,
    algorithms: Option<Algorithms>,
    /// The writable side of a transform stream: its controller.
    transform: Option<ObjectId>,
    strategy: Strategy,
    controller: ObjectId,
    /// Writes waiting for the sink, with the promise each returns.
    queue: VecDeque<(Value, f64, PromiseRef)>,
    queue_total_size: f64,
    started: bool,
    writing: bool,
    /// `close()` was called: the sink is closed once the queue drains.
    close_request: Option<PromiseRef>,
    /// Promises waiting for backpressure to ease.
    ready_waiters: Vec<PromiseRef>,
    /// What happens to the writer's `closed` promise.
    closed_waiters: Vec<PromiseRef>,
}
platform_object!(WritableStreamObject, WritableStream);

pub struct WritableControllerObject {
    stream: Option<ObjectId>,
}
platform_object!(WritableControllerObject, WritableStreamDefaultController);

pub struct WritableWriterObject {
    stream: Option<ObjectId>,
    closed: PromiseRef,
}
platform_object!(WritableWriterObject, WritableStreamDefaultWriter);

fn writable<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut WritableStreamObject) -> R,
) -> Fallible<R> {
    cx.page.with::<WritableStreamObject, _>(this, f)
}

fn writer<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut WritableWriterObject) -> R,
) -> Fallible<R> {
    cx.page.with::<WritableWriterObject, _>(this, f)
}

fn create_writable(
    cx: &mut Cx<'_>,
    algorithms: Algorithms,
    strategy: Strategy,
) -> Fallible<ObjectId> {
    let controller_id = cx.page.alloc(WritableControllerObject { stream: None });
    let stream_id = cx.page.alloc(WritableStreamObject {
        state: WritableState::Writable,
        writer: None,
        algorithms: Some(algorithms.clone()),
        transform: None,
        strategy,
        controller: controller_id,
        queue: VecDeque::new(),
        queue_total_size: 0.0,
        started: false,
        writing: false,
        close_request: None,
        ready_waiters: Vec::new(),
        closed_waiters: Vec::new(),
    });
    cx.page
        .with::<WritableControllerObject, _>(controller_id, |c| c.stream = Some(stream_id))?;
    cx.pin(controller_id);
    cx.pin(stream_id);
    let start = algorithms.call(cx, &algorithms.start, &[Value::Object(controller_id)]);
    match start {
        Ok(value) => when_settled(cx, value, move |cx, outcome| match outcome {
            Ok(_) => {
                let _ = writable(cx, stream_id, |w| w.started = true);
                advance_writes(cx, stream_id);
            }
            Err(reason) => error_writable(cx, stream_id, reason),
        }),
        Err(e) => {
            let reason = cx.script.exception_value(&e);
            error_writable(cx, stream_id, reason);
        }
    }
    Ok(stream_id)
}

fn writable_desired_size(cx: &Cx<'_>, stream_id: ObjectId) -> Option<f64> {
    writable(cx, stream_id, |w| match w.state {
        WritableState::Errored(_) => None,
        WritableState::Closed => Some(0.0),
        WritableState::Writable => Some(w.strategy.high_water_mark - w.queue_total_size),
    })
    .ok()
    .flatten()
}

/// Lets `ready` promises through when there is room again.
fn ease_backpressure(cx: &mut Cx<'_>, stream_id: ObjectId) {
    if writable_desired_size(cx, stream_id).is_some_and(|size| size > 0.0) {
        let waiters =
            writable(cx, stream_id, |w| std::mem::take(&mut w.ready_waiters)).unwrap_or_default();
        for waiter in waiters {
            cx.script.resolve_promise(&waiter, Value::Undefined);
        }
    }
}

/// Hands the next queued write to the sink, or closes the sink once the
/// queue is empty and a close was requested.
fn advance_writes(cx: &mut Cx<'_>, stream_id: ObjectId) {
    let next = writable(cx, stream_id, |w| {
        if !w.started || w.writing || !matches!(w.state, WritableState::Writable) {
            return None;
        }
        if let Some((chunk, size, promise)) = w.queue.pop_front() {
            w.queue_total_size = (w.queue_total_size - size).max(0.0);
            w.writing = true;
            return Some(Ok((chunk, promise)));
        }
        w.close_request.take().map(Err)
    })
    .ok()
    .flatten();
    let (algorithms, transform) =
        match writable(cx, stream_id, |w| (w.algorithms.clone(), w.transform)) {
            Ok(x) => x,
            Err(_) => return,
        };
    match next {
        None => {}
        Some(Ok((chunk, promise))) => {
            ease_backpressure(cx, stream_id);
            let controller_id = writable(cx, stream_id, |w| w.controller).unwrap_or(stream_id);
            let result = match (&algorithms, transform) {
                (_, Some(transform)) => transform_write(cx, transform, chunk),
                (Some(algorithms), None) => {
                    algorithms.call(cx, &algorithms.main, &[chunk, Value::Object(controller_id)])
                }
                (None, None) => Ok(Value::Undefined),
            };
            match result {
                Ok(value) => when_settled(cx, value, move |cx, outcome| {
                    let _ = writable(cx, stream_id, |w| w.writing = false);
                    match outcome {
                        Ok(_) => {
                            cx.script.resolve_promise(&promise, Value::Undefined);
                            advance_writes(cx, stream_id);
                        }
                        Err(reason) => {
                            cx.script
                                .reject_promise(&promise, Exception::Value(reason.clone()));
                            error_writable(cx, stream_id, reason);
                        }
                    }
                }),
                Err(e) => {
                    let _ = writable(cx, stream_id, |w| w.writing = false);
                    let reason = cx.script.exception_value(&e);
                    cx.script.reject_promise(&promise, e);
                    error_writable(cx, stream_id, reason);
                }
            }
        }
        Some(Err(close_promise)) => {
            let _ = writable(cx, stream_id, |w| w.writing = true);
            let result = match (&algorithms, transform) {
                (_, Some(transform)) => transform_flush(cx, transform),
                (Some(algorithms), None) => algorithms.call(cx, &algorithms.close, &[]),
                (None, None) => Ok(Value::Undefined),
            };
            match result {
                Ok(value) => when_settled(cx, value, move |cx, outcome| match outcome {
                    Ok(_) => {
                        finish_closing(cx, stream_id);
                        if let Some(transform) = transform {
                            transform_done(cx, transform);
                        }
                        cx.script.resolve_promise(&close_promise, Value::Undefined);
                    }
                    Err(reason) => {
                        cx.script
                            .reject_promise(&close_promise, Exception::Value(reason.clone()));
                        error_writable(cx, stream_id, reason);
                    }
                }),
                Err(e) => {
                    let reason = cx.script.exception_value(&e);
                    cx.script.reject_promise(&close_promise, e);
                    error_writable(cx, stream_id, reason);
                }
            }
        }
    }
}

fn finish_closing(cx: &mut Cx<'_>, stream_id: ObjectId) {
    let (waiters, ready) = match writable(cx, stream_id, |w| {
        w.state = WritableState::Closed;
        w.writing = false;
        w.algorithms = None;
        (
            std::mem::take(&mut w.closed_waiters),
            std::mem::take(&mut w.ready_waiters),
        )
    }) {
        Ok(x) => x,
        Err(_) => return,
    };
    for waiter in waiters {
        cx.script.resolve_promise(&waiter, Value::Undefined);
    }
    for waiter in ready {
        cx.script.resolve_promise(&waiter, Value::Undefined);
    }
}

fn error_writable(cx: &mut Cx<'_>, stream_id: ObjectId, reason: Value) {
    let taken = writable(cx, stream_id, |w| {
        if !matches!(w.state, WritableState::Writable) {
            return None;
        }
        w.state = WritableState::Errored(reason.clone());
        w.algorithms = None;
        w.writing = false;
        let queue: Vec<PromiseRef> = w.queue.drain(..).map(|(_, _, p)| p).collect();
        w.queue_total_size = 0.0;
        Some((
            queue,
            w.close_request.take(),
            std::mem::take(&mut w.ready_waiters),
            std::mem::take(&mut w.closed_waiters),
        ))
    })
    .ok()
    .flatten();
    let Some((queue, close_request, ready, closed)) = taken else {
        return;
    };
    for promise in queue
        .into_iter()
        .chain(close_request)
        .chain(ready)
        .chain(closed)
    {
        cx.script
            .reject_promise(&promise, Exception::Value(reason.clone()));
    }
}

fn abort_writable(cx: &mut Cx<'_>, stream_id: ObjectId, reason: Value) -> Fallible<PromiseRef> {
    let (state, algorithms) = writable(cx, stream_id, |w| (w.state.clone(), w.algorithms.clone()))?;
    match state {
        WritableState::Closed => return Ok(resolved(cx, Value::Undefined)),
        WritableState::Errored(_) => return Ok(resolved(cx, Value::Undefined)),
        WritableState::Writable => {}
    }
    let transform = writable(cx, stream_id, |w| w.transform)?;
    error_writable(cx, stream_id, reason.clone());
    if let Some(transform) = transform {
        transform_error(cx, transform, reason.clone());
    }
    let result = match algorithms {
        Some(algorithms) => algorithms.call(cx, &algorithms.cancel, &[reason]),
        None => Ok(Value::Undefined),
    };
    let promise = cx.script.new_promise();
    match result {
        Ok(value) => {
            let settled = promise.clone();
            when_settled(cx, value, move |cx, outcome| match outcome {
                Ok(_) => cx.script.resolve_promise(&settled, Value::Undefined),
                Err(reason) => cx.script.reject_promise(&settled, Exception::Value(reason)),
            });
        }
        Err(e) => cx.script.reject_promise(&promise, e),
    }
    Ok(promise)
}

fn write_chunk(cx: &mut Cx<'_>, stream_id: ObjectId, chunk: Value) -> Fallible<PromiseRef> {
    let state = writable(cx, stream_id, |w| w.state.clone())?;
    match state {
        WritableState::Closed => {
            return Ok(rejected_type_error(cx, "The stream is closed"));
        }
        WritableState::Errored(reason) => return Ok(rejected(cx, reason)),
        WritableState::Writable => {}
    }
    if writable(cx, stream_id, |w| w.close_request.is_some())? {
        return Ok(rejected_type_error(cx, "The stream is closing"));
    }
    let strategy = writable(cx, stream_id, |w| w.strategy.clone())?;
    let size = match strategy.measure(cx, &chunk) {
        Ok(size) => size,
        Err(e) => {
            let reason = cx.script.exception_value(&e);
            error_writable(cx, stream_id, reason.clone());
            return Ok(rejected(cx, reason));
        }
    };
    let promise = cx.script.new_promise();
    writable(cx, stream_id, |w| {
        w.queue.push_back((chunk, size, promise.clone()));
        w.queue_total_size += size;
    })?;
    advance_writes(cx, stream_id);
    Ok(promise)
}

fn close_writable(cx: &mut Cx<'_>, stream_id: ObjectId) -> Fallible<PromiseRef> {
    let state = writable(cx, stream_id, |w| w.state.clone())?;
    match state {
        WritableState::Closed => {
            return Ok(rejected_type_error(cx, "The stream is already closed"));
        }
        WritableState::Errored(reason) => return Ok(rejected(cx, reason)),
        WritableState::Writable => {}
    }
    if writable(cx, stream_id, |w| w.close_request.is_some())? {
        return Ok(rejected_type_error(cx, "The stream is already closing"));
    }
    let promise = cx.script.new_promise();
    writable(cx, stream_id, |w| w.close_request = Some(promise.clone()))?;
    advance_writes(cx, stream_id);
    Ok(promise)
}

fn acquire_writer(cx: &mut Cx<'_>, stream_id: ObjectId) -> Fallible<ObjectId> {
    let (locked, state) = writable(cx, stream_id, |w| (w.writer.is_some(), w.state.clone()))?;
    if locked {
        return Err(Exception::type_error(
            "The stream is already locked to a writer",
        ));
    }
    let closed = match state {
        WritableState::Writable => {
            let promise = cx.script.new_promise();
            writable(cx, stream_id, |w| w.closed_waiters.push(promise.clone()))?;
            promise
        }
        WritableState::Closed => resolved(cx, Value::Undefined),
        WritableState::Errored(reason) => rejected(cx, reason),
    };
    let writer_id = cx.page.alloc(WritableWriterObject {
        stream: Some(stream_id),
        closed,
    });
    writable(cx, stream_id, |w| w.writer = Some(writer_id))?;
    cx.pin(writer_id);
    Ok(writer_id)
}

impl web::WritableStreamImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        underlying_sink: Value,
        strategy: web::QueuingStrategy,
    ) -> Fallible<ObjectId> {
        let strategy = Strategy::new(&strategy, 1.0)?;
        let algorithms = match underlying_sink {
            Value::Undefined | Value::Null => Algorithms {
                this: Value::Undefined,
                start: None,
                main: None,
                cancel: None,
                close: None,
            },
            sink => Algorithms::read(cx, &sink, ["start", "write", "abort", "close"])?,
        };
        create_writable(cx, algorithms, strategy)
    }

    fn locked(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        writable(cx, this, |w| w.writer.is_some())
    }

    fn abort(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<PromiseRef> {
        if writable(cx, this, |w| w.writer.is_some())? {
            return Ok(rejected_type_error(cx, "The stream is locked to a writer"));
        }
        abort_writable(cx, this, reason)
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        if writable(cx, this, |w| w.writer.is_some())? {
            return Ok(rejected_type_error(cx, "The stream is locked to a writer"));
        }
        close_writable(cx, this)
    }

    fn get_writer(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        acquire_writer(cx, this)
    }
}

fn writer_stream(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Result<ObjectId, Value>> {
    match writer(cx, this, |w| w.stream)? {
        Some(stream_id) => Ok(Ok(stream_id)),
        None => Ok(Err(type_error_value(cx, "The writer has been released"))),
    }
}

impl web::WritableStreamDefaultWriterImpl for Web {
    fn constructor(cx: &mut Cx<'_>, stream: ObjectId) -> Fallible<ObjectId> {
        acquire_writer(cx, stream)
    }

    fn closed(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        writer(cx, this, |w| w.closed.clone())
    }

    fn desired_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<f64>> {
        match writer_stream(cx, this)? {
            Ok(stream_id) => Ok(writable_desired_size(cx, stream_id)),
            Err(_) => Err(Exception::type_error("The writer has been released")),
        }
    }

    fn ready(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let stream_id = match writer_stream(cx, this)? {
            Ok(stream_id) => stream_id,
            Err(reason) => return Ok(rejected(cx, reason)),
        };
        let state = writable(cx, stream_id, |w| w.state.clone())?;
        if let WritableState::Errored(reason) = state {
            return Ok(rejected(cx, reason));
        }
        if writable_desired_size(cx, stream_id).is_none_or(|size| size > 0.0) {
            return Ok(resolved(cx, Value::Undefined));
        }
        let promise = cx.script.new_promise();
        writable(cx, stream_id, |w| w.ready_waiters.push(promise.clone()))?;
        Ok(promise)
    }

    fn abort(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<PromiseRef> {
        match writer_stream(cx, this)? {
            Ok(stream_id) => abort_writable(cx, stream_id, reason),
            Err(reason) => Ok(rejected(cx, reason)),
        }
    }

    fn close(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        match writer_stream(cx, this)? {
            Ok(stream_id) => close_writable(cx, stream_id),
            Err(reason) => Ok(rejected(cx, reason)),
        }
    }

    fn release_lock(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let Some(stream_id) = writer(cx, this, |w| w.stream.take())? else {
            return Ok(());
        };
        writable(cx, stream_id, |w| w.writer = None)?;
        let released = type_error_value(cx, "The writer was released");
        let fresh = rejected(cx, released.clone());
        writer(cx, this, |w| w.closed = fresh)?;
        cx.unpin(this);
        Ok(())
    }

    fn write(cx: &mut Cx<'_>, this: ObjectId, chunk: Value) -> Fallible<PromiseRef> {
        match writer_stream(cx, this)? {
            Ok(stream_id) => write_chunk(cx, stream_id, chunk),
            Err(reason) => Ok(rejected(cx, reason)),
        }
    }
}

impl web::WritableStreamDefaultControllerImpl for Web {
    fn error(cx: &mut Cx<'_>, this: ObjectId, e: Value) -> Fallible<()> {
        let stream_id = cx
            .page
            .with::<WritableControllerObject, _>(this, |c| c.stream)?;
        if let Some(stream_id) = stream_id {
            error_writable(cx, stream_id, e);
        }
        Ok(())
    }
}

// ---- piping -----------------------------------------------------------------

/// <https://streams.spec.whatwg.org/#readable-stream-pipe-to>, reading
/// chunk by chunk into a writer.
fn pipe(
    cx: &mut Cx<'_>,
    source: ObjectId,
    destination: ObjectId,
    options: web::StreamPipeOptions,
) -> Fallible<PromiseRef> {
    let reader_id = acquire_reader(cx, source)?;
    let writer_id = acquire_writer(cx, destination)?;
    let promise = cx.script.new_promise();
    let pipe_id = cx.page.alloc(PipeObject {
        reader: reader_id,
        writer: writer_id,
        promise: promise.clone(),
        prevent_close: options.prevent_close,
        prevent_abort: options.prevent_abort,
        prevent_cancel: options.prevent_cancel,
    });
    cx.pin(pipe_id);
    pipe_step(cx, pipe_id);
    Ok(promise)
}

pub struct PipeObject {
    reader: ObjectId,
    writer: ObjectId,
    promise: PromiseRef,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
}
platform_object!(PipeObject, ReadableStream);

fn pipe_step(cx: &mut Cx<'_>, pipe_id: ObjectId) {
    let Ok((reader_id, writer_id)) = cx
        .page
        .with::<PipeObject, _>(pipe_id, |p| (p.reader, p.writer))
    else {
        return;
    };
    // A failed destination ends the pipe before another read.
    let destination = writer(cx, writer_id, |w| w.stream).ok().flatten();
    let errored = destination
        .and_then(|d| writable(cx, d, |w| w.state.clone()).ok())
        .and_then(|state| match state {
            WritableState::Errored(reason) => Some(reason),
            _ => None,
        });
    if let Some(reason) = errored {
        pipe_finish(cx, pipe_id, Err((reason, true)));
        return;
    }
    let request = cx.script.new_promise();
    let Some(source) = reader(cx, reader_id, |r| r.stream).ok().flatten() else {
        return;
    };
    read_into(cx, source, reader_id, request.clone());
    when_settled(cx, Value::Promise(request), move |cx, outcome| {
        let Ok((reader_id, writer_id)) = cx
            .page
            .with::<PipeObject, _>(pipe_id, |p| (p.reader, p.writer))
        else {
            return;
        };
        let _ = reader_id;
        match outcome {
            Err(reason) => pipe_finish(cx, pipe_id, Err((reason, false))),
            Ok(result) => {
                let (value, done) = read_outcome(cx, &result);
                if done {
                    pipe_finish(cx, pipe_id, Ok(()));
                    return;
                }
                let Some(destination) = writer(cx, writer_id, |w| w.stream).ok().flatten() else {
                    return;
                };
                match write_chunk(cx, destination, value) {
                    Ok(written) => {
                        when_settled(
                            cx,
                            Value::Promise(written),
                            move |cx, outcome| match outcome {
                                Ok(_) => pipe_step(cx, pipe_id),
                                Err(reason) => pipe_finish(cx, pipe_id, Err((reason, true))),
                            },
                        )
                    }
                    Err(e) => {
                        let reason = cx.script.exception_value(&e);
                        pipe_finish(cx, pipe_id, Err((reason, true)));
                    }
                }
            }
        }
    });
}

/// Ends a pipe: closing or aborting the destination and cancelling the
/// source as the options say, then settling the pipe's promise.
fn pipe_finish(cx: &mut Cx<'_>, pipe_id: ObjectId, outcome: Result<(), (Value, bool)>) {
    let Ok(pipe) = cx.page.with::<PipeObject, _>(pipe_id, |p| {
        (
            p.reader,
            p.writer,
            p.promise.clone(),
            p.prevent_close,
            p.prevent_abort,
            p.prevent_cancel,
        )
    }) else {
        return;
    };
    let (reader_id, writer_id, promise, prevent_close, prevent_abort, prevent_cancel) = pipe;
    let destination = writer(cx, writer_id, |w| w.stream).ok().flatten();
    let source = reader(cx, reader_id, |r| r.stream).ok().flatten();
    match &outcome {
        Ok(()) => {
            if !prevent_close && let Some(destination) = destination {
                let _ = close_writable(cx, destination);
            }
        }
        // The destination failed: the source is cancelled.
        Err((reason, true)) => {
            if !prevent_cancel && let Some(source) = source {
                let _ = cancel_stream(cx, source, reason.clone());
            }
        }
        // The source failed: the destination is aborted.
        Err((reason, false)) => {
            if !prevent_abort && let Some(destination) = destination {
                let _ = abort_writable(cx, destination, reason.clone());
            }
        }
    }
    let _ = release_reader(cx, reader_id);
    let _ = <Web as web::WritableStreamDefaultWriterImpl>::release_lock(cx, writer_id);
    match outcome {
        Ok(()) => cx.script.resolve_promise(&promise, Value::Undefined),
        Err((reason, _)) => cx.script.reject_promise(&promise, Exception::Value(reason)),
    }
    cx.unpin(pipe_id);
}

// ---- transform streams ------------------------------------------------------

pub struct TransformStreamObject {
    readable: ObjectId,
    writable: ObjectId,
}
platform_object!(TransformStreamObject, TransformStream);

pub struct TransformControllerObject {
    /// The readable side's controller.
    readable_controller: ObjectId,
    writable: ObjectId,
    algorithms: Option<Algorithms>,
}
platform_object!(TransformControllerObject, TransformStreamDefaultController);

fn transform_controller<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut TransformControllerObject) -> R,
) -> Fallible<R> {
    cx.page.with::<TransformControllerObject, _>(this, f)
}

impl web::TransformStreamImpl for Web {
    fn constructor(
        cx: &mut Cx<'_>,
        transformer: Value,
        writable_strategy: web::QueuingStrategy,
        readable_strategy: web::QueuingStrategy,
    ) -> Fallible<ObjectId> {
        let writable_strategy = Strategy::new(&writable_strategy, 1.0)?;
        let readable_strategy = Strategy::new(&readable_strategy, 0.0)?;
        let transformer = match transformer {
            Value::Undefined | Value::Null => None,
            object => Some(Algorithms::read(
                cx,
                &object,
                ["start", "transform", "cancel", "flush"],
            )?),
        };
        let none = Algorithms {
            this: Value::Undefined,
            start: None,
            main: None,
            cancel: None,
            close: None,
        };
        // The readable side is fed by the controller; the writable side
        // feeds the transform.
        let readable = create_readable(cx, none.clone(), readable_strategy)?;
        let readable_controller = stream(cx, readable, |s| s.controller)?;
        let controller_id = cx.page.alloc(TransformControllerObject {
            readable_controller,
            writable: readable,
            algorithms: transformer.clone(),
        });
        cx.pin(controller_id);
        let writable = create_writable(cx, none, writable_strategy)?;
        transform_controller(cx, controller_id, |c| c.writable = writable)?;
        writable_hooks(cx, writable, controller_id);
        let stream_id = cx.page.alloc(TransformStreamObject { readable, writable });
        cx.pin(stream_id);
        if let Some(transformer) = transformer {
            let start = transformer.call(cx, &transformer.start, &[Value::Object(controller_id)]);
            match start {
                Ok(value) => when_settled(cx, value, move |cx, outcome| {
                    if let Err(reason) = outcome {
                        transform_error(cx, controller_id, reason);
                    }
                }),
                Err(e) => {
                    let reason = cx.script.exception_value(&e);
                    transform_error(cx, controller_id, reason);
                }
            }
        }
        Ok(stream_id)
    }

    fn readable(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page
            .with::<TransformStreamObject, _>(this, |t| t.readable)
    }

    fn writable(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        cx.page
            .with::<TransformStreamObject, _>(this, |t| t.writable)
    }
}

/// Makes the writable side of a transform run the transformer.
fn writable_hooks(cx: &mut Cx<'_>, writable_id: ObjectId, controller_id: ObjectId) {
    let _ = writable(cx, writable_id, |w| {
        w.algorithms = None;
        w.transform = Some(controller_id);
        w.started = true;
    });
    let _ = cx
        .page
        .with::<TransformControllerObject, _>(controller_id, |c| c.writable = writable_id);
}

/// A chunk written to the transform: the transformer's `transform`, or
/// the chunk passed through as it is.
fn transform_write(cx: &mut Cx<'_>, controller_id: ObjectId, chunk: Value) -> Fallible<Value> {
    let algorithms = transform_controller(cx, controller_id, |c| c.algorithms.clone())?;
    match algorithms.as_ref().filter(|a| a.main.is_some()) {
        Some(algorithms) => {
            algorithms.call(cx, &algorithms.main, &[chunk, Value::Object(controller_id)])
        }
        None => {
            <Web as web::TransformStreamDefaultControllerImpl>::enqueue(cx, controller_id, chunk)?;
            Ok(Value::Undefined)
        }
    }
}

/// The writable side is closing: the transformer's `flush`.
fn transform_flush(cx: &mut Cx<'_>, controller_id: ObjectId) -> Fallible<Value> {
    let algorithms = transform_controller(cx, controller_id, |c| c.algorithms.clone())?;
    match algorithms {
        Some(algorithms) => algorithms.call(cx, &algorithms.close, &[Value::Object(controller_id)]),
        None => Ok(Value::Undefined),
    }
}

/// The writable side has closed: so does the readable side.
fn transform_done(cx: &mut Cx<'_>, controller_id: ObjectId) {
    if let Ok(readable_controller) =
        transform_controller(cx, controller_id, |c| c.readable_controller)
    {
        close_readable(cx, readable_controller);
    }
}

fn transform_error(cx: &mut Cx<'_>, controller_id: ObjectId, reason: Value) {
    let Ok((readable_controller, writable_id)) =
        transform_controller(cx, controller_id, |c| (c.readable_controller, c.writable))
    else {
        return;
    };
    error_readable(cx, readable_controller, reason.clone());
    error_writable(cx, writable_id, reason);
}

impl web::TransformStreamDefaultControllerImpl for Web {
    fn desired_size(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<f64>> {
        let readable_controller = transform_controller(cx, this, |c| c.readable_controller)?;
        Ok(desired_size(cx, readable_controller))
    }

    fn enqueue(cx: &mut Cx<'_>, this: ObjectId, chunk: Value) -> Fallible<()> {
        let readable_controller = transform_controller(cx, this, |c| c.readable_controller)?;
        if !can_close_or_enqueue(cx, readable_controller) {
            return Err(Exception::type_error(
                "The readable side is closed or errored",
            ));
        }
        if let Err(e) = enqueue_readable(cx, readable_controller, chunk) {
            let reason = cx.script.exception_value(&e);
            transform_error(cx, this, reason);
            return Err(e);
        }
        Ok(())
    }

    fn error(cx: &mut Cx<'_>, this: ObjectId, reason: Value) -> Fallible<()> {
        transform_error(cx, this, reason);
        Ok(())
    }

    fn terminate(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let (readable_controller, writable_id) =
            transform_controller(cx, this, |c| (c.readable_controller, c.writable))?;
        close_readable(cx, readable_controller);
        let reason = type_error_value(cx, "The transform stream was terminated");
        error_writable(cx, writable_id, reason);
        Ok(())
    }
}
