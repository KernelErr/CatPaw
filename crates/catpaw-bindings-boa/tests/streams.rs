//! Streams: readable, writable and transform, and `Response.body`.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const FIXTURE: &str = r#"<!doctype html><body>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  // Collects everything a stream yields.
  async function drain(stream) {
    var reader = stream.getReader(), out = [];
    for (;;) { var r = await reader.read(); if (r.done) break; out.push(r.value); }
    reader.releaseLock();
    return out;
  }
</script>"#;

fn settle(page: &mut BoaPage) {
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
}

fn load(html: &str) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    settle(&mut page);
    page
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

/// Runs `source` (which may be asynchronous), lets the page settle, and
/// returns what was logged.
fn step(page: &mut BoaPage, source: &str) -> String {
    let result = eval(
        page,
        &format!(
            "log = []; (async function () {{ {source} }})().catch(function (e) {{ log.push('rejected ' + (e && e.name ? e.name : e)); }}); 0"
        ),
    );
    assert_eq!(result, "0", "{source}");
    settle(page);
    eval(page, "log.join(' / ')")
}

#[test]
fn readable_streams_deliver_what_their_source_enqueues() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var pulls = 0;
             var s = new ReadableStream({
               start(c) { log.push('start ' + (c instanceof ReadableStreamDefaultController) + ' ' + c.desiredSize); c.enqueue('a'); c.enqueue('b'); },
               pull(c) { pulls++; if (pulls <= 2) c.enqueue('p' + pulls); else c.close(); },
               cancel(reason) { log.push('cancel ' + reason); }
             }, { highWaterMark: 2 });
             log.push([s instanceof ReadableStream, s.locked, typeof s.getReader].join());
             var chunks = await drain(s);
             log.push(chunks.join() + ' ' + s.locked + ' pulls ' + pulls)"
        ),
        "start true 2 / true,false,function / a,b,p1,p2 false pulls 3"
    );
    // Backpressure: pulls happen only as far as the high water mark.
    assert_eq!(
        step(
            &mut page,
            "var n = 0, lazy = new ReadableStream({ pull(c) { n++; c.enqueue(n); } }, new CountQueuingStrategy({ highWaterMark: 3 }));
             var turn = function () { return new Promise(function (r) { setTimeout(r, 0); }); };
             await turn(); log.push('queued ' + n);
             var reader = lazy.getReader();
             var first = await reader.read(); log.push(first.value + ' ' + first.done + ' ' + (await reader.read()).value);
             await turn(); log.push('after reads ' + n);
             reader.releaseLock(); log.push(lazy.locked)"
        ),
        "queued 3 / 1 false 2 / after reads 5 / false"
    );
    assert_eq!(
        step(
            &mut page,
            "var bytes = new ReadableStream({ start(c) { c.enqueue(new Uint8Array([1, 2])); c.enqueue(new Uint8Array([3])); c.close(); } }, new ByteLengthQueuingStrategy({ highWaterMark: 16 }));
             var parts = []; for await (const part of bytes) parts.push(part.length);
             log.push(parts.join() + ' ' + bytes.locked)"
        ),
        "2,1 false"
    );
}

#[test]
fn readers_lock_close_and_cancel() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var s = new ReadableStream({ start(c) { c.enqueue(1); }, cancel(r) { log.push('cancelled with ' + r); } });
             var r = s.getReader();
             log.push(attempt(function () { return s.getReader(); }) + ' ' + attempt(function () { return new ReadableStreamDefaultReader(s); }));
             r.closed.then(function () { log.push('closed resolved'); });
             log.push((await r.read()).value);
             var pending = r.read().then(function (x) { log.push('pending read ' + x.done); });
             await r.cancel('why');
             await pending;
             log.push(s.locked);
             r.releaseLock();
             log.push(attempt(function () { return s.getReader() !== null; }))"
        ),
        "TypeError TypeError / 1 / cancelled with why / closed resolved / pending read true / true / true"
    );
    assert_eq!(
        step(
            &mut page,
            "var failing = new ReadableStream({ start(c) { c.enqueue('x'); c.error(new RangeError('bad')); } });
             var reader = failing.getReader();
             reader.closed.catch(function (e) { log.push('closed ' + e.name); });
             // Erroring empties the queue: nothing enqueued before is read.
             try { await reader.read(); } catch (e) { log.push('read ' + e.name); }
             log.push(attempt(function () { new ReadableStream({ start(c) { c.close(); c.enqueue(1); } }); return 'enqueue after close ignored'; }))"
        ),
        "closed RangeError / read RangeError / enqueue after close ignored"
    );
    assert_eq!(
        step(
            &mut page,
            "var released = new ReadableStream({ start(c) { c.enqueue(1); } });
             var rr = released.getReader(); var waiting = rr.read(); await waiting;
             var late = rr.read(); rr.releaseLock();
             try { await late; } catch (e) { log.push('released read ' + e.name); }
             try { await rr.read(); } catch (e) { log.push('read without stream ' + e.name); }
             log.push(attempt(function () { return new ReadableStream({ start() { throw new Error('no'); } }) instanceof ReadableStream; }))"
        ),
        "released read TypeError / read without stream TypeError / true"
    );
}

#[test]
fn tee_pipe_and_transform() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var source = new ReadableStream({ start(c) { c.enqueue('a'); c.enqueue('b'); c.close(); } });
             var [one, two] = source.tee();
             log.push(source.locked + ' ' + (one instanceof ReadableStream));
             log.push((await drain(one)).join() + ' ' + (await drain(two)).join())"
        ),
        "true true / a,b a,b"
    );
    assert_eq!(
        step(
            &mut page,
            "var written = [];
             var sink = new WritableStream({
               start(c) { log.push('sink start ' + (c instanceof WritableStreamDefaultController)); },
               write(chunk) { written.push(chunk); return new Promise(function (r) { setTimeout(r, 1); }); },
               close() { log.push('sink closed'); }
             });
             var w = sink.getWriter();
             log.push(sink.locked + ' ' + w.desiredSize);
             await w.write(1); w.write(2); await w.ready; await w.close();
             await w.closed; log.push(written.join() + ' ' + w.desiredSize)"
        ),
        "sink start true / true 1 / sink closed / 1,2 0"
    );
    assert_eq!(
        step(
            &mut page,
            "var upper = new TransformStream({ transform(chunk, c) { c.enqueue(chunk.toUpperCase()); }, flush(c) { c.enqueue('!'); } });
             var out = new ReadableStream({ start(c) { c.enqueue('x'); c.enqueue('y'); c.close(); } }).pipeThrough(upper);
             log.push((await drain(out)).join(''));
             var identity = new TransformStream();
             var collected = [];
             await new ReadableStream({ start(c) { c.enqueue(1); c.enqueue(2); c.close(); } })
               .pipeTo(new WritableStream({ write(c) { collected.push(c); }, close() { collected.push('closed'); } }));
             log.push(collected.join());
             var pass = new ReadableStream({ start(c) { c.enqueue('p'); c.close(); } }).pipeThrough(identity);
             log.push((await drain(pass)).join())"
        ),
        "XY! / 1,2,closed / p"
    );
    assert_eq!(
        step(
            &mut page,
            "var errors = [];
             var failing = new WritableStream({ write() { throw new Error('refused'); } });
             try { await new ReadableStream({ start(c) { c.enqueue(1); c.close(); } }).pipeTo(failing); } catch (e) { log.push('pipe ' + e.message); }
             var cancelled = new ReadableStream({ start(c) { c.enqueue(1); }, cancel(r) { log.push('source cancel'); } });
             try { await cancelled.pipeTo(new WritableStream({ write() { return Promise.reject(new Error('later')); } })); } catch (e) { log.push('pipe ' + e.message); }
             var aborted = new WritableStream({ abort(r) { log.push('sink abort ' + r.message); } });
             try { await new ReadableStream({ start(c) { c.error(new Error('upstream')); } }).pipeTo(aborted); } catch (e) { log.push('pipe ' + e.message); }"
        ),
        "pipe refused / source cancel / pipe later / sink abort upstream / pipe upstream"
    );
}

#[test]
fn response_bodies_are_streams() {
    let mut page = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var r = new Response('hello');
             var body = r.body;
             log.push([body instanceof ReadableStream, r.body === body, r.bodyUsed].join());
             var reader = body.getReader(); log.push(r.bodyUsed);
             var chunk = await reader.read(); log.push(new TextDecoder().decode(chunk.value) + ' ' + (await reader.read()).done);
             try { await r.text(); } catch (e) { log.push('text ' + e.name); }
             var t = new Response('twice'); await t.text(); log.push(t.body + ' ' + t.bodyUsed);
             log.push(new Response(null).body + ' ' + new Response(null, { status: 204 }).body);
             var parts = []; for await (const part of new Response('ab').body) parts.push(part.length); log.push(parts.join());
             var fresh = new Response('again'); fresh.body; log.push(await fresh.text())"
        ),
        "true,true,false / true / hello true / text TypeError / null true / null null / 2 / again"
    );
}

#[test]
fn strategies_and_iteration() {
    let mut page = load(FIXTURE);
    for (source, expected) in [
        (
            "new CountQueuingStrategy({ highWaterMark: 4 }).highWaterMark + ' ' + new CountQueuingStrategy({ highWaterMark: 4 }).size({})",
            "4 1",
        ),
        (
            "var b = new ByteLengthQueuingStrategy({ highWaterMark: 1 }); b.size(new Uint8Array(7)) + ' ' + (b.size === new ByteLengthQueuingStrategy({ highWaterMark: 2 }).size)",
            "7 true",
        ),
        (
            "attempt(function () { return new CountQueuingStrategy(); })",
            "TypeError",
        ),
        (
            "attempt(function () { return new CountQueuingStrategy({}); })",
            "TypeError",
        ),
        (
            "typeof ReadableStream.prototype[Symbol.asyncIterator] + ' ' + typeof ReadableStream.prototype.values",
            "function function",
        ),
        (
            "attempt(function () { return new ReadableStream({}, { highWaterMark: -1 }); })",
            "RangeError",
        ),
        (
            "attempt(function () { return new ReadableStream({ pull: 5 }); })",
            "TypeError",
        ),
        (
            "attempt(function () { return new ReadableStream().getReader({ mode: 'byob' }); })",
            "TypeError",
        ),
        (
            "String(new ReadableStream()) + ' ' + String(new WritableStream()) + ' ' + String(new TransformStream())",
            "[object ReadableStream] [object WritableStream] [object TransformStream]",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
}

#[test]
fn strategy_objects_convert_to_dictionaries() {
    let mut page = load(FIXTURE);
    assert_eq!(
        eval(
            &mut page,
            "var seen; new ReadableStream({ start(c) { seen = c.desiredSize; } }, new CountQueuingStrategy({ highWaterMark: 3 }));
             var plain; new ReadableStream({ start(c) { plain = c.desiredSize; } }, { highWaterMark: 3 });
             seen + ' ' + plain"
        ),
        "3 3"
    );
}
