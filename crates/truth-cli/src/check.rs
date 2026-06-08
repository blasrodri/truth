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

    // Code-reference evidence for usage claims: "nobody uses X" is far stronger
    // (or refuted) by whether X is referenced anywhere in the indexed code, not
    // just whether it shows up in logs. Lazy, check-time scan.
    let mut code_references: Option<usize> = None;
    if matches!(
        claim.claim_type,
        truth_core::claim::ClaimType::UsageCount | truth_core::claim::ClaimType::DependencyUsed
    ) {
        if let Some(subject) = claim.subject.as_deref() {
            // Reference count feeds the verdict (the usage signal for code with
            // no logs); the evidence line is for display.
            let report = crate::refs::build_report(conn, subject)?;
            // Only feed a reference COUNT to the verdict when the subject is a
            // real thing in the repo: referenced (count>0), or defined-but-unused
            // (definition_only). A subject that's simply "not found" (likely a
            // typo / nonexistent route) yields no code signal → stays inconclusive.
            if report.files_scanned > 0 && report.status != "unreferenced" {
                code_references = Some(report.count);
            }
            if let Some((line, j)) = code_reference_evidence(conn, subject)? {
                evidence_lines.push(line);
                evidence_json.push(j);
            }
            // Doc coverage / drift: flag when a subject is documented but absent
            // from code (phantom feature), or present but undocumented.
            if let Some((line, j)) = doc_evidence(conn, subject)? {
                evidence_lines.push(line);
                evidence_json.push(j);
            }
        }
    }

    // Symbol-existence evidence: for "I added/removed function X" claims, the
    // index symbol status (referenced / definition_only / unreferenced) tells us
    // whether the symbol is present. The diff (below) overrides for this-turn
    // changes.
    let mut symbol_status: Option<String> = None;
    if matches!(claim.claim_type, truth_core::claim::ClaimType::SymbolExists) {
        if let Some(subject) = claim.subject.as_deref() {
            // Prefer AST `symbol_exists` definitions: a real `fn`/`struct`/...
            // declaration, never fooled by the name in a comment or string. If the
            // repo was indexed with AST (any symbol_exists facts exist), trust AST
            // EXCLUSIVELY for symbols — a symbol with no AST definition genuinely
            // isn't defined, so we must NOT fall back to the text scan (which would
            // match comments/strings and re-introduce the imprecision).
            let ast_def = truth_db::repo::evidence_by_subject(conn, subject)?
                .into_iter()
                .any(|i| {
                    i.predicate.as_deref() == Some("symbol_exists")
                        && i.value_json
                            .as_ref()
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                });
            let repo_has_ast_symbols = truth_db::repo::has_predicate(conn, "symbol_exists")?;
            if ast_def {
                symbol_status = Some("definition_only".to_string());
                evidence_lines.push(format!("code: `{subject}` is a defined symbol (AST)"));
            } else if repo_has_ast_symbols {
                // AST mode is active and this symbol has no definition → absent.
                symbol_status = Some("unreferenced".to_string());
                evidence_lines.push(format!("code: `{subject}` has no definition (AST)"));
            } else {
                let report = crate::refs::build_report(conn, subject)?;
                if report.files_scanned > 0 {
                    symbol_status = Some(report.status.clone());
                    evidence_lines.push(match report.status.as_str() {
                        "referenced" => format!(
                            "code: `{subject}` is defined/referenced ({} hit(s))",
                            report.count
                        ),
                        "definition_only" => {
                            format!("code: `{subject}` is defined (no other references)")
                        }
                        _ => format!("code: `{subject}` not found in the indexed code"),
                    });
                }
            }
        }
    }

    // Diff evidence: for existence claims, "did THIS working tree add/remove the
    // subject?" is the freshest, most direct signal — it answers "I added /x" /
    // "I removed /x" before any re-index. Ranked ABOVE the index by prepending,
    // and it short-circuits to Untouched (no item) on a clean tree, so repos
    // with no diff fall through to the index unchanged.
    if matches!(
        claim.claim_type,
        truth_core::claim::ClaimType::RouteExists | truth_core::claim::ClaimType::SymbolExists
    ) {
        if let Some(subject) = claim.subject.as_deref() {
            let fact = crate::diff_facts::scan(&config.repo.root, subject)?;
            if let Some(item) = fact.as_existence_item() {
                evidence_lines.insert(0, fact.evidence_line());
                evidence_json.insert(
                    0,
                    EvidenceJson {
                        source: "diff".into(),
                        kind: "route_exists".into(),
                        subject: Some(subject.to_string()),
                        value: fact.present_after().map(|b| b.into()),
                        unit: None,
                        citation: fact.file.clone(),
                    },
                );
                // Prepend so diff outranks the index in `decide_existence`.
                repo_items.insert(0, item);
            }
        }
    }

    let mut decision = decide(&VerdictInput {
        claim: &claim,
        items: &repo_items,
        query_results: &query_results,
        usage_threshold: 0,
        code_references,
        symbol_status,
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
        unit: matches!(
            res.query_type,
            QueryType::RouteCount | QueryType::EventCount
        )
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
            // Concept resolution: if the subject isn't an indexed route, try to
            // resolve a fuzzy subject ("old checkout") to the nearest one. But a
            // LITERAL path ("/v1/ghost") is a precise existence claim — fuzzy-
            // matching it to a different real route ("/v1/checkout") would launder
            // a fabricated route into a real one, so we only fuzzy-resolve
            // natural-language subjects, never literal `/`-paths.
            let is_literal_path = subj.trim_start().starts_with('/');
            if !is_literal_path
                && !items
                    .iter()
                    .any(|i| i.predicate.as_deref() == Some("route_exists"))
            {
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
                // Git recency: when the route's file exists, surface when it was
                // last changed — "the file hasn't changed since X" strengthens a
                // "nobody uses it" verdict. Lazy, per-check; no-ops outside git.
                if let Some((line, j)) = git_recency_for(it) {
                    lines.push(line);
                    json.push(j);
                }
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
        QueryType::DependencyExists => {
            let subj = needle.unwrap_or_default();
            let items: Vec<EvidenceItem> = truth_db::repo::evidence_by_subject(conn, &subj)?
                .into_iter()
                .filter(|i| i.predicate.as_deref() == Some("dependency_exists"))
                .collect();
            if let Some(it) = items.first() {
                lines.push(format!(
                    "repo: `{subj}` is a declared dependency ({})",
                    uri_line(it)
                ));
                json.push(EvidenceJson {
                    source: "repo".into(),
                    kind: "dependency_exists".into(),
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
        let Some(route) = i.subject_text else {
            continue;
        };
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
    let no_subject = claim
        .subject
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
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

/// Code-reference evidence for a usage subject: how many times it is referenced
/// in the indexed code (excluding its definition). `None` if not indexed.
fn code_reference_evidence(
    conn: &Connection,
    subject: &str,
) -> Result<Option<(String, EvidenceJson)>> {
    let report = crate::refs::build_report(conn, subject)?;
    // Only attach when we actually scanned code.
    if report.files_scanned == 0 {
        return Ok(None);
    }
    let line = match report.status.as_str() {
        "referenced" => format!(
            "code: `{subject}` is referenced {} time(s) in the repo",
            report.count
        ),
        "definition_only" => {
            format!("code: `{subject}` is defined but never referenced (dead-code signal)")
        }
        _ => format!("code: `{subject}` not found in the indexed code"),
    };
    let j = EvidenceJson {
        source: "code".into(),
        kind: "reference_count".into(),
        subject: Some(subject.to_string()),
        value: Some(report.count.into()),
        unit: Some("references".into()),
        citation: report
            .samples
            .first()
            .map(|h| format!("{}:{}", h.file, h.line)),
    };
    Ok(Some((line, j)))
}

/// Documentation coverage / drift evidence for a subject. Only surfaced when
/// informative (documented or drift); stays quiet for plain undocumented code to
/// avoid noise on every check.
fn doc_evidence(conn: &Connection, subject: &str) -> Result<Option<(String, EvidenceJson)>> {
    let report = crate::docs::build_report(conn, subject)?;
    let line = match report.status.as_str() {
        "documented" => format!(
            "docs: `{subject}` is documented ({} mention(s))",
            report.doc_count
        ),
        "drift" => format!("docs: `{subject}` appears in docs but not in code (possible drift)"),
        _ => return Ok(None),
    };
    let j = EvidenceJson {
        source: "docs".into(),
        kind: report.status.clone(),
        subject: Some(subject.to_string()),
        value: Some(report.doc_count.into()),
        unit: Some("mentions".into()),
        citation: report
            .doc_samples
            .first()
            .map(|h| format!("{}:{}", h.file, h.line)),
    };
    Ok(Some((line, j)))
}

/// Git last-modified evidence for the file an item came from. Returns a human
/// line + structured evidence, or `None` outside a git repo / on any error.
fn git_recency_for(item: &EvidenceItem) -> Option<(String, EvidenceJson)> {
    let uri = item.metadata_json.get("uri").and_then(|v| v.as_str())?;
    let path = std::path::Path::new(uri);

    let history = truth_git::GitHistory::for_file(path);
    if !history.available() {
        return None;
    }
    let ts = history.last_modified(path)?;
    let date = chrono::DateTime::from_timestamp(ts, 0)?
        .format("%Y-%m-%d")
        .to_string();
    let line = format!("git: `{uri}` last changed {date}");
    let j = EvidenceJson {
        source: "git".into(),
        kind: "last_modified".into(),
        subject: Some(uri.to_string()),
        value: Some(serde_json::Value::String(date)),
        unit: None,
        citation: Some(uri.to_string()),
    };
    Some((line, j))
}

fn uri_line_opt(item: &EvidenceItem) -> Option<String> {
    let uri = item.metadata_json.get("uri").and_then(|v| v.as_str())?;
    let line = item
        .metadata_json
        .get("line")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(if line > 0 {
        format!("{uri}:{line}")
    } else {
        uri.to_string()
    })
}

fn uri_line(item: &EvidenceItem) -> String {
    let uri = item
        .metadata_json
        .get("uri")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let line = item
        .metadata_json
        .get("line")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
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
