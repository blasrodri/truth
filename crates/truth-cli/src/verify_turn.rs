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

/// Aggregate outcome for a whole agent turn.
pub struct TurnReport {
    pub message: String,
    pub verdicts: Vec<ClaimVerdict>,
}

impl TurnReport {
    pub fn supported(&self) -> usize {
        self.verdicts.iter().filter(|v| v.status == VerdictStatus::Supported).count()
    }
    pub fn contradicted(&self) -> usize {
        self.verdicts.iter().filter(|v| v.status == VerdictStatus::Contradicted).count()
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
    // Normalize hard breaks to a sentinel, then split on it.
    let mut buf = String::with_capacity(message.len());
    for ch in message.chars() {
        match ch {
            '.' | '!' | '?' | ';' | '\n' | '\r' => buf.push('\u{1}'),
            ',' => buf.push('\u{1}'),
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
    if words == 0 || words > 40 {
        return false;
    }
    if words >= 3 {
        return true;
    }
    // Short clause: keep only if it has a concrete, checkable signal.
    let lower = s.to_ascii_lowercase();
    let has_path = s.contains('/');
    let has_number = s.chars().any(|c| c.is_ascii_digit());
    let has_const = s.split_whitespace().any(|w| {
        w.len() >= 3 && w.chars().all(|c| c.is_ascii_uppercase() || c == '_')
    });
    let has_verb = ["added", "removed", "deleted", "dropped", "created", "wired"]
        .iter()
        .any(|v| lower.contains(v));
    has_path || has_number || has_const || has_verb
}

/// Run a full turn verification against the indexed repo / diff / logs.
pub fn verify(
    conn: &Connection,
    config: &Config,
    message: &str,
    local_log: Option<&str>,
) -> Result<TurnReport> {
    let mut verdicts = Vec::new();
    for seg in segment(message) {
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
    Ok(TurnReport { message: message.to_string(), verdicts })
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
    out.trim_end().to_string()
}

/// CLI entry point for `truth verify-turn`.
pub fn verify_turn(message: &str, local_log: Option<&str>, json: bool) -> Result<()> {
    let config = load_config()?;
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
    fn keeps_terse_path_clause_but_drops_bare_filler() {
        // "removed /v1/checkout" (2 words, has a path) is kept; "tests pass"
        // (2 words, no concrete subject) is dropped as unverifiable filler.
        let segs = segment("removed /v1/checkout, tests pass");
        assert!(segs.iter().any(|s| s.contains("/v1/checkout")));
        assert!(!segs.iter().any(|s| s == "tests pass"));
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
