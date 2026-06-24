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

    /// The "wall of refusals" evasion (threat model #1): the single most
    /// effective way to dodge a verifier is to never make a checkable claim —
    /// report only judgments ("cleaner", "more robust", "should be faster").
    /// truth refuses those, correctly, but a turn that is ALL refusals while
    /// claiming substantive work is itself a signal: nothing the agent said
    /// could be checked. We flag it when several claim-shaped segments were
    /// found and NONE resolved to a Supported/Contradicted verdict.
    ///
    /// Deliberately conservative — needs ≥3 segments so short conversational
    /// turns ("Done, let me know.") don't trip it, and stays silent the moment
    /// anything was checkable (one Supported claim means the agent did make a
    /// falsifiable statement).
    pub fn is_wall_of_refusals(&self) -> bool {
        self.verdicts.len() >= 3 && self.supported() == 0 && self.contradicted() == 0
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "message": self.message,
            "summary": {
                "supported": self.supported(),
                "contradicted": self.contradicted(),
                "refused": self.refused(),
                "claims": self.verdicts.len(),
                "all_refused": self.is_wall_of_refusals(),
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
    // Markdown furniture isn't prose: table rows ("| cell | ✅ |") and
    // blockquotes ("> someone else's words") judged as claims produced
    // garbage verdicts in the wild.
    let trimmed = s.trim_start();
    if trimmed.starts_with('|') || trimmed.starts_with('>') {
        return false;
    }
    // Intent prose is a plan, not an assertion: "Let me verify X", "confirm
    // the helper exists" describe what the agent is ABOUT to check, and
    // judging them as claims produced false verdicts in the wild. (Past tense
    // — "verified that X" — still counts as a claim.)
    let lower = trimmed.to_ascii_lowercase();
    const INTENT: &[&str] = &[
        "let me ",
        "let's ",
        "confirm ",
        "verify ",
        "check ",
        "checking ",
        "ensure ",
        "i'll ",
        "i will ",
        "going to ",
        "to confirm ",
        "to verify ",
        "to check ",
        "now i ",
    ];
    if INTENT.iter().any(|p| lower.starts_with(p)) {
        return false;
    }
    // Meta-prose: the agent talking ABOUT claims/verdicts rather than asserting
    // a fact ("noted in memory: truth still FPs the import subject", "this was a
    // false positive", "the loop closed"). Judging these as claims is what
    // produced the self-referential false-accusations when verify-turn ran over
    // a SUMMARY of the work instead of a change report. These phrases never
    // appear in a genuine "I changed X" assertion, so dropping them is safe.
    const META: &[&str] = &[
        "false positive",
        "false accusation",
        "false pass",
        "over-claim",
        "overclaim",
        " fps ",
        " fp ",
        "noted in memory",
        "in memory as",
        "the loop ",
        "this means",
        "what made that",
        "calibration",
        "regression test",
        "recorded via",
        "the safety property",
    ];
    if META.iter().any(|p| lower.contains(p)) {
        return false;
    }
    // A quoted example claim ("...as a dependency cue") is the agent CITING a
    // sentence, not asserting it. When most of the segment sits inside quotes
    // it's an example under discussion, not the agent's own claim.
    if is_quoted_example(s) {
        return false;
    }
    // Concrete, checkable anchors. A genuine change claim names a path, a
    // number, an ALL_CAPS constant, a backticked identifier, a change verb, or a
    // command-success word. Narrative recap ("it drove a real engine fix") has
    // none of these.
    let lower = s.to_ascii_lowercase();
    let has_path = s.contains('/');
    let has_number = s.chars().any(|c| c.is_ascii_digit());
    let has_const = s
        .split_whitespace()
        .any(|w| w.len() >= 3 && w.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
    let has_backtick = s.contains('`');
    let has_verb = ["added", "removed", "deleted", "dropped", "created", "wired"]
        .iter()
        .any(|v| lower.contains(v));
    // Command-success clauses ("tests pass", "clippy clean") are checkable
    // against recorded run receipts, so terse ones survive segmentation.
    let has_success = ["pass", "green", "compiles", "clean"]
        .iter()
        .any(|v| lower.contains(v));
    let has_anchor = has_path || has_number || has_const || has_backtick || has_verb || has_success;

    // A segment is a claim only if it carries a concrete anchor OR is phrased as
    // the agent reporting its own action ("I …", "we …"). Everything else is
    // narrative/recap ("the fix is the gate", "it drove a real engine fix", "not
    // endless per-word patches") — judging it as a claim is what produced the
    // self-referential false-accusations when verify-turn ran over a summary.
    // This replaces a permissive "any 3+ word segment is a claim" rule, which
    // admitted anchorless prose wholesale.
    let agent_led = lower.starts_with("i ")
        || lower.starts_with("i'")
        || lower.starts_with("we ")
        || lower.starts_with("we'");
    has_anchor || agent_led
}

/// Whether a segment carries a quoted example — the agent citing a sentence
/// (e.g. discussing a claim phrasing) rather than asserting it. True when a
/// double-quoted span either dominates the segment OR is itself a multi-word
/// phrase: a ≥3-word quote inside narration ("my sentence \"added import as a
/// cue\" fired") is a citation, not the agent's own claim, even when trailing
/// prose dilutes it below half.
fn is_quoted_example(s: &str) -> bool {
    let first = s.find('"');
    let last = s.rfind('"');
    match (first, last) {
        (Some(a), Some(b)) if b > a + 1 => {
            let inside_str = &s[a + 1..b];
            let inside = inside_str.chars().filter(|c| !c.is_whitespace()).count();
            let total = s.chars().filter(|c| !c.is_whitespace()).count();
            let dominates = total > 0 && inside * 2 >= total;
            let multiword_quote = inside_str.split_whitespace().count() >= 3;
            dominates || multiword_quote
        }
        _ => false,
    }
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

/// Bring the index up to date with the working tree before a verdict, so checks
/// never run against a stale snapshot (the root cause behind several symbol/
/// usage false-contradictions). Incremental — skips unchanged files via one
/// parallel hash pass (~10-50ms on a clean repo) — UNLESS the index format
/// changed (after a truth upgrade), where a full rebuild re-stamps the version
/// so we don't silently serve old-format evidence. Best-effort: returns whether
/// it refreshed; on failure the caller's freshness warning still fires.
pub fn auto_refresh_index(conn: &Connection, config: &Config) -> bool {
    let format_stale = truth_db::index_format_is_stale(conn).unwrap_or(false);
    let refreshed = truth_indexer::index_repo_opts(
        conn,
        &config.repo,
        Some(std::path::Path::new(&config.repo.root)),
        !format_stale, // incremental unless the format changed → full rebuild
        config.indexer.extractor,
    )
    .is_ok();
    if refreshed {
        let _ = truth_db::set_index_format_version(conn);
    }
    refreshed
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
    let auto_indexed = auto_refresh_index(conn, config);

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
    // otherwise fall back to segmenting the raw message ourselves. The PROVENANCE
    // of each claim decides whether it may contradict (phase 3):
    //   - agent-supplied claims  → the LLM that wrote the code parsed its own
    //     sentence; trust it, full verdict power including Contradicted.
    //   - our own prose-segmentation → the regex extractor's FP surface. A
    //     contradiction here is trusted ONLY when it rests on a STRUCTURED fact
    //     (a file present/absent in the diff, an AST symbol, an exact value, a
    //     command exit code) — `decision.structured`. A contradiction NOT so
    //     backed (a count, or a verdict on a possibly-mis-extracted subject) is
    //     downgraded to Inconclusive: that's the false-accusation surface the
    //     precision gate drives to zero. After phase 2 counts no longer
    //     contradict at all, so what survives for prose is exactly the sound,
    //     binary-fact contradictions ("I removed X" but X is still in the diff).
    let (segments, from_prose): (Vec<String>, bool) = match claims {
        Some(list) => (
            list.iter()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .collect(),
            false,
        ),
        None => (segment(message), true),
    };

    let mut verdicts = Vec::new();
    for seg in segments {
        let outcome = run_check(conn, config, &seg, Trigger::Cli, local_log)?;
        let citation = first_citation(&outcome.evidence);
        // Prose-provenance gate: a contradiction from our own segmentation holds
        // only when the verdict engine tagged it `structured` — a binary fact
        // (diff presence/absence, AST/index symbol or route, exact value, command
        // exit code). A non-structured contradiction (a count, after phase 2
        // already downgraded those) is the false-accusation surface and refuses.
        // The engine decides structured-ness, not a string-sniff here.
        let status = if from_prose
            && outcome.decision.status == VerdictStatus::Contradicted
            && !outcome.decision.structured
        {
            VerdictStatus::Inconclusive
        } else {
            outcome.decision.status
        };
        verdicts.push(ClaimVerdict {
            text: seg,
            status,
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
    // Wall-of-refusals signal: nothing the agent claimed was checkable. Not a
    // contradiction (so it doesn't block), but worth surfacing — staying vague
    // is the easiest way to evade verification.
    if report.is_wall_of_refusals() {
        out.push_str(
            "\n  ⚠ Nothing in this turn was checkable — every claim refused. \
             The agent described work without stating anything verifiable.\n",
        );
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

    #[test]
    fn intent_prose_is_not_a_claim() {
        // Plans aren't assertions — judging them produced false verdicts in
        // the wild ("Let me verify X" / "confirm the helper exists").
        let segs = segment(
            "Let me verify the transitionState lacks a guard. confirm the timing-safe helper exists. \
             Checking whether forUpdate is available. I set MAX_RETRIES to 5.",
        );
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert!(segs[0].contains("MAX_RETRIES"));
        // Past tense still counts as a claim.
        let done = segment("I verified that MAX_RETRIES is 5 in src/config.rs");
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn meta_prose_about_claims_is_not_a_claim() {
        // Summaries/discussion ABOUT truth's own verdicts are not assertions
        // about code — judging them produced self-referential false accusations
        // when verify-turn ran over a summary rather than a change report.
        let segs = segment(
            "noted in memory: truth still FPs the bare-word import subject. \
             this was a false positive in the calibration. the loop closed. \
             I set MAX_RETRIES to 5 in src/config.rs",
        );
        assert_eq!(
            segs.len(),
            1,
            "only the real change claim survives: {segs:?}"
        );
        assert!(segs[0].contains("MAX_RETRIES"));
        // A quoted example claim is a citation, not an assertion.
        let quoted = segment("my summary sentence \"added import as a dependency cue\" fired");
        assert!(
            !quoted.iter().any(|s| s.contains("dependency cue")),
            "quoted example must not be a claim: {quoted:?}"
        );
    }

    #[test]
    fn markdown_furniture_is_not_a_claim() {
        // Table rows and blockquotes judged as claims produced garbage
        // verdicts in the wild.
        let segs = segment(
            "| `admin.js` → `public/js/` | ✅ Deployed |\n\
             > entrar al panel de admin y probar\n\
             I set MAX_RETRIES to 5 in src/config.rs",
        );
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert!(segs[0].contains("MAX_RETRIES"));
    }

    #[test]
    fn summary_prose_corpus_does_not_leak_claims() {
        // CALIBRATION INSTRUMENT for the false-ACCUSATION side. Each entry is
        // real assistant SUMMARY prose (the kind that fired self-referential
        // false-contradictions this session when verify-turn ran over a recap
        // instead of a change report). The contract: a summary that asserts NO
        // concrete code change must segment to ZERO surviving claims — so the
        // engine can't contradict narrative. `max_claims` is the ceiling; a
        // regression that re-opens the gate trips this. Real embedded change
        // claims (the control cases) MUST still survive.
        struct Case {
            prose: &'static str,
            max_claims: usize,
            must_contain: Option<&'static str>,
        }
        let cases = [
            // --- pure narrative: must leak NOTHING ---
            Case {
                prose: "noted in memory: truth still FPs the bare-word import \
                        subject (e.g. my own summary sentence \"added import as a \
                        dependency cue\") — a narrower sub-case than the one fixed",
                max_claims: 0,
                must_contain: None,
            },
            Case {
                prose: "The loop closed concretely: this dataset didn't just \
                        measure over-claims, it drove a real engine fix. This is \
                        exactly the calibration cycle the README promises.",
                max_claims: 0,
                must_contain: None,
            },
            Case {
                prose: "What made that possible: the safety property holds on a \
                        now-meaningful sample, and zero false passes were observed \
                        across the calibration.",
                max_claims: 0,
                must_contain: None,
            },
            Case {
                prose: "truth's weakness is false accusations on prose, not false \
                        passes. The fix is the gate, not endless per-word patches.",
                max_claims: 0,
                must_contain: None,
            },
            // --- control: narrative WRAPPING a real change claim. The claim
            //     must survive; the surrounding recap must not inflate the count. ---
            Case {
                prose: "To recap what made that possible, I set MAX_RETRIES to 5 \
                        in src/config.rs, and the calibration loop confirmed it.",
                max_claims: 1,
                must_contain: Some("MAX_RETRIES"),
            },
        ];
        for c in &cases {
            let segs = segment(c.prose);
            assert!(
                segs.len() <= c.max_claims,
                "summary leaked {} claim(s) (max {}): {segs:?}\n  prose: {}",
                segs.len(),
                c.max_claims,
                c.prose,
            );
            if let Some(needle) = c.must_contain {
                assert!(
                    segs.iter().any(|s| s.contains(needle)),
                    "control case lost its real claim `{needle}`: {segs:?}",
                );
            }
        }
    }

    fn report(verdicts: Vec<ClaimVerdict>) -> TurnReport {
        TurnReport {
            message: String::new(),
            verdicts,
            freshness: Freshness {
                repo: ".".into(),
                artifacts: 5,
                last_indexed_at: Some(0),
                age_secs: Some(1),
                auto_indexed: true,
            },
        }
    }
    fn refused(text: &str) -> ClaimVerdict {
        ClaimVerdict {
            text: text.into(),
            status: VerdictStatus::Inconclusive,
            confidence: 0.0,
            checkable: false,
            citation: None,
        }
    }
    fn supported(text: &str) -> ClaimVerdict {
        ClaimVerdict {
            text: text.into(),
            status: VerdictStatus::Supported,
            confidence: 0.9,
            checkable: true,
            citation: None,
        }
    }

    #[test]
    fn all_refused_turn_is_flagged() {
        // Three claim-shaped segments, none checkable → the wall-of-refusals
        // evasion (#1): the agent described work without saying anything
        // falsifiable.
        let r = report(vec![
            refused("I cleaned up the error handling"),
            refused("made the flow more robust"),
            refused("the code is more maintainable now"),
        ]);
        assert!(r.is_wall_of_refusals());
        assert!(render_text(&r).contains("Nothing in this turn was checkable"));
    }

    #[test]
    fn one_checkable_claim_silences_the_signal() {
        // A single Supported claim means the agent DID make a falsifiable
        // statement — not a wall of refusals, even amid vague prose.
        let r = report(vec![
            refused("I cleaned things up"),
            refused("it's more robust"),
            supported("set MAX_RETRIES to 5"),
        ]);
        assert!(!r.is_wall_of_refusals());
    }

    #[test]
    fn short_conversational_turn_does_not_trip() {
        // Below the 3-segment floor: a brief "done" turn is not flagged.
        let r = report(vec![refused("done"), refused("let me know")]);
        assert!(!r.is_wall_of_refusals());
    }
}
