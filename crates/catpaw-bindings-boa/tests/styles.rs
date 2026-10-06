//! `getComputedStyle()` and the style sheets it is computed from: `<style>`
//! elements and fetched `<link rel=stylesheet>` sheets.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

/// Serves a fixed set of files, typed by their extension. Requests started
/// asynchronously complete the next time the page polls.
#[derive(Default)]
struct TestNet {
    files: HashMap<String, String>,
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
    requested: RefCell<Vec<String>>,
}

impl TestNet {
    fn respond(&self, request: &NetRequest) -> NetResult {
        let path = request.url.path();
        self.requested.borrow_mut().push(path.to_string());
        let content_type = if path.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if path.ends_with(".js") {
            "text/javascript"
        } else {
            "text/plain"
        };
        let (status, body) = match self.files.get(path) {
            Some(body) => (200, body.clone()),
            None => (404, String::new()),
        };
        Ok(NetResponse {
            url: request.url.clone(),
            status,
            status_text: String::new(),
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: body.into_bytes(),
            redirected: false,
        })
    }
}

impl NetHost for TestNet {
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

fn settle(page: &mut BoaPage) {
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
}

fn load_with(html: &str, net: Option<Rc<TestNet>>) -> BoaPage {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/app/index.html").unwrap(),
        PageConfig::default(),
    ));
    if let Some(net) = net {
        state.set_net(net);
    }
    let mut page = BoaPage::new(state).expect("page setup");
    page.with_cx(|cx| scripting::load_document(cx, html));
    settle(&mut page);
    page
}

fn serve(files: &[(&str, &str)]) -> Rc<TestNet> {
    Rc::new(TestNet {
        files: files
            .iter()
            .map(|(path, body)| (path.to_string(), body.to_string()))
            .collect(),
        ..TestNet::default()
    })
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

fn check(page: &mut BoaPage, cases: &[(&str, &str)]) {
    for (source, expected) in cases {
        assert_eq!(eval(page, source), *expected, "{source}");
    }
}

const DOCUMENT: &str = r#"<!doctype html><style>
  :root { --brand: #336699; }
  body { color: rgb(10, 20, 30); margin: 0; }
  .box { display: flex; opacity: 0.5; transition: opacity 0.3s ease 0.1s; }
  .box::before { content: "pre"; }
  .gone { display: none; }
  @media (min-width: 2000px) { .box { display: grid; } }
</style>
<style media="print">.box { display: none; }</style>
<style type="text/x-other">.box { display: table; }</style>
<div id="box" class="box" style="margin-left: 5px"><span id="in">x</span></div>
<div class="gone"><b id="deep">y</b></div>
<script>
  var box = document.getElementById('box'), cs = getComputedStyle(box);
  function attempt(f) { try { return String(f()); } catch (e) { return e.name; } }
</script>"#;

#[test]
fn computed_styles_come_from_the_cascade() {
    let mut page = load_with(DOCUMENT, None);
    check(
        &mut page,
        &[
            ("cs.display", "flex"),
            ("cs.opacity", "0.5"),
            ("cs.getPropertyValue('opacity')", "0.5"),
            ("cs.color", "rgb(10, 20, 30)"),
            ("cs.marginLeft", "5px"),
            ("cs['margin-left']", "5px"),
            ("cs.margin", "0px 0px 0px 5px"),
            ("cs.cssFloat", "none"),
            ("cs.position + ' ' + cs.visibility", "static visible"),
            ("cs.getPropertyValue('--brand')", "#336699"),
            (
                "cs.transitionDuration + ' ' + cs.transitionDelay",
                "0.3s 0.1s",
            ),
            ("cs.getPropertyValue('no-such-property')", ""),
            ("cs.getPropertyPriority('display')", ""),
            ("cs.cssText", ""),
            ("cs instanceof CSSStyleDeclaration", "true"),
            (
                "cs.length > 100 && cs.item(0) === cs[0] && typeof cs[0]",
                "string",
            ),
            ("Array.from(cs).indexOf('display') >= 0", "true"),
            ("cs.item(100000)", ""),
            ("getComputedStyle(document.body).display", "block"),
            ("getComputedStyle(document.head).display", "none"),
            // A flex item is blockified.
            (
                "getComputedStyle(document.getElementById('in')).display",
                "block",
            ),
            // Elements in a subtree that is not rendered have styles too.
            (
                "getComputedStyle(document.getElementById('deep')).display",
                "inline",
            ),
            (
                "getComputedStyle(document.getElementById('deep')).color",
                "rgb(10, 20, 30)",
            ),
        ],
    );
}

#[test]
fn computed_styles_are_live_and_read_only() {
    let mut page = load_with(DOCUMENT, None);
    check(
        &mut page,
        &[
            ("box.className = ''; cs.display", "block"),
            ("box.style.display = 'inline-block'; cs.display", "inline-block"),
            ("box.style.display = ''; box.className = 'box'; cs.display", "flex"),
            // The sheets follow the document.
            (
                "var extra = document.createElement('style'); extra.textContent = '.box { display: grid }';
                 document.head.appendChild(extra); cs.display",
                "grid",
            ),
            ("extra.textContent = '.box { opacity: 1 }'; cs.display + ' ' + cs.opacity", "flex 1"),
            ("extra.media = 'print'; cs.opacity", "0.5"),
            ("extra.removeAttribute('media'); cs.opacity", "1"),
            ("extra.remove(); cs.opacity", "0.5"),
            // An element out of the document has no style.
            ("box.remove(); cs.display + '|' + cs.length", "|0"),
            ("document.body.appendChild(box); cs.display", "flex"),
            ("attempt(function () { cs.display = 'none'; })", "NoModificationAllowedError"),
            (
                "attempt(function () { cs.setProperty('display', 'none'); })",
                "NoModificationAllowedError",
            ),
            (
                "attempt(function () { cs.removeProperty('display'); })",
                "NoModificationAllowedError",
            ),
            ("attempt(function () { cs.cssText = 'display: none'; })", "NoModificationAllowedError"),
            ("cs.display", "flex"),
            ("attempt(function () { return getComputedStyle(); })", "TypeError"),
            ("attempt(function () { return getComputedStyle(document); })", "TypeError"),
        ],
    );
}

#[test]
fn pseudo_elements_have_computed_styles() {
    let mut page = load_with(DOCUMENT, None);
    check(
        &mut page,
        &[
            ("getComputedStyle(box, '::before').content", "\"pre\""),
            ("getComputedStyle(box, ':before').content", "\"pre\""),
            ("getComputedStyle(box, '::before').color", "rgb(10, 20, 30)"),
            ("getComputedStyle(box, '::after').content", "none"),
            // Not a pseudo-element there are styles for: nothing has a value.
            (
                "var none = getComputedStyle(box, '::nonsense'); none.display + '|' + none.length",
                "|0",
            ),
            // Not a pseudo-element selector at all: the element itself.
            ("getComputedStyle(box, 'before').display", "flex"),
            ("getComputedStyle(box, '').display", "flex"),
            ("getComputedStyle(box, null).display", "flex"),
        ],
    );
}

#[test]
fn linked_style_sheets_load_and_hold_up_what_must_wait() {
    let net = serve(&[
        (
            "/main.css",
            ":root { --theme: dark } p { color: rgb(1, 2, 3) }",
        ),
        ("/print.css", "p { color: red }"),
        ("/alt.css", "p { color: red }"),
        ("/icon.css", "p { color: red }"),
        ("/wrong-type.txt", "p { color: red }"),
    ]);
    let mut page = load_with(
        r#"<!doctype html><script>var log = [];</script>
<link rel="stylesheet" href="/main.css" onload="log.push('main loaded')">
<script>log.push('script sees ' + getComputedStyle(document.documentElement).getPropertyValue('--theme'));</script>
<link rel="stylesheet" href="/missing.css" onerror="log.push('missing failed')">
<link rel="stylesheet" href="/wrong-type.txt" onerror="log.push('wrong type failed')">
<link rel="stylesheet" href="/print.css" media="print" onload="log.push('print loaded')">
<link rel="alternate stylesheet" href="/alt.css" onload="log.push('alt loaded')">
<link rel="icon" href="/icon.css" onload="log.push('icon loaded')">
<link rel="stylesheet" onload="log.push('no href loaded')">
<body><p id="p">text</p>
<script>
  window.addEventListener('load', function () { log.push('window load'); });
  var p = document.getElementById('p');
</script>"#,
        Some(net.clone()),
    );
    assert_eq!(
        eval(&mut page, "log.join(', ')"),
        "main loaded, script sees dark, missing failed, wrong type failed, print loaded, window load"
    );
    assert_eq!(eval(&mut page, "getComputedStyle(p).color"), "rgb(1, 2, 3)");
    assert_eq!(
        *net.requested.borrow(),
        ["/main.css", "/missing.css", "/wrong-type.txt", "/print.css"]
    );
}

#[test]
fn links_inserted_by_script_load_their_sheets() {
    let net = serve(&[
        ("/late.css", "p { color: rgb(4, 5, 6) }"),
        ("/other.css", "p { color: rgb(7, 8, 9) }"),
    ]);
    let mut page = load_with(
        r#"<!doctype html><style>p { color: rgb(1, 2, 3) }</style><body><p id="p">text</p>
<script>
  var log = [], p = document.getElementById('p');
  function color() { return getComputedStyle(p).color; }
  var link = document.createElement('link');
  link.rel = 'stylesheet';
  link.href = '/late.css';
  link.onload = function () { log.push('loaded ' + link.getAttribute('href') + ' ' + color()); };
  link.onerror = function () { log.push('failed ' + link.getAttribute('href')); };
  document.head.appendChild(link);
  log.push('before ' + color());
  window.onload = function () { log.push('window load ' + color()); };
</script>"#,
        Some(net.clone()),
    );
    assert_eq!(
        eval(&mut page, "log.join(', ')"),
        "before rgb(1, 2, 3), loaded /late.css rgb(4, 5, 6), window load rgb(4, 5, 6)"
    );

    // A sheet applies while its link is in the document and enabled.
    assert_eq!(
        eval(&mut page, "log = []; link.remove(); color()"),
        "rgb(1, 2, 3)"
    );
    // Elsewhere a link loads nothing...
    assert_eq!(
        eval(&mut page, "link.href = '/other.css'; color()"),
        "rgb(1, 2, 3)"
    );
    settle(&mut page);
    assert_eq!(eval(&mut page, "log.join(', ')"), "");
    // ...until it is inserted again.
    eval(&mut page, "document.head.appendChild(link); 0");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join(', ')"),
        "loaded /other.css rgb(7, 8, 9)"
    );

    // Pointing it elsewhere fetches again; what cannot be loaded fails.
    eval(&mut page, "log = []; link.href = '/late.css'; 0");
    assert_eq!(
        eval(&mut page, "color()"),
        "rgb(1, 2, 3)",
        "the old sheet is gone at once"
    );
    settle(&mut page);
    eval(&mut page, "link.href = '/nowhere.css'; 0");
    settle(&mut page);
    assert_eq!(
        eval(&mut page, "log.join(', ')"),
        "loaded /late.css rgb(4, 5, 6), failed /nowhere.css"
    );
    assert_eq!(eval(&mut page, "color()"), "rgb(1, 2, 3)");

    eval(&mut page, "log = []; link.href = '/late.css'; 0");
    settle(&mut page);
    assert_eq!(eval(&mut page, "color()"), "rgb(4, 5, 6)");
    assert_eq!(
        eval(&mut page, "link.setAttribute('disabled', ''); color()"),
        "rgb(1, 2, 3)"
    );
    assert_eq!(
        *net.requested.borrow(),
        [
            "/late.css",
            "/other.css",
            "/late.css",
            "/nowhere.css",
            "/late.css"
        ]
    );
}

#[test]
fn without_a_network_links_fail_and_the_page_still_loads() {
    let mut page = load_with(
        r#"<!doctype html><script>var log = [];</script>
<link rel="stylesheet" href="/main.css" onload="log.push('loaded')" onerror="log.push('failed')">
<script>log.push('script ran');</script>
<body onload="log.push('window load')">"#,
        None,
    );
    assert_eq!(
        eval(&mut page, "log.join(', ')"),
        "script ran, failed, window load"
    );
}
