//! JSON-RPC 2.0 messages, one per line.

use serde_json::{Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// A message from the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A call that wants an answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A call that does not.
    Notification { method: String, params: Value },
    /// An answer to a request of ours.
    Response { id: Value, body: Value },
}

/// Parses one line. `Err` is the error response to send.
pub fn parse(line: &str) -> Result<Incoming, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| error(Value::Null, PARSE_ERROR, &e.to_string()))?;
    let Value::Object(map) = value else {
        return Err(error(
            Value::Null,
            INVALID_REQUEST,
            "expected one JSON-RPC message (batches are not supported)",
        ));
    };
    let id = map.get("id").cloned();
    let params = map.get("params").cloned().unwrap_or(Value::Null);
    match (map.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Ok(Incoming::Request {
            id,
            method: method.to_string(),
            params,
        }),
        (Some(method), None) => Ok(Incoming::Notification {
            method: method.to_string(),
            params,
        }),
        (None, Some(id)) => Ok(Incoming::Response {
            id,
            body: Value::Object(map),
        }),
        (None, None) => Err(error(Value::Null, INVALID_REQUEST, "no method and no id")),
    }
}

/// A result response.
pub fn result(id: Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// An error response.
pub fn error(id: Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_told_apart() {
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Ok(Incoming::Request { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Ok(Incoming::Notification { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#),
            Ok(Incoming::Response { .. })
        ));
        assert!(parse("[1]").unwrap_err().contains("-32600"));
        assert!(parse("{").unwrap_err().contains("-32700"));
    }
}
