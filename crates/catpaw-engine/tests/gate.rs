//! Gates on navigations and on the requests script makes: what is held
//! is not sent until released, then sent once; what is refused is never
//! sent. Against a small local HTTP server that logs what it is asked.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use catpaw_engine::{Gate, LoopLimits, PageEvent, PageOptions, with_html, with_page};
use catpaw_net::NetConfig;
use url::Url;

type Log = Arc<Mutex<Vec<String>>>;

/// Reads one request, head and body; its first line.
fn read_request(stream: &mut TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&data[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if data.len() >= end + 4 + length {
                return head.lines().next().unwrap_or("").to_string();
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => {
                return String::from_utf8_lossy(&data)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
}

/// Serves `pages` (path to HTML, or `redirect:<path>` for a 302 there,
/// `redirect307:<path>` for a 307;
/// `/api` answers "ok") and logs each request as `METHOD /path`.
fn serve(pages: HashMap<&'static str, &'static str>) -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let line = read_request(&mut stream);
            let mut words = line.split_whitespace();
            let method = words.next().unwrap_or("GET").to_string();
            let path = words.next().unwrap_or("/").to_string();
            seen.lock().unwrap().push(format!("{method} {path}"));
            let body = match pages.get(path.as_str()) {
                Some(body) => body.to_string(),
                None if path.starts_with("/api") => "ok".to_string(),
                None => "<p>not found</p>".to_string(),
            };
            let redirect = body
                .strip_prefix("redirect:")
                .map(|to| ("302 Found", to))
                .or_else(|| {
                    body.strip_prefix("redirect307:")
                        .map(|to| ("307 Temporary Redirect", to))
                });
            if let Some((status, to)) = redirect {
                let head = format!(
                    "HTTP/1.1 {status}\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes());
                continue;
            }
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
        let held = page.held_navigations().to_vec();
        assert_eq!(held.len(), 1, "the submission is held");
        assert_eq!(held[0].request.url.path(), "/done");
        assert_eq!(count(&seen, "POST /done"), 0, "held means not sent");
        assert!(page.url().path().ends_with("/form"));

        assert!(page.release_held(held[0].id).unwrap());
        assert_eq!(count(&seen, "POST /done"), 1);
        assert!(page.url().path().ends_with("/done"));
        assert!(page.held_navigations().is_empty());
        assert!(
            !page.release_held(held[0].id).unwrap(),
            "nothing is held twice"
        );
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
        assert_eq!(held[0].method, "POST");
        assert_eq!(count(&seen, "POST /api"), 0);

        assert_eq!(page.release_held_requests(&[held[0].id]), 1);
        page.settle(&options.limits);
        assert_eq!(count(&seen, "POST /api"), 1);
        assert_eq!(page.eval("document.title").unwrap(), "ok");
        assert!(page.held_requests().is_empty());
    })
    .unwrap();
}

#[test]
fn a_held_preflight_is_given_as_the_request_it_prepares() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>waiting</title>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let mut options = options();
    options.limits.settle = Some(Default::default());
    let gated = options.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), gated, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "PUT" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        // A request to another origin with a header of its own needs a
        // preflight: that is what waits, and letting it go lets the PUT go.
        page.eval(&format!(
            "fetch('http://localhost:{port}/api', {{method: 'PUT', headers: {{'X-Thing': '1'}}, body: 'x'}}); 1"
        ))
        .unwrap();
        page.settle(&options.limits);
        let held = page.held_requests();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].method, "PUT", "{held:?}");
        assert_eq!(count(&seen, "OPTIONS /api"), 0);
        assert_eq!(count(&seen, "PUT /api"), 0);
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
        let held = page.held_requests();
        assert_eq!(held.len(), 1);
        assert_eq!(page.drop_held_requests(&[held[0].id]), 1);
        page.settle(&limits);
        assert_eq!(page.eval("document.title").unwrap(), "failed");
        assert_eq!(count(&seen, "POST /api"), 0);
    })
    .unwrap();
}

#[test]
fn a_release_lets_go_only_the_holds_it_names() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>waiting</title><script>for (const n of ['one', 'two']) fetch('/api-' + n, {method: 'POST', body: n}).then(r => r.text(), e => 'failed').then(t => document.title += ' ' + n + '=' + t);</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let mut options = options();
    options.limits.settle = Some(Default::default());
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        let mark = page.hold_watermark();
        page.goto(url).unwrap();
        page.settle(&limits);
        let held = page.held_requests();
        assert_eq!(held.len(), 2, "{held:?}");
        assert!(held.iter().all(|h| h.id >= mark), "{held:?}");
        let one = held.iter().find(|h| h.url.path() == "/api-one").unwrap().id;
        let two = held.iter().find(|h| h.url.path() == "/api-two").unwrap().id;
        assert_ne!(one, two);

        assert_eq!(page.release_held_requests(&[one]), 1);
        page.settle(&limits);
        assert_eq!(count(&seen, "POST /api-one"), 1);
        assert_eq!(count(&seen, "POST /api-two"), 0, "not named, still held");
        let left = page.held_requests();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, two);
        assert_eq!(page.release_held_requests(&[one]), 0, "released once");
        assert_eq!(page.eval("document.title").unwrap(), "waiting one=ok");
    })
    .unwrap();
}

#[test]
fn held_requests_go_with_their_document() {
    let (port, log) = serve(HashMap::from([
        (
            "/app",
            "<!doctype html><script>fetch('/api', {method: 'POST', body: 'x'});</script><a id=away href=/next>next</a>",
        ),
        ("/next", "<!doctype html><title>next</title>"),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let options = options();
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        page.settle(&limits);
        let held = page.held_requests();
        assert_eq!(held.len(), 1);
        page.take_events();
        page.click("#away").unwrap();
        let events = page.take_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PageEvent::HoldDropped { id } if *id == held[0].id)),
            "{events:?}"
        );
        assert!(page.held_requests().is_empty());
        assert_eq!(page.release_held_requests(&[held[0].id]), 0);
        page.settle(&limits);
        assert_eq!(count(&seen, "POST /api"), 0, "never sent");
    })
    .unwrap();
}

#[test]
fn a_redirect_meets_the_gate_at_every_hop() {
    let (port, log) = serve(HashMap::from([
        (
            "/start",
            "<!doctype html><a id=hop href=/hop>hop</a><a id=post href=/post-hop>post</a>",
        ),
        ("/hop", "redirect:/forbidden"),
        ("/forbidden", "<!doctype html><title>forbidden</title>"),
        ("/post-hop", "redirect:/held"),
        ("/held", "<!doctype html><title>held</title>"),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/start")).unwrap();
    let seen = log.clone();
    with_page(url.clone(), options(), move |page| {
        page.set_navigation_gate(Some(Box::new(|request| match request.url.path() {
            "/forbidden" => Gate::Deny("not on the list".to_string()),
            "/held" => Gate::Hold,
            _ => Gate::Allow,
        })));
        page.take_events();
        page.click("#hop").unwrap();
        let events = page.take_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                PageEvent::NavigationBlocked { reason, .. } if reason == "not on the list"
            )),
            "{events:?}"
        );
        assert_eq!(count(&seen, "GET /hop"), 1);
        assert_eq!(
            count(&seen, "GET /forbidden"),
            0,
            "the refused hop is not fetched"
        );
        assert!(page.url().path().ends_with("/start"));

        // A hop the gate holds waits as its own navigation, and goes when
        // released.
        page.click("#post").unwrap();
        let held = page.held_navigations().to_vec();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].request.url.path(), "/held");
        assert_eq!(count(&seen, "GET /held"), 0);
        assert!(page.release_held(held[0].id).unwrap());
        assert_eq!(count(&seen, "GET /held"), 1);
        assert!(page.url().path().ends_with("/held"));
    })
    .unwrap();
}

#[test]
fn synchronous_requests_meet_the_gate() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>start</title><script>try { const x = new XMLHttpRequest(); x.open('POST', '/api-sync', false); x.send('x'); document.title = 'sent ' + x.status; } catch (e) { document.title = 'refused ' + e.name; }</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let options = options();
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        page.settle(&limits);
        let title = page.eval("document.title").unwrap();
        assert!(title.starts_with("refused"), "{title}");
        assert_eq!(count(&seen, "POST /api-sync"), 0);
        let events = page.take_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                PageEvent::RequestBlocked { method, .. } if method == "POST"
            )),
            "{events:?}"
        );
    })
    .unwrap();
}

#[test]
fn sockets_meet_the_gate() {
    let (port, log) = serve(HashMap::from([(
        "/app",
        "<!doctype html><title>start</title><script>const ws = new WebSocket('ws://' + location.host + '/ws'); ws.onerror = () => document.title = 'refused'; ws.onopen = () => document.title = 'open';</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let options = options();
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.url.scheme() == "ws" {
                Gate::Deny("not on the list".to_string())
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        page.settle(&limits);
        assert_eq!(page.eval("document.title").unwrap(), "refused");
        assert_eq!(count(&seen, "GET /ws"), 0, "never connected");
        let events = page.take_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                PageEvent::RequestBlocked { url, reason, .. }
                    if url.path() == "/ws" && reason == "not on the list"
            )),
            "{events:?}"
        );
    })
    .unwrap();
}

#[test]
fn another_navigation_gives_up_the_held_one_and_a_failed_one_leaves_the_page_running() {
    let (port, log) = serve(HashMap::from([(
        "/done",
        "<!doctype html><title>Done</title>",
    )]));
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let html = format!(
        "<!doctype html><title>start</title><form method=post action=/done><button id=go>Go</button></form>\
         <button id=fail onclick=\"setTimeout(() => document.title = 'ran on', 50); location = 'http://127.0.0.1:{closed}/'\">fail</button>"
    );
    let url = Url::parse(&format!("http://127.0.0.1:{port}/form")).unwrap();
    let seen = log.clone();
    with_html(url, html, options(), move |page| {
        page.set_navigation_gate(Some(Box::new(|request| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.click("#go").unwrap();
        let held = page.held_navigations().to_vec();
        assert_eq!(held.len(), 1);
        page.take_events();
        let _ = page.click("#fail");
        let events = page.take_events();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PageEvent::HoldDropped { id } if *id == held[0].id)),
            "{events:?}"
        );
        assert!(page.held_navigations().is_empty());
        assert!(!page.release_held(held[0].id).unwrap());
        assert_eq!(count(&seen, "POST /done"), 0);
        // The failed navigation left the document, which ran on.
        assert_eq!(page.eval("document.title").unwrap(), "ran on");
    })
    .unwrap();
}

#[test]
fn an_approved_request_goes_on_through_its_redirects() {
    let (port, log) = serve(HashMap::from([
        (
            "/app",
            "<!doctype html><title>waiting</title><script>fetch('/moved', {method: 'POST', body: 'x'}).then(r => r.text()).then(t => document.title = t, e => document.title = 'failed');</script>",
        ),
        ("/moved", "redirect307:/api-final"),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/app")).unwrap();
    let seen = log.clone();
    let mut options = options();
    options.limits.settle = Some(Default::default());
    let limits = options.limits.clone();
    let blank = Url::parse(&format!("http://127.0.0.1:{port}/blank")).unwrap();
    with_html(blank, String::new(), options, move |page| {
        page.set_request_gate(Some(Rc::new(|request: &catpaw_web::net::NetRequest| {
            if request.method == "POST" {
                Gate::Hold
            } else {
                Gate::Allow
            }
        })));
        page.goto(url).unwrap();
        page.settle(&limits);
        let held = page.held_requests();
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(page.release_held_requests(&[held[0].id]), 1);
        page.settle(&limits);
        assert_eq!(count(&seen, "POST /moved"), 1);
        assert_eq!(
            count(&seen, "POST /api-final"),
            1,
            "the hop the user approved with it"
        );
        assert!(page.held_requests().is_empty());
        assert_eq!(page.eval("document.title").unwrap(), "ok");
    })
    .unwrap();
}
