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
        | ClaimType::FeatureFlagEnabled
        | ClaimType::FileChanged
        | ClaimType::OnlyChanged
        | ClaimType::ChangeCount
        | ClaimType::SymbolRenamed
        | ClaimType::CommandSucceeded => QuestionType::CurrentImplementation,
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
        ClaimType::FileChanged => decide_file_changed(input),
        ClaimType::OnlyChanged => decide_only_changed(input),
        ClaimType::ChangeCount => decide_change_count(input),
        ClaimType::SymbolRenamed => decide_renamed(input),
        ClaimType::CommandSucceeded => decide_command(input),
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

/// File-change claim ("I edited src/auth.rs"). Decided from the working-tree
/// diff file list (`file_status` / `diff_files` items). `claim.value` holds the
/// expected change kind: "added" | "modified" | "deleted".
fn decide_file_changed(input: &VerdictInput) -> VerdictDecision {
    let subject = input.claim.subject.as_deref().unwrap_or("the file");
    let expected = input
        .claim
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap_or("modified");
    let status_item = input
        .items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("file_status"));
    let diff_nonempty = input
        .items
        .iter()
        .any(|i| i.predicate.as_deref() == Some("diff_files"));

    match status_item {
        Some(item) => {
            let actual = item
                .value_json
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let file = item
                .metadata_json
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or(subject);
            let ok = match expected {
                "added" => actual == "added",
                "deleted" => actual == "deleted",
                // "I edited X" is satisfied by any change that leaves the file
                // present — a brand-new file still counts as edited.
                _ => matches!(actual, "modified" | "added" | "renamed"),
            };
            if ok {
                VerdictDecision {
                    status: VerdictStatus::Supported,
                    confidence: 0.9,
                    evidence_ids: vec![format!("diff:{file}={actual}")],
                    caveats: vec!["Based on the working-tree git diff vs HEAD.".to_string()],
                    suggested_action: None,
                }
            } else {
                VerdictDecision {
                    status: VerdictStatus::Contradicted,
                    confidence: 0.88,
                    evidence_ids: vec![format!("diff:{file}={actual}")],
                    caveats: vec![format!(
                        "Claimed {expected}, but the diff shows `{file}` was {actual}."
                    )],
                    suggested_action: Some("Re-check what actually changed.".to_string()),
                }
            }
        }
        // The diff has changes, but none touch this file → the claim is false
        // for THIS turn's work.
        None if diff_nonempty => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.8,
            evidence_ids: vec!["diff:files".to_string()],
            caveats: vec![format!(
                "The working-tree diff does not touch `{subject}`."
            )],
            suggested_action: Some(
                "Claimed a change to this file, but the diff doesn't include it.".to_string(),
            ),
        },
        None => inconclusive(
            "The working tree has no changes vs HEAD, so I can't verify what this turn changed (already committed?).",
            None,
        ),
    }
}

/// Scope claim ("I only changed X" / "no other files were touched"). Every
/// path in the diff file list must match the subject.
fn decide_only_changed(input: &VerdictInput) -> VerdictDecision {
    let subject = match input.claim.subject.as_deref() {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            return inconclusive(
                "An 'only changed X' claim needs a concrete file or path to compare against.",
                None,
            )
        }
    };
    let files: Vec<String> = input
        .items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("diff_files"))
        .and_then(|i| i.value_json.as_ref())
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if files.is_empty() {
        return inconclusive(
            "The working tree has no changes vs HEAD, so I can't verify the scope of this turn's work (already committed?).",
            None,
        );
    }

    let matches_subject = |p: &str| p.contains(subject);
    let offenders: Vec<&String> = files.iter().filter(|p| !matches_subject(p)).collect();
    let touched = files.iter().any(|p| matches_subject(p));

    if !touched {
        return VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.85,
            evidence_ids: vec!["diff:files".to_string()],
            caveats: vec![format!(
                "The diff does not touch `{subject}` at all ({} other file(s) changed).",
                files.len()
            )],
            suggested_action: Some("Re-check what actually changed.".to_string()),
        };
    }
    if offenders.is_empty() {
        return VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.88,
            evidence_ids: vec!["diff:files".to_string()],
            caveats: vec![format!(
                "All {} changed file(s) match `{subject}` (working-tree diff vs HEAD).",
                files.len()
            )],
            suggested_action: None,
        };
    }
    let shown: Vec<&str> = offenders.iter().take(3).map(|s| s.as_str()).collect();
    let more = offenders.len().saturating_sub(shown.len());
    let mut list = shown.join(", ");
    if more > 0 {
        list.push_str(&format!(" (+{more} more)"));
    }
    VerdictDecision {
        status: VerdictStatus::Contradicted,
        confidence: 0.88,
        evidence_ids: vec!["diff:files".to_string()],
        caveats: vec![format!("The diff also touches: {list}.")],
        suggested_action: Some(
            "Other files were changed too; mention them or revert them.".to_string(),
        ),
    }
}

/// Count claim about the change itself ("updated all 4 call sites of X").
/// Compared against changed-line hits for the subject in the diff. Line-based,
/// so it's an approximation — mismatches are Partial, not Contradicted.
fn decide_change_count(input: &VerdictInput) -> VerdictDecision {
    let subject = input.claim.subject.as_deref().unwrap_or("the subject");
    let Some(expected) = input.claim.expected_number() else {
        return VerdictDecision {
            status: VerdictStatus::NeedsMoreContext,
            confidence: 0.4,
            evidence_ids: vec![],
            caveats: vec!["The claim did not state a count to compare against.".to_string()],
            suggested_action: None,
        };
    };
    let item = input
        .items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("diff_hits"));
    let Some(item) = item else {
        return inconclusive(
            "The working tree has no changes vs HEAD, so I can't count this turn's edits (already committed?).",
            None,
        );
    };
    let hits = item
        .value_json
        .as_ref()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    if hits == 0.0 {
        return VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.85,
            evidence_ids: vec![format!("diff:hits={hits}")],
            caveats: vec![format!(
                "The diff has no changed lines mentioning `{subject}`."
            )],
            suggested_action: Some("Re-check what actually changed.".to_string()),
        };
    }
    if (hits - expected).abs() < f64::EPSILON {
        return VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.8,
            evidence_ids: vec![format!("diff:hits={hits}")],
            caveats: vec![
                "Counted changed lines mentioning the subject (an approximation of sites)."
                    .to_string(),
            ],
            suggested_action: None,
        };
    }
    VerdictDecision {
        status: VerdictStatus::PartiallySupported,
        confidence: 0.7,
        evidence_ids: vec![format!("diff:hits={hits}")],
        caveats: vec![format!(
            "Claimed {expected}, but the diff shows {hits} changed line(s) mentioning `{subject}` (line-based count)."
        )],
        suggested_action: Some("Verify the exact number of sites changed.".to_string()),
    }
}

/// Rename claim ("I renamed X to Y"). The diff must show the old name removed
/// and the new name added; the old name surviving the diff (or the index, when
/// the diff is silent) catches the lie.
fn decide_renamed(input: &VerdictInput) -> VerdictDecision {
    let old = input.claim.subject.as_deref().unwrap_or("the old name");
    let new = input
        .claim
        .value
        .as_ref()
        .and_then(|v| v.get("to"))
        .and_then(|v| v.as_str())
        .unwrap_or("the new name");

    // Old-name presence after this turn's change, from the diff.
    let old_present_after: Option<bool> = input
        .items
        .iter()
        .find(|i| {
            i.predicate.as_deref() == Some("route_exists")
                && i.metadata_json
                    .get("from_diff")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
        })
        .and_then(|i| i.value_json.as_ref())
        .and_then(|v| v.as_bool());
    // New-name presence after this turn's change, from the diff.
    let new_added: Option<bool> = input
        .items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("renamed_to_exists"))
        .and_then(|i| i.value_json.as_ref())
        .and_then(|v| v.as_bool());

    match (old_present_after, new_added) {
        (Some(true), _) => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.85,
            evidence_ids: vec![format!("diff:{old}")],
            caveats: vec![format!(
                "`{old}` is still present after this turn's changes."
            )],
            suggested_action: Some(
                "Claimed renamed, but the old name survives. Re-check the change.".to_string(),
            ),
        },
        (Some(false), Some(true)) => VerdictDecision {
            status: VerdictStatus::Supported,
            confidence: 0.88,
            evidence_ids: vec![format!("diff:{old}->{new}")],
            caveats: vec![format!("The diff removes `{old}` and adds `{new}`.")],
            suggested_action: None,
        },
        (Some(false), _) => VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.75,
            evidence_ids: vec![format!("diff:{old}")],
            caveats: vec![format!(
                "`{old}` was removed, but `{new}` was not added by this diff."
            )],
            suggested_action: Some("The new name is missing from the change.".to_string()),
        },
        // Diff is silent. The index can still catch "renamed" when the old
        // name is in fact still defined.
        (None, _) => match input.symbol_status.as_deref() {
            Some("referenced") | Some("definition_only") => VerdictDecision {
                status: VerdictStatus::Contradicted,
                confidence: 0.75,
                evidence_ids: vec![format!("code:{old}")],
                caveats: vec![format!("`{old}` is still defined in the indexed code.")],
                suggested_action: Some(
                    "Claimed renamed, but the old name is still defined.".to_string(),
                ),
            },
            _ => inconclusive(
                "I couldn't find the rename in the working-tree diff or the index.",
                None,
            ),
        },
    }
}

/// Command-success claim ("tests pass", "it compiles"). Decided ONLY from
/// recorded command receipts (`truth run -- <cmd>`): a successful run must
/// postdate the last working-tree edit, otherwise it proves nothing about the
/// current code. No receipt → refused, never guessed.
fn decide_command(input: &VerdictInput) -> VerdictDecision {
    let subject = input.claim.subject.as_deref().unwrap_or("the command");
    let receipt = input
        .items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("command_receipt"));

    let Some(item) = receipt else {
        return inconclusive(
            &format!(
                "No recorded run of {subject} — record runs with `truth run -- <cmd>` (or the agent hook) so success claims become checkable."
            ),
            None,
        );
    };

    let exit_code = item
        .metadata_json
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let fresh = item
        .metadata_json
        .get("fresh")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let command = item
        .metadata_json
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or(subject);

    if exit_code != 0 {
        return VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.92,
            evidence_ids: vec![format!("run:{command}")],
            caveats: vec![format!(
                "The most recent recorded run of `{command}` exited {exit_code}."
            )],
            suggested_action: Some(
                "Fix the failure or re-run before claiming success.".to_string(),
            ),
        };
    }
    if !fresh {
        return inconclusive(
            &format!(
                "The last successful run of `{command}` predates the latest working-tree edits, so it proves nothing about the current code. Re-run it."
            ),
            None,
        );
    }
    VerdictDecision {
        status: VerdictStatus::Supported,
        confidence: 0.9,
        evidence_ids: vec![format!("run:{command}")],
        caveats: vec![format!(
            "`{command}` exited 0 after the last working-tree edit (recorded receipt)."
        )],
        suggested_action: None,
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

    // ---- diff-native claim family -----------------------------------------

    fn decide_with(claim: &StructuredClaim, items: &[EvidenceItem]) -> VerdictDecision {
        decide(&VerdictInput {
            claim,
            items,
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: None,
        })
    }

    fn typed_claim(
        ct: ClaimType,
        subject: &str,
        value: Option<serde_json::Value>,
    ) -> StructuredClaim {
        StructuredClaim {
            is_checkable: true,
            claim_type: ct,
            subject: Some(subject.into()),
            predicate: None,
            operator: ClaimOperator::Exists,
            value,
            unit: None,
            time_window: None,
            environment: None,
            confidence: 0.8,
            needs_clarification: false,
            clarification_question: None,
        }
    }

    fn pred_item(
        predicate: &str,
        value: serde_json::Value,
        metadata: serde_json::Value,
    ) -> EvidenceItem {
        let mut it = def_item(predicate, 0.0);
        it.evidence_type = EvidenceType::Change;
        it.value_json = Some(value);
        it.metadata_json = metadata;
        it
    }

    #[test]
    fn file_changed_supported_and_contradicted_by_status() {
        let claim = typed_claim(
            ClaimType::FileChanged,
            "src/auth.rs",
            Some(json!("modified")),
        );
        let items = [pred_item(
            "file_status",
            json!("modified"),
            json!({"from_diff": true, "file": "src/auth.rs"}),
        )];
        assert_eq!(decide_with(&claim, &items).status, VerdictStatus::Supported);

        // Claimed deleted, but the diff shows it was modified.
        let lie = typed_claim(
            ClaimType::FileChanged,
            "src/auth.rs",
            Some(json!("deleted")),
        );
        assert_eq!(
            decide_with(&lie, &items).status,
            VerdictStatus::Contradicted
        );
    }

    #[test]
    fn file_changed_contradicted_when_diff_skips_the_file() {
        // Other files changed, but not the claimed one.
        let claim = typed_claim(
            ClaimType::FileChanged,
            "src/auth.rs",
            Some(json!("modified")),
        );
        let items = [pred_item(
            "diff_files",
            json!(["src/other.rs"]),
            json!({"from_diff": true}),
        )];
        assert_eq!(
            decide_with(&claim, &items).status,
            VerdictStatus::Contradicted
        );
    }

    #[test]
    fn file_changed_unknown_on_clean_tree() {
        // Empty diff: the work may already be committed — never contradict.
        let claim = typed_claim(
            ClaimType::FileChanged,
            "src/auth.rs",
            Some(json!("modified")),
        );
        assert_eq!(decide_with(&claim, &[]).status, VerdictStatus::Inconclusive);
    }

    #[test]
    fn only_changed_catches_collateral_edits() {
        let claim = typed_claim(ClaimType::OnlyChanged, "src/parser", None);
        let clean = [pred_item(
            "diff_files",
            json!(["src/parser/mod.rs", "src/parser/expr.rs"]),
            json!({"from_diff": true}),
        )];
        assert_eq!(decide_with(&claim, &clean).status, VerdictStatus::Supported);

        let collateral = [pred_item(
            "diff_files",
            json!(["src/parser/mod.rs", "src/api/routes.rs"]),
            json!({"from_diff": true}),
        )];
        let d = decide_with(&claim, &collateral);
        assert_eq!(d.status, VerdictStatus::Contradicted);
        assert!(d.caveats.iter().any(|c| c.contains("src/api/routes.rs")));
    }

    #[test]
    fn change_count_exact_partial_and_zero() {
        let claim = typed_claim(ClaimType::ChangeCount, "parse_config", Some(json!(4)));
        let exact = [pred_item("diff_hits", json!(4), json!({"from_diff": true}))];
        assert_eq!(decide_with(&claim, &exact).status, VerdictStatus::Supported);

        let off = [pred_item("diff_hits", json!(2), json!({"from_diff": true}))];
        assert_eq!(
            decide_with(&claim, &off).status,
            VerdictStatus::PartiallySupported
        );

        let zero = [pred_item("diff_hits", json!(0), json!({"from_diff": true}))];
        assert_eq!(
            decide_with(&claim, &zero).status,
            VerdictStatus::Contradicted
        );
    }

    #[test]
    fn rename_requires_old_gone_and_new_added() {
        let claim = typed_claim(
            ClaimType::SymbolRenamed,
            "parse_legacy",
            Some(json!({"to": "parse_v2"})),
        );
        // Old removed + new added → Supported.
        let good = [
            diff_item(false),
            pred_item("renamed_to_exists", json!(true), json!({"from_diff": true})),
        ];
        assert_eq!(decide_with(&claim, &good).status, VerdictStatus::Supported);

        // Old name survives the diff → Contradicted.
        let survives = [diff_item(true)];
        assert_eq!(
            decide_with(&claim, &survives).status,
            VerdictStatus::Contradicted
        );

        // Old removed but new name never added → Contradicted.
        let half = [diff_item(false)];
        assert_eq!(
            decide_with(&claim, &half).status,
            VerdictStatus::Contradicted
        );
    }

    #[test]
    fn rename_falls_back_to_index_when_diff_silent() {
        let claim = typed_claim(
            ClaimType::SymbolRenamed,
            "parse_legacy",
            Some(json!({"to": "parse_v2"})),
        );
        // No diff evidence, but the index still has the old symbol defined.
        let d = decide(&VerdictInput {
            claim: &claim,
            items: &[],
            query_results: &[],
            usage_threshold: 0,
            code_references: None,
            symbol_status: Some("definition_only".into()),
        });
        assert_eq!(d.status, VerdictStatus::Contradicted);
    }

    // ---- command receipts ---------------------------------------------------

    fn receipt_item(exit_code: i64, fresh: bool) -> EvidenceItem {
        pred_item(
            "command_receipt",
            json!({"exit_code": exit_code}),
            json!({
                "command": "cargo test",
                "exit_code": exit_code,
                "fresh": fresh,
            }),
        )
    }

    #[test]
    fn tests_pass_supported_only_by_fresh_green_receipt() {
        let claim = typed_claim(ClaimType::CommandSucceeded, "test", None);
        let fresh_green = [receipt_item(0, true)];
        assert_eq!(
            decide_with(&claim, &fresh_green).status,
            VerdictStatus::Supported
        );
    }

    #[test]
    fn tests_pass_contradicted_by_failing_receipt() {
        let claim = typed_claim(ClaimType::CommandSucceeded, "test", None);
        let red = [receipt_item(101, true)];
        let d = decide_with(&claim, &red);
        assert_eq!(d.status, VerdictStatus::Contradicted);
        assert!(d.caveats.iter().any(|c| c.contains("101")));
    }

    #[test]
    fn tests_pass_stale_receipt_proves_nothing() {
        // Green run BEFORE the latest edits: refused, not supported.
        let claim = typed_claim(ClaimType::CommandSucceeded, "test", None);
        let stale = [receipt_item(0, false)];
        assert_eq!(
            decide_with(&claim, &stale).status,
            VerdictStatus::Inconclusive
        );
    }

    #[test]
    fn tests_pass_refused_without_any_receipt() {
        let claim = typed_claim(ClaimType::CommandSucceeded, "test", None);
        assert_eq!(decide_with(&claim, &[]).status, VerdictStatus::Inconclusive);
    }
}
