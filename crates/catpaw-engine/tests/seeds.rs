//! Seeded runs: tabs, frames and workers that show the same thing draw
//! numbers of their own, and the same run draws the same numbers again.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use catpaw_engine::{
    LoopLimits, PAGE_STACK_SIZE, Page, PageConfig, PageOptions, SharedNet, with_html, with_page,
};
use catpaw_net::NetConfig;
use url::Url;

/// What a realm draws: `Math.random` and `crypto` numbers.
const DRAW: &str = "[Math.random(), crypto.randomUUID()].join(' ')";

fn options() -> PageOptions {
    PageOptions {
        net: NetConfig {
            allow_private_network: true,
            ..NetConfig::default()
        },
        page: PageConfig {
            random_seed: Some(5),
            ..PageConfig::default()
        },
        limits: LoopLimits {
            wall: Duration::from_secs(10),
            ..LoopLimits::default()
        },
        ..PageOptions::default()
    }
}

/// Runs `f` on a thread with a page thread's stack.
fn on_page_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(PAGE_STACK_SIZE)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn tabs_on_one_url_draw_numbers_of_their_own() {
    let draws = || {
        on_page_thread(|| {
            let net = SharedNet::new(options().net).unwrap();
            let mut tabs = [
                Page::blank(&options(), &net).unwrap(),
                Page::blank(&options(), &net).unwrap(),
            ];
            tabs.each_mut().map(|tab| tab.eval(DRAW).unwrap())
        })
    };
    let first = draws();
    assert_ne!(first[0], first[1], "the two tabs drew the same");
    assert_eq!(draws(), first, "the same tabs drew differently");
}

/// Each frame starts a worker on the same script, under the same name, and
/// passes on what it drew.
const FRAMES: &str = r#"<!doctype html>
<script>
  window.log = [];
  window.addEventListener('message', e => log.push(e.data));
</script>
<iframe srcdoc="<script>new Worker('data:text/javascript,postMessage(' + encodeURIComponent('[Math.random(), crypto.randomUUID()].join(&quot; &quot;)') + ')', { name: 'w' }).onmessage = e => parent.postMessage(e.data, '*');</script>"></iframe>
<iframe srcdoc="<script>new Worker('data:text/javascript,postMessage(' + encodeURIComponent('[Math.random(), crypto.randomUUID()].join(&quot; &quot;)') + ')', { name: 'w' }).onmessage = e => parent.postMessage(e.data, '*');</script>"></iframe>"#;

#[test]
fn the_same_worker_in_two_frames_draws_numbers_of_its_own() {
    let run = || {
        with_html(
            Url::parse("https://parent.test/page").unwrap(),
            FRAMES.to_string(),
            options(),
            |page| page.eval("log.join('|')").unwrap(),
        )
        .unwrap()
    };
    let drawn = run();
    let values: Vec<&str> = drawn.split('|').collect();
    assert_eq!(values.len(), 2, "{drawn}");
    assert_ne!(values[0], values[1], "the two workers drew the same");
    assert_eq!(run(), drawn);
}

/// Serves `pages` (path to HTML) one request per connection.
fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut data = Vec::new();
            let mut buf = vec![0u8; 8192];
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
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
                .unwrap_or("/");
            let body = pages.get(path).copied().unwrap_or("");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    port
}

#[test]
fn a_page_loaded_again_starts_a_worker_that_draws_numbers_of_its_own() {
    let port = serve(HashMap::from([(
        "/",
        "<!doctype html><script>window.drawn = ''; new Worker('data:text/javascript,postMessage(' + encodeURIComponent('[Math.random(), crypto.randomUUID()].join(\" \")') + ')').onmessage = e => { drawn = e.data; };</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let run = || {
        with_page(url.clone(), options(), |page| {
            let first = page.eval("drawn").unwrap();
            page.reload().unwrap();
            [first, page.eval("drawn").unwrap()]
        })
        .unwrap()
    };
    let drawn = run();
    assert!(!drawn[0].is_empty(), "the worker answered");
    assert_ne!(
        drawn[0], drawn[1],
        "the worker drew the same after a reload"
    );
    assert_eq!(run(), drawn);
}

#[test]
fn web_crypto_keys_come_from_the_seed() {
    // An AES key made and exported: the same seed makes the same key.
    let make = |seed: u64| {
        let mut options = options();
        options.page.random_seed = Some(seed);
        let html = "<!doctype html><script>crypto.subtle.generateKey({name: 'AES-GCM', length: 128}, true, ['encrypt']).then(k => crypto.subtle.exportKey('raw', k)).then(b => { window.key = Array.from(new Uint8Array(b)).join(','); });</script>";
        let url = Url::parse("https://keys.example/").unwrap();
        with_html(url, html.to_string(), options, |page| {
            page.eval("window.key").unwrap()
        })
        .unwrap()
    };
    let (one, again, other) = (make(7), make(7), make(8));
    assert_eq!(one.split(',').count(), 16, "{one}");
    assert_eq!(one, again);
    assert_ne!(one, other);
}
