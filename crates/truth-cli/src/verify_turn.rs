//! `truth verify-turn` — fact-check what an AI coding agent said about its work.
//!
//! An agent doesn't make one claim; it emits a paragraph ("I added /v1/refund,
//! bumped the timeout to 30s, removed the old checkout handler, and tests
//! pass"). This command splits that message into candidate claims, runs each
//! through the existing deterministic check pipeline against the indexed repo +
//! working-tree diff + logs, and returns a per-claim cited verdict.
//!
//! It is deliberately conservative: a segment that doesn't resolve to a
//! concrete, checkable claim is reported as **Refused** (unverifiable), never
//! guessed. Refusing the agent's "tests pass" / "much cleaner" is correct, not a
//! gap.

use crate::check::run_check;
use crate::config_util::load_config;
use crate::service::EvidenceJson;
use anyhow::Result;
use rusqlite::Connection;
use truth_core::config::Config;
use truth_core::enums::{Trigger, VerdictStatus};

/// One segment of the agent message and its verdict.
pub struct ClaimVerdict {
    pub text: String,
    pub status: VerdictStatus,
    pub confidence: f32,
    /// Whether the claim was checkable at all (false → refused/unverifiable).
    pub checkable: bool,
    pub citation: Option<String>,
}

/// How trustworthy the underlying index is for this verification. A verifier
/// that silently checks against an empty or stale index can FALSE-PASS, which is
/// worse than no verifier — so we make the index state explicit in every report.
#[derive(Debug, Clone)]
pub struct Freshness {
    /// The repo root whose `.truth` store was queried.
    pub repo: String,
    pub artifacts: i64,
    pub last_indexed_at: Option<i64>,
    /// Age of the index in seconds at check time, if known.
    pub age_secs: Option<i64>,
    /// Whether verify auto-refreshed the index (incremental) before checking,
    /// so index-backed claims reflect the current working tree.
    pub auto_indexed: bool,
}

impl Freshness {
    pub fn is_empty(&self) -> bool {
        self.artifacts == 0
    }
    /// Stale if older than 24h. Diff claims are still fresh (read live), but
    /// index-backed claims (usage/exists/config) may be out of date.
    pub fn is_stale(&self) -> bool {
        self.age_secs.map(|a| a > 24 * 3600).unwrap_or(false)
    }
    /// A human-facing warning when the index can't be fully trusted.
    pub fn warning(&self) -> Option<String> {
        if self.is_empty() {
            Some(format!(
                "index is EMPTY for {} — only working-tree diff claims were checked; \
                 run `truth index {}` so repo/usage/config claims can be verified.",
                self.repo, self.repo
            ))
        } else if self.is_stale() {
            let days = self.age_secs.unwrap_or(0) / 86_400;
            Some(format!(
                "index for {} is ~{}d old — index-backed claims may be out of date; \
                 re-run `truth index {}`.",
                self.repo, days, self.repo
            ))
        } else {
            None
        }
    }
}

/// Aggregate outcome for a whole agent turn.
pub struct TurnReport {
    pub message: String,
    pub verdicts: Vec<ClaimVerdict>,
    pub freshness: Freshness,
}

impl TurnReport {
    pub fn supported(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| v.status == VerdictStatus::Supported)
            .count()
    }
    pub fn contradicted(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| v.status == VerdictStatus::Contradicted)
            .count()
    }
    /// Refused = anything we couldn't turn into a checkable verdict.
    pub fn refused(&self) -> usize {
        self.verdicts
            .iter()
            .filter(|v| !v.checkable || v.status == VerdictStatus::Inconclusive)
            .count()
    }

    /// Did the agent assert something the evidence contradicts?
    pub fn has_contradiction(&self) -> bool {
        self.contradicted() > 0
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "message": self.message,
            "summary": {
                "supported": self.supported(),
                "contradicted": self.contradicted(),
                "refused": self.refused(),
                "claims": self.verdicts.len(),
            },
            "index": {
                "repo": self.freshness.repo,
                "artifacts": self.freshness.artifacts,
                "empty": self.freshness.is_empty(),
                "stale": self.freshness.is_stale(),
                "auto_indexed": self.freshness.auto_indexed,
                "last_indexed_at": self.freshness.last_indexed_at,
                "age_secs": self.freshness.age_secs,
                "warning": self.freshness.warning(),
            },
            "claims": self.verdicts.iter().map(|v| serde_json::json!({
                "text": v.text,
                "status": if v.checkable { v.status.as_db_str() } else { "refused" },
                "checkable": v.checkable,
                "confidence": v.confidence,
                "citation": v.citation,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Split an agent message into candidate claim segments.
///
/// Agents write prose, often on one line, joining independent assertions with
/// punctuation and conjunctions ("I added X, removed Y, and bumped Z"). We split
/// on sentence terminators, newlines, semicolons, and a small set of clause
/// conjunctions, then keep only segments long enough to plausibly be a claim.
pub fn segment(message: &str) -> Vec<String> {
    // Normalize hard breaks to a sentinel, then split on it. A '.' only ends a
    // sentence when followed by whitespace or end-of-text — otherwise it's part
    // of a token ("src/config.rs", "3.5") and must survive segmentation.
    let mut buf = String::with_capacity(message.len());
    let chars: Vec<char> = message.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        match ch {
            '.' => {
                let next = chars.get(i + 1);
                if next.is_none_or(|c| c.is_whitespace()) {
                    buf.push('\u{1}');
                } else {
                    buf.push(ch);
                }
            }
            '!' | '?' | ';' | '\n' | '\r' | ',' => buf.push('\u{1}'),
            _ => buf.push(ch),
        }
    }
    // Also break on clause conjunctions surrounded by spaces.
    let lowered = buf.clone();
    for conj in [" and ", " then ", " also ", " but ", " plus "] {
        // Case-insensitive replace by scanning the lowercased copy positions.
        replace_ci(&mut buf, &lowered, conj, "\u{1}");
    }

    buf.split('\u{1}')
        .map(|s| s.trim())
        .filter(|s| is_plausible_claim(s))
        .map(|s| s.to_string())
        .collect()
}

/// Replace occurrences of `needle` (matched case-insensitively against
/// `lowered`, a same-length lowercase copy of `buf`) with `repl` inside `buf`.
fn replace_ci(buf: &mut String, lowered: &str, needle: &str, repl: &str) {
    let needle_l = needle.to_ascii_lowercase();
    let lower_now = buf.to_ascii_lowercase();
    let _ = lowered; // kept for signature symmetry / future caching
    let mut result = String::with_capacity(buf.len());
    let bytes = buf.as_bytes();
    let mut i = 0;
    while i < buf.len() {
        if lower_now[i..].starts_with(&needle_l) {
            result.push_str(repl);
            i += needle.len();
        } else {
            // Push the current char (handle UTF-8 boundaries safely).
            let ch_len = utf8_len(bytes[i]);
            result.push_str(&buf[i..i + ch_len]);
            i += ch_len;
        }
    }
    *buf = result;
}

fn utf8_len(b: u8) -> usize {
    match b {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// A segment is a plausible claim if it carries a checkable signal. We keep
/// terse clauses like "removed /v1/checkout" (2 words) when they contain a
/// concrete subject — a `/path`, an ALL_CAPS constant, a number, or a
/// change verb — and otherwise require a few words so bare "done"/"ok" drop.
fn is_plausible_claim(s: &str) -> bool {
    let words = s.split_whitespace().count();
    // A single token ("+4", "done", "8080") is never a self-contained claim —
    // comma-segmented prose produces these constantly.
    if !(2..=40).contains(&words) {
        return false;
    }
    if words >= 3 {
        return true;
    }
    // Short clause: keep only if it has a concrete, checkable signal.
    let lower = s.to_ascii_lowercase();
    let has_path = s.contains('/');
    let has_number = s.chars().any(|c| c.is_ascii_digit());
    let has_const = s
        .split_whitespace()
        .any(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
    let has_verb = ["added", "removed", "deleted", "dropped", "created", "wired"]
        .iter()
        .any(|v| lower.contains(v));
    // Command-success clauses ("tests pass", "clippy clean") are checkable
    // against recorded run receipts, so terse ones survive segmentation.
    let has_success = ["pass", "green", "compiles", "clean"]
        .iter()
        .any(|v| lower.contains(v));
    has_path || has_number || has_const || has_verb || has_success
}

/// Run a full turn verification against the indexed repo / diff / logs.
///
/// Convenience wrapper: derive claims from `message` by segmenting it.
pub fn verify(
    conn: &Connection,
    config: &Config,
    message: &str,
    local_log: Option<&str>,
) -> Result<TurnReport> {
    verify_claims(conn, config, message, None, local_log)
}

/// Verify a turn. If `claims` is `Some`, those agent-provided claim strings are
/// checked directly (one verdict each) — the calling LLM did the parsing, which
/// is more reliable than our regex segmenter and costs nothing extra since the
/// agent is already mid-turn. If `claims` is `None`, the raw `message` is
/// segmented and parsed here as a fallback. The deterministic engine still
/// decides every verdict from real evidence — the agent only supplies phrasing.
pub fn verify_claims(
    conn: &Connection,
    config: &Config,
    message: &str,
    claims: Option<&[String]>,
    local_log: Option<&str>,
) -> Result<TurnReport> {
    // Auto-refresh the index before checking. The agent calling this has no idea
    // it needs to run `truth index` first, and shouldn't — a verifier must keep
    // its own data current. The incremental pass skips unchanged files (one
    // parallel hash pass, ~10-50ms on a clean repo), so this is nearly free and
    // means index-backed claims ("X is unused", "MAX_RETRIES is 5") reflect the
    // CURRENT working tree, not a stale snapshot.
    //
    // BUT: if the index was built by a binary with a different INDEX FORMAT
    // (e.g. after upgrading truth, when extraction logic changed), an
    // incremental pass would keep the old binary's evidence for unchanged files
    // — silently serving stale-format data. In that case we force a FULL rebuild
    // and re-stamp the format version. Best-effort throughout: on failure we
    // fall through and the freshness warning still fires.
    let format_stale = truth_db::index_format_is_stale(conn).unwrap_or(false);
    let auto_indexed = truth_indexer::index_repo_opts(
        conn,
        &config.repo,
        Some(std::path::Path::new(&config.repo.root)),
        !format_stale, // incremental unless the format changed → full rebuild
        config.indexer.extractor,
    )
    .is_ok();
    if auto_indexed {
        let _ = truth_db::set_index_format_version(conn);
    }

    // Capture index freshness AFTER the refresh so the report reflects current
    // state — a verifier that silently checks against no data must not look
    // "clean".
    let status = truth_db::repo::index_status(conn)?;
    let age_secs = status
        .last_indexed_at
        .map(|t| (truth_core::now_secs() - t).max(0));
    let freshness = Freshness {
        repo: config.repo.root.clone(),
        artifacts: status.artifacts,
        last_indexed_at: status.last_indexed_at,
        age_secs,
        auto_indexed,
    };

    // Agent-provided structured claims take precedence (precise, no segmenting);
    // otherwise fall back to segmenting the raw message ourselves.
    let segments: Vec<String> = match claims {
        Some(list) => list
            .iter()
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect(),
        None => segment(message),
    };

    let mut verdicts = Vec::new();
    for seg in segments {
        let outcome = run_check(conn, config, &seg, Trigger::Cli, local_log)?;
        let citation = first_citation(&outcome.evidence);
        verdicts.push(ClaimVerdict {
            text: seg,
            status: outcome.decision.status,
            confidence: outcome.decision.confidence,
            checkable: outcome.claim.is_checkable,
            citation,
        });
    }
    Ok(TurnReport {
        message: message.to_string(),
        verdicts,
        freshness,
    })
}

fn first_citation(evidence: &[EvidenceJson]) -> Option<String> {
    evidence.iter().find_map(|e| e.citation.clone())
}

/// Render the verdict table as plain text.
pub fn render_text(report: &TurnReport) -> String {
    let mut out = String::new();
    out.push_str("truth verify-turn\n\n");
    for v in &report.verdicts {
        let (mark, label) = if !v.checkable || v.status == VerdictStatus::Inconclusive {
            ("—", "Refused".to_string())
        } else {
            match v.status {
                VerdictStatus::Supported => ("✓", "Supported".into()),
                VerdictStatus::Contradicted => ("✗", "Contradicted".into()),
                VerdictStatus::PartiallySupported => ("~", "Partial".into()),
                _ => ("—", "Refused".into()),
            }
        };
        let cite = v
            .citation
            .as_ref()
            .map(|c| format!("  ({c})"))
            .unwrap_or_default();
        out.push_str(&format!("  {mark} {label:<13} {}{cite}\n", v.text));
    }
    out.push_str(&format!(
        "\n  {} supported · {} contradicted · {} refused\n",
        report.supported(),
        report.contradicted(),
        report.refused()
    ));
    if report.has_contradiction() {
        out.push_str("\n  ⚠ The agent's message contradicts the evidence above.\n");
    }
    // Trust caveat: if the index is empty or stale, the index-backed verdicts
    // above are unreliable. Surface it loudly so a "clean" result isn't trusted
    // blindly.
    if let Some(w) = report.freshness.warning() {
        out.push_str(&format!("\n  ⚠ {w}\n"));
    }
    out.trim_end().to_string()
}

/// Point `config` at an explicit repo root so we open THAT repo's `.truth`
/// store and diff THAT working tree — never silently trusting the process CWD.
/// The DB lives at `<repo>/.truth/truth.sqlite`.
pub fn retarget_repo(config: &mut Config, repo: &str) {
    config.repo.root = repo.to_string();
    let db = std::path::Path::new(repo)
        .join(".truth")
        .join("truth.sqlite");
    config.database.path = db.to_string_lossy().into_owned();
}

/// CLI entry point for `truth verify-turn`.
pub fn verify_turn(
    message: &str,
    repo: Option<&str>,
    local_log: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut config = load_config()?;
    if let Some(r) = repo {
        retarget_repo(&mut config, r);
    }
    let conn = crate::commands::open_db(&config)?;
    let report = verify(&conn, &config, message, local_log)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else if report.verdicts.is_empty() {
        println!("No checkable claims found in the message.");
    } else {
        println!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_a_compound_agent_message() {
        let segs = segment(
            "I added the /v1/refund endpoint, bumped the timeout to 30s, \
             removed /v1/checkout, and the suite passes.",
        );
        // The three checkable clauses survive (including the terse 2-word
        // "removed /v1/checkout", kept because it carries a `/path`).
        assert!(segs.iter().any(|s| s.contains("/v1/refund")));
        assert!(segs.iter().any(|s| s.contains("timeout")));
        assert!(segs.iter().any(|s| s.contains("/v1/checkout")));
    }

    #[test]
    fn empty_index_warns_and_stale_index_warns() {
        let empty = Freshness {
            repo: ".".into(),
            artifacts: 0,
            last_indexed_at: Some(100),
            age_secs: Some(10),
            auto_indexed: true,
        };
        assert!(empty.is_empty());
        assert!(empty.warning().unwrap().contains("EMPTY"));

        let stale = Freshness {
            repo: ".".into(),
            artifacts: 5,
            last_indexed_at: Some(0),
            age_secs: Some(3 * 86_400),
            auto_indexed: true,
        };
        assert!(stale.is_stale());
        assert!(stale.warning().unwrap().contains("old"));

        let fresh = Freshness {
            repo: ".".into(),
            artifacts: 5,
            last_indexed_at: Some(0),
            age_secs: Some(60),
            auto_indexed: true,
        };
        assert!(fresh.warning().is_none());
    }

    #[test]
    fn retarget_points_db_under_repo() {
        let mut cfg = Config::default();
        retarget_repo(&mut cfg, "/work/proj");
        assert_eq!(cfg.repo.root, "/work/proj");
        assert!(
            cfg.database
                .path
                .ends_with("/work/proj/.truth/truth.sqlite")
                || cfg.database.path.contains("/work/proj/.truth")
        );
    }

    #[test]
    fn keeps_terse_path_clause_and_success_clause_drops_filler() {
        // "removed /v1/checkout" (2 words, has a path) is kept; "tests pass"
        // is now ALSO kept — it's checkable against recorded run receipts
        // (`truth run`). Bare filler like "done" still drops.
        let segs = segment("removed /v1/checkout, tests pass, done");
        assert!(segs.iter().any(|s| s.contains("/v1/checkout")));
        assert!(segs.iter().any(|s| s == "tests pass"));
        assert!(!segs.iter().any(|s| s == "done"));
    }

    #[test]
    fn drops_trivial_fragments() {
        let segs = segment("Done. OK. I set the retry count to 5.");
        assert_eq!(segs.len(), 1);
        assert!(segs[0].contains("retry count"));
    }

    #[test]
    fn splits_on_newlines_and_semicolons() {
        let segs = segment("I bumped the port to 8080;\nI added /v1/refund route");
        assert_eq!(segs.len(), 2);
    }
}
