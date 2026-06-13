//! `truth audit-session` — point truth at your own Claude Code session history
//! and show the over-claims it would have caught.
//!
//! This is the wedge: the value of a fact-checker is abstract until you watch it
//! catch your OWN agent, on YOUR OWN repo. It reads the JSONL transcripts Claude
//! Code writes under `~/.claude/projects/<repo>/<session>.jsonl`, pulls the
//! agent's work-report turns, and verifies each against the repo.
//!
//! THE METHODOLOGY THAT MATTERS — time alignment. A transcript from last week
//! captured claims about the code AS IT WAS THEN. Checking them against HEAD
//! today would falsely accuse the agent of lying every time the repo moved on
//! (a function it correctly removed gets re-added; a route it added gets
//! renamed). truth must NEVER cry wolf — so each turn is checked against the
//! commit that was HEAD at the moment the turn was written (`git rev-list -1
//! --before=<ts>`), via a throwaway git worktree. Only then is "agent said X,
//! code says not-X" a real over-claim and not an artifact of history.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config_util::load_config;
use crate::verify_turn::{self, TurnReport};

/// A caught over-claim: (session short id, repo path, [(claim text, citation)]).
type SessionHits = (String, String, Vec<(String, Option<String>)>);

/// One agent turn pulled from a transcript: what it claimed and when.
struct Turn {
    /// ISO-8601 timestamp the assistant wrote this (for commit alignment).
    timestamp: String,
    /// The agent's text (work report).
    text: String,
}

/// A session's worth of turns against one repo.
struct Session {
    id: String,
    repo: PathBuf,
    /// Newest-message timestamp, for sorting sessions newest-first.
    latest: String,
    turns: Vec<Turn>,
}

/// Options parsed from the CLI.
pub struct AuditOpts {
    /// How many recent sessions to audit (default a handful).
    pub last: usize,
    /// Restrict to sessions whose repo is this path (default: all repos).
    pub repo: Option<String>,
    pub json: bool,
}

impl Default for AuditOpts {
    fn default() -> Self {
        Self {
            last: 5,
            repo: None,
            json: false,
        }
    }
}

/// Is this assertional WORK-REPORT prose ("I added X", "set Y to 5"), or is it
/// reasoning/quoting/listing that merely MENTIONS a checkable fact? Auditing the
/// latter is a false-positive factory: an agent writing `"nobody uses
/// /v1/checkout"` as a quoted example, or "the README says we retry 3 times",
/// is NOT claiming that about the current code. We only audit lines that read as
/// the agent reporting its OWN work in the first person.
fn is_work_report_line(line: &str) -> bool {
    let t = line.trim();
    // Quoted text and markdown furniture are discussion, not a self-report.
    if t.starts_with('>') || t.starts_with('|') || t.starts_with('#') {
        return false;
    }
    // Shell-command examples ("$ truth verify-turn ...") and any line invoking
    // truth itself are demos of the tool, not the agent reporting its work —
    // their quoted `--message "I added X"` payloads leaked as fake claims.
    if t.starts_with('$') || t.starts_with("```") {
        return false;
    }
    let lower_all = t.to_ascii_lowercase();
    // Tool invocations only — NOT a bare `verify_turn`, which is a real symbol
    // an agent legitimately claims to add/remove ("I removed verify_turn").
    if lower_all.contains("truth verify-turn")
        || lower_all.contains("truth check")
        || lower_all.contains("--message")
        || lower_all.contains("verify-turn --")
    {
        return false;
    }
    // A line that is mostly inside quotes/backticks is quoting, not asserting.
    let quote_chars = t.chars().filter(|&c| c == '"' || c == '`').count();
    if quote_chars >= 2 {
        return false;
    }
    // A continuation line that CLOSES a quoted block (a multi-line
    // `--message "I added X,\n removed Y"` demo) ends in a stray quote and has
    // no opener — it's the tail of a quote, not a self-report.
    if (t.ends_with('"') || t.ends_with("\",") || t.ends_with("\".")) && quote_chars == 1 {
        return false;
    }
    let lower = &lower_all;
    // Meta-discussion about claims/verdicts/examples — never a work report.
    const META: &[&str] = &[
        "for example",
        "e.g.",
        "such as",
        "would be",
        "could be",
        "the readme says",
        "the docs say",
        "imagine",
        "suppose",
        "contradicted",
        "supported",
        "refused",
        "verdict",
    ];
    if META.iter().any(|m| lower.contains(m)) {
        return false;
    }
    // First-person past-tense work signal: the agent reporting what it did.
    const REPORT: &[&str] = &[
        "i added",
        "i removed",
        "i deleted",
        "i changed",
        "i set",
        "i updated",
        "i created",
        "i renamed",
        "i fixed",
        "i wired",
        "i bumped",
        "i lowered",
        "i raised",
        "i moved",
        "i implemented",
        "i only changed",
        "i edited",
        "added the",
        "removed the",
        "set the",
        "renamed ",
        "tests pass",
        "it compiles",
        "clippy clean",
    ];
    REPORT.iter().any(|r| lower.contains(r))
}

/// Keep only the work-report lines of a turn's text (joined back into prose for
/// the segmenter). Empty result → nothing assertional to audit.
fn work_report_text(text: &str) -> String {
    text.lines()
        .filter(|l| is_work_report_line(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Root of the Claude Code transcript store.
fn projects_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude").join("projects"))
}

/// Pull assistant turns (timestamp + text + cwd) from one transcript. Only keeps
/// turns with non-empty prose — tool-only turns carry no claims.
fn read_turns(transcript: &Path) -> Vec<(String, String, Option<String>)> {
    let Ok(content) = std::fs::read_to_string(transcript) else {
        return vec![];
    };
    let mut out = vec![];
    for line in content.lines() {
        let Ok(entry): std::result::Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let cwd = entry
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let Some(message) = entry.get("message") else {
            continue;
        };
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
            out.push((ts, text, cwd));
        }
    }
    out
}

/// Discover sessions, newest-first, each mapped to its repo via the `cwd`
/// recorded in the transcript (more reliable than decoding the dir name).
fn discover_sessions(opts: &AuditOpts) -> Result<Vec<Session>> {
    let root = projects_dir().context("cannot locate ~/.claude/projects")?;
    if !root.is_dir() {
        anyhow::bail!(
            "no Claude Code transcripts found at {} — has Claude Code run on this machine?",
            root.display()
        );
    }

    let mut sessions = vec![];
    for project in read_dir_sorted(&root) {
        for transcript in read_dir_sorted(&project) {
            if transcript.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let raw = read_turns(&transcript);
            if raw.is_empty() {
                continue;
            }
            // Repo = the cwd recorded on the turns. Skip if it isn't a git repo
            // on disk now (we can't time-travel a repo that's gone).
            let repo = raw
                .iter()
                .filter_map(|(_, _, cwd)| cwd.clone())
                .next_back()
                .map(PathBuf::from);
            let Some(repo) = repo else { continue };
            if !repo.join(".git").exists() {
                continue;
            }
            if let Some(filter) = &opts.repo {
                if repo != Path::new(filter) {
                    continue;
                }
            }
            let latest = raw.last().map(|(ts, _, _)| ts.clone()).unwrap_or_default();
            // Keep only the assertional WORK-REPORT content of each turn — the
            // agent reporting what it did, not reasoning/quoting/listing about
            // checkable facts (auditing those is 81% false positives, measured).
            // Turns with nothing self-reported are dropped.
            let turns: Vec<Turn> = raw
                .into_iter()
                .filter_map(|(timestamp, text, _)| {
                    let report = work_report_text(&text);
                    (!report.trim().is_empty()).then_some(Turn {
                        timestamp,
                        text: report,
                    })
                })
                .collect();
            sessions.push(Session {
                id: transcript
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string(),
                repo,
                latest,
                turns,
            });
        }
    }

    sessions.sort_by(|a, b| b.latest.cmp(&a.latest));
    sessions.truncate(opts.last);
    Ok(sessions)
}

fn read_dir_sorted(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

/// The commit that was HEAD in `repo` at-or-before `iso_ts`. None if the repo
/// has no commit that old (the session predates the repo's history here).
fn commit_at(repo: &Path, iso_ts: &str) -> Option<String> {
    if iso_ts.is_empty() {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "-1", &format!("--before={iso_ts}"), "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Create a detached worktree of `repo` at `commit` in a temp dir, run `f`
/// against it, then remove it. Keeps the user's working tree untouched.
fn with_worktree<T>(repo: &Path, commit: &str, f: impl FnOnce(&Path) -> T) -> Result<T> {
    let wt = std::env::temp_dir().join(format!("truth-audit-{commit}"));
    // Best-effort cleanup of a stale dir from a previous interrupted run.
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(&wt)
        .output();
    let _ = std::fs::remove_dir_all(&wt);

    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach", "--quiet"])
        .arg(&wt)
        .arg(commit)
        .output()
        .context("git worktree add")?;
    if !add.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }

    let result = f(&wt);

    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(&wt)
        .output();
    let _ = std::fs::remove_dir_all(&wt);
    Ok(result)
}

/// Verify one turn against the repo AS IT WAS at the turn's timestamp.
fn audit_turn(repo: &Path, turn: &Turn) -> Result<Option<TurnReport>> {
    let Some(commit) = commit_at(repo, &turn.timestamp) else {
        return Ok(None); // No commit that old → can't honestly check.
    };
    let text = turn.text.clone();
    with_worktree(repo, &commit, |wt| {
        // Index that snapshot into its own ephemeral store and verify against it.
        let mut config = load_config().unwrap_or_default();
        verify_turn::retarget_repo(&mut config, &wt.to_string_lossy());
        let conn = truth_db::open(&config.database.path).ok()?;
        verify_turn::auto_refresh_index(&conn, &config);
        verify_turn::verify(&conn, &config, &text, None).ok()
    })
}

/// Aggregate counts across the audit.
#[derive(Default)]
struct Tally {
    sessions: usize,
    turns_checked: usize,
    claims: usize,
    contradicted: usize,
    all_refused_turns: usize,
}

pub fn audit(opts: &AuditOpts) -> Result<()> {
    let sessions = discover_sessions(opts)?;
    if sessions.is_empty() {
        println!(
            "No auditable Claude Code sessions found{}.",
            opts.repo
                .as_deref()
                .map(|r| format!(" for repo {r}"))
                .unwrap_or_default()
        );
        return Ok(());
    }

    let mut tally = Tally::default();
    // (session short id, repo, list of (claim text, citation)) for contradictions.
    let mut hits: Vec<SessionHits> = vec![];

    for s in &sessions {
        tally.sessions += 1;
        let mut session_hits: Vec<(String, Option<String>)> = vec![];
        for turn in &s.turns {
            let Some(report) = audit_turn(&s.repo, turn)? else {
                continue;
            };
            tally.turns_checked += 1;
            tally.claims += report.verdicts.len();
            tally.contradicted += report.contradicted();
            if report.is_wall_of_refusals() {
                tally.all_refused_turns += 1;
            }
            for v in &report.verdicts {
                if v.checkable && v.status == truth_core::enums::VerdictStatus::Contradicted {
                    session_hits.push((v.text.clone(), v.citation.clone()));
                }
            }
        }
        if !session_hits.is_empty() {
            hits.push((
                s.id.chars().take(8).collect(),
                s.repo.to_string_lossy().into_owned(),
                session_hits,
            ));
        }
    }

    if opts.json {
        print_json(&tally, &hits, &sessions);
        return Ok(());
    }

    println!("truth audit-session — your agent's claims, checked against the code at the time\n");
    for (id, repo, claims) in &hits {
        let name = Path::new(repo)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(repo);
        println!("  session {id}…  ({name})");
        for (text, cite) in claims {
            let c = cite
                .as_deref()
                .map(|c| format!("  → {c}"))
                .unwrap_or_default();
            println!("  ✗ {text}{c}");
        }
        println!();
    }
    println!(
        "  audited {} session(s) · {} turn(s) · {} claim(s) · {} over-claim(s) caught",
        tally.sessions, tally.turns_checked, tally.claims, tally.contradicted
    );
    if tally.all_refused_turns > 0 {
        println!(
            "  {} turn(s) said nothing checkable at all (vague work reports).",
            tally.all_refused_turns
        );
    }
    if tally.contradicted == 0 {
        println!("\n  No over-claims — every checkable claim matched the code at the time. ✓");
    } else {
        println!(
            "\n  → {} time(s) the agent told you something the code-at-the-time contradicted.",
            tally.contradicted
        );
    }
    Ok(())
}

fn print_json(tally: &Tally, hits: &[SessionHits], sessions: &[Session]) {
    let by_repo: BTreeMap<&str, usize> = sessions.iter().fold(BTreeMap::new(), |mut m, s| {
        *m.entry(s.repo.to_str().unwrap_or("?")).or_default() += 1;
        m
    });
    let out = serde_json::json!({
        "sessions": tally.sessions,
        "turns_checked": tally.turns_checked,
        "claims": tally.claims,
        "contradicted": tally.contradicted,
        "all_refused_turns": tally.all_refused_turns,
        "repos": by_repo,
        "contradictions": hits.iter().map(|(id, repo, claims)| serde_json::json!({
            "session": id,
            "repo": repo,
            "claims": claims.iter().map(|(t, c)| serde_json::json!({
                "text": t, "citation": c,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_first_person_work_reports() {
        for line in [
            "I added the /v1/refund route to the router.",
            "I set MAX_RETRIES to 5 in config.rs.",
            "I removed the legacy verify_turn function.",
            "tests pass after the change.",
        ] {
            assert!(is_work_report_line(line), "should keep: {line}");
        }
    }

    #[test]
    fn drops_quoting_and_meta_discussion() {
        // The 81%-false-positive class: prose that MENTIONS a checkable fact
        // without the agent claiming it about the current code.
        for line in [
            "- `set INDEX_FORMAT_VERSION to 9` (it's not 9) ✓",
            "`depends on tokio` HAS the word \"depends on\"",
            "for example, \"nobody uses /v1/checkout\" would be Contradicted",
            "the README says we retry 3 times",
            "> entrar al panel de admin",
            "| `admin.js` | deployed |",
            "**Bug: dependency claims that are TRUE get contradicted.**",
            // Demo invocations of truth itself — their quoted --message payload
            // leaked as fake claims ("bumped the timeout to 30s") in the wild.
            "$ truth verify-turn --message \"I added /v1/refund, bumped the timeout to 30s\"",
            "running `truth check \"I set MAX_RETRIES to 5\"` returns Supported",
        ] {
            assert!(!is_work_report_line(line), "should drop: {line}");
        }
    }

    #[test]
    fn work_report_text_keeps_only_report_lines() {
        let turn = "Here's my analysis of the bug.\n\
                    For example `MAX_RETRIES is 3` would be wrong.\n\
                    I set MAX_RETRIES to 5 in src/config.rs.\n\
                    The verdict would be Supported.";
        let kept = work_report_text(turn);
        assert!(kept.contains("I set MAX_RETRIES to 5"));
        assert!(!kept.contains("For example"));
        assert!(!kept.contains("verdict"));
    }
}
