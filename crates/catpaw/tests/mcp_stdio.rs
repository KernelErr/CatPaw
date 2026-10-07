//! `catpaw mcp --stdio` as a host runs it: a separate process speaking
//! JSON-RPC lines, asking the client to approve a form submission with
//! `elicitation/create` while the call waits.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

const FORM: &str = "<!doctype html><title>Order</title><form method=post action=/placed><input name=item value=socks><button>Place order</button></form>";

/// Serves the form; answers a POST with `placed <body>`.
fn serve() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let page = if request.starts_with("POST") {
                let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
                format!("<!doctype html><title>Placed</title><p>placed {body}</p>")
            } else {
                FORM.to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_catpaw"))
            .args(["mcp", "--stdio", "--allow-private-network"])
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

#[test]
fn a_host_approves_through_elicitation() {
    let port = serve();
    let mut server = Server::start();
    server.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"elicitation": {}},
            "clientInfo": {"name": "test", "version": "0"}
        }}),
    );
    assert_eq!(server.next()["id"], 1);
    server.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    server.send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "navigate", "arguments": {"url": format!("http://127.0.0.1:{port}/")}
        }}),
    );
    assert_eq!(server.next()["id"], 2);
    server.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "click", "arguments": {"target": "button \"Place order\""}
        }}),
    );
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
    let text = done["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("(confirmed c1) → "), "{text}");
    assert!(text.contains("placed item=socks"), "{text}");
}
