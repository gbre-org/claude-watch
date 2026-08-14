//! Streamable-HTTP MCP transport + JSON-RPC dispatch.
//!
//! One axum app, one endpoint (`/mcp`), no upstream. Requests are answered
//! in-process: `initialize`, `tools/list`, `tools/call`, `ping`. Because
//! `tools/list` is a static in-process registry, the "connected-but-no-tools"
//! failure mode (a wedged proxy hop returning an empty tool set) cannot occur.
//!
//! Responses use `application/json` for a single JSON-RPC result (spec-valid
//! for the streamable-HTTP transport when the client sends
//! `Accept: application/json`, which Claude Code does). Notifications get a
//! bare `202 Accepted`. `GET /mcp` returns 405 — this server never initiates
//! server→client streams.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use crate::config::ServerConfig;
use crate::exec;

/// Protocol version echoed when the client doesn't request one.
const FALLBACK_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "host-bash";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct AppState {
    pub config: ServerConfig,
    session_counter: AtomicU64,
}

pub fn router(config: ServerConfig) -> Router {
    let state = Arc::new(AppState { config, session_counter: AtomicU64::new(1) });
    Router::new()
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// This server offers no server-initiated stream; a GET has nothing to return.
async fn mcp_get() -> impl IntoResponse {
    StatusCode::METHOD_NOT_ALLOWED
}

/// Stateless server — session teardown is a no-op success.
async fn mcp_delete() -> impl IntoResponse {
    StatusCode::NO_CONTENT
}

/// Constant-time byte comparison for the bearer token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Returns `None` if auth passes (or no bearer configured); `Some(response)`
/// with a 401 to short-circuit otherwise.
fn check_bearer(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let Some(expected) = state.config.bearer.as_deref() else {
        return None;
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));
    match presented {
        Some(tok) if constant_time_eq(tok.as_bytes(), expected.as_bytes()) => None,
        _ => Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32001, "message": "missing or invalid bearer token" }
                })),
            )
                .into_response(),
        ),
    }
}

fn ok_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_result(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The static tool registry returned by `tools/list`.
fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "run_command",
                "description": "Run a shell command on the HOST and return its combined stdout+stderr. \
                    When shell operators are enabled the string is executed via `bash -c` (pipes, \
                    redirects, &&, 2>&1 all work). Bounded by the server's command timeout and \
                    allow-list.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The command line to run on the host." }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "run_script",
                "description": "Run a script on the HOST by feeding the script body to the given \
                    interpreter on STDIN (never tokenized, so heredocs/quotes/redirects are safe). \
                    The interpreter must be a bare basename on the allow-list.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "interpreter": { "type": "string", "description": "Interpreter basename, e.g. bash, python, node." },
                        "script": { "type": "string", "description": "Script body, delivered on the interpreter's STDIN." }
                    },
                    "required": ["interpreter", "script"]
                }
            },
            {
                "name": "show_security_rules",
                "description": "Report the effective host-bash security policy (allow-list, timeout, \
                    shell-operator gate, path fence).",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": is_error })
}

/// Dispatch a single JSON-RPC message. Returns `Some(response)` for a request,
/// `None` for a notification (no `id`).
async fn handle_message(state: &AppState, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let is_request = id.is_some();

    // Notifications (no id) carry no response.
    if !is_request {
        return None;
    }
    let id = id.unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let protocol_version = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(|v| v.as_str())
                .unwrap_or(FALLBACK_PROTOCOL_VERSION)
                .to_string();
            Some(ok_result(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
                }),
            ))
        }
        "ping" => Some(ok_result(id, json!({}))),
        "tools/list" => Some(ok_result(id, tool_definitions())),
        "tools/call" => {
            let params = msg.get("params");
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = params.and_then(|p| p.get("arguments"));
            let policy = &state.config.policy;
            let out = match name {
                "run_command" => {
                    let command =
                        args.and_then(|a| a.get("command")).and_then(|c| c.as_str()).unwrap_or("");
                    exec::run_command(policy, command).await
                }
                "run_script" => {
                    let interpreter = args
                        .and_then(|a| a.get("interpreter"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let script =
                        args.and_then(|a| a.get("script")).and_then(|c| c.as_str()).unwrap_or("");
                    exec::run_script(policy, interpreter, script).await
                }
                "show_security_rules" => exec::show_security_rules(policy),
                other => {
                    return Some(err_result(
                        id,
                        -32602,
                        &format!("unknown tool: {other}"),
                    ));
                }
            };
            Some(ok_result(id, tool_result(out.text, out.is_error)))
        }
        other => Some(err_result(id, -32601, &format!("method not found: {other}"))),
    }
}

async fn mcp_post(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(resp) = check_bearer(&state, &headers) {
        return resp;
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(err_result(Value::Null, -32700, "Parse error")),
            )
                .into_response();
        }
    };

    // Batch (array) support — process each; if none produced a response
    // (all notifications), return 202.
    if let Value::Array(msgs) = &value {
        let mut responses = Vec::new();
        for m in msgs {
            if let Some(r) = handle_message(&state, m).await {
                responses.push(r);
            }
        }
        if responses.is_empty() {
            return StatusCode::ACCEPTED.into_response();
        }
        return (StatusCode::OK, Json(Value::Array(responses))).into_response();
    }

    let is_initialize =
        value.get("method").and_then(|m| m.as_str()) == Some("initialize");
    match handle_message(&state, &value).await {
        None => StatusCode::ACCEPTED.into_response(),
        Some(resp) if is_initialize => {
            let sid = format!(
                "hbs-{}",
                state.session_counter.fetch_add(1, Ordering::Relaxed)
            );
            (
                StatusCode::OK,
                [(header::HeaderName::from_static("mcp-session-id"), sid)],
                Json(resp),
            )
                .into_response()
        }
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Policy;

    fn state() -> AppState {
        AppState {
            config: ServerConfig {
                bind_host: "127.0.0.1".into(),
                port: 8766,
                bearer: None,
                config_path: "/nonexistent".into(),
                config_present: false,
                policy: Policy {
                    allow_all_commands: true,
                    allowed_commands: Default::default(),
                    allowed_flags_all: true,
                    allowed_flags: Default::default(),
                    allowed_dir: "/".into(),
                    command_timeout: 10,
                    max_command_length: 8192,
                    allow_shell_operators: true,
                    profile: "test".into(),
                    profile_warning: None,
                },
            },
            session_counter: AtomicU64::new(1),
        }
    }

    #[tokio::test]
    async fn initialize_returns_capabilities() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}});
        let resp = handle_message(&state(), &msg).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "host-bash");
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn tools_list_has_three_tools() {
        let msg = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        let resp = handle_message(&state(), &msg).await.unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"run_script"));
        assert!(names.contains(&"show_security_rules"));
    }

    #[tokio::test]
    async fn tools_call_run_command_roundtrip() {
        let msg = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"run_command","arguments":{"command":"echo hello-mcp"}}});
        let resp = handle_message(&state(), &msg).await.unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello-mcp"));
    }

    #[tokio::test]
    async fn notification_has_no_response() {
        let msg = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(handle_message(&state(), &msg).await.is_none());
    }

    #[tokio::test]
    async fn unknown_method_is_error() {
        let msg = json!({"jsonrpc":"2.0","id":9,"method":"bogus/method"});
        let resp = handle_message(&state(), &msg).await.unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }
}
