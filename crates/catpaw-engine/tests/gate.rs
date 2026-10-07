//! Gates on navigations and on the requests script makes: what is held
//! is not sent until released, then sent once; what is refused is never
//! sent. Against a small local HTTP server that logs what it is asked.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use catpaw_engine::{Gate, LoopLimits, PageEvent, PageOptions, with_html, with_page};
use catpaw_net::NetConfig;
use url::Url;

type Log = Arc<Mutex<Vec<String>>>;

/// Serves `pages` (path to HTML; `/api` answers "ok") and logs each
/// request as `METHOD /path`.
fn serve(pages: HashMap<&'static str, &'static str>) -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let mut words = request.lines().next().unwrap_or("").split_whitespace();
            let method = words.next().unwrap_or("GET").to_string();
            let path = words.next().unwrap_or("/").to_string();
            seen.lock().unwrap().push(format!("{method} {path}"));
            let body = match pages.get(path.as_str()) {
                Some(body) => body.to_string(),
                None if path == "/api" => "ok".to_string(),
                None => "<p>not found</p>".to_string(),
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (port, log)
}

fn options() -> PageOptions {
    PageOptions {
        net: NetConfig {
            allow_private_network: true,
            ..NetConfig::default()
        },
        limits: LoopLimits {
            wall: std::time::Duration::from_secs(10),
            virtual_ms: 5_000.0,
            max_steps: 100_000,
            settle: None,
        },
        ..PageOptions::default()
    }
}

fn count(log: &Log, line: &str) -> usize {
    log.lock().unwrap().iter().filter(|l| *l == line).count()
}

#[test]
fn a_held_form_submission_goes_once_when_released() {
    let (port, log) = serve(HashMap::from([
        (
            "/form",
            "<!doctype html><form method=post action=/done><input name=q value=x><button id=go>Go</button></form><a id=away href=/away>away</a>",
        ),
        ("/done", "<!doctype html><title>Done</title>"),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/form")).unwrap();
    let seen = log.clone();
    with_page(url.clone(), options(), move |page| {
        page.set_navigation_gate(Some(Box::new(|request| {
            match (request.method, request.url.path()) {
                ("POST", _) => Gate::Hold,
                (_, "/away") => Gate::Deny("not on the list".to_string()),
                _ => Gate::Allow,
            }
        })));
        page.take_events();
        page.click("#go").unwrap();
        let events = page.take_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PageEvent::NavigationHeld { method, .. } if method == "POST")),
            "{events:?}"
        );
        let held = page.held().expect("the submission is held");
        assert_eq!(held.request.url.path(), "/done");
        assert_eq!(count(&seen, "POST /done"), 0, "held means not sent");
        assert!(page.url().path().ends_with("/form"));

        assert!(page.release_held().unwrap());
        assert_eq!(count(&seen, "POST /done"), 1);
        assert!(page.url().path().ends_with("/done"));
        assert!(page.held().is_none());
        assert!(!page.release_held().unwrap(), "nothing is held twice");
        assert_eq!(count(&seen, "POST /done"), 1);

        page.goto(url).unwrap();
        page.take_events();
        page.click("#away").unwrap();
        let events = page.take_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                PageEvent::NavigationBlocked { reason, .. } if reason == "not on the list"
            )),
            "{events:?}"
        );
        assert_eq!(count(&seen, "GET /away"), 0);
    })
    .unwrap();
}

#[test]
fn a_held_script_request_waits_without_keeping_the_page_busy() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>waiting</title><script>fetch('/api', {method: 'POST', body: 'x'}).then(r => r.text()).then(t => document.title = t, e => document.title = 'failed');</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let mut options = options();
    options.limits.settle = Some(Default::default());
    let gated = options.clone();
    // The gate must be in place before the page's script runs: start
    // from an empty document, gate it, then go.
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), gated, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        let report = page.settle(&options.limits).clone();
        assert!(page.is_settled(), "{:?}", report.stop);
        assert_eq!(page.eval("document.title").unwrap(), "waiting");
        let held = page.held_requests();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].0, "POST");
        assert_eq!(count(&seen, "POST /api"), 0);

        page.release_held_requests();
        page.settle(&options.limits);
        assert_eq!(count(&seen, "POST /api"), 1);
        assert_eq!(page.eval("document.title").unwrap(), "ok");
        assert!(page.held_requests().is_empty());
    })
    .unwrap();
}

#[test]
fn a_dropped_script_request_fails_for_the_page() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>waiting</title><script>fetch('/api', {method: 'POST', body: 'x'}).then(r => r.text()).then(t => document.title = t, e => document.title = 'failed');</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let options = options();
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|_: &catpaw_web::net::NetRequest| Gate::Hold)));
        page.goto(url).unwrap();
        page.settle(&limits);
        assert_eq!(page.held_requests().len(), 1);
        page.drop_held_requests();
        page.settle(&limits);
        assert_eq!(page.eval("document.title").unwrap(), "failed");
        assert_eq!(count(&seen, "POST /api"), 0);
    })
    .unwrap();
}
