//! `catpaw mcp --stdio` as a host runs it: a separate process speaking
//! JSON-RPC lines, asking the client to approve a form submission with
//! `elicitation/create` while the call waits, noticing a cancelled call,
//! answering lines that are not UTF-8, and saving its profile when told
//! to stop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

const FORM: &str = "<!doctype html><title>Order</title><form method=post action=/placed><input name=item value=socks><button>Place order</button></form>";

type Log = Arc<Mutex<Vec<String>>>;

/// Reads one request: its head and its body.
fn read_request(stream: &mut TcpStream) -> (String, String) {
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
                let body = String::from_utf8_lossy(&data[end + 4..end + 4 + length]).into_owned();
                return (head, body);
            }
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return (String::from_utf8_lossy(&data).into_owned(), String::new()),
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
}

/// Serves the form, setting a cookie; answers a POST with `placed
/// <body>` and logs it.
fn serve() -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (head, body) = read_request(&mut stream);
            let page = if head.starts_with("POST") {
                seen.lock().unwrap().push(body.clone());
                format!("<!doctype html><title>Placed</title><p>placed {body}</p>")
            } else {
                FORM.to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: visited=yes; Max-Age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, log)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start() -> Self {
        Self::start_with(&[])
    }

    fn start_with(extra: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_catpaw"))
            .args([
                "mcp",
                "--stdio",
                "--allow-private-network",
                "--approval-port",
                "0",
            ])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Initializes as a client that can elicit (or not), then opens the
    /// form and clicks its button; the next line is what that leads to.
    fn order(&mut self, port: u16, elicits: bool) {
        let capabilities = if elicits {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        self.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": {"name": "test", "version": "0"}
            }}),
        );
        assert_eq!(self.next()["id"], 1);
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        self.send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "navigate", "arguments": {"url": format!("http://127.0.0.1:{port}/")}
            }}),
        );
        assert_eq!(self.next()["id"], 2);
        self.send(
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "click", "arguments": {"target": "button \"Place order\""}
            }}),
        );
    }

    fn next(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(60))
            .expect("the server answers");
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(reply: &Value) -> &str {
    reply["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn a_host_approves_through_elicitation() {
    let (port, log) = serve();
    let mut server = Server::start();
    server.order(port, true);
    let ask = server.next();
    assert_eq!(ask["method"], "elicitation/create", "{ask}");
    let message = ask["params"]["message"].as_str().unwrap();
    assert!(
        message.contains("would submit → POST") && message.contains("item=socks"),
        "{message}"
    );
    // A ping while the call waits is answered at once.
    server.send(json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}));
    assert_eq!(server.next()["id"], 4);
    server.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": {
        "action": "accept", "content": {"approve": true}
    }}));
    let done = server.next();
    assert_eq!(done["id"], 3, "{done}");
    let text = text(&done);
    assert!(text.contains("(confirmed c1) → "), "{text}");
    assert!(text.contains("placed item=socks"), "{text}");
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn only_an_explicit_yes_approves() {
    let (port, log) = serve();
    let mut server = Server::start();
    server.order(port, true);
    let ask = server.next();
    assert_eq!(ask["method"], "elicitation/create", "{ask}");
    // Accepted, but without saying yes.
    server.send(json!({"jsonrpc": "2.0", "id": ask["id"], "result": {
        "action": "accept", "content": {}
    }}));
    let done = server.next();
    assert_eq!(done["id"], 3, "{done}");
    assert_eq!(text(&done), "blocked user: declined c1");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn a_cancelled_call_stops_waiting_and_sends_nothing() {
    let (port, log) = serve();
    let mut server = Server::start();
    server.order(port, true);
    let ask = server.next();
    assert_eq!(ask["method"], "elicitation/create", "{ask}");
    server.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {
            "requestId": 3, "reason": "the user moved on"
        }}),
    );
    // The cancelled call is not answered; the next request is.
    server.send(json!({"jsonrpc": "2.0", "id": 4, "method": "ping"}));
    assert_eq!(server.next()["id"], 4);
    assert!(log.lock().unwrap().is_empty());
    // What the click held was dropped: the page is the form's still.
    server.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "snapshot", "arguments": {}
        }}),
    );
    let snapshot = server.next();
    assert!(text(&snapshot).contains("title=\"Order\""), "{snapshot}");
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn a_line_that_is_not_utf8_gets_a_parse_error() {
    let mut server = Server::start();
    server
        .stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\xffng\"}\n")
        .unwrap();
    server.stdin.flush().unwrap();
    let error = server.next();
    assert_eq!(error["error"]["code"], -32700, "{error}");
    assert_eq!(error["id"], Value::Null);
    server.send(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(server.next()["id"], 2, "the session goes on");
}

#[cfg(unix)]
#[test]
fn a_terminated_server_saves_its_profile() {
    let (port, _log) = serve();
    let profile: std::path::PathBuf =
        std::env::temp_dir().join(format!("catpaw-stdio-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let mut server = Server::start_with(&["--profile", profile.to_str().unwrap()]);
    server.order(port, false);
    let asked = server.next();
    assert!(text(&asked).starts_with("needs_confirmation c1"), "{asked}");
    // A second session cannot take the profile while this one has it.
    let second = Command::new(env!("CARGO_BIN_EXE_catpaw"))
        .args(["mcp", "--stdio", "--profile", profile.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("in use by another session"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    std::fs::remove_file(profile.join("cookies.json")).unwrap();
    let pid = server.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(0), "a clean stop");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the server stops"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let cookies = std::fs::read_to_string(profile.join("cookies.json")).unwrap();
    assert!(cookies.contains("visited"), "{cookies}");
    let _ = std::fs::remove_dir_all(&profile);
}
