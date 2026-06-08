//! Deterministic verdict engine (spec §13).
//!
//! The LLM never decides truth. Given a structured claim and the evidence
//! gathered for it, this module applies fixed rules to produce a verdict.

use crate::claim::{ClaimType, StructuredClaim};
use crate::enums::{Authority, QuestionType, VerdictStatus};
use crate::models::EvidenceItem;
use crate::query::{EvidenceQueryResult, QueryType};

/// Output of the verdict engine.
#[derive(Debug, Clone)]
pub struct VerdictDecision {
    pub status: VerdictStatus,
    pub confidence: f32,
    pub evidence_ids: Vec<String>,
    pub caveats: Vec<String>,
    pub suggested_action: Option<String>,
}

/// Bundle of everything the engine reasons over for one check.
pub struct VerdictInput<'a> {
    pub claim: &'a StructuredClaim,
    /// Repo/code/config evidence already stored as items.
    pub items: &'a [EvidenceItem],
    /// Live query results (logs).
    pub query_results: &'a [EvidenceQueryResult],
    /// Count above which a usage/error observation counts as "present".
    pub usage_threshold: i64,
    /// References to the subject found in the indexed code (excluding its
    /// definition). `None` if no code scan was run. This is the usage signal for
    /// libraries / code that has no runtime logs.
    pub code_references: Option<usize>,
    /// For symbol-existence claims: the refs status of the subject in the index
    /// — `"referenced"` / `"definition_only"` (both ⇒ present) or
    /// `"unreferenced"` (absent). `None` if no symbol scan was run.
    pub symbol_status: Option<String>,
}

/// Authority ordering by question type (spec §13.2). Higher = more authoritative.
pub fn authority_rank(question: QuestionType, authority: Authority) -> u8 {
    use Authority::*;
    // Returns a descending-priority list; index → rank.
    let order: &[Authority] = match question {
        QuestionType::CurrentRuntimeState | QuestionType::Usage => &[
            RuntimeLogs,
            Metrics,
            ProductionConfig,
            Code,
            Config,
            OfficialDoc,
            SlackMessage,
        ],
        QuestionType::CurrentImplementation | QuestionType::ConfigValue => {
            &[Code, Config, Test, OfficialDoc, SlackMessage]
        }
        QuestionType::IncidentStatus => &[RuntimeLogs, Metrics, Issue, PullRequest, SlackMessage],
        QuestionType::DocumentationAccuracy => &[Code, Config, OfficialDoc, SlackMessage],
        QuestionType::Unknown => &[
            RuntimeLogs,
            Metrics,
            Code,
            Config,
            OfficialDoc,
            SlackMessage,
        ],
    };
    match order.iter().position(|a| *a == authority) {
        Some(idx) => (order.len() - idx) as u8,
        None => 0,
    }
}

/// Map a claim type to the question type that drives authority ordering.
pub fn question_type_for(claim_type: ClaimType) -> QuestionType {
    match claim_type {
        ClaimType::UsageCount | ClaimType::DependencyUsed => QuestionType::Usage,
        ClaimType::ErrorStillHappening | ClaimType::JobLastSuccess => QuestionType::IncidentStatus,
        ClaimType::LatestOccurrence => QuestionType::CurrentRuntimeState,
        ClaimType::RouteExists
        | ClaimType::SymbolExists
        | ClaimType::EnvVarExists
        | ClaimType::FeatureFlagEnabled => QuestionType::CurrentImplementation,
        ClaimType::ConfigValue
        | ClaimType::RetryCount
        | ClaimType::TimeoutValue
        | ClaimType::VersionRequired => QuestionType::ConfigValue,
        ClaimType::Unknown => QuestionType::Unknown,
    }
}

/// Apply deterministic verdict rules.
pub fn decide(input: &VerdictInput) -> VerdictDecision {
    let claim = input.claim;

    if !claim.is_checkable || claim.claim_type == ClaimType::Unknown {
        return inconclusive(
            "I could not turn this into a concrete, checkable claim.",
            claim.clarification_question.clone(),
        );
    }

    match claim.claim_type {
        ClaimType::UsageCount | ClaimType::DependencyUsed => decide_usage(input),
        ClaimType::ErrorStillHappening => decide_error(input),
        ClaimType::LatestOccurrence => decide_latest(input),
        ClaimType::RouteExists | ClaimType::EnvVarExists | ClaimType::FeatureFlagEnabled => {
            decide_existence(input)
        }
        ClaimType::SymbolExists => decide_symbol(input),
        ClaimType::ConfigValue
        | ClaimType::RetryCount
        | ClaimType::TimeoutValue
        | ClaimType::VersionRequired => decide_value(input),
        ClaimType::JobLastSuccess => decide_latest(input),
        ClaimType::Unknown => unreachable!(),
    }
}

/// Total observed count across log query results.
fn total_observed_count(input: &VerdictInput) -> i64 {
    input
        .query_results
        .iter()
        .filter_map(|r| r.count)
        .sum::<i64>()
}

fn route_exists_in_repo(input: &VerdictInput) -> bool {
    // Diff evidence is authoritative when present: it reflects the post-change
    // state of THIS working tree, which outranks a possibly-stale index. If the
    // diff says the route was removed (value=false), that wins over a lingering
    // index item that still says it exists.
    if let Some(diff_item) = input.items.iter().find(|i| {
        i.predicate.as_deref() == Some("route_exists")
            && i.metadata_json
                .get("from_diff")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }) {
        return diff_item
            .value_json
            .as_ref()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }
    input.items.iter().any(|i| {
        i.predicate.as_deref() == Some("route_exists")
            && i.value_json
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    })
}

/// Whether the subject is declared as a dependency in the index (Cargo.toml,
/// package.json, requirements.txt, ...), via the `dependency_exists` predicate.
fn dependency_declared(input: &VerdictInput) -> bool {
    input.items.iter().any(|i| {
        i.predicate.as_deref() == Some("dependency_exists")
            && i.value_json
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    })
}

/// Usage claim: "nobody uses X" → expected count 0.
fn decide_usage(input: &VerdictInput) -> VerdictDecision {
    use crate::claim::{ClaimOperator, ClaimType};
    // Dependency claims resolve against the declared-dependency index fact first:
    // a package listed in the manifest IS a dependency, regardless of how often
    // its bare name appears in source. This is the authoritative signal.
    if input.claim.claim_type == ClaimType::DependencyUsed {
        let declared = dependency_declared(input);
        let claims_absent = input.claim.operator == ClaimOperator::NotExists;
        // Only decide here when we have a manifest signal or an explicit absence
        // claim; otherwise fall through to the code-reference logic below.
        if declared || claims_absent {
            let subject = input.claim.subject.as_deref().unwrap_or("the dependency");
            return match (declared, claims_absent) {
                (true, false) => VerdictDecision {
                    status: VerdictStatus::Supported,
                    confidence: 0.9,
                    evidence_ids: vec!["repo:dependency_exists".into()],
                    caveats: vec![format!(
                        "`{subject}` is declared as a dependency in the manifest."
                    )],
                    suggested_action: None,
                },
                (true, true) => VerdictDecision {
                    status: VerdictStatus::Contradicted,
                    confidence: 0.9,
                    evidence_ids: vec!["repo:dependency_exists".into()],
                    caveats: vec![format!("`{subject}` is still declared as a dependency.")],
                    suggested_action: Some("It is still a dependency; re-check the change.".into()),
                },
                (false, true) => VerdictDecision {
                    status: VerdictStatus::Supported,
                    confidence: 0.78,
                    evidence_ids: vec![],
                    caveats: vec![format!("`{subject}` is not declared as a dependency.")],
                    suggested_action: None,
                },
                (false, false) => VerdictDecision {
                    status: VerdictStatus::Contradicted,
                    confidence: 0.75,
                    evidence_ids: vec![],
                    caveats: vec![format!(
                        "`{subject}` is not declared as a dependency in the manifest."
                    )],
                    suggested_action: None,
                },
            };
        }
    }

    let observed = total_observed_count(input);
    let has_logs = input
        .query_results
        .iter()
        .any(|r| matches!(r.query_type, QueryType::RouteCount | QueryType::EventCount));
    let route_exists = route_exists_in_repo(input);

    let mut evidence_ids = log_evidence_labels(input);
    if route_exists {
        evidence_ids.push("repo:route_exists".to_string());
    }
    let code_refs = input.code_references.unwrap_or(0);
    let has_code_refs = input.code_references.is_some();
    if code_refs > 0 {
        evidence_ids.push(format!("code:references={code_refs}"));
    }

    // Claim asserts ~zero usage ("nobody uses ...").
    let expects_zero = input
        .claim
        .expected_number()
        .map(|v| v == 0.0)
        .unwrap_or(true);

    // Code-reference signal: for libraries with no logs, "referenced N times in
    // the code" directly answers "is X used?".
    let subject = input.claim.subject.as_deref().unwrap_or("the subject");
    if code_refs > input.usage_threshold as usize {
        // Referenced in code. Contradicts an "unused"/"nobody uses" claim, and
        // *supports* a positive "X is (still) used" claim.
        let referenced =
            format!("`{subject}` is referenced {code_refs} time(s) in the indexed code.");
        if expects_zero {
            return VerdictDecision {
                status: VerdictStatus::Contradicted,
                confidence: 0.9,
                evidence_ids,
                caveats: vec![referenced],
                suggested_action: Some("It is used in code; treat it as in use.".to_string()),
            };
        }
        return VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.9,
            evidence_ids,
            caveats: vec![referenced],
            suggested_action: None,
        };
    }
    // Positive "X is used" claim with a definitive zero-reference code signal:
    // contradicted (it is not used in code).
    if !expects_zero && has_code_refs && code_refs == 0 && !has_logs {
        return VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.75,
            evidence_ids,
            caveats: vec![format!(
                "`{subject}` is not referenced in the indexed code."
            )],
            suggested_action: None,
        };
    }

    if !has_logs && !route_exists && !has_code_refs {
        return inconclusive(
            "I found no usage logs and could not confirm the subject exists in the repo.",
            Some("Try naming the exact route or environment.".to_string()),
        );
    }

    // Expects-zero and we have a definitive code signal of zero references.
    if expects_zero && has_code_refs && code_refs == 0 && !has_logs {
        return VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.75,
            evidence_ids,
            caveats: vec![
                "Not referenced in the indexed code (a strong but not absolute unused signal)."
                    .to_string(),
            ],
            suggested_action: None,
        };
    }

    if expects_zero {
        if observed > input.usage_threshold {
            return VerdictDecision {
                status: VerdictStatus::Contradicted,
                confidence: 0.94,
                evidence_ids,
                caveats: usage_caveats(input),
                suggested_action: Some(
                    "Treat the subject as still in use before removing it.".to_string(),
                ),
            };
        }
        if observed == 0 && has_logs {
            if !route_exists {
                // Zero traffic AND we can't confirm the subject exists. We can't
                // tell "genuinely unused" from "never a real route" — stay safe.
                return inconclusive(
                    "I found no traffic and could not confirm the subject exists in the repo, so I can't confirm it is genuinely unused.",
                    Some("Name the exact route, e.g. `/v1/checkout`.".to_string()),
                );
            }
            // No traffic, but it still exists in code: supported, with caveats.
            let mut caveats = usage_caveats(input);
            caveats.push(
                "No traffic in the configured window does not prove it is unused forever."
                    .to_string(),
            );
            return VerdictDecision {
                status: VerdictStatus::Supported,
                confidence: 0.7,
                evidence_ids,
                caveats,
                suggested_action: None,
            };
        }
    }

    inconclusive("Usage evidence was insufficient to decide.", None)
}

fn decide_error(input: &VerdictInput) -> VerdictDecision {
    let observed = total_observed_count(input);
    let has_logs = input
        .query_results
        .iter()
        .any(|r| matches!(r.query_type, QueryType::ErrorCount | QueryType::EventCount));
    let evidence_ids = log_evidence_labels(input);

    if !has_logs {
        return inconclusive(
            "I have no error logs configured to confirm whether this is fixed.",
            None,
        );
    }
    if observed > input.usage_threshold {
        VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.9,
            evidence_ids,
            caveats: usage_caveats(input),
            suggested_action: Some("The error still occurs; reopen or investigate.".to_string()),
        }
    } else {
        let mut caveats = usage_caveats(input);
        caveats.push("Absence in the window is not a permanent guarantee.".to_string());
        VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.72,
            evidence_ids,
            caveats,
            suggested_action: None,
        }
    }
}

fn decide_latest(input: &VerdictInput) -> VerdictDecision {
    let latest = input
        .query_results
        .iter()
        .filter_map(|r| r.latest_seen)
        .max();
    let evidence_ids = log_evidence_labels(input);
    match latest {
        Some(_) => VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.8,
            evidence_ids,
            caveats: usage_caveats(input),
            suggested_action: None,
        },
        None => inconclusive("No occurrence found in the configured window.", None),
    }
}

fn decide_existence(input: &VerdictInput) -> VerdictDecision {
    use crate::claim::ClaimOperator;
    let exists = route_exists_in_repo(input)
        || input.items.iter().any(|i| {
            matches!(
                i.predicate.as_deref(),
                Some("env_var_exists") | Some("exists")
            ) && i
                .value_json
                .as_ref()
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });

    // The claim's polarity decides what "match" means. A positive existence
    // claim ("/x is still registered") is Supported when present; a negative
    // one ("I removed /x") is Contradicted when the thing is still present.
    let claims_absence = input.claim.operator == ClaimOperator::NotExists;

    match (exists, claims_absence) {
        // "still registered" and it is there → Supported.
        (true, false) => VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.85,
            evidence_ids: repo_evidence_labels(input),
            caveats: vec!["Based on static repo contents at index time.".to_string()],
            suggested_action: None,
        },
        // "I removed it" but it is still there → Contradicted (caught the lie).
        (true, true) => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.85,
            evidence_ids: repo_evidence_labels(input),
            caveats: vec!["The subject is still present in the indexed repo.".to_string()],
            suggested_action: Some(
                "Claimed removed/absent, but it is still defined. Re-check the change.".to_string(),
            ),
        },
        // "I removed it" and it is gone → Supported.
        (false, true) => VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.78,
            evidence_ids: repo_evidence_labels(input),
            caveats: vec!["Absent from the indexed repo at index time.".to_string()],
            suggested_action: None,
        },
        // Claimed present but not found. If the DIFF positively shows it was
        // removed this turn, that's a contradiction of "still registered" — we
        // caught the lie. Otherwise it may just be unindexed → can't confirm.
        (false, false) => {
            if diff_says_removed(input) {
                VerdictDecision {
                    status: VerdictStatus::Contradicted,
                    confidence: 0.85,
                    evidence_ids: repo_evidence_labels(input),
                    caveats: vec![
                        "The working-tree diff shows this was removed this turn.".to_string()
                    ],
                    suggested_action: Some(
                        "Claimed still present, but the diff removed it. Re-check the change."
                            .to_string(),
                    ),
                }
            } else {
                inconclusive(
                    "I could not find this in the indexed repo. It may exist elsewhere or be unindexed.",
                    None,
                )
            }
        }
    }
}

/// True when diff evidence positively asserts the subject was added this turn
/// (a `from_diff` `route_exists` item with value=true).
fn diff_says_added(input: &VerdictInput) -> bool {
    input.items.iter().any(|i| {
        i.predicate.as_deref() == Some("route_exists")
            && i.metadata_json
                .get("from_diff")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            && i.value_json.as_ref().and_then(|v| v.as_bool()) == Some(true)
    })
}

/// Symbol-existence claim ("I added/removed function X", "X exists"). The diff
/// is authoritative for what changed THIS turn; otherwise the index symbol
/// status (referenced / definition_only ⇒ present; unreferenced ⇒ absent)
/// decides. Honors claim polarity (added/exists vs removed/deleted).
fn decide_symbol(input: &VerdictInput) -> VerdictDecision {
    use crate::claim::ClaimOperator;
    let subject = input.claim.subject.as_deref().unwrap_or("the symbol");
    let claims_absence = input.claim.operator == ClaimOperator::NotExists;

    // Resolve presence: diff wins, then index status.
    let present: Option<bool> = if diff_says_added(input) {
        Some(true)
    } else if diff_says_removed(input) {
        Some(false)
    } else {
        match input.symbol_status.as_deref() {
            Some("referenced") | Some("definition_only") => Some(true),
            Some("unreferenced") => Some(false),
            _ => None,
        }
    };

    let present = match present {
        Some(p) => p,
        // No diff signal and no symbol scan / not indexed → can't confirm.
        None => {
            return inconclusive(
                &format!("I couldn't find `{subject}` in the working-tree diff or the index."),
                Some("Run `truth index .` so symbol claims can be checked.".to_string()),
            )
        }
    };

    match (present, claims_absence) {
        // "I added / X exists" and it is present → Supported.
        (true, false) => supported_symbol(subject, "is present in the code"),
        // "I removed X" but it is still present → Contradicted (caught the lie).
        (true, true) => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.85,
            evidence_ids: vec![format!("code:{subject}")],
            caveats: vec![format!("`{subject}` is still present in the code.")],
            suggested_action: Some("Claimed removed, but it is still defined.".to_string()),
        },
        // "I removed X" and it is gone → Supported.
        (false, true) => supported_symbol(subject, "is absent from the code"),
        // "I added / X exists" but it is absent → Contradicted.
        (false, false) => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.8,
            evidence_ids: vec![],
            caveats: vec![format!(
                "`{subject}` is not present in the working tree or index."
            )],
            suggested_action: Some("Claimed present, but it could not be found.".to_string()),
        },
    }
}

fn supported_symbol(subject: &str, why: &str) -> VerdictDecision {
    VerdictDecision {
        status: VerdictStatus::Supported,
        confidence: 0.85,
        evidence_ids: vec![format!("code:{subject}")],
        caveats: vec![format!("`{subject}` {why}.")],
        suggested_action: None,
    }
}

/// True when diff evidence positively asserts the subject was removed this turn
/// (a `from_diff` `route_exists` item with value=false).
fn diff_says_removed(input: &VerdictInput) -> bool {
    input.items.iter().any(|i| {
        i.predicate.as_deref() == Some("route_exists")
            && i.metadata_json
                .get("from_diff")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            && i.value_json.as_ref().and_then(|v| v.as_bool()) == Some(false)
    })
}

/// Value claim: compare the claimed value against a code/config definition.
fn decide_value(input: &VerdictInput) -> VerdictDecision {
    let expected = input.claim.expected_number();
    // Find a defining item whose predicate matches the claim subject/predicate.
    let defined: Option<(f64, &EvidenceItem)> = input.items.iter().find_map(|i| {
        let n = i.value_json.as_ref().and_then(|v| v.as_f64())?;
        if i.evidence_type == crate::enums::EvidenceType::Definition {
            Some((n, i))
        } else {
            None
        }
    });

    match (expected, defined) {
        (Some(exp), Some((def, item))) => {
            let label = format!(
                "repo:{}={}",
                item.predicate.as_deref().unwrap_or("value"),
                def
            );
            if (exp - def).abs() < f64::EPSILON {
                VerdictDecision {
                    status: VerdictStatus::Supported,
                    confidence: 0.92,
                    evidence_ids: vec![label],
                    caveats: vec!["Compared against the indexed source definition.".to_string()],
                    suggested_action: None,
                }
            } else {
                VerdictDecision {
                    status: VerdictStatus::Contradicted,
                    confidence: 0.92,
                    evidence_ids: vec![label],
                    caveats: vec![format!("Claim says {exp} but the source defines {def}.")],
                    suggested_action: Some("Update the claim or the source to match.".to_string()),
                }
            }
        }
        (None, Some((def, item))) => VerdictDecision {
            // Claim had no concrete number; just report what the source says.
            status: VerdictStatus::NeedsMoreContext,
            confidence: 0.5,
            evidence_ids: vec![format!(
                "repo:{}={}",
                item.predicate.as_deref().unwrap_or("value"),
                def
            )],
            caveats: vec!["The claim did not state a value to compare against.".to_string()],
            suggested_action: Some(
                "Restate with the specific value, e.g. \"retry count is 3\".".to_string(),
            ),
        },
        _ => inconclusive(
            "I could not find a source definition for this value in the indexed repo.",
            None,
        ),
    }
}

fn usage_caveats(input: &VerdictInput) -> Vec<String> {
    let mut c = vec![];
    if input
        .query_results
        .iter()
        .any(|r| matches!(r.source, crate::enums::SourceKind::Loki))
    {
        c.push("This only checks the configured Loki source.".to_string());
    } else if !input.query_results.is_empty() {
        c.push("This only checks the configured local log source.".to_string());
    }
    c.push("Logs may be sampled or incomplete.".to_string());
    c
}

fn log_evidence_labels(input: &VerdictInput) -> Vec<String> {
    input
        .query_results
        .iter()
        .map(|r| format!("{}:{}", r.source.as_db_str(), r.query_type.as_label()))
        .collect()
}

fn repo_evidence_labels(input: &VerdictInput) -> Vec<String> {
    input
        .items
        .iter()
        .filter_map(|i| i.predicate.clone().map(|p| format!("repo:{p}")))
        .collect()
}

fn inconclusive(msg: &str, action: Option<String>) -> VerdictDecision {
    VerdictDecision {
        status: VerdictStatus::Inconclusive,
        confidence: 0.3,
        evidence_ids: vec![],
        caveats: vec![msg.to_string()],
        suggested_action: action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{ClaimOperator, ClaimType};
    use crate::enums::*;
    use serde_json::json;

    fn usage_claim(expected: i64) -> StructuredClaim {
        StructuredClaim {
            is_checkable: true,
            claim_type: ClaimType::UsageCount,
            subject: Some("/v1/checkout".into()),
            predicate: Some("request_count".into()),
            operator: ClaimOperator::Equals,
            value: Some(json!(expected)),
            unit: Some("requests".into()),
            time_window: Some("7d".into()),
            environment: Some("prod".into()),
            confidence: 0.86,
            needs_clarification: false,
            clarification_question: None,
        }
    }

    fn log_result(count: i64) -> EvidenceQueryResult {
        EvidenceQueryResult {
            source: SourceKind::Loki,
            query_type: QueryType::RouteCount,
            query_text: "sum(...)".into(),
            count: Some(count),
            latest_seen: Some(1_000),
            redacted_samples: vec![],
            time_from: None,
            time_to: None,
            extra: json!({}),
        }
    }

    fn def_item(predicate: &str, value: f64) -> EvidenceItem {
        EvidenceItem {
            id: "e1".into(),
            span_id: "s1".into(),
            evidence_type: EvidenceType::Definition,
            subject_text: None,
            subject_concept_id: None,
            predicate: Some(predicate.into()),
            object_text: None,
            value_json: Some(json!(value)),
            unit: None,
            confidence: 1.0,
            authority: Authority::Code,
            valid_from: None,
            valid_to: None,
            extraction_method: ExtractionMethod::Deterministic,
            metadata_json: json!({}),
        }
    }

    #[test]
    fn usage_zero_but_traffic_present_is_contradicted() {
        let claim = usage_claim(0);
        let results = [log_result(12481)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &results,
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Contradicted);
    }

    #[test]
    fn positive_usage_claim_supported_by_code_refs() {
        // "X is still used" (expects nonzero) + the symbol is referenced in code
        // -> Supported, even with no logs.
        let claim = usage_claim(1);
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &[],
            usage_threshold: 0,
            code_references: Some(313),
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Supported);
    }

    #[test]
    fn positive_usage_claim_contradicted_when_no_code_refs() {
        // "X is still used" but it's defined and referenced nowhere -> Contradicted.
        let claim = usage_claim(1);
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &[],
            usage_threshold: 0,
            code_references: Some(0),
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Contradicted);
    }

    #[test]
    fn usage_zero_with_no_traffic_but_route_exists_is_supported() {
        let claim = usage_claim(0);
        let results = [log_result(0)];
        let mut route = def_item("route_exists", 0.0);
        route.value_json = Some(json!(true));
        let items = [route];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &results,
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Supported);
    }

    #[test]
    fn usage_zero_with_no_traffic_and_no_route_is_inconclusive() {
        let claim = usage_claim(0);
        let results = [log_result(0)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &results,
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Inconclusive);
    }

    #[test]
    fn retry_value_mismatch_is_contradicted() {
        let mut claim = usage_claim(3);
        claim.claim_type = ClaimType::RetryCount;
        let items = [def_item("retry_count", 5.0)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Contradicted);
    }

    #[test]
    fn config_value_match_is_supported() {
        let mut claim = usage_claim(8080);
        claim.claim_type = ClaimType::ConfigValue;
        let items = [def_item("port", 8080.0)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Supported);
    }

    #[test]
    fn no_evidence_is_inconclusive() {
        let claim = usage_claim(0);
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Inconclusive);
    }

    #[test]
    fn authority_runtime_logs_beats_slack_for_usage() {
        let r_logs = authority_rank(QuestionType::Usage, Authority::RuntimeLogs);
        let r_slack = authority_rank(QuestionType::Usage, Authority::SlackMessage);
        assert!(r_logs > r_slack);
    }

    // ---- diff-evidence existence (agent fact-checking) -------------------

    fn route_exists_claim(op: ClaimOperator) -> StructuredClaim {
        StructuredClaim {
            is_checkable: true,
            claim_type: ClaimType::RouteExists,
            subject: Some("/v1/checkout".into()),
            predicate: Some("route_exists".into()),
            operator: op,
            value: None,
            unit: None,
            time_window: None,
            environment: None,
            confidence: 0.8,
            needs_clarification: false,
            clarification_question: None,
        }
    }

    fn diff_item(present: bool) -> EvidenceItem {
        let mut it = def_item("route_exists", 0.0);
        it.evidence_type = EvidenceType::Change;
        it.value_json = Some(json!(present));
        it.metadata_json = json!({ "from_diff": true });
        it
    }

    #[test]
    fn diff_removed_outranks_stale_index_for_still_present_claim() {
        // Positive "still registered" claim, but the diff removed it while a
        // stale index item still says it exists -> Contradicted (caught the lie).
        let claim = route_exists_claim(ClaimOperator::Exists);
        let mut stale = def_item("route_exists", 0.0);
        stale.value_json = Some(json!(true));
        let items = [diff_item(false), stale];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Contradicted);
    }

    #[test]
    fn diff_added_supports_positive_existence_claim() {
        let claim = route_exists_claim(ClaimOperator::Exists);
        let items = [diff_item(true)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Supported);
    }

    #[test]
    fn diff_removed_supports_negative_existence_claim() {
        // "I removed /x" and the diff shows it gone -> Supported.
        let claim = route_exists_claim(ClaimOperator::NotExists);
        let items = [diff_item(false)];
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        });
        assert_eq!(d.status, VerdictStatus::Supported);
    }
}
