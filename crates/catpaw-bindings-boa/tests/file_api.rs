//! Blob, File, FileReader, object URLs and FormData, and the bodies made
//! of them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

/// Echoes every request's body and content type back as JSON.
#[derive(Default)]
struct EchoNet {
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
    seen: RefCell<Vec<(String, Vec<u8>)>>,
}

impl EchoNet {
    fn respond(&self, request: &NetRequest) -> NetResult {
        let content_type = request
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let body = request.body.clone().unwrap_or_default();
        self.seen
            .borrow_mut()
            .push((content_type.clone(), body.clone()));
        if request.url.path() == "/form" {
            return Ok(NetResponse {
                url: request.url.clone(),
                status: 200,
                status_text: "OK".to_string(),
                headers: vec![("content-type".to_string(), content_type)],
                body,
                redirected: false,
            });
        }
        Ok(NetResponse {
            url: request.url.clone(),
            status: 200,
            status_text: "OK".to_string(),
            headers: vec![("content-type".to_string(), "image/png".to_string())],
            body: b"\x89PNG".to_vec(),
            redirected: false,
        })
    }
}

impl NetHost for EchoNet {
    fn fetch_blocking(&self, request: NetRequest) -> NetResult {
        self.respond(&request)
    }

    fn start(&self, request: NetRequest) -> u64 {
        let token = self.next_token.get() + 1;
        self.next_token.set(token);
        let result = self.respond(&request);
        self.completed.borrow_mut().push((token, result));
        token
    }

    fn poll(&self, _wait: Option<Duration>) -> Vec<(u64, NetResult)> {
        std::mem::take(&mut *self.completed.borrow_mut())
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

const FIXTURE: &str = r#"<!doctype html><body>
<form id="f"><input name="a" value="1"><input name="b" type="checkbox" checked><input name="c" type="checkbox">
<input name="d" type="radio" value="r1"><input name="d" type="radio" value="r2" checked><input name="e" disabled value="x">
<input type="submit" name="s" value="go"><textarea name="t">line1
line2</textarea><select name="sel"><option>first</option><option value="v2" selected>second</option></select>
<select name="multi" multiple><option value="m1" selected>1</option><option value="m2">2</option></select>
<fieldset disabled><input name="in-disabled-fieldset" value="y"></fieldset><input name="_charset_" type="hidden"></form>
<script>
  var log = [];
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
  function hex(b) { return Array.from(new Uint8Array(b)).map(function (x) { return x.toString(16).padStart(2, '0'); }).join(''); }
</script></body></html>"#;

fn settle(page: &mut BoaPage) {
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
}

fn load(html: &str) -> (BoaPage, Rc<EchoNet>) {
    let net = Rc::new(EchoNet::default());
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    state.set_net(net.clone());
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    settle(&mut page);
    (page, net)
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
fn blobs_and_files() {
    let (mut page, _) = load(FIXTURE);
    for (source, expected) in [
        (
            "var b = new Blob(['ab', new Uint8Array([99]), new Blob(['d'])], { type: 'Text/Plain' }); [String(b), b.size, b.type, b instanceof Blob, b instanceof File].join(' ')",
            "[object Blob] 4 text/plain true false",
        ),
        (
            "var s = b.slice(1, -1, 'x/y'); var e = b.slice(-1); var n = new Blob([], { type: 'bad\\u00e9' }); [s.size, s.type, e.size, b.slice(10).size, b.slice(2, 1).size, n.type === '', new Blob().size].join(' ')",
            "2 x/y 1 0 0 true 0",
        ),
        (
            "var f = new File(['hello'], 'h.txt', { type: 'text/plain', lastModified: 5 }); [String(f), f.name, f.size, f.type, f.lastModified, f instanceof Blob, new File([], 'x').lastModified > 1e12].join(' ')",
            "[object File] h.txt 5 text/plain 5 true true",
        ),
        (
            "var w = new Blob(['a\\r\\nb\\nc'], { endings: 'native' }).size + ' ' + new Blob(['a\\r\\nb\\nc']).size; w",
            if cfg!(windows) { "7 6" } else { "5 6" },
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
    assert_eq!(
        step(
            &mut page,
            "log.push(await b.text(), hex(await b.arrayBuffer()), (await b.bytes()).length, new TextDecoder().decode((await b.stream().getReader().read()).value));"
        ),
        "abcd / 61626364 / 4 / abcd"
    );
}

#[test]
fn file_reader_reads_in_a_task() {
    let (mut page, _) = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            r#"var r = new FileReader(); var events = []; ['loadstart', 'progress', 'load', 'loadend', 'abort', 'error'].forEach(function (t) { r.addEventListener(t, function (e) { events.push(t + ':' + e.loaded + '/' + e.total + ':' + r.readyState); }); });
               log.push(r.readyState, String(r.result), String(r.error));
               r.readAsText(new Blob(['héllo'], { type: 'text/plain' }));
               log.push(r.readyState, attempt(function () { r.readAsText(new Blob(['x'])); }));
               await new Promise(function (ok) { r.onloadend = ok; });
               log.push(r.readyState, r.result, events.join(','));
               r.readAsDataURL(new Blob([new Uint8Array([1, 2, 3])], { type: 'a/b' }));
               await new Promise(function (ok) { r.onloadend = ok; });
               log.push(r.result);
               r.readAsArrayBuffer(new Blob(['xy']));
               await new Promise(function (ok) { r.onloadend = ok; });
               log.push(hex(r.result));
               r.readAsBinaryString(new Blob([new Uint8Array([200, 65])]));
               await new Promise(function (ok) { r.onloadend = ok; });
               log.push(r.result.length + ':' + r.result.charCodeAt(0));
               r.readAsText(new Blob([new Uint8Array([0xff, 0xfe, 0x68, 0x00])]), 'utf-16');
               await new Promise(function (ok) { r.onloadend = ok; });
               log.push(r.result);
               var r2 = new FileReader(); r2.readAsText(new Blob(['z'])); r2.abort(); log.push(r2.readyState, String(r2.result), r2.error && r2.error.name);"#
        ),
        "0 / null / null / 1 / InvalidStateError / 2 / h\u{e9}llo / loadstart:0/6:1,progress:6/6:2,load:6/6:2,loadend:6/6:2 / data:a/b;base64,AQID / 7879 / 2:200 / h / 2 / null / AbortError"
    );
}

#[test]
fn form_data_entries_and_bodies() {
    let (mut page, net) = load(FIXTURE);
    for (source, expected) in [
        (
            "var fd = new FormData(); fd.append('a', '1'); fd.append('a', '2'); fd.append('f', new Blob(['xy'], { type: 'text/plain' })); fd.append('g', new File(['q'], 'q.txt'), 'renamed.txt'); [String(fd), fd.get('a'), fd.getAll('a').join('+'), fd.has('f'), fd.get('f') instanceof File, fd.get('f').name, fd.get('g').name, fd.get('none')].map(String).join(' ')",
            "[object FormData] 1 1+2 true true blob renamed.txt null",
        ),
        (
            "fd.set('a', '3'); fd.delete('g'); Array.from(fd).map(function (e) { return e[0] + '=' + (e[1] instanceof File ? 'file:' + e[1].name : e[1]); }).join(',') + ' ' + Array.from(fd.keys()).join('') + ' ' + [...fd.entries()].length",
            "a=3,f=file:blob af 2",
        ),
        (
            "var ff = new FormData(document.getElementById('f')); Array.from(ff).map(function (e) { return e[0] + '=' + JSON.stringify(e[1]); }).join(',')",
            "a=\"1\",b=\"on\",d=\"r2\",t=\"line1\\nline2\",sel=\"v2\",multi=\"m1\",_charset_=\"UTF-8\"",
        ),
        (
            "Array.from(new FormData(document.getElementById('f'), document.querySelector('[type=submit]'))).pop().join('=') + ' ' + attempt(function () { new FormData(document.body); }) + ' ' + attempt(function () { new FormData(document.getElementById('f'), document.createElement('button')); })",
            "s=go TypeError NotFoundError",
        ),
    ] {
        assert_eq!(eval(&mut page, source), expected, "{source}");
    }
    assert_eq!(
        step(
            &mut page,
            "var r = await fetch('/form', { method: 'POST', body: fd }); var back = await r.formData(); log.push(r.headers.get('content-type').split(';')[0], back.get('a'), back.get('f') instanceof File, back.get('f').name, back.get('f').type, await back.get('f').text());"
        ),
        "multipart/form-data / 3 / true / blob / text/plain / xy"
    );
    let (content_type, body) = net.seen.borrow().last().cloned().unwrap();
    let boundary = content_type.split("boundary=").nth(1).unwrap().to_string();
    let text = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(
        text,
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n3\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"f\"; filename=\"blob\"\r\nContent-Type: text/plain\r\n\r\nxy\r\n--{boundary}--\r\n"
        )
    );
    assert_eq!(
        step(
            &mut page,
            "var r = await fetch('/form', { method: 'POST', body: new URLSearchParams('x=1&y=2&x=3') }); var back = await r.formData(); log.push(back.getAll('x').join(), back.get('y')); var blob = await (await fetch('/img')).blob(); log.push(blob.type, blob.size, await new Response('{\"k\":1}', { headers: { 'content-type': 'text/plain' } }).formData().catch(function (e) { return e.name; }));"
        ),
        "1,3 / 2 / image/png / 4 / TypeError"
    );
    assert_eq!(
        step(
            &mut page,
            "var x = new XMLHttpRequest(); x.open('POST', '/form'); x.responseType = 'blob'; x.send(new Blob(['hi'], { type: 'text/x-hi' })); await new Promise(function (ok) { x.onload = ok; }); log.push(x.response instanceof Blob, x.response.type, await x.response.text()); var x2 = new XMLHttpRequest(); x2.open('POST', '/form'); x2.send(fd); await new Promise(function (ok) { x2.onload = ok; }); log.push(x2.getResponseHeader('content-type').split(';')[0]);"
        ),
        "true / text/x-hi / hi / multipart/form-data"
    );
}

#[test]
fn object_urls_resolve_in_the_page() {
    let (mut page, _) = load(FIXTURE);
    assert_eq!(
        step(
            &mut page,
            "var u = URL.createObjectURL(new Blob(['obj'], { type: 'text/plain' })); log.push(u.indexOf('blob:https://example.test/') === 0, u.length > 40); var r = await fetch(u); log.push(r.status, r.headers.get('content-type'), await r.text()); URL.revokeObjectURL(u); log.push(await fetch(u).then(function () { return 'ok'; }, function (e) { return e.name; }));"
        ),
        "true / true / 200 / text/plain / obj / TypeError"
    );
}
