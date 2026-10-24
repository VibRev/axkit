//! A stdio MCP server for tools that are already a CLI (CONVENTIONS §9.2).
//!
//! JSON-RPC 2.0, one message per line on stdin and stdout, the subset a
//! code-mode tool needs: `initialize`, `ping`, `tools/list`, `tools/call`.
//! Requests are answered in order, one at a time; a tool call's text is
//! what the CLI would print, so the tool keeps one renderer.

use serde_json::{Map, Value, json};
use std::io::{self, BufRead, Write};

/// Protocol versions spoken, newest first; a client asking for one of these
/// gets it, any other gets the newest.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What `initialize` says about the server.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    /// Shown to the model by most clients: when to use the tools.
    pub instructions: Option<String>,
}

/// One tool as `tools/list` shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDef {
    /// Letters, digits, `_` and `-`: some clients refuse a `.`.
    pub name: String,
    pub description: String,
    /// A JSON Schema object for the arguments.
    pub input_schema: Value,
}

/// A tool call's answer: text for the model, and whether it reports a
/// failure. A failure is still an answer, not a protocol error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

impl CallResult {
    pub fn ok(text: impl Into<String>) -> CallResult {
        CallResult { text: text.into(), is_error: false }
    }

    pub fn error(text: impl Into<String>) -> CallResult {
        CallResult { text: text.into(), is_error: true }
    }
}

/// The tools a server serves.
pub trait Handler {
    fn tools(&self) -> Vec<ToolDef>;

    /// Run `name` with `args`; `None` when there is no such tool.
    fn call(&mut self, name: &str, args: &Map<String, Value>) -> Option<CallResult>;
}

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serve `handler` until `input` ends.
///
/// # Errors
///
/// Reading `input` or writing `output` failed.
pub fn serve(info: &ServerInfo, handler: &mut impl Handler, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = answer(info, handler, &line) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The reply to one message; `None` for a notification or a response.
fn answer(info: &ServerInfo, handler: &mut impl Handler, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(&Value::Null, PARSE_ERROR, &format!("not JSON: {e}"))),
    };
    let Some(message) = message.as_object() else {
        return Some(error(&Value::Null, INVALID_REQUEST, "a message is one JSON object; batches are not supported"));
    };
    let Some(id) = message.get("id").filter(|id| !id.is_null()) else {
        // A notification (`notifications/initialized`, `…/cancelled`):
        // nothing to say back.
        return None;
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response to a request this server never sends.
        return None;
    };
    let empty = Map::new();
    let params = message.get("params").and_then(Value::as_object).unwrap_or(&empty);
    let result = match method {
        "initialize" => initialize(info, params),
        "ping" => json!({}),
        "tools/list" => {
            let tools: Vec<Value> = handler
                .tools()
                .into_iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
                .collect();
            json!({ "tools": tools })
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, INVALID_PARAMS, "tools/call needs a tool name"));
            };
            let args = params.get("arguments").and_then(Value::as_object).unwrap_or(&empty);
            match handler.call(name, args) {
                Some(r) => json!({ "content": [{ "type": "text", "text": r.text }], "isError": r.is_error }),
                None => return Some(error(id, INVALID_PARAMS, &format!("no tool named {name}"))),
            }
        }
        other => return Some(error(id, METHOD_NOT_FOUND, &format!("method not found: {other}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn initialize(info: &ServerInfo, params: &Map<String, Value>) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
    let mut result = json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": info.name, "version": info.version },
    });
    if let Some(text) = &info.instructions {
        result["instructions"] = json!(text);
    }
    result
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Builders for the argument schemas a tool takes.
pub mod schema {
    use serde_json::{Map, Value, json};

    /// `{"type": "object", "properties": …, "required": […]}` from
    /// `(name, schema, required)`.
    pub fn object(props: &[(&str, Value, bool)]) -> Value {
        let properties: Map<String, Value> = props.iter().map(|(n, s, _)| ((*n).to_owned(), s.clone())).collect();
        let required: Vec<&str> = props.iter().filter(|p| p.2).map(|p| p.0).collect();
        json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
    }

    pub fn string(description: &str) -> Value {
        json!({ "type": "string", "description": description })
    }

    pub fn integer(description: &str) -> Value {
        json!({ "type": "integer", "description": description })
    }

    pub fn boolean(description: &str) -> Value {
        json!({ "type": "boolean", "description": description })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo {
        calls: usize,
    }

    impl Handler for Echo {
        fn tools(&self) -> Vec<ToolDef> {
            vec![ToolDef {
                name: "echo".into(),
                description: "Say it back.".into(),
                input_schema: schema::object(&[("text", schema::string("what to say"), true)]),
            }]
        }

        fn call(&mut self, name: &str, args: &Map<String, Value>) -> Option<CallResult> {
            (name == "echo").then(|| {
                self.calls += 1;
                match args.get("text").and_then(Value::as_str) {
                    Some(t) => CallResult::ok(t),
                    None => CallResult::error("Error: usage: text is required\nhint: pass {\"text\": \"…\"}"),
                }
            })
        }
    }

    fn info() -> ServerInfo {
        ServerInfo { name: "tx".into(), version: "0.1.0".into(), instructions: Some("Use echo.".into()) }
    }

    /// The replies to `lines`, parsed.
    fn talk(lines: &[&str]) -> (Vec<Value>, usize) {
        let mut handler = Echo { calls: 0 };
        let mut out = Vec::new();
        serve(&info(), &mut handler, lines.join("\n").as_bytes(), &mut out).unwrap();
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (replies, handler.calls)
    }

    #[test]
    fn a_session_initializes_lists_and_calls() {
        let (replies, calls) = talk(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":"three","method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"echo","arguments":{}}}"#,
            r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
        ]);
        assert_eq!(replies.len(), 5, "the notification gets no reply: {replies:?}");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "tx");
        assert_eq!(replies[0]["result"]["instructions"], "Use echo.");
        assert_eq!(replies[1]["result"]["tools"][0]["inputSchema"]["required"][0], "text");
        assert_eq!(replies[2]["id"], "three");
        assert_eq!(replies[2]["result"]["content"][0]["text"], "hi");
        assert_eq!(replies[2]["result"]["isError"], false);
        assert_eq!(replies[3]["result"]["isError"], true, "a failed call is an answer");
        assert_eq!(replies[4]["result"], json!({}));
        assert_eq!(calls, 2);
    }

    #[test]
    fn an_unknown_version_gets_the_newest() {
        let (replies, _) = talk(&[r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#]);
        assert_eq!(replies[0]["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn protocol_errors_have_their_codes() {
        let (replies, calls) = talk(&[
            "not json",
            "[1,2]",
            r#"{"jsonrpc":"2.0","id":1,"method":"resources/list"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nope"}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":9,"result":{}}"#,
        ]);
        let codes: Vec<i64> = replies.iter().map(|r| r["error"]["code"].as_i64().unwrap()).collect();
        assert_eq!(codes, [PARSE_ERROR, INVALID_REQUEST, METHOD_NOT_FOUND, INVALID_PARAMS, INVALID_PARAMS]);
        assert_eq!(replies[0]["id"], Value::Null);
        assert_eq!(calls, 0);
    }
}
