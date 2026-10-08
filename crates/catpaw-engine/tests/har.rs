//! Recording a page's traffic and replaying it: the same page, offline,
//! with the same random numbers and clock; WebSockets refused; every
//! realm with random numbers of its own.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use catpaw_engine::{LoopLimits, PageConfig, PageOptions, with_html, with_page};
use catpaw_net::{Misses, NetConfig, Recording};
use url::Url;

fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            // The head and any body it announces, however they arrive.
            let mut data = Vec::new();
            let mut buf = vec![0u8; 8192];
            loop {
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
                let n = stream.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let request = String::from_utf8_lossy(&data);
            let path = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .split('?')
                .next()
                .unwrap()
                .to_string();
            let body = pages.get(path.as_str()).copied().unwrap_or("missing");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: seen=1; Path=/\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    port
}

const PAGE: &str = r#"<!doctype html><p id=out></p>
<script>
const out = document.getElementById("out");
const parts = [Math.random().toFixed(6), crypto.getRandomValues(new Uint32Array(1))[0]];
fetch("/data?t=" + Date.now()).then(r => r.text()).then(t => {
  parts.push(t, document.cookie);
  setTimeout(() => { parts.push(Math.random().toFixed(6), Date.now()); out.textContent = parts.join(" "); }, 500);
});
</script>"#;

fn options(recording: Recording) -> PageOptions {
    PageOptions {
        net: NetConfig {
            allow_private_network: true,
            recording: Some(recording),
            ..NetConfig::default()
        },
        page: PageConfig {
            random_seed: Some(42),
            time_origin_unix_ms: Some(1_790_000_000_000.0),
            ..PageConfig::default()
        },
        limits: LoopLimits {
            wall: Duration::from_secs(10),
            ..LoopLimits::default()
        },
        ..PageOptions::default()
    }
}

#[test]
fn a_recorded_page_replays_offline_the_same_every_time() {
    let port = serve(HashMap::from([("/", PAGE), ("/data", "payload")]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let dir = std::env::temp_dir().join(format!("catpaw-har-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let har = dir.join("run.har");

    let recorded = with_page(
        url.clone(),
        options(Recording::Record(har.clone())),
        |page| {
            let text = page
                .eval("document.getElementById('out').textContent")
                .unwrap();
            page.net().client().save_recording().unwrap();
            text
        },
    )
    .unwrap();
    assert!(recorded.contains("payload seen=1"), "{recorded}");
    assert!(recorded.ends_with(" 1790000000500"), "{recorded}");

    let replay = || {
        with_page(
            url.clone(),
            options(Recording::Replay {
                path: har.clone(),
                misses: Misses::Fail,
            }),
            |page| {
                page.eval("document.getElementById('out').textContent")
                    .unwrap()
            },
        )
        .unwrap()
    };
    let first = replay();
    let second = replay();
    assert_eq!(first, second);
    // The same run, but for the cookie: the recording keeps a placeholder
    // for the value the server set, never the value.
    let cookie = first
        .split(' ')
        .find(|part| part.starts_with("seen="))
        .unwrap();
    assert!(cookie.starts_with("seen=redacted-"), "{first}");
    assert_eq!(first.replace(cookie, "seen=1"), recorded);
    let file = std::fs::read_to_string(&har).unwrap();
    assert!(!file.contains("seen=1"), "{file}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A server that accepts connections, counts them and closes them at once.
fn counting_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            seen.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    (port, count)
}

#[test]
fn websockets_are_refused_at_once_while_replaying() {
    let (socket_port, connections) = counting_server();
    let page: &'static str = Box::leak(
        format!(
            r#"<!doctype html><p id=out></p>
<script>
const events = [];
const socket = new WebSocket("ws://127.0.0.1:{socket_port}/");
socket.onerror = () => events.push("error");
socket.onclose = e => {{
  events.push("close " + e.code + " at " + performance.now());
  document.getElementById("out").textContent = events.join(", ");
}};
</script>"#
        )
        .into_boxed_str(),
    );
    let port = serve(HashMap::from([("/", page)]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let dir = std::env::temp_dir().join(format!("catpaw-har-ws-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let har = dir.join("run.har");
    let out = |page: &mut catpaw_engine::Page| {
        page.eval("document.getElementById('out').textContent")
            .unwrap()
    };

    let recorded = with_page(
        url.clone(),
        options(Recording::Record(har.clone())),
        move |page| {
            page.net().client().save_recording().unwrap();
            out(page)
        },
    )
    .unwrap();
    assert!(recorded.contains("close 1006"), "{recorded}");
    let tried = connections.load(Ordering::SeqCst);
    assert_eq!(tried, 1, "the live run connects");

    let replay = || {
        with_page(
            url.clone(),
            options(Recording::Replay {
                path: har.clone(),
                misses: Misses::Fail,
            }),
            move |page| {
                // The socket is refused as it is made, not when an answer
                // comes back from the network task.
                let refusal = page
                    .net()
                    .requests()
                    .into_iter()
                    .find(|r| r.url.scheme() == "ws")
                    .and_then(|r| r.error);
                (out(page), refusal)
            },
        )
        .unwrap()
    };
    let first = replay();
    let second = replay();
    // No time passes while the socket fails: page time shows only the
    // clock's reads.
    assert_eq!(first.0, "error, close 1006 at 0.02");
    assert_eq!(
        first.1.as_deref(),
        Some("WebSockets are refused while replaying a recording")
    );
    assert_eq!(second, first);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        tried,
        "a replay opens no connection"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn every_document_frame_and_worker_draws_random_numbers_of_its_own() {
    // Two frames with the same URL, and two workers running the same
    // script, start between the top document's draws: nobody's numbers
    // repeat anybody's.
    let html = r#"<!doctype html>
<script>
  const draw = () => [Math.random(), crypto.randomUUID(), crypto.getRandomValues(new Uint32Array(1))[0]];
  window.log = { before: draw(), frames: [], workers: [] };
  const finish = () => {
    if (log.frames.length === 2 && log.workers.length === 2 && !log.after) log.after = draw();
  };
  window.addEventListener('message', e => { log.frames.push(e.data); finish(); });
  const source = 'postMessage([Math.random(), crypto.randomUUID(), crypto.getRandomValues(new Uint32Array(1))[0]])';
  const script = URL.createObjectURL(new Blob([source], { type: 'text/javascript' }));
  for (let i = 0; i < 2; i++) {
    new Worker(script).onmessage = e => { log.workers.push(e.data); finish(); };
  }
</script>
<iframe srcdoc="<script>parent.postMessage([Math.random(), crypto.randomUUID(), crypto.getRandomValues(new Uint32Array(1))[0]], '*')</script>"></iframe>
<iframe srcdoc="<script>parent.postMessage([Math.random(), crypto.randomUUID(), crypto.getRandomValues(new Uint32Array(1))[0]], '*')</script>"></iframe>"#;
    let run = || {
        let options = PageOptions {
            page: PageConfig {
                random_seed: Some(7),
                ..PageConfig::default()
            },
            limits: LoopLimits {
                wall: Duration::from_secs(10),
                ..LoopLimits::default()
            },
            ..PageOptions::default()
        };
        with_html(
            Url::parse("https://parent.test/page").unwrap(),
            html.to_string(),
            options,
            |page| page.eval("JSON.stringify(log)").unwrap(),
        )
        .unwrap()
    };
    let log = run();
    assert!(log.contains("\"after\""), "{log}");
    let values: Vec<&str> = log
        .split(['[', ']', ',', '{', '}', ':'])
        .map(|v| v.trim_matches('"'))
        .filter(|v| !v.is_empty() && !matches!(*v, "before" | "frames" | "workers" | "after"))
        .collect();
    assert_eq!(values.len(), 18, "{log}");
    let mut distinct = values.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), values.len(), "a value repeats: {log}");
    // And a seeded run gives the same numbers every time.
    assert_eq!(run(), log);
}
