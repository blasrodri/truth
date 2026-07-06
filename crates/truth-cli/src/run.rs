//! `truth run -- <cmd>` — execute a command and record a receipt (command,
//! exit code, timestamps, output digest) in the `.truth` store. Receipts are
//! the evidence source that makes agent success claims ("tests pass", "it
//! compiles") verifiable: a claim is Supported only when a matching successful
//! run postdates the last working-tree edit.
//!
//! `truth record-run` records a receipt for a command that already ran
//! elsewhere (agent hooks, CI) without re-executing it.

use crate::config_util::load_config;
use anyhow::{Context, Result};
use std::process::Command;
use truth_core::models::Run;
use truth_core::{new_id, now_secs};

/// Map a command line to a receipt kind. Order matters: "cargo clippy --tests"
/// must classify as lint, not test.
pub fn classify_kind(command: &str) -> &'static str {
    let c = command.to_ascii_lowercase();
    // git commands are never receipts: `git commit -m "fix tests"` matching the
    // "test" cue would fabricate a green test receipt out of a commit message.
    if c.trim_start().starts_with("git ") {
        return "other";
    }
    // fmt before lint/build: `cargo fmt --check` is a format receipt, and
    // `ruff format` must not classify as lint via the "ruff" cue.
    if c.contains("fmt") || c.contains("format") || c.contains("prettier") {
        "format"
    } else if c.contains("clippy")
        || c.contains("lint")
        || c.contains("eslint")
        || c.contains("ruff")
        || c.contains(" vet")
    {
        "lint"
    } else if c.contains("tsc") || c.contains("typecheck") || c.contains("mypy") {
        "typecheck"
    } else if c.contains("test") || c.contains("spec") || c.contains("pytest") {
        "test"
    } else if c.contains("build") || c.contains("compile") || c.contains("check") {
        "build"
    } else {
        "other"
    }
}

/// Timestamp of the most recent working-tree edit: max mtime over files the
/// diff touches, falling back to the HEAD commit time on a clean tree. `None`
/// outside git / on any error — callers treat that as "freshness unknown".
pub fn last_edit_time(repo_root: &str) -> Option<i64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["diff", "HEAD", "--name-only", "--no-color"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let files = String::from_utf8_lossy(&out.stdout);
    let mut latest: Option<i64> = None;
    for f in files.lines().filter(|l| !l.is_empty()) {
        let path = std::path::Path::new(repo_root).join(f);
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(mtime) = meta.modified() {
                if let Ok(secs) = mtime.duration_since(std::time::UNIX_EPOCH) {
                    let s = secs.as_secs() as i64;
                    latest = Some(latest.map_or(s, |l: i64| l.max(s)));
                }
            }
        }
    }
    if latest.is_some() {
        return latest;
    }
    // Clean tree: the last edit is whatever HEAD committed.
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["log", "-1", "--format=%ct"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// FNV-1a over the combined output — a cheap, dependency-free content digest.
fn digest(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Last `max_chars` of the output, redacted line by line (emails, tokens,
/// UUIDs, IPs never reach the store).
pub(crate) fn redacted_tail(output: &str, max_chars: usize) -> String {
    let start = output
        .char_indices()
        .rev()
        .nth(max_chars.saturating_sub(1))
        .map(|(i, _)| i)
        .unwrap_or(0);
    output[start..]
        .lines()
        .map(truth_logs::redact::redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `truth run -- <cmd...>`: execute, print the output, record the receipt.
/// Returns the child's exit code so the caller can propagate it (CI-friendly).
pub fn run(cmd: &[String], json: bool) -> Result<i32> {
    anyhow::ensure!(
        !cmd.is_empty(),
        "no command given — usage: truth run -- <cmd> [args...]"
    );
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;

    let command_line = cmd.join(" ");
    let started_at = now_secs();
    let t0 = std::time::Instant::now();
    let out = Command::new(&cmd[0])
        .args(&cmd[1..])
        .output()
        .with_context(|| format!("running `{command_line}`"))?;
    let duration_ms = t0.elapsed().as_millis() as i64;
    let exit_code = out.status.code().unwrap_or(-1);

    // Pass the child's output through untouched, like `time` would.
    use std::io::Write;
    std::io::stdout().write_all(&out.stdout).ok();
    std::io::stderr().write_all(&out.stderr).ok();

    let mut combined = Vec::with_capacity(out.stdout.len() + out.stderr.len());
    combined.extend_from_slice(&out.stdout);
    combined.extend_from_slice(&out.stderr);

    let run = Run {
        id: new_id(),
        command: command_line.clone(),
        kind: classify_kind(&command_line).to_string(),
        exit_code: exit_code as i64,
        started_at,
        finished_at: now_secs(),
        duration_ms: Some(duration_ms),
        output_digest: Some(digest(&combined)),
        output_tail: Some(redacted_tail(&String::from_utf8_lossy(&combined), 1200)),
        metadata_json: serde_json::json!({ "recorded_by": "truth run" }),
    };
    truth_db::repo::insert_run(&conn, &run)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&run)?);
    } else {
        eprintln!(
            "\ntruth: recorded `{command_line}` → exit {exit_code} ({} receipt)",
            run.kind
        );
    }
    Ok(exit_code)
}

/// `truth record-run` — store a receipt for a command that already ran
/// (agent hooks, CI). Never executes anything.
pub fn record(
    command: &str,
    exit_code: i64,
    kind: Option<&str>,
    repo: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut config = load_config()?;
    if let Some(r) = repo {
        crate::verify_turn::retarget_repo(&mut config, r);
    }
    let conn = truth_db::open(&config.database.path)?;
    let now = now_secs();
    let run = Run {
        id: new_id(),
        command: command.to_string(),
        kind: kind.unwrap_or_else(|| classify_kind(command)).to_string(),
        exit_code,
        started_at: now,
        finished_at: now,
        duration_ms: None,
        output_digest: None,
        output_tail: None,
        metadata_json: serde_json::json!({ "recorded_by": "truth record-run" }),
    };
    truth_db::repo::insert_run(&conn, &run)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&run)?);
    } else {
        println!(
            "Recorded `{command}` → exit {exit_code} ({} receipt)",
            run.kind
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_command_kinds() {
        assert_eq!(classify_kind("cargo test --workspace"), "test");
        assert_eq!(classify_kind("npx jest src/"), "other"); // no keyword — honest
        assert_eq!(classify_kind("pytest -x"), "test");
        assert_eq!(classify_kind("cargo clippy --tests"), "lint");
        assert_eq!(classify_kind("cargo build --release"), "build");
        assert_eq!(classify_kind("cargo check"), "build");
        assert_eq!(classify_kind("tsc --noEmit"), "typecheck");
        assert_eq!(classify_kind("ls -la"), "other");
        assert_eq!(classify_kind("cargo fmt --check"), "format");
        assert_eq!(classify_kind("go vet ./..."), "lint");
        // A commit message mentioning tests must not fabricate a receipt.
        assert_eq!(classify_kind("git commit -m \"fix tests\""), "other");
    }

    #[test]
    fn digest_is_stable_and_tail_is_bounded() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
        let long = "x".repeat(5000);
        assert!(redacted_tail(&long, 1200).len() <= 1200);
    }

    #[test]
    fn tail_redacts_secrets() {
        let t = redacted_tail("token sent to admin@example.com ok", 1200);
        assert!(!t.contains("admin@example.com"), "{t}");
    }
}
