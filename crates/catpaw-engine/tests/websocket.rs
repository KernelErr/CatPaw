#![allow(clippy::result_large_err)]
//! WebSockets against a local echo server.

use std::net::TcpListener;

use catpaw_engine::{LoopLimits, PageOptions, SettlePolicy, with_html};
use catpaw_net::NetConfig;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use url::Url;

/// An echo server: text and binary come back; "close-me" closes with
/// 4001; the first offered subprotocol is accepted; the Origin header
/// is echoed as a first message.
fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut origin = String::new();
                    let callback = |req: &Request, mut res: Response| {
                        origin = req
                            .headers()
                            .get("origin")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("none")
                            .to_string();
                        if let Some(p) = req.headers().get("sec-websocket-protocol") {
                            let first = p.to_str().unwrap().split(',').next().unwrap().trim();
                            res.headers_mut()
                                .insert("sec-websocket-protocol", first.parse().unwrap());
                        }
                        Ok(res)
                    };
                    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await
                    else {
                        return;
                    };
                    let _ = ws
                        .send(Message::Text(format!("origin={origin}").into()))
                        .await;
                    while let Some(Ok(message)) = ws.next().await {
                        match message {
                            Message::Text(text) if text.as_str() == "close-me" => {
                                let _ = ws
                                    .send(Message::Close(Some(
                                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                            code: 4001.into(),
                                            reason: "bye".into(),
                                        },
                                    )))
                                    .await;
                            }
                            Message::Text(text) => {
                                let _ = ws.send(Message::Text(text)).await;
                            }
                            Message::Binary(bytes) => {
                                let _ = ws.send(Message::Binary(bytes)).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
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

fn run(html: String, f: impl FnOnce(&mut catpaw_engine::Page) + Send + 'static) {
    with_html(Url::parse("https://app.test/").unwrap(), html, options(), f).expect("the page runs");
}

#[test]
fn messages_go_both_ways_and_the_page_closes() {
    let port = echo_server();
    let html = format!(
        r#"<!doctype html>
<script>
  window.log = [];
  const ws = new WebSocket('ws://127.0.0.1:{port}/echo?x=1', ['chat', 'other']);
  log.push('state ' + ws.readyState + ' url ' + ws.url);
  try {{ ws.send('early'); }} catch (e) {{ log.push(e.name); }}
  ws.binaryType = 'arraybuffer';
  ws.onopen = () => {{
    log.push('open proto=' + ws.protocol + ' state=' + ws.readyState);
    ws.send('hello');
    ws.send(new Uint8Array([1, 2, 3]));
  }};
  ws.onmessage = e => {{
    if (typeof e.data === 'string') {{
      log.push('text ' + e.data + ' origin=' + e.origin);
      if (e.data === 'hello') ws.send(new Blob(['blob!']));
    }} else {{
      const bytes = Array.from(new Uint8Array(e.data));
      log.push('binary ' + bytes.join(','));
      if (bytes.length === 5) ws.close(4000, 'done');
    }}
  }};
  ws.onerror = () => log.push('error');
  ws.onclose = e => log.push('close ' + e.code + ' ' + e.reason + ' clean=' + e.wasClean + ' state=' + ws.readyState);
</script>"#
    );
    run(html, move |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        assert_eq!(
            page.eval("log.join(' | ')").unwrap(),
            "state 0 url ws://127.0.0.1:PORT/echo?x=1 | InvalidStateError | open proto=chat state=1 | text origin=https://app.test origin=ws://127.0.0.1:PORT | text hello origin=ws://127.0.0.1:PORT | binary 1,2,3 | binary 98,108,111,98,33 | close 4000 done clean=true state=3"
                .replace("PORT", &port.to_string())
        );
    });
}

#[test]
fn the_server_can_close_and_failures_are_reported() {
    let port = echo_server();
    let html = format!(
        r#"<!doctype html>
<script>
  window.log = [];
  const ws = new WebSocket('ws://127.0.0.1:{port}/');
  ws.onopen = () => ws.send('close-me');
  ws.onclose = e => log.push('server closed ' + e.code + ' ' + e.reason + ' ' + e.wasClean);
  const dead = new WebSocket('ws://127.0.0.1:1/');
  dead.onerror = () => log.push('dead error');
  dead.onclose = e => log.push('dead close ' + e.code + ' ' + e.wasClean);
  try {{ new WebSocket('ftp://x/'); }} catch (e) {{ log.push(e.name); }}
  try {{ new WebSocket('ws://x/#frag'); }} catch (e) {{ log.push(e.name + '2'); }}
  try {{ new WebSocket('ws://x/', ['a', 'A']); }} catch (e) {{ log.push(e.name + '3'); }}
  try {{ ws.close(1234); }} catch (e) {{ log.push(e.name + '4'); }}
</script>"#
    );
    run(html, move |page| {
        assert!(page.is_settled(), "{:?}", page.report());
        let log = page.eval("log.join(' | ')").unwrap();
        assert!(
            log.starts_with("SyntaxError | SyntaxError2 | SyntaxError3 | InvalidAccessError4 | "),
            "{log}"
        );
        assert!(log.contains("dead error | dead close 1006 false"), "{log}");
        assert!(log.contains("server closed 4001 bye true"), "{log}");
    });
}

/// Takes connections and never answers them; returns the port.
fn silent_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    port
}

#[test]
fn a_handshake_that_hangs_stops_holding_the_page_up() {
    let port = silent_server();
    let html = format!(
        "<!doctype html><title>start</title><script>const ws = new WebSocket('ws://127.0.0.1:{port}/'); document.title = 'waiting';</script>"
    );
    let mut options = options();
    options.limits.settle = Some(Default::default());
    let started = std::time::Instant::now();
    with_html(
        Url::parse("https://app.test/").unwrap(),
        html,
        options,
        |page| {
            assert!(page.is_settled(), "{:?}", page.report());
            assert_eq!(page.eval("ws.readyState").unwrap(), "0", "still connecting");
        },
    )
    .expect("the page runs");
    assert!(started.elapsed() < std::time::Duration::from_secs(9));
}

#[test]
fn a_handshake_with_an_ignored_host_does_not_hold_the_page_up() {
    let port = silent_server();
    let html = format!(
        "<!doctype html><script>const ws = new WebSocket('ws://127.0.0.1:{port}/');</script>"
    );
    let mut options = options();
    // Waiting for the handshake would outlast the run's wall time.
    options.limits.settle = Some(SettlePolicy {
        asset_timeout: std::time::Duration::from_secs(60),
        ignore_hosts: vec!["127.0.0.1".to_string()],
        ..SettlePolicy::default()
    });
    with_html(
        Url::parse("https://app.test/").unwrap(),
        html,
        options,
        |page| {
            assert!(page.is_settled(), "{:?}", page.report());
            assert_eq!(page.eval("ws.readyState").unwrap(), "0", "still connecting");
        },
    )
    .expect("the page runs");
}
