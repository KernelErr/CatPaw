//! Recording a page's traffic and replaying it: the same page, offline,
//! with the same random numbers and clock.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use catpaw_engine::{LoopLimits, PageConfig, PageOptions, with_page};
use catpaw_net::{Misses, NetConfig, Recording};
use url::Url;

fn serve(pages: HashMap<&'static str, &'static str>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
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
    assert_eq!(first, recorded);
    assert_eq!(second, recorded);
    let _ = std::fs::remove_dir_all(dir);
}
