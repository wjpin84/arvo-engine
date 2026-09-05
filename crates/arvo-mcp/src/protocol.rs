//! The wire format, with no I/O in it.
//!
//! Everything here is pure: build a request value, parse a response body.
//! That is deliberate — the protocol is where the fiddly, easy-to-get-wrong
//! details live (envelope shapes, two different response framings, errors that
//! arrive as successful HTTP), and pure functions can be tested exhaustively
//! without a server, a network, or a token.
//!
//! The transport in [`crate::client`] is then thin enough to read in one sitting.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The protocol version this client asks for.
///
/// The server answers with the version it will actually use, which may differ.
/// We record what it chose rather than insisting: only the core methods are
/// used here and they have been stable across revisions, so refusing to talk
/// to a newer server would break more than it protects.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Header a server may issue on initialize, to be echoed on every later call.
pub const SESSION_HEADER: &str = "Mcp-Session-Id";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("response was not JSON")]
    Malformed(#[source] serde_json::Error),
    #[error("server returned an error: {message} (code {code})")]
    Rpc { code: i64, message: String },
    #[error("response had neither a result nor an error")]
    Empty,
    #[error("event stream carried no JSON-RPC message")]
    NoMessage,
    #[error("unexpected shape: {0}")]
    Shape(String),
}

/// A tool the server offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// A JSON-RPC request envelope.
#[must_use]
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A notification: a request with no id, which expects no reply.
#[must_use]
pub fn notification(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": method })
}

#[must_use]
pub fn initialize(id: u64, client_name: &str, client_version: &str) -> Value {
    request(
        id,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            // Honest about what we are: this client reads. It declares no
            // sampling, roots or elicitation capability because it implements
            // none, and claiming otherwise invites a server to try using them.
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": client_version },
        }),
    )
}

#[must_use]
pub fn initialized() -> Value {
    notification("notifications/initialized")
}

#[must_use]
pub fn tools_list(id: u64) -> Value {
    request(id, "tools/list", json!({}))
}

#[must_use]
pub fn tools_call(id: u64, name: &str, arguments: Value) -> Value {
    request(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// Pulls the JSON-RPC payload out of a response body.
///
/// A Streamable HTTP server may answer a POST with either plain JSON or an
/// SSE stream carrying the same message, and which one it picks is the
/// server's choice, not ours. Handling only the first works right up until a
/// server decides to stream, at which point every call fails with a parse
/// error that says nothing about why.
///
/// # Errors
///
/// Returns [`ProtocolError`] if the body is not JSON, carries a JSON-RPC
/// error, or contains no message at all.
pub fn parse(body: &str, content_type: Option<&str>) -> Result<Value, ProtocolError> {
    let is_stream = content_type.is_some_and(|value| value.contains("text/event-stream"))
        // Sniff as well as trust: some servers send the stream framing with a
        // generic content type, and the framing is unmistakable.
        || body.trim_start().starts_with("event:")
        || body.trim_start().starts_with("data:");

    let payload = if is_stream {
        last_data_event(body).ok_or(ProtocolError::NoMessage)?
    } else {
        body.to_owned()
    };

    let value: Value = serde_json::from_str(&payload).map_err(ProtocolError::Malformed)?;
    result_of(&value)
}

/// The final `data:` payload in an SSE body.
///
/// The last one, not the first: a server may send progress notifications
/// before the actual reply, and taking the first would return a notification
/// where the caller expects a result.
fn last_data_event(body: &str) -> Option<String> {
    let mut found = None;
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            // Skip anything that is not a reply to us — a notification has no
            // id, and the stream may carry several.
            if data.is_empty() {
                continue;
            }
            if serde_json::from_str::<Value>(data)
                .ok()
                .is_some_and(|value| value.get("id").is_some())
            {
                found = Some(data.to_owned());
            }
        }
    }
    found
}

/// Unwraps `result`, turning a JSON-RPC `error` into a Rust error.
///
/// A JSON-RPC error arrives inside a perfectly successful HTTP 200, so the
/// transport cannot catch it — every caller would otherwise treat a failure
/// as a result with missing fields.
fn result_of(value: &Value) -> Result<Value, ProtocolError> {
    if let Some(error) = value.get("error") {
        return Err(ProtocolError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_owned(),
        });
    }
    value.get("result").cloned().ok_or(ProtocolError::Empty)
}

/// The version the server said it would speak.
///
/// # Errors
///
/// Returns [`ProtocolError::Shape`] if the initialize result has no version.
pub fn negotiated_version(result: &Value) -> Result<String, ProtocolError> {
    result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| ProtocolError::Shape("initialize result had no protocolVersion".to_owned()))
}

/// The tools from a `tools/list` result.
///
/// # Errors
///
/// Returns [`ProtocolError::Shape`] if `tools` is absent or not a list.
pub fn tools(result: &Value) -> Result<Vec<Tool>, ProtocolError> {
    let list = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::Shape("tools/list result had no tools array".to_owned()))?;

    Ok(list
        .iter()
        .filter_map(|tool| {
            Some(Tool {
                name: tool.get("name")?.as_str()?.to_owned(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        })
        .collect())
}

/// The text a `tools/call` returned.
///
/// MCP wraps tool output in a content list, and every server used here returns
/// its payload as a JSON document inside a text block. Concatenates the text
/// blocks in order and ignores other content kinds.
///
/// # Errors
///
/// Returns [`ProtocolError::Rpc`] when the tool itself reported failure —
/// `isError` arrives inside a *successful* JSON-RPC result, so it is invisible
/// to both the transport and [`parse`], and a caller that did not check it
/// would read an error message as data.
pub fn tool_text(result: &Value) -> Result<String, ProtocolError> {
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::Shape("tool result had no content array".to_owned()))?;

    let text: String = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");

    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(ProtocolError::Rpc {
            code: 0,
            message: if text.is_empty() {
                "the tool reported an error with no message".to_owned()
            } else {
                text
            },
        });
    }

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_initialize_request_states_a_version_and_a_client() {
        let value = initialize(1, "arvo", "0.1.0");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["method"], "initialize");
        assert_eq!(value["params"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(value["params"]["clientInfo"]["name"], "arvo");
    }

    #[test]
    fn a_notification_carries_no_id_because_it_expects_no_reply() {
        let value = initialized();
        assert_eq!(value["method"], "notifications/initialized");
        assert!(value.get("id").is_none());
    }

    #[test]
    fn a_plain_json_response_yields_its_result() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
        let result = parse(body, Some("application/json")).expect("plain json");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn an_event_stream_response_yields_the_same_result() {
        // The server chooses the framing, not us. Handling only plain JSON
        // works until a server decides to stream, and then every call fails
        // with a parse error that explains nothing.
        let body =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let result = parse(body, Some("text/event-stream")).expect("sse");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn stream_framing_is_recognised_even_without_the_content_type() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let result = parse(body, Some("application/json")).expect("sniffed");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn the_reply_is_taken_from_the_last_event_not_the_first() {
        // Progress notifications can precede the actual reply; taking the
        // first would hand back a notification where a result was expected.
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        );
        let result = parse(body, Some("text/event-stream")).expect("sse");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn a_json_rpc_error_is_an_error_even_though_the_http_call_succeeded() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no such method"}}"#;
        let err = parse(body, Some("application/json")).expect_err("should fail");
        assert!(
            matches!(err, ProtocolError::Rpc { code: -32601, ref message } if message == "no such method"),
            "{err}"
        );
    }

    #[test]
    fn tools_are_read_out_of_a_list_result() {
        let result = serde_json::json!({
            "tools": [
                {"name": "get_accounts", "description": "List accounts"},
                {"name": "get_portfolio"},
            ]
        });
        let tools = tools(&result).expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "get_accounts");
        assert_eq!(tools[1].description, "", "a missing description is empty");
    }

    #[test]
    fn tool_text_joins_the_text_blocks() {
        let result = serde_json::json!({
            "content": [
                {"type": "text", "text": "{\"a\":"},
                {"type": "image", "data": "ignored"},
                {"type": "text", "text": "1}"},
            ]
        });
        assert_eq!(tool_text(&result).expect("text"), "{\"a\":1}");
    }

    #[test]
    fn a_tool_that_reports_failure_is_an_error_not_data() {
        // isError rides inside a SUCCESSFUL JSON-RPC result, so nothing below
        // this function can see it. A caller that skipped the check would
        // parse an error message as if it were the payload.
        let result = serde_json::json!({
            "isError": true,
            "content": [{"type": "text", "text": "account not found"}],
        });
        let err = tool_text(&result).expect_err("should fail");
        assert!(
            matches!(err, ProtocolError::Rpc { ref message, .. } if message == "account not found"),
            "{err}"
        );
    }

    #[test]
    fn a_negotiated_version_is_reported_rather_than_assumed() {
        let result = serde_json::json!({"protocolVersion": "2099-01-01"});
        assert_eq!(negotiated_version(&result).expect("version"), "2099-01-01");
        assert!(negotiated_version(&serde_json::json!({})).is_err());
    }
}
