//! Popups: `window.open()` after user activation opens a page of its own,
//! which talks to its opener and can close itself.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;

use catpaw_engine::{LoopLimits, PageOptions, with_page};
use catpaw_net::NetConfig;
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

#[test]
fn popups_need_activation_and_talk_to_their_opener() {
    let port = serve(HashMap::from([
        (
            "/",
            r#"<!doctype html><title>Main</title>
<button id="go" onclick="window.popup = window.open('/popup'); log.push('opened ' + (popup !== null) + ' ' + popup.closed)">open</button>
<script>
  window.log = [];
  window.blocked = window.open('/popup');
  addEventListener('message', e => { log.push('msg ' + e.data + ' from popup=' + (e.source === popup)); });
</script>"#,
        ),
        (
            "/popup",
            r#"<!doctype html><title>Popup</title>
<script>
  window.hasOpener = opener !== null && opener === top.opener;
  window.isTop = top === window && parent === window;
  opener.postMessage('hello', '*');
  window.close();
</script>"#,
        ),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(), |page| {
        assert_eq!(page.eval("blocked").unwrap(), "null", "no activation yet");
        assert!(
            page.state()
                .console_messages()
                .iter()
                .any(|m| m.text.contains("blocked"))
        );
        page.click("#go").unwrap();
        assert_eq!(
            page.eval("log.join(' | ')").unwrap(),
            "opened true false | msg hello from popup=true"
        );
        // The popup closed itself after speaking.
        assert_eq!(page.eval("popup.closed").unwrap(), "true");
        assert_eq!(page.frames().len(), 1);
        assert!(page.select_latest_popup().is_err());
    })
    .unwrap();
}

#[test]
fn an_open_popup_can_be_addressed() {
    let port = serve(HashMap::from([
        (
            "/",
            r#"<!doctype html><title>Main</title>
<button id="go" onclick="window.popup = window.open('/login')">open</button>
<script>window.got = null; addEventListener('message', e => got = e.data);</script>"#,
        ),
        (
            "/login",
            r#"<!doctype html><title>Login</title>
<input id="user"><button id="send" onclick="opener.postMessage('user=' + document.getElementById('user').value, '*')">send</button>
<script>window.isTop = top === window;</script>"#,
        ),
    ]));
    let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
    with_page(url, options(), |page| {
        page.click("#go").unwrap();
        let frames = page.frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[1].popup && frames[1].depth == 0);
        assert_eq!(frames[1].url.path(), "/login");
        page.select_latest_popup().unwrap();
        assert_eq!(
            page.eval("document.title + ' ' + isTop + ' ' + (opener !== null)")
                .unwrap(),
            "Login true true"
        );
        page.fill("#user", "paw").unwrap();
        page.click("#send").unwrap();
        page.select_top_frame();
        assert_eq!(page.eval("got").unwrap(), "user=paw");
        assert_eq!(page.eval("popup.closed").unwrap(), "false");
    })
    .unwrap();
}
