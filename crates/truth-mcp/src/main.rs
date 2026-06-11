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
                write_msg(
                    &mut out,
                    &error_response(Value::Null, -32700, &format!("parse error: {e}")),
                );
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
                    Some(error_response(
                        id.clone().unwrap_or(Value::Null),
                        -32601,
                        &format!("method not found: {other}"),
                    ))
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
                "Before telling the user a code change is done, call `verify_turn`. \
                 Extract the concrete factual claims from your message and pass them \
                 in the `claims` array (one short sentence each) — you parse them more \
                 reliably than truth can re-parse prose, and it's free. Also pass the \
                 raw `message`. truth checks each claim against the repo, the \
                 working-tree diff, recorded command runs, and logs and returns a \
                 deterministic, cited verdict. This covers files you edited/created/\
                 deleted, 'I only changed X' scope claims, renames, values set, routes/\
                 functions added or removed, and 'tests pass' (verified against runs \
                 recorded by `truth run -- <cmd>` — run tests through it so the claim \
                 is checkable). Fix or soften any claim it marks `contradicted`; \
                 claims it marks `refused` are unverifiable — do not treat a refusal \
                 as confirmation."
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> Value {
    let tool = json!({
        "name": "verify_turn",
        "description":
            "Fact-check your own claims about code you changed, BEFORE telling the \
             user a change is done — to catch over-claims (wrong values, routes or \
             functions you said you added/removed, files you said you edited, \
             'I only changed X' scope claims, renames, and 'tests pass'). \
             HOW TO CALL: extract the concrete factual claims from what you're about \
             to say and pass them in `claims` (one short sentence each) — you parse \
             them far more reliably than truth can re-parse prose, and it's free \
             since you're already writing the message. Also pass the raw `message` \
             as a backstop. \
             Each claim is checked against the indexed repo + working-tree git diff \
             + recorded command runs + logs and gets a cited supported / \
             contradicted / refused verdict. 'tests pass' is only supported when a \
             matching run recorded via `truth run -- <cmd>` exited 0 AFTER your last \
             edit. Deterministic: the verdict comes from evidence, not from a model \
             — so listing a claim can't make it pass. 'refused' means unverifiable, \
             NOT confirmed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What the agent is about to claim about its work \
                        (the raw prose). Always include this — truth scans it as a \
                        backstop so claims you forget to list still get checked."
                },
                "claims": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "RECOMMENDED. The individual factual claims from \
                        your message, each as a short self-contained sentence (e.g. \
                        [\"I set MAX_RETRIES to 5\", \"I added the /v1/refund route\", \
                        \"I removed the parse_legacy function\"]). You (the calling \
                        model) extract these from your own message — it's free and far \
                        more reliable than truth re-parsing your prose. Only list \
                        checkable state claims; skip vague/subjective ones. truth still \
                        decides each verdict from real evidence, not from your wording."
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
        return error_response(
            id.unwrap_or(Value::Null),
            -32602,
            &format!("unknown tool: {name}"),
        );
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let message = match args.get("message").and_then(|m| m.as_str()) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => return tool_error(id, "`message` is required and must be non-empty"),
    };
    let repo = args
        .get("repo")
        .and_then(|r| r.as_str())
        .map(|s| s.to_string());
    let local_log = args
        .get("local_log")
        .and_then(|l| l.as_str())
        .map(|s| s.to_string());
    // Optional agent-extracted claims (preferred over re-parsing the prose).
    let claims: Option<Vec<String>> = args.get("claims").and_then(|c| c.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    });

    match run_verify(
        &message,
        claims.as_deref(),
        repo.as_deref(),
        local_log.as_deref(),
    ) {
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
    claims: Option<&[String]>,
    repo: Option<&str>,
    local_log: Option<&str>,
) -> anyhow::Result<(String, Value)> {
    let mut config = load_config()?;
    if let Some(r) = repo {
        verify_turn::retarget_repo(&mut config, r);
    }
    let conn = truth_db::open(&config.database.path)?;
    let report = verify_turn::verify_claims(&conn, &config, message, claims, local_log)?;
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
