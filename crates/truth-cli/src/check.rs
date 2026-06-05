//! The check pipeline: claim extraction → query plan → run repo + log adapters
//! → verdict engine → response. Persists the Check / EvidenceQuery / Verdict
//! rows as an audit trail (spec §15.4).

use anyhow::Result;
use rusqlite::Connection;
use truth_core::claim::StructuredClaim;
use truth_core::config::Config;
use truth_core::enums::*;
use truth_core::models::*;
use truth_core::query::{EvidenceQueryResult, PlannedQuery, QueryType};
use truth_core::verdict::{decide, question_type_for, VerdictDecision, VerdictInput};
use truth_core::{new_id, now_secs};
use truth_llm::{plan_for, render, ResponseInput};

use crate::service::EvidenceJson;

pub struct CheckOutcome {
    pub check_id: String,
    pub claim: StructuredClaim,
    pub decision: VerdictDecision,
    pub response_text: String,
    pub evidence: Vec<EvidenceJson>,
}

impl CheckOutcome {
    /// Stable machine-readable JSON for `truth check --json`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.decision.status.as_db_str(),
            "confidence": self.decision.confidence,
            "summary": self.response_text,
            "claim": self.claim,
            "evidence": self.evidence,
            "caveats": self.decision.caveats,
            "suggested_action": self.decision.suggested_action,
            "check_id": self.check_id,
        })
    }
}

/// Run a full check for `question`. `local_log_path` enables the offline log
/// adapter when Loki is disabled.
pub fn run_check(
    conn: &Connection,
    config: &Config,
    question: &str,
    trigger: Trigger,
    local_log_path: Option<&str>,
) -> Result<CheckOutcome> {
    let mut claim = truth_llm::extract_claim(config, question);
    // Concept resolution: a usage/route claim with no concrete subject (e.g.
    // "does anyone still use old checkout") gets its subject resolved against the
    // indexed routes before planning.
    let mut resolved_note: Option<String> = None;
    if needs_subject_resolution(&claim) {
        if let Some(res) = resolve_from_text(conn, question)? {
            resolved_note = Some(format!(
                "Interpreted this as `{}` (confidence {:.0}%).",
                res.label,
                res.confidence * 100.0
            ));
            claim.subject = Some(res.label);
            claim.is_checkable = true;
            if claim.claim_type == truth_core::claim::ClaimType::Unknown {
                claim.claim_type = truth_core::claim::ClaimType::UsageCount;
            }
        }
    }
    let question_type = question_type_for(claim.claim_type);

    let check = Check {
        id: new_id(),
        trigger,
        question: question.to_string(),
        question_type: Some(question_type),
        status: CheckStatus::Running,
        created_at: now_secs(),
        metadata_json: serde_json::json!({ "claim": claim }),
    };
    truth_db::repo::insert_check(conn, &check)?;

    let plan = plan_for(&claim, config.loki.enabled, default_service(config));

    let mut query_results: Vec<EvidenceQueryResult> = Vec::new();
    let mut repo_items: Vec<EvidenceItem> = Vec::new();
    let mut evidence_lines: Vec<String> = Vec::new();
    let mut evidence_json: Vec<EvidenceJson> = Vec::new();

    for pq in &plan.queries {
        match pq.source {
            SourceKind::Loki | SourceKind::LocalLogs => {
                if let Some(res) = run_log_query(config, pq, local_log_path)? {
                    evidence_lines.push(format_log_line(pq, &res));
                    evidence_json.push(log_evidence_json(config, pq, &res, local_log_path));
                    persist_query(conn, &check.id, &res)?;
                    query_results.push(res);
                }
            }
            SourceKind::GitRepo => {
                let (items, lines, json) = run_repo_query(conn, pq)?;
                evidence_lines.extend(lines);
                evidence_json.extend(json);
                repo_items.extend(items);
            }
            _ => {}
        }
    }

    let mut decision = decide(&VerdictInput {
        claim: &claim,
        items: &repo_items,
        query_results: &query_results,
        usage_threshold: 0,
    });
    // Surface the concept interpretation so the user can confirm it (conservative
    // UX — we never silently substitute a different subject).
    if let Some(note) = &resolved_note {
        decision.caveats.insert(0, note.clone());
    }

    let response_text = render(&ResponseInput {
        claim_text: question,
        decision: &decision,
        evidence_lines: &evidence_lines,
    });

    // Persist the verdict.
    let verdict = Verdict {
        id: new_id(),
        check_id: check.id.clone(),
        status: decision.status,
        confidence: decision.confidence,
        summary: response_text.clone(),
        caveats_json: serde_json::json!(decision.caveats),
        evidence_ids_json: serde_json::json!(decision.evidence_ids),
        suggested_action: decision.suggested_action.clone(),
        created_at: now_secs(),
    };
    truth_db::repo::insert_verdict(conn, &verdict)?;

    // Mark the check complete.
    let mut completed = check.clone();
    completed.status = CheckStatus::Completed;
    truth_db::repo::insert_check(conn, &completed)?;

    Ok(CheckOutcome {
        check_id: check.id,
        claim,
        decision,
        response_text,
        evidence: evidence_json,
    })
}

fn default_service(config: &Config) -> Option<&str> {
    config.loki.labels.get("service").map(String::as_str)
}

fn log_evidence_json(
    config: &Config,
    pq: &PlannedQuery,
    res: &EvidenceQueryResult,
    local_log: Option<&str>,
) -> EvidenceJson {
    let needle = pq.route.clone().or_else(|| pq.pattern.clone());
    let citation = if config.loki.enabled {
        Some(config.loki.base_url.clone())
    } else {
        local_log.map(str::to_string)
    };
    EvidenceJson {
        source: res.source.as_db_str().to_string(),
        kind: res.query_type.as_label().to_string(),
        subject: needle,
        value: res.count.map(|c| c.into()),
        unit: matches!(res.query_type, QueryType::RouteCount | QueryType::EventCount)
            .then(|| "requests".to_string()),
        citation,
    }
}

/// Persist a log query result as an `EvidenceQuery` audit row (spec §15.4).
fn persist_query(conn: &Connection, check_id: &str, res: &EvidenceQueryResult) -> Result<()> {
    let eq = EvidenceQuery {
        id: new_id(),
        check_id: check_id.to_string(),
        source: res.source,
        query_type: res.query_type.as_label().to_string(),
        query_text: res.query_text.clone(),
        time_from: res.time_from,
        time_to: res.time_to,
        result_summary_json: res.summary_json(),
        executed_at: now_secs(),
    };
    truth_db::repo::insert_evidence_query(conn, &eq)
}

fn run_log_query(
    config: &Config,
    pq: &PlannedQuery,
    local_log_path: Option<&str>,
) -> Result<Option<EvidenceQueryResult>> {
    let needle = pq
        .route
        .clone()
        .or_else(|| pq.pattern.clone())
        .unwrap_or_default();
    crate::service::run_log_query(
        config,
        pq.query_type,
        &needle,
        pq.window.as_deref(),
        pq.environment.as_deref(),
        pq.service.as_deref(),
        local_log_path,
    )
}

/// Repo-backed query types resolve against stored evidence.
fn run_repo_query(
    conn: &Connection,
    pq: &PlannedQuery,
) -> Result<(Vec<EvidenceItem>, Vec<String>, Vec<EvidenceJson>)> {
    let needle = pq.name.clone().or_else(|| pq.route.clone());
    let mut lines = Vec::new();
    let mut json = Vec::new();

    let items = match pq.query_type {
        QueryType::RouteExists => {
            let mut subj = needle.unwrap_or_default();
            let mut items = truth_db::repo::evidence_by_subject(conn, &subj)?;
            // Concept resolution: if the literal subject isn't an indexed route,
            // try to resolve a fuzzy subject ("old checkout") to the nearest one.
            if !items.iter().any(|i| i.predicate.as_deref() == Some("route_exists")) {
                if let Some(res) = resolve_route(conn, &subj)? {
                    lines.push(format!(
                        "resolved `{}` → `{}` (confidence {:.0}%)",
                        subj,
                        res.label,
                        res.confidence * 100.0
                    ));
                    subj = res.label;
                    items = truth_db::repo::evidence_by_subject(conn, &subj)?;
                }
            }
            let found: Vec<EvidenceItem> = items
                .into_iter()
                .filter(|i| i.predicate.as_deref() == Some("route_exists"))
                .collect();
            if let Some(it) = found.first() {
                lines.push(format!("repo: route `{}` exists ({})", subj, uri_line(it)));
                json.push(EvidenceJson {
                    source: "repo".into(),
                    kind: "route_exists".into(),
                    subject: Some(subj.clone()),
                    value: Some(true.into()),
                    unit: None,
                    citation: uri_line_opt(it),
                });
            }
            found
        }
        QueryType::EnvVarExists => {
            let subj = needle.unwrap_or_default();
            let items: Vec<EvidenceItem> = truth_db::repo::evidence_by_subject(conn, &subj)?
                .into_iter()
                .filter(|i| i.predicate.as_deref() == Some("env_var_exists"))
                .collect();
            if let Some(it) = items.first() {
                json.push(EvidenceJson {
                    source: "repo".into(),
                    kind: "env_var_exists".into(),
                    subject: Some(subj.clone()),
                    value: Some(true.into()),
                    unit: None,
                    citation: uri_line_opt(it),
                });
            }
            items
        }
        QueryType::ConfigValue | QueryType::ConstantValue => {
            // Look up by the predicate keyword (port, retry_count, timeout, ...).
            let predicate = needle.unwrap_or_default();
            let items = truth_db::repo::evidence_by_predicate(conn, &predicate)?;
            if let Some(it) = items.first() {
                if let Some(v) = &it.value_json {
                    lines.push(format!("repo: `{predicate}` = {v} ({})", uri_line(it)));
                    json.push(EvidenceJson {
                        source: "repo".into(),
                        kind: predicate.clone(),
                        subject: it.subject_text.clone(),
                        value: Some(v.clone()),
                        unit: it.unit.clone(),
                        citation: uri_line_opt(it),
                    });
                }
            }
            items
        }
        _ => Vec::new(),
    };

    Ok((items, lines, json))
}

/// Indexed routes as resolver candidates. The canonical label is the route
/// path; the search text is the enriched human description (`object_text`, e.g.
/// "checkout handle checkout legacy flow") when available — that's what makes
/// fuzzy/embedding resolution match human phrasing.
fn route_candidates(conn: &Connection) -> Result<Vec<truth_core::concept::Candidate>> {
    use std::collections::BTreeMap;
    let mut by_route: BTreeMap<String, String> = BTreeMap::new();
    for i in truth_db::repo::all_evidence(conn)? {
        if i.predicate.as_deref() != Some("route_exists") {
            continue;
        }
        let Some(route) = i.subject_text else { continue };
        // Accumulate the richest search text seen for this route. Always include
        // the path words themselves so token-overlap still works.
        let label = i.object_text.unwrap_or_default();
        let search = format!("{route} {label}");
        by_route
            .entry(route)
            .and_modify(|s| {
                if search.len() > s.len() {
                    *s = search.clone();
                }
            })
            .or_insert(search);
    }
    Ok(by_route
        .into_iter()
        .map(|(route, search)| truth_core::concept::Candidate::with_search_text(route, search))
        .collect())
}

/// Resolve a fuzzy route subject to the nearest indexed route, if confident.
fn resolve_route(
    conn: &Connection,
    subject: &str,
) -> Result<Option<truth_core::concept::Resolution>> {
    use truth_core::concept::{ConceptResolver, FuzzyResolver};
    Ok(FuzzyResolver::default().resolve(subject, &route_candidates(conn)?))
}

/// Whether a claim lacks a concrete subject we can act on, so concept resolution
/// against indexed routes is worth attempting.
fn needs_subject_resolution(claim: &truth_core::claim::StructuredClaim) -> bool {
    use truth_core::claim::ClaimType;
    let no_subject = claim.subject.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true);
    no_subject
        && matches!(
            claim.claim_type,
            ClaimType::UsageCount | ClaimType::RouteExists | ClaimType::Unknown
        )
}

/// Resolve a whole claim phrase ("does anyone still use old checkout") to an
/// indexed route, using a slightly higher bar than the route-query fallback to
/// avoid spurious interpretations.
fn resolve_from_text(
    conn: &Connection,
    text: &str,
) -> Result<Option<truth_core::concept::Resolution>> {
    use truth_core::concept::{ConceptResolver, FuzzyResolver};
    let resolver = FuzzyResolver { threshold: 0.25 };
    Ok(resolver.resolve(text, &route_candidates(conn)?))
}

fn uri_line_opt(item: &EvidenceItem) -> Option<String> {
    let uri = item.metadata_json.get("uri").and_then(|v| v.as_str())?;
    let line = item.metadata_json.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    Some(if line > 0 { format!("{uri}:{line}") } else { uri.to_string() })
}

fn uri_line(item: &EvidenceItem) -> String {
    let uri = item.metadata_json.get("uri").and_then(|v| v.as_str()).unwrap_or("?");
    let line = item.metadata_json.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    if line > 0 {
        format!("{uri}:{line}")
    } else {
        uri.to_string()
    }
}

fn format_log_line(pq: &PlannedQuery, res: &EvidenceQueryResult) -> String {
    let src = res.source.as_db_str();
    let needle = pq.route.as_deref().or(pq.pattern.as_deref()).unwrap_or("");
    match res.query_type {
        QueryType::RouteCount | QueryType::EventCount => {
            let mut s = format!(
                "{src} route_count for `{needle}`: {} request(s)",
                res.count.unwrap_or(0)
            );
            if let Some(latest) = res.latest_seen {
                s.push_str(&format!(", latest at {}", fmt_ts(latest)));
            }
            s
        }
        QueryType::ErrorCount => format!(
            "{src} error_count for `{needle}`: {} error(s)",
            res.count.unwrap_or(0)
        ),
        QueryType::LatestOccurrence => match res.latest_seen {
            Some(latest) => format!("{src} latest occurrence of `{needle}`: {}", fmt_ts(latest)),
            None => format!("{src}: no occurrence of `{needle}` in window"),
        },
        QueryType::JobSuccess => format!(
            "{src} job successes for `{needle}`: {}",
            res.count.unwrap_or(0)
        ),
        _ => format!("{src}: {} match(es)", res.count.unwrap_or(0)),
    }
}

fn fmt_ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| secs.to_string())
}
