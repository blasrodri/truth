//! `truth hook` — make verification a gate the agent cannot skip.
//!
//! The MCP tool relies on the agent CHOOSING to call `verify_turn`. Hooks
//! remove that choice: `truth hook install` registers truth in Claude Code's
//! settings so that
//!
//! - on **Stop** (the agent finishing its turn), the agent's final message is
//!   fact-checked against the repo/diff/receipts; contradictions BLOCK the
//!   stop and are fed back so the agent corrects itself before the user ever
//!   sees the claim;
//! - on **PostToolUse** for Bash, test/build/lint commands the agent runs are
//!   recorded as command receipts (when the hook payload carries an exit
//!   code), which is what makes the agent's later "tests pass" verifiable.
//!
//! `truth hook claude` is the hook entry point itself: it reads the hook JSON
//! from stdin and dispatches on `hook_event_name`. It is deliberately
//! fail-open — any internal error exits 0 silently so a broken verifier can
//! never wedge the user's session.

use crate::config_util::{anchor_at, discover_root_from};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use truth_core::config::Config;

/// `truth hook install [--user] [--receipts]` — register truth's hooks in
/// Claude Code settings (project `.claude/settings.json` by default).
///
/// `--receipts` installs ONLY the receipt recorders (PostToolUse +
/// PostToolUseFailure on Bash), skipping the Stop fact-check gate. That's the
/// right global (`--user`) install: receipts are pure evidence-gathering with
/// no behavior change, while a blocking Stop gate should be a per-repo
/// decision.
pub fn install(user: bool, receipts_only: bool) -> Result<()> {
    let path = if user {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
        home.join(".claude").join("settings.json")
    } else {
        PathBuf::from(".claude").join("settings.json")
    };

    let mut settings: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));

    let mut added = Vec::new();
    if !receipts_only && ensure_hook(&mut settings, "Stop", None) {
        added.push("Stop");
    }
    if ensure_hook(&mut settings, "PostToolUse", Some("Bash")) {
        added.push("PostToolUse(Bash)");
    }
    // Nonzero exits fire a separate event; a red receipt ("tests FAILED") is
    // exactly what catches a later "tests pass" over-claim.
    if ensure_hook(&mut settings, "PostToolUseFailure", Some("Bash")) {
        added.push("PostToolUseFailure(Bash)");
    }

    if added.is_empty() {
        println!("truth hooks already installed in {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    println!(
        "Installed {} hook(s) in {}",
        added.join(" + "),
        path.display()
    );
    if !receipts_only {
        println!(
            "- Stop: the agent's final message is fact-checked; contradictions block until fixed."
        );
    }
    println!("- PostToolUse/PostToolUseFailure(Bash): test/build/lint/fmt runs are recorded as receipts for \"tests pass\" claims.");
    Ok(())
}

/// Add `truth hook claude` under the given event if it isn't there yet.
/// Returns true when settings were modified.
fn ensure_hook(settings: &mut Value, event: &str, matcher: Option<&str>) -> bool {
    let hooks = settings
        .as_object_mut()
        .expect("settings is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let entries = hooks
        .as_object_mut()
        .expect("hooks is an object")
        .entry(event)
        .or_insert_with(|| json!([]));
    let arr = entries.as_array_mut().expect("event entry is an array");

    let already = arr.iter().any(|e| {
        e.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains("truth hook claude"))
            })
        })
    });
    if already {
        return false;
    }
    let mut entry = json!({
        "hooks": [{ "type": "command", "command": "truth hook claude" }]
    });
    if let Some(m) = matcher {
        entry["matcher"] = json!(m);
    }
    arr.push(entry);
    true
}

/// `truth hook claude` — the hook entry point. Reads the event JSON from
/// stdin. Fail-open: errors exit 0 with no output.
pub fn claude() -> Result<()> {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return Ok(());
    }
    let Ok(event): std::result::Result<Value, _> = serde_json::from_str(&input) else {
        return Ok(());
    };
    let name = event
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match name {
        "Stop" => on_stop(&event),
        "PostToolUse" | "PostToolUseFailure" => on_post_tool_use(&event),
        _ => Ok(()),
    }
}

/// Load the truth config for the repo the hook fired in. Falls back to the
/// git root when no `.truth`/`truth.toml` exists yet — installing the hooks
/// IS the consent to fact-checking, so an uninitialized repo shouldn't
/// silently opt out. The store auto-creates (and self-gitignores) on first
/// use. Opt out of the fallback with `truth hook auto off`.
fn config_for(event: &Value) -> Option<Config> {
    let cwd = event
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let root =
        discover_root_from(&cwd).or_else(|| if auto_enabled() { git_root(&cwd) } else { None })?;
    let toml = root.join("truth.toml");
    let mut config = if toml.is_file() {
        Config::load(&toml).ok()?
    } else {
        Config::from_toml_str("").ok()?
    };
    anchor_at(&mut config, &root);
    Some(config)
}

/// Top-level directory of the git repo containing `dir`, if any.
fn git_root(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".truth").join("config.json"))
}

/// Whether hooks may auto-enable in git repos that haven't run `truth init`.
/// Default ON — `truth hook auto off` writes the opt-out.
pub fn auto_enabled() -> bool {
    let Some(path) = global_config_path() else {
        return true;
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("auto_enable").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

/// `truth hook auto on|off|status` — toggle the zero-setup fallback.
pub fn auto(mode: &str) -> Result<()> {
    match mode {
        "status" => {
            println!(
                "auto-enable is {} — hooks {} fact-check git repos that haven't run `truth init`.",
                if auto_enabled() { "ON" } else { "OFF" },
                if auto_enabled() { "DO" } else { "do NOT" },
            );
            Ok(())
        }
        "on" | "off" => {
            let path = global_config_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut config: Value = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({}));
            config["auto_enable"] = json!(mode == "on");
            std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
            println!("auto-enable set to {mode} ({}).", path.display());
            Ok(())
        }
        other => anyhow::bail!("unknown mode `{other}` (expected on | off | status)"),
    }
}

/// Stop hook: fact-check the agent's final message; block on contradictions.
fn on_stop(event: &Value) -> Result<()> {
    // Already continuing from a previous stop-hook block — never loop.
    if event
        .get("stop_hook_active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let Some(config) = config_for(event) else {
        return Ok(());
    };
    let Some(transcript) = event.get("transcript_path").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(message) = last_assistant_text(Path::new(transcript)) else {
        return Ok(());
    };

    // Fail-open: any verification error must not wedge the session.
    let report = (|| -> Result<_> {
        let conn = truth_db::open(&config.database.path)?;
        crate::verify_turn::verify(&conn, &config, &message, None)
    })();
    let Ok(report) = report else { return Ok(()) };

    // Block on a real contradiction OR an unproven success claim. The latter
    // is the "tests pass" leak: the agent asserted a green run without a
    // receipt, and truth's job is to DEMAND the proof — send it back to run the
    // command rather than let an untested "it passes" reach the user.
    if report.has_contradiction() || report.has_unproven() {
        let table = crate::verify_turn::render_text(&report);
        let reason = if report.has_contradiction() {
            format!(
                "truth fact-checked your message against the repo, the working-tree \
                 diff, and recorded runs — it contradicts the evidence:\n\n{table}\n\n\
                 Fix the code or correct the contradicted claims, then finish."
            )
        } else {
            format!(
                "truth fact-checked your message: you claimed a command succeeded \
                 (tests/build/lint) without a recorded run to prove it:\n\n{table}\n\n\
                 Run it through `truth run -- <cmd>` so the receipt exists, then finish. \
                 truth will not confirm an unproven \"it passes\"."
            )
        };
        println!("{}", json!({ "decision": "block", "reason": reason }));
    }
    Ok(())
}

/// PostToolUse / PostToolUseFailure (Bash) hook: record test/build/lint/fmt
/// runs as command receipts. The exit code comes from the payload when
/// present; otherwise the EVENT NAME is itself a deterministic signal from the
/// harness (PostToolUse fires on exit 0, PostToolUseFailure on nonzero), so we
/// fall back to it rather than dropping the receipt — field data showed zero
/// hook receipts ever recorded, partly because payload shapes drifted.
fn on_post_tool_use(event: &Value) -> Result<()> {
    if event.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return Ok(());
    }
    let Some(command) = event
        .pointer("/tool_input/command")
        .and_then(|v| v.as_str())
    else {
        return Ok(());
    };
    let kind = crate::run::classify_kind(command);
    if kind == "other" {
        return Ok(());
    }
    let payload_exit = exit_code_from(event.get("tool_response"));
    let Some(exit_code) = resolve_exit_code(
        payload_exit,
        event.get("hook_event_name").and_then(|v| v.as_str()),
    ) else {
        return Ok(());
    };
    let Some(config) = config_for(event) else {
        return Ok(());
    };
    // Redacted output tail: lets the verdict spot empty green runs ("0 tests
    // run") from hook-recorded receipts, same as `truth run` ones.
    let output_tail = event.get("tool_response").map(|r| {
        let stdout = r.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        let stderr = r.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
        crate::run::redacted_tail(&format!("{stdout}\n{stderr}"), 1200)
    });
    // Best-effort: a failed insert must not surface into the session.
    let _ = (|| -> Result<()> {
        let conn = truth_db::open(&config.database.path)?;
        let now = truth_core::now_secs();
        truth_db::repo::insert_run(
            &conn,
            &truth_core::models::Run {
                id: truth_core::new_id(),
                command: command.to_string(),
                kind: kind.to_string(),
                exit_code,
                started_at: now,
                finished_at: now,
                duration_ms: None,
                output_digest: None,
                output_tail,
                metadata_json: json!({
                    "recorded_by": "claude-code hook",
                    "exit_source": if payload_exit.is_some() { "payload" } else { "event_name" },
                }),
            },
        )
    })();
    Ok(())
}

/// The exit code to record: the payload's when present, else inferred from
/// the event name (the harness routes exit 0 to PostToolUse and nonzero to
/// PostToolUseFailure — a deterministic signal, not a guess). None → don't
/// record.
fn resolve_exit_code(payload_exit: Option<i64>, event_name: Option<&str>) -> Option<i64> {
    match (payload_exit, event_name) {
        (Some(code), _) => Some(code),
        (None, Some("PostToolUse")) => Some(0),
        (None, Some("PostToolUseFailure")) => Some(1),
        _ => None,
    }
}

/// Find an exit code in the PostToolUse tool_response, across the field names
/// different Claude Code versions have used. None → don't record.
fn exit_code_from(response: Option<&Value>) -> Option<i64> {
    let r = response?;
    for key in ["exit_code", "exitCode", "code", "returncode"] {
        if let Some(n) = r.get(key).and_then(|v| v.as_i64()) {
            return Some(n);
        }
    }
    // Some payloads nest the result.
    for key in ["result", "output"] {
        if let Some(n) = exit_code_from(r.get(key)) {
            return Some(n);
        }
    }
    None
}

/// Last assistant message text from a Claude Code transcript (JSONL).
fn last_assistant_text(transcript: &Path) -> Option<String> {
    let content = std::fs::read_to_string(transcript).ok()?;
    for line in content.lines().rev() {
        let Ok(entry): std::result::Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let message = entry.get("message")?;
        let text = match message.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_hook_is_idempotent_and_preserves_settings() {
        let mut s = json!({ "permissions": { "allow": ["Bash(ls:*)"] } });
        assert!(ensure_hook(&mut s, "Stop", None));
        assert!(
            !ensure_hook(&mut s, "Stop", None),
            "second install is a no-op"
        );
        assert!(ensure_hook(&mut s, "PostToolUse", Some("Bash")));
        assert!(ensure_hook(&mut s, "PostToolUseFailure", Some("Bash")));
        // Pre-existing settings survive.
        assert_eq!(s["permissions"]["allow"][0], "Bash(ls:*)");
        assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Bash");
        assert_eq!(s["hooks"]["PostToolUseFailure"][0]["matcher"], "Bash");
    }

    #[test]
    fn exit_code_falls_back_to_event_name() {
        // Payload exit code always wins.
        assert_eq!(resolve_exit_code(Some(2), Some("PostToolUse")), Some(2));
        // No payload code: the event name is a deterministic success/failure
        // signal from the harness. Field data: zero hook receipts were ever
        // recorded partly because payload shapes drifted across versions.
        assert_eq!(resolve_exit_code(None, Some("PostToolUse")), Some(0));
        assert_eq!(resolve_exit_code(None, Some("PostToolUseFailure")), Some(1));
        // Unknown event and no code → never guess.
        assert_eq!(resolve_exit_code(None, Some("SomethingElse")), None);
        assert_eq!(resolve_exit_code(None, None), None);
    }

    #[test]
    fn extracts_exit_code_across_shapes() {
        assert_eq!(exit_code_from(Some(&json!({"exit_code": 1}))), Some(1));
        assert_eq!(exit_code_from(Some(&json!({"exitCode": 0}))), Some(0));
        assert_eq!(
            exit_code_from(Some(&json!({"result": {"code": 101}}))),
            Some(101)
        );
        assert_eq!(exit_code_from(Some(&json!({"stdout": "ok"}))), None);
    }

    #[test]
    fn reads_last_assistant_text_from_transcript() {
        let tmp = std::env::temp_dir().join(format!("truth-hook-{}.jsonl", std::process::id()));
        std::fs::write(
            &tmp,
            concat!(
                r#"{"type":"user","message":{"content":"do it"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"I set MAX_RETRIES to 5"}]}}"#,
                "\n",
                r#"{"type":"system","subtype":"other"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            last_assistant_text(&tmp).as_deref(),
            Some("I set MAX_RETRIES to 5")
        );
        std::fs::remove_file(&tmp).ok();
    }
}
