//! MCP (Model Context Protocol) over stdio: newline-delimited JSON-RPC on
//! stdin and stdout. Stdout carries nothing but protocol messages; logs go
//! to stderr.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::Instant;

use base64::Engine as _;
use catpaw_protocol::{INSTRUCTIONS, OPTIONAL_TOOLS, PROTOCOL_VERSIONS, TOOLS, ToolDef};
use serde_json::{Value, json};

use crate::confirm::LIFETIME;
use crate::jsonrpc::{self, Incoming};
use crate::output::ToolOutput;
use crate::session::{Asker, NoAsker, Session, SessionConfig};

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

    /// The session, to change.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
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
        self.handle_line_with(line, &mut NoAsker)
    }

    /// [`McpServer::handle_line`], with a way to ask the client's user
    /// while a call runs.
    pub fn handle_line_with(&mut self, line: &str, asker: &mut dyn Asker) -> Option<String> {
        let message = match jsonrpc::parse(line) {
            Ok(message) => message,
            Err(response) => return Some(response),
        };
        match message {
            Incoming::Request { id, method, params } => {
                Some(self.request(id, &method, params, asker))
            }
            // `notifications/initialized`, `notifications/cancelled` (calls
            // run to completion), and answers to requests we never send.
            Incoming::Notification { .. } | Incoming::Response { .. } => None,
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value, asker: &mut dyn Asker) -> String {
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
                let output = if self.elicits {
                    self.session.call_tool_asking(name, arguments, asker)
                } else {
                    self.session.call_tool(name, arguments)
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
    let (lines_tx, lines_rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("catpaw-mcp-stdin".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        })?;
    // Lines that came while a call waited for an answer of the client's.
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut asked = 0;
    loop {
        let line = match queue.pop_front() {
            Some(line) => line,
            None => match lines_rx.recv() {
                Ok(line) => line,
                Err(_) => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let mut asker = StdioAsker {
            lines: &lines_rx,
            queue: &mut queue,
            asked: &mut asked,
        };
        if let Some(reply) = server.handle_line_with(&line, &mut asker) {
            write_line(&reply)?;
        }
    }
    on_exit(&server.session);
    Ok(())
}

fn write_line(line: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Asks the client's user with `elicitation/create`, reading stdin for the
/// answer while the call waits; other messages wait their turn (pings are
/// answered at once).
struct StdioAsker<'a> {
    lines: &'a mpsc::Receiver<String>,
    queue: &'a mut VecDeque<String>,
    asked: &'a mut u64,
}

impl Asker for StdioAsker<'_> {
    fn approve(&mut self, message: &str) -> Option<bool> {
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
        write_line(&request.to_string()).ok()?;
        let deadline = Instant::now() + LIFETIME;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let line = self.lines.recv_timeout(left).ok()?;
            match jsonrpc::parse(&line) {
                Ok(Incoming::Response { id: answered, body }) if answered == id => {
                    return elicited(&body);
                }
                Ok(Incoming::Request {
                    id: ping, method, ..
                }) if method == "ping" => {
                    write_line(&jsonrpc::result(ping, json!({}))).ok()?;
                }
                _ => self.queue.push_back(line),
            }
        }
    }
}

/// The user's answer in an `elicitation/create` response; `None` when the
/// client could not ask.
fn elicited(body: &Value) -> Option<bool> {
    let result = body.get("result")?;
    match result["action"].as_str()? {
        "accept" => Some(result["content"]["approve"] != json!(false)),
        "decline" | "cancel" => Some(false),
        _ => None,
    }
}
