//! Actionable guidance for inconclusive `truth check` results. These are hints
//! appended to the human-readable output; they never change the verdict.

use crate::check::CheckOutcome;
use anyhow::Result;
use rusqlite::Connection;
use truth_core::claim::ClaimType;
use truth_core::config::Config;
use truth_core::enums::VerdictStatus;

/// Returns a hint string when the check was inconclusive and a likely cause is
/// detectable, otherwise `None`.
pub fn check_hint(
    conn: &Connection,
    config: &Config,
    outcome: &CheckOutcome,
    local_log: Option<&str>,
) -> Result<Option<String>> {
    if outcome.decision.status != VerdictStatus::Inconclusive {
        return Ok(None);
    }

    let indexed = truth_db::repo::index_counts(conn)?.evidence_items > 0;
    let claim = &outcome.claim;

    // 1. Claim could not be resolved into anything concrete.
    if !claim.is_checkable || claim.claim_type == ClaimType::Unknown {
        return Ok(Some(format!(
            "I could not resolve {} to a route, event, config key, or indexed concept.\n\
             Try a more concrete claim:\n  \
             truth check \"nobody uses /v1/checkout anymore\"",
            claim
                .subject
                .as_deref()
                .map(|s| format!("`{s}`"))
                .unwrap_or_else(|| "this claim".to_string()),
        )));
    }

    let needs_logs = matches!(
        claim.claim_type,
        ClaimType::UsageCount
            | ClaimType::ErrorStillHappening
            | ClaimType::LatestOccurrence
            | ClaimType::JobLastSuccess
            | ClaimType::DependencyUsed
    );
    let log_available = local_log.is_some() || config.loki.enabled;

    // 2. Repo-backed claim but nothing indexed.
    if !indexed && !needs_logs {
        return Ok(Some(
            "I do not have indexed repo evidence yet.\nRun:\n  truth index .".to_string(),
        ));
    }

    // 3. Runtime claim but no log source configured.
    if needs_logs && !log_available {
        return Ok(Some(format!(
            "This claim requires runtime log evidence, but no log source is configured.\n\
             Options:\n  \
             truth check \"{}\" --local-log path/to/log\n  \
             configure [loki] in truth.toml",
            outcome_question_hint(claim),
        )));
    }

    // 4. Indexed + log source present, still inconclusive: suggest indexing if
    // the repo half is empty, otherwise leave the verdict's own caveats to speak.
    if !indexed {
        return Ok(Some(
            "I have no indexed repo evidence to cross-check.\nRun:\n  truth index .".to_string(),
        ));
    }

    Ok(None)
}

fn outcome_question_hint(claim: &truth_core::claim::StructuredClaim) -> String {
    match &claim.subject {
        Some(s) => format!("nobody uses {s} anymore"),
        None => "nobody uses /v1/checkout anymore".to_string(),
    }
}
