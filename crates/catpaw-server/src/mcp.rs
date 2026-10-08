//! MCP (Model Context Protocol) over stdio: newline-delimited JSON-RPC on
//! stdin and stdout. Stdout carries nothing but protocol messages; logs go
//! to stderr.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use base64::Engine as _;
use catpaw_protocol::{INSTRUCTIONS, OPTIONAL_TOOLS, PROTOCOL_VERSIONS, TOOLS, ToolDef};
use serde_json::{Value, json};

use crate::jsonrpc::{self, Incoming};
use crate::output::ToolOutput;
use crate::session::{Approval, Host, NoHost, Session, SessionConfig};

/// The MCP side of a session: answers requests one at a time.
pub struct McpServer {
    session: Session,
    /// The client can ask its user to approve things (MCP elicitation).
    elicits: bool,
}

impl McpServer {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            elicits: false,
        }
    }

    /// The session the server speaks for.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The tools this server lists: the standard ones and the optional
    /// ones the session offers.
    fn tools(&self) -> impl Iterator<Item = &'static ToolDef> + '_ {
        let optional = self.session.optional_tools();
        TOOLS.iter().chain(
            OPTIONAL_TOOLS
                .iter()
                .filter(move |t| optional.iter().any(|o| o == t.name)),
        )
    }

    /// Handles one line from the client; returns the line to answer with,
    /// if any.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        self.handle_line_with(line, &mut NoHost)
    }

    /// [`McpServer::handle_line`], with a way to ask the client's user
    /// while a call runs.
    pub fn handle_line_with(&mut self, line: &str, host: &mut dyn Host) -> Option<String> {
        let message = match jsonrpc::parse(line) {
            Ok(message) => message,
            Err(response) => return Some(response),
        };
        match message {
            Incoming::Request { id, method, params } => {
                Some(self.request(id, &method, params, host))
            }
            // `notifications/initialized`, `notifications/cancelled` for a
            // call that is not waiting (a call that waits notices its own),
            // and answers to requests we never send.
            Incoming::Notification { .. } | Incoming::Response { .. } => None,
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value, host: &mut dyn Host) -> String {
        match method {
            "initialize" => {
                self.elicits = params["capabilities"]["elicitation"].is_object();
                jsonrpc::result(id, initialize(&params))
            }
            "ping" => jsonrpc::result(id, json!({})),
            "tools/list" => {
                let tools: Vec<Value> = self.tools().map(|t| t.to_json()).collect();
                jsonrpc::result(id, json!({ "tools": tools }))
            }
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return jsonrpc::error(
                        id,
                        jsonrpc::INVALID_PARAMS,
                        "tools/call needs a tool name",
                    );
                };
                if !self.tools().any(|t| t.name == name) {
                    return jsonrpc::error(
                        id,
                        jsonrpc::INVALID_PARAMS,
                        &format!("Unknown tool: {name}"),
                    );
                }
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                // A client that cannot elicit still gets pings answered
                // while a call waits.
                let output = if self.elicits {
                    self.session.call_tool_with(name, arguments, host)
                } else {
                    self.session
                        .call_tool_with(name, arguments, &mut NoElicit(host))
                };
                jsonrpc::result(id, tool_result(output))
            }
            _ => jsonrpc::error(id, jsonrpc::METHOD_NOT_FOUND, "Method not found"),
        }
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "catpaw",
            "title": "CatPaw",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

/// A tool's output as an MCP `CallToolResult`.
fn tool_result(output: ToolOutput) -> Value {
    let mut content = vec![json!({ "type": "text", "text": output.text })];
    if let Some(png) = output.image {
        content.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(png),
            "mimeType": "image/png",
        }));
    }
    let mut result = json!({ "content": content });
    if output.is_error {
        result["isError"] = json!(true);
    }
    result
}

/// Serves MCP on stdin and stdout until stdin closes.
pub fn serve_stdio(config: SessionConfig) -> std::io::Result<()> {
    serve_stdio_with(config, |_| {})
}

/// [`serve_stdio`], calling `on_exit` with the session once stdin closes
/// (to save its cookies and storage, say).
pub fn serve_stdio_with(
    config: SessionConfig,
    on_exit: impl FnOnce(&Session),
) -> std::io::Result<()> {
    let session = Session::new(config).map_err(std::io::Error::other)?;
    let mut server = McpServer::new(session);
    // Lines are read on a thread of their own, so that a call can ask the
    // client (elicitation) and read the answer while it runs.
    let (lines_tx, lines_rx) = mpsc::channel::<Line>();
    let stdin_tx = lines_tx.clone();
    std::thread::Builder::new()
        .name("catpaw-mcp-stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match stdin.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => match std::str::from_utf8(&buf) {
                        Ok(line) => {
                            let line = line.trim_end().to_string();
                            if stdin_tx.send(Line::Message(line)).is_err() {
                                break;
                            }
                        }
                        // A line that is not UTF-8 is answered with a parse
                        // error; the session goes on.
                        Err(_) => {
                            let error = jsonrpc::error(
                                Value::Null,
                                jsonrpc::PARSE_ERROR,
                                "the message is not UTF-8",
                            );
                            let _ = write_line(&error);
                        }
                    },
                }
            }
            let _ = stdin_tx.send(Line::End);
        })?;
    let stopping = Arc::new(AtomicBool::new(false));
    stop_on_signal(lines_tx, stopping.clone());
    let outcome = server.serve_lines(&mut Stdio { lines: lines_rx }, &stopping);
    // Whatever ended the session, what it keeps (recordings, cookies,
    // storage, the profile) is written.
    on_exit(&server.session);
    outcome
}

/// Lines of JSON-RPC to and from a client: stdin and stdout here; any
/// transport of whole messages serves the same way.
pub(crate) trait LineTransport {
    /// Sends one message.
    fn send(&mut self, line: &str) -> std::io::Result<()>;
    /// The next message, waiting up to `wait` (however long, when `None`).
    fn recv(&mut self, wait: Option<Duration>) -> Received;
}

/// What waiting for a message brought.
pub(crate) enum Received {
    Line(String),
    TimedOut,
    /// The client is gone, or the server was asked to stop.
    Ended,
}

impl McpServer {
    /// Answers messages from `transport` until it ends or `stopping` is
    /// set.
    pub(crate) fn serve_lines(
        &mut self,
        transport: &mut dyn LineTransport,
        stopping: &AtomicBool,
    ) -> std::io::Result<()> {
        // Messages that came while a call waited for an answer of the
        // client's.
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut asked = 0;
        loop {
            if stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            let line = match queue.pop_front() {
                Some(line) => line,
                None => match transport.recv(None) {
                    Received::Line(line) => line,
                    Received::TimedOut => continue,
                    Received::Ended => return Ok(()),
                },
            };
            if line.trim().is_empty() {
                continue;
            }
            let current = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|v| v.get("id").cloned());
            let mut host = LineHost {
                transport: &mut *transport,
                queue: &mut queue,
                asked: &mut asked,
                current,
                cancelled: false,
                ended: false,
            };
            let reply = self.handle_line_with(&line, &mut host);
            let (cancelled, ended) = (host.cancelled, host.ended);
            // A cancelled request is not answered (the client stopped
            // waiting for it).
            if let Some(reply) = reply.filter(|_| !cancelled) {
                transport.send(&reply)?;
            }
            if ended {
                return Ok(());
            }
        }
    }
}

/// What the reading thread hands the loop.
enum Line {
    Message(String),
    /// Stdin closed, or the process was asked to stop.
    End,
}

/// Stdin, read on a thread of its own, and stdout.
struct Stdio {
    lines: mpsc::Receiver<Line>,
}

impl LineTransport for Stdio {
    fn send(&mut self, line: &str) -> std::io::Result<()> {
        write_line(line)
    }

    fn recv(&mut self, wait: Option<Duration>) -> Received {
        let line = match wait {
            Some(wait) => match self.lines.recv_timeout(wait) {
                Err(mpsc::RecvTimeoutError::Timeout) => return Received::TimedOut,
                other => other.ok(),
            },
            None => self.lines.recv().ok(),
        };
        match line {
            Some(Line::Message(line)) => Received::Line(line),
            Some(Line::End) | None => Received::Ended,
        }
    }
}

/// Ends the session (cleanly: what it keeps is written) on SIGINT or
/// SIGTERM, once the call under way returns; a second signal ends the
/// process at once.
fn stop_on_signal(lines: mpsc::Sender<Line>, stopping: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("catpaw-signals".to_string())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{SignalKind, signal};
                    let (Ok(mut term), Ok(mut int)) = (
                        signal(SignalKind::terminate()),
                        signal(SignalKind::interrupt()),
                    ) else {
                        return;
                    };
                    for first in [true, false] {
                        tokio::select! {
                            _ = term.recv() => {}
                            _ = int.recv() => {}
                        }
                        if first {
                            stopping.store(true, Ordering::SeqCst);
                            let _ = lines.send(Line::End);
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                    stopping.store(true, Ordering::SeqCst);
                    let _ = lines.send(Line::End);
                    let _ = tokio::signal::ctrl_c().await;
                }
                std::process::exit(130);
            });
        });
}

fn write_line(line: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// How long an `elicitation/create` waits for the user's answer before
/// the approval page takes over.
const ELICIT_WAIT: Duration = Duration::from_secs(300);

/// The client on the other end of a line transport: asks its user with
/// `elicitation/create`, and while a call waits reads the transport for
/// answers, answering pings at once, noticing the call's cancellation and
/// keeping other messages for later.
struct LineHost<'a> {
    transport: &'a mut dyn LineTransport,
    queue: &'a mut VecDeque<String>,
    asked: &'a mut u64,
    /// The id of the request being answered (for cancellations).
    current: Option<Value>,
    /// The client cancelled that request.
    cancelled: bool,
    /// Stdin closed or a signal came while the call waited.
    ended: bool,
}

/// What a line read while waiting meant.
enum Heard {
    /// The answer waited for.
    Answer(Value),
    /// The call was cancelled.
    Cancelled,
    /// Nothing of interest (answered or kept for later).
    Other,
}

impl LineHost<'_> {
    /// Reads one line, waiting up to `wait`; `None` when nothing came or
    /// the input ended.
    fn hear(&mut self, wait: Duration, answer_to: Option<&Value>) -> Option<Heard> {
        let line = match self.transport.recv(Some(wait)) {
            Received::Line(line) => line,
            Received::Ended => {
                self.ended = true;
                return None;
            }
            Received::TimedOut => return None,
        };
        Some(match jsonrpc::parse(&line) {
            Ok(Incoming::Response { id, body }) if Some(&id) == answer_to => Heard::Answer(body),
            Ok(Incoming::Request { id, method, .. }) if method == "ping" => {
                let _ = self.transport.send(&jsonrpc::result(id, json!({})));
                Heard::Other
            }
            Ok(Incoming::Notification { method, params })
                if method == "notifications/cancelled"
                    && self.current.is_some()
                    && params.get("requestId") == self.current.as_ref() =>
            {
                self.cancelled = true;
                Heard::Cancelled
            }
            _ => {
                self.queue.push_back(line);
                Heard::Other
            }
        })
    }
}

impl Host for LineHost<'_> {
    fn approve(&mut self, message: &str) -> Approval {
        *self.asked += 1;
        let id = Value::String(format!("catpaw-ask-{}", self.asked));
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "elicitation/create",
            "params": {
                "message": message,
                "requestedSchema": {
                    "type": "object",
                    "properties": {
                        "approve": {
                            "type": "boolean",
                            "title": "Approve",
                            "description": "Let the agent go ahead",
                        }
                    },
                    "required": ["approve"],
                },
            },
        });
        if self.transport.send(&request.to_string()).is_err() {
            return Approval::Unavailable;
        }
        let deadline = Instant::now() + ELICIT_WAIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.hear(left, Some(&id)) {
                Some(Heard::Answer(body)) => return elicited(&body),
                Some(Heard::Cancelled) => return Approval::Cancelled,
                Some(Heard::Other) => {}
                None if self.ended => return Approval::Unavailable,
                None => {}
            }
        }
        Approval::Unavailable
    }

    fn pause(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.hear(left, None) {
                Some(Heard::Cancelled) => return true,
                None if self.ended => return true,
                _ => {}
            }
        }
        false
    }
}

/// The user's answer in an `elicitation/create` response: approved only
/// when the user accepted with `approve: true`.
fn elicited(body: &Value) -> Approval {
    let Some(result) = body.get("result") else {
        return Approval::Unavailable;
    };
    match result["action"].as_str() {
        Some("accept") if result["content"]["approve"] == json!(true) => Approval::Approved,
        Some("accept") | Some("decline") => Approval::Declined,
        Some("cancel") => Approval::Cancelled,
        _ => Approval::Unavailable,
    }
}

/// A host whose client cannot ask its user: never asked, still served.
struct NoElicit<'a>(&'a mut dyn Host);

impl Host for NoElicit<'_> {
    fn approve(&mut self, _message: &str) -> Approval {
        Approval::Unavailable
    }

    fn pause(&mut self, wait: Duration) -> bool {
        self.0.pause(wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Policy, Preset, SessionConfig};

    /// Lines a test client sends, and what the server sent back.
    struct Scripted {
        incoming: VecDeque<String>,
        sent: Vec<Value>,
    }

    impl LineTransport for Scripted {
        fn send(&mut self, line: &str) -> std::io::Result<()> {
            self.sent.push(serde_json::from_str(line).unwrap());
            Ok(())
        }

        fn recv(&mut self, _wait: Option<Duration>) -> Received {
            match self.incoming.pop_front() {
                Some(line) => Received::Line(line),
                None => Received::Ended,
            }
        }
    }

    fn call(id: u32, name: &str, arguments: Value) -> String {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}}).to_string()
    }

    fn run(answer: Value) -> Vec<Value> {
        let config = SessionConfig {
            policy: Policy {
                preset: Preset::Strict,
                ..Policy::default()
            },
            ..SessionConfig::default()
        };
        let mut server = McpServer::new(Session::new(config).unwrap());
        let lines = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {"elicitation": {}}, "clientInfo": {"name": "test", "version": "0"}}}).to_string(),
            call(2, "navigate", json!({"url": "about:blank"})),
            call(3, "evaluate", json!({"script": "1 + 1"})),
            // While the call waits for the user: a ping is answered at
            // once, another request waits its turn.
            json!({"jsonrpc": "2.0", "id": 99, "method": "ping"}).to_string(),
            json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}).to_string(),
            json!({"jsonrpc": "2.0", "id": "catpaw-ask-1", "result": answer}).to_string(),
        ];
        let mut transport = Scripted {
            incoming: lines.into_iter().collect(),
            sent: Vec::new(),
        };
        server
            .serve_lines(&mut transport, &AtomicBool::new(false))
            .unwrap();
        transport.sent
    }

    fn text(reply: &Value) -> &str {
        reply["result"]["content"][0]["text"].as_str().unwrap_or("")
    }

    #[test]
    fn a_waiting_call_answers_pings_and_keeps_other_requests_for_later() {
        let sent = run(json!({"action": "decline"}));
        let order: Vec<Value> = sent
            .iter()
            .map(|m| m.get("id").cloned().unwrap_or(Value::Null))
            .collect();
        assert_eq!(
            order,
            [
                json!(1),
                json!(2),
                json!("catpaw-ask-1"),
                json!(99),
                json!(3),
                json!(4)
            ]
        );
        assert_eq!(sent[2]["method"], "elicitation/create");
        assert!(
            text(&sent[4]).starts_with("blocked user: declined c1"),
            "{}",
            sent[4]
        );
        assert!(sent[5]["result"]["tools"].is_array());
    }

    #[test]
    fn a_cancelled_question_blocks_the_call() {
        let sent = run(json!({"action": "cancel"}));
        assert!(
            text(&sent[4]).starts_with("blocked user: cancelled c1"),
            "{}",
            sent[4]
        );
    }

    #[test]
    fn only_an_explicit_yes_approves() {
        let yes = json!({"action": "accept", "content": {"approve": true}});
        assert_eq!(elicited(&json!({"result": yes})), Approval::Approved);
        let no = json!({"action": "accept", "content": {"approve": false}});
        assert_eq!(elicited(&json!({"result": no})), Approval::Declined);
        let error = json!({"error": {"code": -32601, "message": "unknown method"}});
        assert_eq!(elicited(&error), Approval::Unavailable);
        assert_eq!(
            elicited(&json!({"result": {"action": "?"}})),
            Approval::Unavailable
        );
    }
}
