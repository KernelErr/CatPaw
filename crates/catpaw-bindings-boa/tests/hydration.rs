//! Hydration of server-rendered React and Vue applications, timed.
//!
//! The frameworks are the production builds in `tests/fixtures/hydration`,
//! served from an in-memory network. Each application is rendered to
//! markup, hydrated in a page, and then driven through a click, so the
//! test checks that hydration attached behaviour rather than re-rendered.
//! The times are printed for CI logs; the bound asserted is loose, there to
//! catch a collapse rather than a drift.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

const ORIGIN: &str = "https://example.test";

/// The frameworks, read from the repository's fixtures.
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/hydration")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[derive(Default)]
struct TestNet {
    files: HashMap<String, String>,
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
}

impl TestNet {
    fn respond(&self, request: &NetRequest) -> NetResult {
        match self.files.get(request.url.path()) {
            Some(body) => Ok(NetResponse {
                url: request.url.clone(),
                status: 200,
                status_text: "OK".to_string(),
                headers: vec![("content-type".to_string(), "text/javascript".to_string())],
                body: body.clone().into_bytes(),
                redirected: false,
            }),
            None => Ok(NetResponse {
                url: request.url.clone(),
                status: 404,
                status_text: "Not Found".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
                redirected: false,
            }),
        }
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

/// Loads `html` with the given scripts on the network, settles the page,
/// and returns it with how long loading took.
fn load(html: &str, files: &[(&str, String)]) -> (BoaPage, Duration) {
    let net = Rc::new(TestNet {
        files: files
            .iter()
            .map(|(path, body)| (path.to_string(), body.clone()))
            .collect(),
        ..TestNet::default()
    });
    let state = Rc::new(PageState::new(
        Url::parse(&format!("{ORIGIN}/index.html")).unwrap(),
        PageConfig::default(),
    ));
    state.set_net(net);
    let mut page = BoaPage::new(state).expect("page setup");
    let started = Instant::now();
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
    (page, started.elapsed())
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

/// A click dispatched the way an agent would, followed by a settle.
fn click(page: &mut BoaPage, selector: &str) {
    let result = eval(
        page,
        &format!(
            "document.querySelector({selector:?}).dispatchEvent(new MouseEvent('click', {{ bubbles: true, cancelable: true }})); 'clicked'"
        ),
    );
    assert_eq!(result, "clicked");
    let report = page.with_cx(|cx| event_loop::run(cx, &LoopLimits::default()));
    assert_eq!(report.stop, StopReason::Idle);
}

#[test]
fn react_hydrates_server_markup() {
    let html = r#"<!doctype html><html><head>
<script src="/react.js"></script>
<script src="/react-dom.js"></script>
<script src="/react-dom-server.js"></script>
</head><body><div id="root"></div>
<script>
  var e = React.createElement;
  function App() {
    var state = React.useState(0), n = state[0], set = state[1];
    var items = React.useMemo(function () { var out = []; for (var i = 0; i < 200; i++) out.push(e('li', { key: i, className: 'item' }, 'Item ' + i)); return out; }, []);
    return e('div', null,
      e('h1', null, 'Count: ', n),
      e('button', { id: 'inc', onClick: function () { set(n + 1); } }, '+1'),
      e('ul', null, items));
  }
  // What a server would have sent.
  var markup = ReactDOMServer.renderToString(e(App));
  document.getElementById('root').innerHTML = markup;
  window.__markupLength = markup.length;
  var errors = [];
  ReactDOM.hydrateRoot(document.getElementById('root'), e(App), {
    onRecoverableError: function (err) { errors.push(String(err)); }
  });
  window.__hydrationErrors = errors;
</script></body></html>"#;
    let (mut page, elapsed) = load(
        html,
        &[
            ("/react.js", fixture("react.production.min.js")),
            ("/react-dom.js", fixture("react-dom.production.min.js")),
            (
                "/react-dom-server.js",
                fixture("react-dom-server-legacy.browser.production.min.js"),
            ),
        ],
    );
    eprintln!("react 18 hydration: {} ms", elapsed.as_millis());
    assert_eq!(
        eval(
            &mut page,
            "window.__markupLength > 3000 && document.querySelectorAll('li.item').length + ' ' + window.__hydrationErrors.length + ' ' + document.querySelector('h1').textContent"
        ),
        "200 0 Count: 0"
    );
    click(&mut page, "#inc");
    click(&mut page, "#inc");
    assert_eq!(
        eval(&mut page, "document.querySelector('h1').textContent"),
        "Count: 2"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "hydration took {elapsed:?}"
    );
}

#[test]
fn vue_hydrates_server_markup() {
    // The markup `renderToString` produces for the application below.
    let mut items = String::new();
    for i in 0..200 {
        items.push_str(&format!("<li class=\"item\">Item {i}</li>"));
    }
    let html = format!(
        r#"<!doctype html><html><head><script src="/vue.js"></script></head><body>
<div id="app"><div><h1>Count: 0</h1><button id="inc">+1</button><ul>{items}</ul></div></div>
<script>
  var warnings = [];
  var app = Vue.createSSRApp({{
    data: function () {{ return {{ n: 0, items: Array.from({{ length: 200 }}, function (_, i) {{ return i; }}) }}; }},
    template: '<div><h1>Count: {{{{ n }}}}</h1><button id="inc" @click="n++">+1</button><ul><li v-for="i in items" class="item">Item {{{{ i }}}}</li></ul></div>'
  }});
  app.config.warnHandler = function (msg) {{ warnings.push(msg); }};
  app.mount('#app');
  window.__warnings = warnings;
</script></body></html>"#
    );
    let (mut page, elapsed) = load(&html, &[("/vue.js", fixture("vue.global.prod.js"))]);
    eprintln!("vue 3 hydration: {} ms", elapsed.as_millis());
    assert_eq!(
        eval(
            &mut page,
            "document.querySelectorAll('li.item').length + ' ' + window.__warnings.length + ' ' + document.querySelector('h1').textContent"
        ),
        "200 0 Count: 0"
    );
    click(&mut page, "#inc");
    click(&mut page, "#inc");
    assert_eq!(
        eval(&mut page, "document.querySelector('h1').textContent"),
        "Count: 2"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "hydration took {elapsed:?}"
    );
}
