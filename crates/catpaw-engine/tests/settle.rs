//! Settling: what an action waits for under a settle policy, against a
//! small local HTTP server that can answer slowly.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use catpaw_engine::{
    DialogAnswer, DialogPolicy, FrameId, LoopLimits, PageOptions, RequestClass, SettlePolicy,
    StopReason, TimerClass, with_page,
};
use catpaw_net::NetConfig;
use url::Url;

/// Serves `pages` by path; a path starting with `/slow` is answered after
/// 300 ms, and `/hang` after 3 s. One thread per connection.
fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    serve_on(listener, pages);
    port
}

fn serve_on(listener: TcpListener, pages: HashMap<&'static str, &'static str>) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let pages = pages.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                let path = target.split('?').next().unwrap_or("/").to_string();
                if path.starts_with("/slow") {
                    std::thread::sleep(Duration::from_millis(300));
                }
                if path.starts_with("/hang") {
                    std::thread::sleep(Duration::from_secs(3));
                }
                let body = pages.get(path.as_str()).copied().unwrap_or("ok");
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });
}

fn options(policy: Option<SettlePolicy>) -> PageOptions {
    PageOptions {
        net: NetConfig {
            allow_private_network: true,
            ..NetConfig::default()
        },
        limits: LoopLimits {
            wall: Duration::from_secs(10),
            virtual_ms: 5_000.0,
            max_steps: 100_000,
            settle: policy,
        },
        ..PageOptions::default()
    }
}

const POLLING: &str = r#"<!doctype html><p id=out>start</p>
<script>
let ticks = 0;
setInterval(() => { ticks++; }, 200);
setTimeout(() => { document.getElementById("out").textContent = "late"; }, 4000);
setTimeout(() => { document.getElementById("out").textContent = "soon"; }, 300);
</script>"#;

#[test]
fn polling_and_far_timers_do_not_hold_a_page_up() {
    let port = serve(HashMap::from([("/", POLLING)]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(Some(SettlePolicy::default())), |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        assert_eq!(page.report().stop, StopReason::Settled);
        // The timer due soon ran; the one due in four seconds did not.
        let text = page
            .eval("document.getElementById('out').textContent")
            .unwrap();
        assert_eq!(text, "soon");
        let pending = page.pending_of(FrameId(0)).unwrap();
        let interval = pending
            .timers
            .iter()
            .find(|t| t.repeat)
            .expect("the interval");
        assert_eq!(interval.class, TimerClass::Polling);
        assert!(interval.arms >= 5, "{interval:?}");
        let site = interval.site.as_ref().expect("a site");
        assert_eq!(site.line, 4, "{site:?}");
        let late = pending
            .timers
            .iter()
            .find(|t| !t.repeat)
            .expect("the late timer");
        assert_eq!(late.class, TimerClass::Far);
        assert!(!pending.blocking());
    })
    .unwrap();
}

#[test]
fn without_a_policy_the_loop_runs_to_its_budget() {
    let port = serve(HashMap::from([("/", POLLING)]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(None), |page| {
        assert_eq!(page.report().stop, StopReason::VirtualBudget);
        assert!(!page.is_settled());
    })
    .unwrap();
}

#[test]
fn requests_the_page_waits_on_are_waited_for_and_analytics_are_not() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // The analytics host is `localhost`; the page's is 127.0.0.1.
    let html = format!(
        r#"<!doctype html><p id=out>loading</p>
<script>
fetch("/slow/data").then(r => r.text()).then(t => {{
  document.getElementById("out").textContent = "loaded " + t;
}});
fetch("http://localhost:{port}/hang/collect", {{mode: "no-cors"}});
</script>"#
    );
    let html: &'static str = Box::leak(html.into_boxed_str());
    serve_on(
        listener,
        HashMap::from([("/", html), ("/slow/data", "data")]),
    );
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let policy = SettlePolicy {
        ignore_hosts: vec!["localhost".to_string()],
        ..SettlePolicy::default()
    };
    let started = Instant::now();
    with_page(url, options(Some(policy)), |page| {
        let text = page
            .eval("document.getElementById('out').textContent")
            .unwrap();
        assert_eq!(text, "loaded data");
        assert!(page.is_settled(), "{:?}", page.report());
        let pending = page.pending_of(FrameId(0)).unwrap();
        let analytics = pending
            .requests
            .iter()
            .find(|r| r.url.host_str() == Some("localhost"))
            .expect("the analytics request is still open");
        assert_eq!(analytics.class, RequestClass::IgnoredHost);
        let site = analytics.site.as_ref().expect("a site");
        assert_eq!(site.line, 6, "{site:?}");
    })
    .unwrap();
    // The page did not wait the three seconds the analytics host takes.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn dialogs_are_answered_by_the_policy() {
    let html = r#"<!doctype html><p id=out></p>
<button onclick="document.getElementById('out').textContent = confirm('Sure?') + ' ' + prompt('Name?', 'anon')">Go</button>"#;
    let port = serve(HashMap::from([("/", html)]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(Some(SettlePolicy::default())), |page| {
        page.click("button").unwrap();
        assert_eq!(
            page.eval("document.getElementById('out').textContent")
                .unwrap(),
            "false null"
        );
        page.set_dialog_policy(DialogPolicy {
            accept: true,
            prompt_text: Some("catpaw".to_string()),
        });
        page.click("button").unwrap();
        assert_eq!(
            page.eval("document.getElementById('out').textContent")
                .unwrap(),
            "true catpaw"
        );
        page.set_dialog_policy(DialogPolicy {
            accept: true,
            prompt_text: None,
        });
        page.click("button").unwrap();
        assert_eq!(
            page.eval("document.getElementById('out').textContent")
                .unwrap(),
            "true anon"
        );
        let dialogs = page.state().dialogs.borrow().clone();
        assert_eq!(dialogs.len(), 6);
        assert_eq!(dialogs[0].answer, DialogAnswer::Dismissed);
        assert_eq!(dialogs[3].answer, DialogAnswer::Text("catpaw".to_string()));
    })
    .unwrap();
}
