//! MCP (Model Context Protocol) over stdio: newline-delimited JSON-RPC on
//! stdin and stdout. Stdout carries nothing but protocol messages; logs go
//! to stderr.

use std::io::{BufRead, Write};
use std::sync::mpsc;

use base64::Engine as _;
use catpaw_protocol::{INSTRUCTIONS, PROTOCOL_VERSIONS, TOOLS};
use serde_json::{Value, json};

use crate::jsonrpc::{self, Incoming};
use crate::output::ToolOutput;
use crate::session::{Session, SessionConfig};

/// The MCP side of a session: answers requests one at a time.
pub struct McpServer {
    session: Session,
}

impl McpServer {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    /// The session the server speaks for.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Handles one line from the client; returns the line to answer with,
    /// if any.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let message = match jsonrpc::parse(line) {
            Ok(message) => message,
            Err(response) => return Some(response),
        };
        match message {
            Incoming::Request { id, method, params } => Some(self.request(id, &method, params)),
            // `notifications/initialized`, `notifications/cancelled` (calls
            // run to completion), and answers to requests we never send.
            Incoming::Notification { .. } | Incoming::Response { .. } => None,
        }
    }

    fn request(&mut self, id: Value, method: &str, params: Value) -> String {
        match method {
            "initialize" => jsonrpc::result(id, initialize(&params)),
            "ping" => jsonrpc::result(id, json!({})),
            "tools/list" => {
                let tools: Vec<Value> = TOOLS.iter().map(|t| t.to_json()).collect();
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
                if catpaw_protocol::tool(name).is_none() {
                    return jsonrpc::error(
                        id,
                        jsonrpc::INVALID_PARAMS,
                        &format!("Unknown tool: {name}"),
                    );
                }
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                let output = self.session.call_tool(name, arguments);
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
    // Lines are read on a thread of their own so that a long call does not
    // stop the reading (later versions answer cancellations and
    // elicitations while a call runs).
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
    let stdout = std::io::stdout();
    for line in lines_rx {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            let mut out = stdout.lock();
            out.write_all(reply.as_bytes())?;
            out.write_all(b"\n")?;
            out.flush()?;
        }
    }
    on_exit(&server.session);
    Ok(())
}
