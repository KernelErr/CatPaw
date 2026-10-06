//! `fetch()`, `XMLHttpRequest` and friends against an in-memory network
//! that records what the page asked for.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{ConsoleLevel, PageConfig, PageState, scripting};
use url::Url;

const PAGE: &str = "https://app.test/dir/page.html";

#[derive(Clone)]
struct Route {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

fn route(status: u16, headers: &[(&'static str, &'static str)], body: &'static str) -> Route {
    Route {
        status,
        headers: headers.to_vec(),
        body,
    }
}

/// What the page sent, as the network host received it.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    credentials: bool,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Routes are keyed by `"METHOD url"`. A URL without a route is a network
/// failure; the body `"<hang>"` never completes.
#[derive(Default)]
struct TestNet {
    routes: HashMap<String, Route>,
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
    seen: RefCell<Vec<Seen>>,
}

impl TestNet {
    fn respond(&self, request: &NetRequest) -> Option<NetResult> {
        self.seen.borrow_mut().push(Seen {
            method: request.method.clone(),
            headers: request.headers.clone(),
            body: request
                .body
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            credentials: request.credentials,
        });
        let key = format!("{} {}", request.method, request.url);
        let Some(route) = self.routes.get(&key) else {
            return Some(Err("connection refused".to_string()));
        };
        if route.body == "<hang>" {
            return None;
        }
        Some(Ok(NetResponse {
            url: request.url.clone(),
            status: route.status,
            status_text: if route.status == 200 { "OK" } else { "" }.to_string(),
            headers: route
                .headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: route.body.as_bytes().to_vec(),
            redirected: false,
        }))
    }
}

impl NetHost for TestNet {
    fn fetch_blocking(&self, request: NetRequest) -> NetResult {
        self.respond(&request)
            .unwrap_or_else(|| Err("timed out".to_string()))
    }

    fn start(&self, request: NetRequest) -> u64 {
        let token = self.next_token.get() + 1;
        self.next_token.set(token);
        if let Some(result) = self.respond(&request) {
            self.completed.borrow_mut().push((token, result));
        }
        token
    }

    fn poll(&self, wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        let done = std::mem::take(&mut *self.completed.borrow_mut());
        if done.is_empty()
            && let Some(wait) = wait
        {
            std::thread::sleep(wait);
        }
        done
    }

    fn abort(&self, token: u64) {
        self.completed.borrow_mut().retain(|(t, _)| *t != token);
    }

    fn inflight(&self) -> usize {
        self.completed.borrow().len()
    }

    fn cookies_for(&self, _url: &Url) -> String {
        String::new()
    }

    fn set_cookie(&self, _url: &Url, _cookie: &str) {}
}

struct Fixture {
    page: BoaPage,
    net: Rc<TestNet>,
}

impl Fixture {
    fn new(routes: &[(&str, Route)]) -> Self {
        let net = Rc::new(TestNet {
            routes: routes
                .iter()
                .map(|(key, route)| (key.to_string(), route.clone()))
                .collect(),
            ..TestNet::default()
        });
        let state = Rc::new(PageState::new(
            Url::parse(PAGE).unwrap(),
            PageConfig::default(),
        ));
        state.set_net(net.clone());
        let mut page = BoaPage::new(state).expect("page setup");
        page.with_cx(|cx| scripting::load_document(cx, "<body></body>"));
        Self { page, net }
    }

    /// Runs `source` (an async function body), waits for the page to settle
    /// and returns what the body returned, rendered as by a console.
    fn run(&mut self, source: &str) -> String {
        let script = format!(
            "var __out = 'pending'; (async function () {{ {source} }})().then(function (v) {{ __out = v; }}, function (e) {{ __out = 'REJECTED ' + (e && e.name) + ': ' + (e && e.message); }}); 0"
        );
        self.page.eval(&script).expect("script");
        let limits = LoopLimits {
            wall: Duration::from_secs(5),
            ..LoopLimits::default()
        };
        self.page.with_cx(|cx| event_loop::run(cx, &limits));
        self.page.eval_to_string("__out").expect("result")
    }

    fn seen(&self) -> Vec<Seen> {
        self.net.seen.borrow().clone()
    }

    fn errors(&self) -> Vec<String> {
        self.page
            .page()
            .console_messages()
            .into_iter()
            .filter(|m| m.level == ConsoleLevel::Error)
            .map(|m| m.text)
            .collect()
    }
}

const JSON: (&str, &str) = ("content-type", "application/json");

#[test]
fn fetch_reads_same_origin_responses() {
    let mut f = Fixture::new(&[
        (
            "GET https://app.test/api/data.json",
            route(
                200,
                &[JSON, ("set-cookie", "s=1"), ("x-extra", "yes")],
                r#"{"n": 42, "list": [1, 2]}"#,
            ),
        ),
        ("GET https://app.test/dir/text", route(200, &[], "héllo")),
        ("GET https://app.test/missing", route(404, &[], "nope")),
        (
            "POST https://app.test/api/echo",
            route(201, &[JSON], r#"{"ok":true}"#),
        ),
    ]);
    assert_eq!(
        f.run("var r = await fetch('/api/data.json'); var data = await r.json(); return [r.ok, r.status, r.statusText, r.type, r.url, r.redirected, r.headers.get('Content-Type'), r.headers.get('x-extra'), r.headers.get('set-cookie'), data.n + data.list.length, r instanceof Response, r.bodyUsed].join('|')"),
        "true|200|OK|basic|https://app.test/api/data.json|false|application/json|yes||44|true|true"
    );
    let seen = f.seen();
    assert_eq!(seen[0].method, "GET");
    assert!(seen[0].credentials, "same-origin requests carry cookies");
    assert_eq!(seen[0].header("origin"), None);

    assert_eq!(
        f.run("var r = await fetch('text'); var t = await r.clone().text(); var b = await r.arrayBuffer(); var again = await r.text().catch(function (e) { return e.name; }); return t + ' ' + b.byteLength + ' ' + (b instanceof ArrayBuffer) + ' ' + again"),
        "héllo 6 true TypeError"
    );
    assert_eq!(
        f.run(
            "var r = await fetch('/missing'); return r.ok + ' ' + r.status + ' ' + await r.text()"
        ),
        "false 404 nope"
    );
    assert_eq!(
        f.run("await fetch('/unreachable'); return 'no'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert!(
        f.errors()
            .iter()
            .any(|e| e.contains("/unreachable: connection refused"))
    );
    assert_eq!(
        f.run("await fetch('ftp://app.test/x'); return 'no'"),
        "REJECTED TypeError: Failed to fetch"
    );

    assert_eq!(
        f.run("var r = await fetch('/api/echo', { method: 'post', headers: { 'X-Token': 'abc', 'Cookie': 'forged=1' }, body: JSON.stringify({ a: 1 }) }); return r.status + ' ' + (await r.json()).ok"),
        "201 true"
    );
    let post = f.seen().into_iter().rfind(|s| s.method == "POST").unwrap();
    assert_eq!(post.body.as_deref(), Some(r#"{"a":1}"#));
    assert_eq!(post.header("x-token"), Some("abc"));
    assert_eq!(
        post.header("content-type"),
        Some("text/plain;charset=UTF-8")
    );
    assert_eq!(post.header("cookie"), None, "forbidden headers are dropped");
    assert_eq!(post.header("origin"), Some("https://app.test"));
}

#[test]
fn cross_origin_responses_need_permission() {
    let open = ("access-control-allow-origin", "*");
    let mine = ("access-control-allow-origin", "https://app.test");
    let mut f = Fixture::new(&[
        (
            "GET https://api.test/open",
            route(
                200,
                &[
                    open,
                    ("x-secret", "s"),
                    ("content-type", "text/plain"),
                    ("access-control-expose-headers", "X-Shown"),
                    ("x-shown", "v"),
                ],
                "open",
            ),
        ),
        ("GET https://api.test/closed", route(200, &[], "closed")),
        (
            "GET https://api.test/other",
            route(
                200,
                &[("access-control-allow-origin", "https://other.test")],
                "other",
            ),
        ),
        (
            "GET https://api.test/creds",
            route(
                200,
                &[mine, ("access-control-allow-credentials", "true")],
                "creds",
            ),
        ),
        ("GET https://api.test/half", route(200, &[mine], "half")),
        // Preflighted.
        (
            "OPTIONS https://api.test/put",
            route(
                204,
                &[
                    mine,
                    ("access-control-allow-methods", "PUT"),
                    ("access-control-allow-headers", "x-custom, content-type"),
                ],
                "",
            ),
        ),
        ("PUT https://api.test/put", route(200, &[mine], "put ok")),
        ("OPTIONS https://api.test/denied", route(204, &[mine], "")),
        (
            "DELETE https://api.test/denied",
            route(200, &[mine], "should not be reached"),
        ),
    ]);
    assert_eq!(
        f.run("var r = await fetch('https://api.test/open'); return [r.type, r.status, await r.text(), r.headers.get('x-secret'), r.headers.get('x-shown'), r.headers.get('content-type')].join('|')"),
        "cors|200|open||v|text/plain"
    );
    let first = &f.seen()[0];
    assert!(
        !first.credentials,
        "cross-origin requests omit cookies by default"
    );
    assert_eq!(first.header("origin"), Some("https://app.test"));

    assert_eq!(
        f.run("await fetch('https://api.test/closed'); return 'read'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert_eq!(
        f.run("await fetch('https://api.test/other'); return 'read'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert_eq!(
        f.run("await fetch('https://api.test/open', { credentials: 'include' }); return 'read'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert_eq!(
        f.run("await fetch('https://api.test/half', { credentials: 'include' }); return 'read'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert_eq!(f.run("var r = await fetch('https://api.test/creds', { credentials: 'include' }); return await r.text()"), "creds");
    assert!(f.seen().last().unwrap().credentials);
    assert_eq!(
        f.run("await fetch('https://api.test/open', { mode: 'same-origin' }); return 'read'"),
        "REJECTED TypeError: Failed to fetch"
    );

    // no-cors: the request goes out, but nothing about the response shows.
    assert_eq!(
        f.run("var r = await fetch('https://api.test/closed', { mode: 'no-cors' }); return [r.type, r.status, r.ok, r.url, await r.text()].join('|')"),
        "opaque|0|false||"
    );

    // A request that is not "simple" asks first.
    let before = f.seen().len();
    assert_eq!(
        f.run("var r = await fetch('https://api.test/put', { method: 'PUT', headers: { 'X-Custom': '1', 'Content-Type': 'application/json' }, body: '{}' }); return await r.text()"),
        "put ok"
    );
    let seen = f.seen();
    let (preflight, actual) = (&seen[before], &seen[before + 1]);
    assert_eq!(preflight.method, "OPTIONS");
    assert_eq!(
        preflight.header("access-control-request-method"),
        Some("PUT")
    );
    assert_eq!(
        preflight.header("access-control-request-headers"),
        Some("content-type,x-custom")
    );
    assert!(!preflight.credentials);
    assert_eq!(actual.method, "PUT");
    assert_eq!(actual.body.as_deref(), Some("{}"));

    let before = f.seen().len();
    assert_eq!(
        f.run("await fetch('https://api.test/denied', { method: 'DELETE' }); return 'sent'"),
        "REJECTED TypeError: Failed to fetch"
    );
    assert_eq!(
        f.seen().len(),
        before + 1,
        "the actual request must not be sent"
    );
    assert!(
        f.errors()
            .iter()
            .any(|e| e.contains("CORS preflight failed"))
    );
}

#[test]
fn requests_can_be_aborted() {
    let mut f = Fixture::new(&[
        ("GET https://app.test/slow", route(200, &[], "<hang>")),
        ("GET https://app.test/fast", route(200, &[], "fast")),
    ]);
    assert_eq!(
        f.run("var c = new AbortController(); var before = c.signal.aborted; c.abort(); try { await fetch('/fast', { signal: c.signal }); return 'fetched'; } catch (e) { return [before, c.signal.aborted, e.name, e === c.signal.reason, e instanceof DOMException].join('|'); }"),
        "false|true|AbortError|true|true"
    );
    assert_eq!(f.seen().len(), 0, "an aborted request is never sent");

    assert_eq!(
        f.run("var c = new AbortController(); var events = []; c.signal.onabort = function (e) { events.push(e.type + (e.target === c.signal)); }; setTimeout(function () { c.abort(new Error('changed my mind')); }, 20); try { await fetch('/slow', { signal: c.signal }); return 'fetched'; } catch (e) { return e.message + '|' + events.join(); }"),
        "changed my mind|aborttrue"
    );
    assert_eq!(
        f.run("try { await fetch('/slow', { signal: AbortSignal.timeout(30) }); return 'fetched'; } catch (e) { return e.name; }"),
        "TimeoutError"
    );
    assert_eq!(
        f.run("var a = new AbortController(), b = new AbortController(); var any = AbortSignal.any([a.signal, b.signal]); b.abort('because'); var already = AbortSignal.any([any]); try { any.throwIfAborted(); } catch (e) { return [e, any.reason, already.aborted, a.signal.aborted, AbortSignal.abort().reason.name].join('|'); }"),
        "because|because|true|false|AbortError"
    );
    // A listener registered with a signal goes away when it aborts.
    assert_eq!(
        f.run("var c = new AbortController(), hits = 0; var t = new EventTarget(); t.addEventListener('x', function () { hits++; }, { signal: c.signal }); t.dispatchEvent(new Event('x')); c.abort(); t.dispatchEvent(new Event('x')); t.addEventListener('x', function () { hits += 10; }, { signal: c.signal }); t.dispatchEvent(new Event('x')); return hits;"),
        "1"
    );
}

#[test]
fn headers_request_and_response_objects() {
    let mut f = Fixture::new(&[]);
    assert_eq!(
        f.run("var h = new Headers([['B', '1'], ['a', '2']]); h.append('b', '3'); h.set('C', 'x'); h.delete('nope'); var out = []; for (var [k, v] of h) out.push(k + '=' + v); h.forEach(function (v, k) { out.push(k); }); return out.join(';') + '|' + h.get('B') + '|' + h.has('c') + '|' + h.get('zz') + '|' + [...new Headers({ 'X-One': '1' }).keys()]"),
        "a=2;b=1, 3;c=x;a;b;c|1, 3|true|null|x-one"
    );
    assert_eq!(
        f.run("try { new Headers({ 'bad name': '1' }); } catch (e) { return e.name; }"),
        "TypeError"
    );

    assert_eq!(
        f.run("var r = new Request('../x?y#z', { method: 'post', headers: { 'X-A': '1' }, body: 'hi' }); var c = r.clone(); return [r.method, r.url, r.headers.get('x-a'), r.headers.get('content-type'), r.mode, r.credentials, r.redirect, await r.text(), r.bodyUsed, await c.text(), r.headers === r.headers, r.signal.aborted].join('|')"),
        "POST|https://app.test/x?y#z|1|text/plain;charset=UTF-8|cors|same-origin|follow|hi|true|hi|true|false"
    );
    assert_eq!(
        f.run("try { new Request('/x', { body: 'b' }); } catch (e) { return e.name; }"),
        "TypeError"
    );
    assert_eq!(
        f.run("try { new Request('/x', { method: 'TRACE' }); } catch (e) { return e.name; }"),
        "TypeError"
    );
    assert_eq!(f.run("var a = new Request('/a', { method: 'PUT', body: 'payload' }); var b = new Request(a, { headers: { k: 'v' } }); return [b.method, b.url, await b.text(), a.bodyUsed, b.headers.get('k')].join('|')"), "PUT|https://app.test/a|payload|true|v");

    assert_eq!(
        f.run("var r = new Response('body', { status: 201, statusText: 'Made', headers: { 'X-H': 'y' } }); r.headers.set('z', '1'); return [r.status, r.statusText, r.ok, r.type, r.url, r.headers.get('x-h'), r.headers.get('content-type'), r.headers.get('z'), await r.text()].join('|')"),
        "201|Made|true|default||y|text/plain;charset=UTF-8|1|body"
    );
    assert_eq!(
        f.run("var j = Response.json({ a: [1] }, { status: 202 }); var e = Response.error(); var red = Response.redirect('/to', 302); return [j.status, j.headers.get('content-type'), JSON.stringify(await j.json()), e.type, e.status, red.status, red.headers.get('location'), new Response(null, { status: 204 }).status, await new Response(new Uint8Array([104, 105])).text(), await new Response(new URLSearchParams({ q: 'a b' })).text()].join('|')"),
        r#"202|application/json|{"a":[1]}|error|0|302|https://app.test/to|204|hi|q=a+b"#
    );
    assert_eq!(
        f.run("try { new Response('x', { status: 204 }); } catch (e) { return e.name; }"),
        "TypeError"
    );
    assert_eq!(
        f.run("try { new Response('x', { status: 99 }); } catch (e) { return e.name; }"),
        "RangeError"
    );
}

#[test]
fn xml_http_request_goes_through_its_states() {
    let mut f = Fixture::new(&[
        (
            "GET https://app.test/api/data.json",
            route(200, &[JSON, ("x-b", "2"), ("X-A", "1")], r#"{"n": 7}"#),
        ),
        (
            "POST https://app.test/api/form",
            route(
                200,
                &[("content-type", "text/plain; charset=iso-8859-1")],
                "caf\u{e9}",
            ),
        ),
        ("GET https://app.test/slow", route(200, &[], "<hang>")),
        ("GET https://api.test/closed", route(200, &[], "secret")),
    ]);
    let events = "function watch(x, log) { ['readystatechange', 'loadstart', 'progress', 'load', 'error', 'abort', 'timeout', 'loadend'].forEach(function (t) { x.addEventListener(t, function (e) { log.push(t === 'readystatechange' ? 'rs' + x.readyState : t + (e instanceof ProgressEvent ? ':' + e.loaded : '')); }); }); } function done(x) { return new Promise(function (resolve) { x.addEventListener('loadend', resolve); }); }";
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(), log = []; watch(x, log); x.open('GET', '/api/data.json'); x.responseType = 'json'; x.send(); log.push('sent'); await done(x); return [log.join(','), x.status, x.statusText, x.response.n, x.getResponseHeader('X-B'), JSON.stringify(x.getAllResponseHeaders()), x.responseURL, x instanceof XMLHttpRequestEventTarget, x.upload === x.upload, XMLHttpRequest.DONE].join('|')")),
        r#"rs1,loadstart:0,sent,rs2,rs3,progress:8,rs4,load:8,loadend:8|200|OK|7|2|"content-type: application/json\r\nx-a: 1\r\nx-b: 2\r\n"|https://app.test/api/data.json|true|true|4"#
    );
    // Request construction, and a response decoded with its declared charset:
    // the fixture sends UTF-8 bytes labelled as Latin-1, so "é" must come out
    // as the two characters those bytes mean in that encoding.
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(); x.open('post', '/api/form'); x.setRequestHeader('X-One', 'a'); x.setRequestHeader('x-one', 'b'); x.setRequestHeader('Cookie', 'no'); x.send(new URLSearchParams({{ k: 'v w' }})); await done(x); return x.responseText + '|' + x.response + '|' + x.readyState")),
        "caf\u{c3}\u{a9}|caf\u{c3}\u{a9}|4"
    );
    let post = f.seen().into_iter().rfind(|s| s.method == "POST").unwrap();
    assert_eq!(post.body.as_deref(), Some("k=v+w"));
    assert_eq!(post.header("x-one"), Some("a, b"));
    assert_eq!(
        post.header("content-type"),
        Some("application/x-www-form-urlencoded;charset=UTF-8")
    );
    assert_eq!(post.header("cookie"), None);

    // Failure, cross-origin denial, timeout and abort.
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(), log = []; watch(x, log); x.open('GET', '/unreachable'); x.send(); await done(x); return log.join(',') + '|' + x.status + '|' + x.responseText")),
        "rs1,loadstart:0,rs4,error:0,loadend:0|0|"
    );
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(), log = []; watch(x, log); x.open('GET', 'https://api.test/closed'); x.send(); await done(x); return log.slice(-2).join(',') + '|' + x.status + '|' + x.responseText")),
        "error:0,loadend:0|0|"
    );
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(), log = []; watch(x, log); x.open('GET', '/slow'); x.timeout = 30; x.send(); await done(x); return log.join(',')")),
        "rs1,loadstart:0,rs4,timeout:0,loadend:0"
    );
    assert_eq!(
        f.run(&format!("{events} var x = new XMLHttpRequest(), log = []; watch(x, log); x.open('GET', '/slow'); x.send(); x.abort(); return log.join(',') + '|' + x.readyState")),
        "rs1,loadstart:0,rs4,abort:0,loadend:0|0"
    );
    // Synchronous requests complete inside send().
    assert_eq!(
        f.run("var x = new XMLHttpRequest(); x.open('GET', '/api/data.json', false); x.send(); var ok = x.status + ' ' + JSON.parse(x.responseText).n; var y = new XMLHttpRequest(); y.open('GET', '/unreachable', false); try { y.send(); } catch (e) { return ok + ' ' + e.name; }"),
        "200 7 NetworkError"
    );
    assert_eq!(f.run("var x = new XMLHttpRequest(); try { x.send(); } catch (e) { var a = e.name; } try { x.open('TRACE', '/'); } catch (e) { return a + ' ' + e.name; }"), "InvalidStateError SecurityError");
}
