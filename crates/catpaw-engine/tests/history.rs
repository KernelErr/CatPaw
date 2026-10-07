//! Session history across documents and `localStorage` across runs,
//! against a small local HTTP server.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;

use catpaw_engine::{LoopLimits, PageOptions, with_page};
use catpaw_net::NetConfig;
use url::Url;

/// Serves `pages` (path to HTML) one request per connection.
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
                .to_string();
            let (status, body) = match pages.get(path.as_str()) {
                Some(body) => ("200 OK", body.to_string()),
                None => ("404 Not Found", "<p>not found</p>".to_string()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    port
}

fn options(storage: HashMap<String, Vec<(String, String)>>) -> PageOptions {
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
        storage,
        ..PageOptions::default()
    }
}

#[test]
fn back_and_forward_load_the_session_entries() {
    let port = serve(HashMap::from([
        (
            "/a",
            "<!doctype html><title>A</title><a id=l href=/b>b</a><script>localStorage.visits = (Number(localStorage.visits || 0) + 1);</script>",
        ),
        (
            "/b",
            "<!doctype html><title>B</title><script>window.hl = history.length; location.href = '/c';</script>",
        ),
        (
            "/c",
            "<!doctype html><title>C</title><script>window.hl = history.length; history.pushState({}, '', '/c2');</script>",
        ),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/a")).unwrap();
    with_page(url.clone(), options(HashMap::new()), move |page| {
        // Clicking the link loads /b, whose script goes on to /c, which
        // pushes a same-document entry.
        page.click("#l").unwrap();
        assert_eq!(page.url().path(), "/c2");
        assert_eq!(
            page.eval("document.title + ' ' + history.length").unwrap(),
            "C 4"
        );
        let (session, index) = page.session_history();
        assert_eq!(
            session.iter().map(|u| u.path()).collect::<Vec<_>>(),
            ["/a", "/b", "/c"]
        );
        assert_eq!(index, 2);

        // Back within the document first (popstate), then across documents.
        page.eval("history.back()").unwrap();
        page.settle(&LoopLimits::default());
        assert_eq!(page.url().path(), "/c");
        page.eval("history.back()").unwrap();
        page.settle(&LoopLimits::default());
        page.follow_navigations().unwrap();
        // /b navigates away again on load, so going back to it lands on /c.
        assert_eq!(page.url().path(), "/c2");

        page.back().unwrap();
        assert_eq!(page.url().path(), "/c");
        page.traverse_history(-2).unwrap();
        assert_eq!(page.eval("document.title").unwrap(), "A");
        assert_eq!(page.session_history().1, 0);
        page.back().unwrap();
        assert_eq!(page.eval("document.title").unwrap(), "A");
        page.forward().unwrap();
        assert_eq!(page.url().path(), "/c2", "/b sends us on to /c again");
        assert_eq!(page.eval("history.length").unwrap(), "4");
        assert_eq!(
            page.storage_snapshot()[&format!("http://127.0.0.1:{port}")],
            vec![("visits".to_string(), "2".to_string())]
        );
    })
    .unwrap();
}

#[test]
fn local_storage_survives_runs_through_the_snapshot() {
    let port = serve(HashMap::from([(
        "/",
        "<!doctype html><script>localStorage.n = Number(localStorage.n || 0) + 1; window.seen = localStorage.getItem('greeting');</script>",
    )]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    let origin = format!("http://127.0.0.1:{port}");
    let seed = HashMap::from([(
        origin.clone(),
        vec![("greeting".to_string(), "hi".to_string())],
    )]);
    let after_first = with_page(url.clone(), options(seed), |page| {
        assert_eq!(page.eval("seen + ' ' + localStorage.n").unwrap(), "hi 1");
        page.storage_snapshot()
    })
    .unwrap();
    assert_eq!(
        after_first[&origin],
        vec![
            ("greeting".to_string(), "hi".to_string()),
            ("n".to_string(), "1".to_string())
        ]
    );
    with_page(url, options(after_first), |page| {
        assert_eq!(
            page.eval("localStorage.n + ' ' + localStorage.length")
                .unwrap(),
            "2 2"
        );
    })
    .unwrap();
}

#[test]
fn a_submit_listener_may_push_a_history_entry() {
    // What single-page apps do on login: take the submission over and
    // route with `pushState`, which writes the document URL while the
    // click that submitted is still being handled.
    let page = r#"<!doctype html><form><input name=u><button>Login</button></form>
<script>
document.querySelector("form").addEventListener("submit", e => {
  e.preventDefault();
  history.pushState({}, "", "/inventory.html");
  document.body.append("routed");
});
</script>"#;
    let port = serve(HashMap::from([("/", page)]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(HashMap::new()), |page| {
        page.click("button").unwrap();
        assert_eq!(page.url().path(), "/inventory.html");
        let text = page.eval("document.body.textContent").unwrap();
        assert!(text.contains("routed"), "{text}");
    })
    .unwrap();
}
