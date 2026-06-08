//! Deterministic observation commands (`usage`, `errors`, `latest`, `config`)
//! and the shared evidence/JSON shapes used by every command.
//!
//! These commands do NOT use the LLM or regex claim extraction. They take an
//! explicit subject/pattern/key and report observations with citations and
//! caveats. They are the robust core that a future Slack UI sits on top of.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use truth_core::config::Config;
use truth_core::query::{EvidenceQueryResult, EvidenceQuerySpec, QueryType};
use truth_core::traits::QueryableSource;
use truth_logs::{LocalFileSource, LokiSource};

/// A single citation-bearing piece of evidence, shared across JSON outputs.
#[derive(Debug, Clone, Serialize)]
pub struct EvidenceJson {
    pub source: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
}

/// Observation status for the deterministic commands. Deliberately distinct
/// from claim `VerdictStatus` so we never imply a claim verdict here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Observed,
    NotObserved,
    Inconclusive,
    Found,
    NotFound,
}

/// Output of a deterministic observation command.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub status: ObservationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_seen: Option<String>,
    pub summary: String,
    pub evidence: Vec<EvidenceJson>,
    pub caveats: Vec<String>,
}

/// Source of the configured log adapter, for caveats. An explicit local log
/// takes precedence over Loki (matching `run_log_query`).
fn log_source_label(config: &Config, local_log: Option<&str>) -> String {
    if let Some(path) = local_log {
        format!("Based only on local log file: {path}.")
    } else if config.loki.enabled {
        "Based only on the configured Loki source and selected time window.".to_string()
    } else {
        "No log source configured (Loki disabled and no --local-log given).".to_string()
    }
}

fn default_service(config: &Config) -> Option<String> {
    config.loki.labels.get("service").cloned()
}

/// Run one log query against Loki or a local file, returning `None` if no log
/// source is available or a Loki network error occurs.
///
/// An explicit `--local-log` always takes precedence over Loki, so the offline
/// demo works regardless of `[loki] enabled` in the config.
pub fn run_log_query(
    config: &Config,
    query_type: QueryType,
    needle: &str,
    window: Option<&str>,
    env: Option<&str>,
    service: Option<&str>,
    local_log: Option<&str>,
) -> Result<Option<EvidenceQueryResult>> {
    let spec = EvidenceQuerySpec {
        query_type,
        needle: Some(needle.to_string()),
        window: window.map(str::to_string),
        environment: env.map(str::to_string),
        service: service
            .map(str::to_string)
            .or_else(|| default_service(config)),
    };

    if let Some(path) = local_log {
        let src = LocalFileSource::new(
            path,
            config.security.max_log_window_days,
            config.security.max_log_samples,
            config.security.include_log_samples,
        );
        Ok(Some(src.execute_query(spec)?))
    } else if config.loki.enabled {
        let src = LokiSource::new(
            &config.loki.base_url,
            &config.loki.default_env,
            config.loki.labels.clone(),
            config.security.max_log_window_days,
            config.security.max_log_samples,
            config.security.include_log_samples,
        );
        match src.execute_query(spec) {
            Ok(r) => Ok(Some(r)),
            Err(_) => Ok(None),
        }
    } else {
        Ok(None)
    }
}

/// Citation `uri:line` for a repo evidence item, if present.
fn uri_line(item: &truth_core::models::EvidenceItem) -> Option<String> {
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

fn fmt_ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| secs.to_string())
}

fn log_source_name(config: &Config, local_log: Option<&str>) -> &'static str {
    if local_log.is_some() {
        "local_logs"
    } else if config.loki.enabled {
        "loki"
    } else {
        "local_logs"
    }
}

fn log_citation(config: &Config, local_log: Option<&str>) -> Option<String> {
    if let Some(path) = local_log {
        Some(path.to_string())
    } else if config.loki.enabled {
        Some(config.loki.base_url.clone())
    } else {
        None
    }
}

/// `truth usage <subject>` — observed traffic + repo route existence.
pub fn run_usage(
    conn: &Connection,
    config: &Config,
    subject: &str,
    window: Option<&str>,
    env: Option<&str>,
    service: Option<&str>,
    local_log: Option<&str>,
) -> Result<Observation> {
    let mut evidence = Vec::new();
    let mut caveats = Vec::new();

    let log_res = run_log_query(
        config,
        QueryType::RouteCount,
        subject,
        window,
        env,
        service,
        local_log,
    )?;
    let count = log_res.as_ref().and_then(|r| r.count);
    let latest = log_res.as_ref().and_then(|r| r.latest_seen);
    if log_res.is_some() {
        evidence.push(EvidenceJson {
            source: log_source_name(config, local_log).to_string(),
            kind: "route_count".to_string(),
            subject: Some(subject.to_string()),
            value: count.map(|c| c.into()),
            unit: Some("requests".to_string()),
            citation: log_citation(config, local_log),
        });
    }

    // Repo route existence.
    let route_items = truth_db::repo::evidence_by_subject(conn, subject)?;
    let route_exists = route_items
        .iter()
        .find(|i| i.predicate.as_deref() == Some("route_exists"));
    if let Some(it) = route_exists {
        evidence.push(EvidenceJson {
            source: "repo".to_string(),
            kind: "route_exists".to_string(),
            subject: Some(subject.to_string()),
            value: Some(true.into()),
            unit: None,
            citation: uri_line(it),
        });
    }

    caveats.push(log_source_label(config, local_log));

    let has_route = route_exists.is_some();
    let (status, summary) = match count {
        Some(c) if c > 0 => (
            ObservationStatus::Observed,
            format!("`{subject}` had {c} matching request(s) in the configured logs."),
        ),
        Some(_) if has_route => {
            caveats.push("No usage observed does not prove nobody uses it.".to_string());
            (
                ObservationStatus::NotObserved,
                format!("I found no matching requests for `{subject}` in the configured logs."),
            )
        }
        Some(_) => {
            caveats.push(
                "No matching route was found in the indexed repo. The repo may be stale or incomplete."
                    .to_string(),
            );
            (
                ObservationStatus::Inconclusive,
                format!(
                    "I found no matching requests and no matching route in the indexed repo for `{subject}`. \
                     It may be misspelled, not indexed, or not covered by configured logs."
                ),
            )
        }
        None if has_route => (
            ObservationStatus::Inconclusive,
            format!("No log source is configured, but `{subject}` exists in the indexed repo."),
        ),
        None => (
            ObservationStatus::Inconclusive,
            format!("No log source configured and `{subject}` not found in the indexed repo."),
        ),
    };

    Ok(Observation {
        status,
        subject: Some(subject.to_string()),
        count,
        latest_seen: latest.map(fmt_ts),
        summary,
        evidence,
        caveats,
    })
}

/// `truth errors <pattern>` — observed error occurrences.
pub fn run_errors(
    config: &Config,
    pattern: &str,
    window: Option<&str>,
    env: Option<&str>,
    service: Option<&str>,
    local_log: Option<&str>,
) -> Result<Observation> {
    let mut evidence = Vec::new();
    let mut caveats = Vec::new();

    let log_res = run_log_query(
        config,
        QueryType::ErrorCount,
        pattern,
        window,
        env,
        service,
        local_log,
    )?;
    let count = log_res.as_ref().and_then(|r| r.count);
    let latest = log_res.as_ref().and_then(|r| r.latest_seen);

    if log_res.is_some() {
        evidence.push(EvidenceJson {
            source: log_source_name(config, local_log).to_string(),
            kind: "error_count".to_string(),
            subject: Some(pattern.to_string()),
            value: count.map(|c| c.into()),
            unit: Some("errors".to_string()),
            citation: log_citation(config, local_log),
        });
    }
    caveats.push(log_source_label(config, local_log));

    let (status, summary) = match count {
        Some(c) if c > 0 => (
            ObservationStatus::Observed,
            format!("`{pattern}` occurred {c} time(s) in the configured logs."),
        ),
        Some(_) => {
            caveats.push("This does not prove the issue is fixed.".to_string());
            (
                ObservationStatus::NotObserved,
                format!("I found no occurrences of `{pattern}` in the configured logs."),
            )
        }
        None => (
            ObservationStatus::Inconclusive,
            format!("No log source configured to check `{pattern}`."),
        ),
    };

    Ok(Observation {
        status,
        subject: Some(pattern.to_string()),
        count,
        latest_seen: latest.map(fmt_ts),
        summary,
        evidence,
        caveats,
    })
}

/// `truth latest <pattern>` — most recent occurrence.
pub fn run_latest(
    config: &Config,
    pattern: &str,
    window: Option<&str>,
    env: Option<&str>,
    service: Option<&str>,
    local_log: Option<&str>,
) -> Result<Observation> {
    let mut evidence = Vec::new();
    let mut caveats = Vec::new();

    let log_res = run_log_query(
        config,
        QueryType::LatestOccurrence,
        pattern,
        window,
        env,
        service,
        local_log,
    )?;
    let latest = log_res.as_ref().and_then(|r| r.latest_seen);

    if log_res.is_some() {
        evidence.push(EvidenceJson {
            source: log_source_name(config, local_log).to_string(),
            kind: "latest_occurrence".to_string(),
            subject: Some(pattern.to_string()),
            value: latest.map(|t| fmt_ts(t).into()),
            unit: None,
            citation: log_citation(config, local_log),
        });
    }
    caveats.push(log_source_label(config, local_log));

    let (status, summary) = match latest {
        Some(t) => (
            ObservationStatus::Observed,
            format!("`{pattern}` last appeared at {}.", fmt_ts(t)),
        ),
        None if log_res.is_some() => (
            ObservationStatus::NotObserved,
            format!("I found no occurrence of `{pattern}` in the configured window."),
        ),
        None => (
            ObservationStatus::Inconclusive,
            format!("No log source configured to check `{pattern}`."),
        ),
    };

    Ok(Observation {
        status,
        subject: Some(pattern.to_string()),
        count: None,
        latest_seen: latest.map(fmt_ts),
        summary,
        evidence,
        caveats,
    })
}

/// `truth config <key>` — indexed code/config definitions matching a key.
pub fn run_config(conn: &Connection, key: &str) -> Result<Observation> {
    let items = truth_db::repo::evidence_matching_key(conn, key)?;
    // Only definitions with a concrete value are interesting here.
    let mut evidence = Vec::new();
    for it in &items {
        let value = it.value_json.clone();
        evidence.push(EvidenceJson {
            source: "repo".to_string(),
            kind: it
                .predicate
                .clone()
                .unwrap_or_else(|| "definition".to_string()),
            subject: it.subject_text.clone(),
            value,
            unit: it.unit.clone(),
            citation: uri_line(it),
        });
    }

    if evidence.is_empty() {
        Ok(Observation {
            status: ObservationStatus::NotFound,
            subject: Some(key.to_string()),
            count: None,
            latest_seen: None,
            summary: format!("No indexed definition found for `{key}`."),
            evidence,
            caveats: vec![
                "Try `truth index .` first.".to_string(),
                "The repo may not be indexed or the key may differ.".to_string(),
            ],
        })
    } else {
        Ok(Observation {
            status: ObservationStatus::Found,
            subject: Some(key.to_string()),
            count: Some(evidence.len() as i64),
            latest_seen: None,
            summary: format!(
                "Found {} config/code definition(s) for `{key}`.",
                evidence.len()
            ),
            evidence,
            caveats: vec!["Based on the indexed repo at index time.".to_string()],
        })
    }
}

/// Render an observation as a human-readable block.
pub fn render_observation(obs: &Observation) -> String {
    let headline = match obs.status {
        ObservationStatus::Observed => "Observed.",
        ObservationStatus::NotObserved => "Not observed.",
        ObservationStatus::Inconclusive => "Inconclusive.",
        ObservationStatus::Found => "Found.",
        ObservationStatus::NotFound => "Not found.",
    };
    let mut out = String::new();
    out.push_str(headline);
    out.push_str("\n\n");
    out.push_str(&obs.summary);
    out.push('\n');

    if !obs.evidence.is_empty() {
        out.push_str("\nEvidence:\n");
        for e in &obs.evidence {
            let val = e
                .value
                .as_ref()
                .map(|v| format!(" = {v}"))
                .unwrap_or_default();
            let cite = e
                .citation
                .as_ref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            out.push_str(&format!("• {} {}{}{}\n", e.source, e.kind, val, cite));
        }
    }
    if !obs.caveats.is_empty() {
        out.push_str("\nCaveats:\n");
        for c in &obs.caveats {
            out.push_str(&format!("• {c}\n"));
        }
    }
    out.trim_end().to_string()
}
