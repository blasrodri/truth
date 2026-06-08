//! `truth-mcp` — a minimal Model Context Protocol (MCP) server exposing
//! truth's agent-fact-checker as a tool any agent can call on ITSELF.
//!
//! Surface B of the product: before an agent says "done", it calls `verify_turn`
//! with its draft message; truth checks every claim against the indexed repo +
//! working-tree diff + logs and returns a cited verdict (supported /
//! contradicted / refused) so the agent can self-correct. The verdict is
//! deterministic — the agent cannot talk truth into a different answer.
//!
//! Transport: stdio, newline-delimited JSON-RPC 2.0 (MCP spec 2025-06-18).
//! Methods: `initialize`, `tools/list`, `tools/call`. Kept dependency-free of
//! any MCP SDK — the protocol surface we need is small.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};
use truth_cli::config_util::load_config;
use truth_cli::verify_turn;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                // Parse error — JSON-RPC code -32700. No id available.
                write_msg(&mut out, &error_response(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
        };

        // Notifications (no `id`) get no response; we just acknowledge silently.
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => Some(handle_initialize(id.clone())),
            "tools/list" => Some(handle_tools_list(id.clone())),
            "tools/call" => Some(handle_tools_call(id.clone(), &req)),
            // Lifecycle notifications we can ignore.
            "notifications/initialized" | "initialized" => None,
            "ping" => Some(result_response(id.clone(), json!({}))),
            other => {
                if id.is_some() {
                    Some(error_response(id.clone().unwrap_or(Value::Null), -32601, &format!("method not found: {other}")))
                } else {
                    None
                }
            }
        };

        if let Some(resp) = response {
            write_msg(&mut out, &resp);
        }
    }
}

fn write_msg(out: &mut impl Write, msg: &Value) {
    // One JSON object per line; no embedded newlines (MCP stdio requirement).
    let _ = writeln!(out, "{}", serde_json::to_string(msg).unwrap_or_default());
    let _ = out.flush();
}

fn handle_initialize(id: Option<Value>) -> Value {
    result_response(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "truth", "version": env!("CARGO_PKG_VERSION") },
            "instructions":
                "Call `verify_turn` with the message you are about to send the user \
                 about code you changed. truth checks each claim against the repo, \
                 the working-tree diff, and logs, and returns a deterministic, cited \
                 verdict. Fix or soften any claim it marks `contradicted`; claims it \
                 marks `refused` are unverifiable (e.g. 'tests pass') — do not treat \
                 a refusal as confirmation."
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> Value {
    let tool = json!({
        "name": "verify_turn",
        "description":
            "Fact-check a coding agent's own message about its work. Splits the \
             message into claims and checks each against the indexed repo + \
             working-tree git diff + logs. Returns supported / contradicted / \
             refused verdicts with citations. Deterministic: the verdict comes \
             from evidence, not from a model. Use before telling the user a change \
             is done to catch over-claims (wrong values, routes you said you \
             added/removed). 'refused' means unverifiable, NOT confirmed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What the agent is about to claim about its work."
                },
                "repo": {
                    "type": "string",
                    "description": "Absolute path to the repo root being worked on. \
                        Strongly recommended: truth opens <repo>/.truth and diffs that \
                        working tree. If omitted, the server's working directory is used, \
                        which may be wrong."
                },
                "local_log": {
                    "type": "string",
                    "description": "Optional path to a local log file for usage/error claims."
                }
            },
            "required": ["message"]
        }
    });
    result_response(id, json!({ "tools": [tool] }))
}

fn handle_tools_call(id: Option<Value>, req: &Value) -> Value {
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name != "verify_turn" {
        return error_response(id.unwrap_or(Value::Null), -32602, &format!("unknown tool: {name}"));
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let message = match args.get("message").and_then(|m| m.as_str()) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return tool_error(id, "`message` is required and must be non-empty"),
    };
    let repo = args.get("repo").and_then(|r| r.as_str()).map(|s| s.to_string());
    let local_log = args.get("local_log").and_then(|l| l.as_str()).map(|s| s.to_string());

    match run_verify(&message, repo.as_deref(), local_log.as_deref()) {
        Ok((text, structured)) => result_response(
            id,
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": structured,
                "isError": false,
            }),
        ),
        Err(e) => tool_error(id, &format!("verify failed: {e}")),
    }
}

/// Run the verification and return (human-readable text, structured JSON).
fn run_verify(
    message: &str,
    repo: Option<&str>,
    local_log: Option<&str>,
) -> anyhow::Result<(String, Value)> {
    let mut config = load_config()?;
    if let Some(r) = repo {
        verify_turn::retarget_repo(&mut config, r);
    }
    let conn = truth_db::open(&config.database.path)?;
    let report = verify_turn::verify(&conn, &config, message, local_log)?;
    let text = verify_turn::render_text(&report);
    Ok((text, report.to_json()))
}

// ---- JSON-RPC helpers -----------------------------------------------------

fn result_response(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool-level error is returned as a successful result with `isError: true`
/// (per MCP: tool failures are reported in the result, not as protocol errors).
fn tool_error(id: Option<Value>, message: &str) -> Value {
    result_response(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    )
}
