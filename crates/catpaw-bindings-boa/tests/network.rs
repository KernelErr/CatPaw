//! Scripts that come from the network: external classic scripts, modules
//! and import maps, against an in-memory network.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::net::{NetHost, NetRequest, NetResponse, NetResult};
use catpaw_web::{ConsoleLevel, PageConfig, PageState, scripting};
use url::Url;

const ORIGIN: &str = "https://example.test";

/// Serves a fixed set of files. Requests started asynchronously complete
/// the next time the event loop polls.
#[derive(Default)]
struct TestNet {
    files: HashMap<String, String>,
    completed: RefCell<Vec<(u64, NetResult)>>,
    next_token: Cell<u64>,
    requested: RefCell<Vec<String>>,
    cookies: RefCell<Vec<String>>,
}

impl TestNet {
    fn respond(&self, request: &NetRequest) -> NetResult {
        self.requested
            .borrow_mut()
            .push(request.url.path().to_string());
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
        self.cookies.borrow().join("; ")
    }

    fn set_cookie(&self, _url: &Url, cookie: &str) {
        let pair = cookie.split(';').next().unwrap_or_default().trim();
        self.cookies.borrow_mut().push(pair.to_string());
    }
}

fn load(html: &str, files: &[(&str, &str)]) -> (BoaPage, Rc<TestNet>) {
    let net = Rc::new(TestNet {
        files: files
            .iter()
            .map(|(path, body)| (path.to_string(), body.to_string()))
            .collect(),
        ..TestNet::default()
    });
    let state = Rc::new(PageState::new(
        Url::parse(&format!("{ORIGIN}/app/index.html")).unwrap(),
        PageConfig::default(),
    ));
    state.set_net(net.clone());
    let mut page = BoaPage::new(state).expect("page setup");
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle, "the page should settle");
    (page, net)
}

fn eval(page: &mut BoaPage, source: &str) -> String {
    match page.eval_to_string(source) {
        Ok(text) => text,
        Err(e) => format!("THROWN {e}"),
    }
}

fn errors(page: &BoaPage) -> Vec<String> {
    page.page()
        .console_messages()
        .into_iter()
        .filter(|m| m.level == ConsoleLevel::Error)
        .map(|m| m.text)
        .collect()
}

#[test]
fn external_scripts_run_in_the_specified_order() {
    let (mut page, net) = load(
        r#"<script>var log = [];</script>
<script src="/deferred.js" defer></script>
<script src="/async.js" async onload="log.push('async load event')"></script>
<script src="/blocking.js"></script>
<script>log.push('inline after blocking');</script>
<script src="/missing.js" onerror="log.push('error event')"></script>
<script>
  var dynamic = document.createElement('script');
  dynamic.src = '../dynamic.js';
  dynamic.onload = function () { log.push('dynamic load event ' + document.readyState); };
  document.head.appendChild(dynamic);
  log.push('after insertion');
  document.addEventListener('DOMContentLoaded', function () { log.push('DOMContentLoaded'); });
  window.addEventListener('load', function () { log.push('load'); });
</script>"#,
        &[
            (
                "/deferred.js",
                "log.push('deferred ' + document.readyState);",
            ),
            ("/async.js", "log.push('async');"),
            (
                "/blocking.js",
                "log.push('blocking ' + document.currentScript.getAttribute('src') + ' ' + document.currentScript.src);",
            ),
            ("/dynamic.js", "log.push('dynamic');"),
        ],
    );
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        "blocking /blocking.js https://example.test/blocking.js; inline after blocking; \
         error event; after insertion; async; async load event; dynamic; \
         dynamic load event interactive; deferred interactive; DOMContentLoaded; load"
    );
    assert_eq!(
        *net.requested.borrow(),
        vec![
            "/deferred.js",
            "/async.js",
            "/blocking.js",
            "/missing.js",
            "/dynamic.js"
        ]
    );
    assert_eq!(errors(&page).len(), 1, "{:?}", errors(&page));
    assert!(errors(&page)[0].contains("/missing.js: HTTP 404"));
}

#[test]
fn module_scripts_load_their_imports() {
    let (mut page, net) = load(
        r#"<script type="importmap">{ "imports": { "lib": "/lib/index.js", "pkg/": "/pkg/" } }</script>
<script>var log = [];</script>
<script type="module" src="/main.js"></script>
<script type="module">
  import { double } from '/math.js';
  log.push('inline module ' + double(4) + ' ' + (document.currentScript === null) + ' ' + typeof this);
</script>
<script nomodule>log.push('nomodule fallback');</script>
<script>log.push('classic');</script>"#,
        &[
            (
                "/main.js",
                "import { double } from './math.js';\n\
                 import lib from 'lib';\n\
                 import { deep } from 'pkg/deep.js';\n\
                 log.push('main ' + double(21) + ' ' + lib + ' ' + deep + ' ' + import.meta.url);\n\
                 const lazy = await import('./lazy.js');\n\
                 log.push(lazy.default);",
            ),
            (
                "/math.js",
                "log.push('math evaluated');\nexport function double(x) { return x * 2; }",
            ),
            ("/lib/index.js", "export default 'lib!';"),
            ("/pkg/deep.js", "export const deep = 'deep';"),
            ("/lazy.js", "export default 'lazy';"),
        ],
    );
    assert_eq!(
        eval(&mut page, "log.join('; ')"),
        "classic; math evaluated; main 42 lib! deep https://example.test/main.js; lazy; \
         inline module 8 true undefined"
    );
    // Each module is fetched once, however often it is imported.
    let requested = net.requested.borrow();
    assert_eq!(requested.iter().filter(|p| *p == "/math.js").count(), 1);
    assert!(errors(&page).is_empty(), "{:?}", errors(&page));
}

#[test]
fn module_failures_are_reported() {
    let (mut page, _net) = load(
        r#"<script>var log = [];</script>
<script type="module">import '/absent.js'; log.push('unreachable');</script>
<script type="module">import 'bare-specifier';</script>
<script type="module">log.push('still runs'); throw new Error('from a module');</script>
<script type="module" src="/syntax.js" onerror="log.push('error event')"></script>"#,
        &[("/syntax.js", "export const = ;")],
    );
    assert_eq!(eval(&mut page, "log.join('; ')"), "still runs; error event");
    let errors = errors(&page);
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors[0].contains("Failed to fetch module https://example.test/absent.js: HTTP 404"));
    assert!(errors[1].contains("Failed to resolve module specifier \"bare-specifier\""));
    assert!(errors[2].contains("Error: from a module"));
    assert!(errors[3].starts_with("Uncaught SyntaxError"));
}

#[test]
fn document_cookie_goes_through_the_network_host() {
    let (mut page, net) = load(
        "<script>document.cookie = 'a=1; Path=/'; document.cookie = 'b=2';</script>",
        &[],
    );
    assert_eq!(eval(&mut page, "document.cookie"), "a=1; b=2");
    assert_eq!(*net.cookies.borrow(), vec!["a=1", "b=2"]);
}
