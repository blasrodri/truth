//! `truth baseline` — an exploratory readiness command. It auto-generates a
//! small set of useful checks from indexed evidence (and configured/local logs)
//! and reports observations. It is observational: it never fails because errors
//! were observed.

use crate::config_util::{load_config, print_json};
use crate::inspect::{self, Category};
use crate::service::{self, ObservationStatus};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use truth_core::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct BaselineCheck {
    pub kind: String,
    pub subject: String,
    /// observed | not_observed | inconclusive | found | not_found
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BaselineSummary {
    pub observed: usize,
    pub inconclusive: usize,
    pub warnings: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaselineReport {
    pub summary: BaselineSummary,
    pub checks: Vec<BaselineCheck>,
}

/// Build and run the baseline checks against indexed evidence + logs.
pub fn run_baseline(
    conn: &Connection,
    config: &Config,
    local_log: Option<&str>,
) -> Result<BaselineReport> {
    let items = inspect::load_items(conn)?;
    let mut checks = Vec::new();
    let mut summary = BaselineSummary::default();

    // Usage checks for each indexed route (deduped, capped for readability).
    for route in distinct(&items, Category::Route, 10) {
        let obs = service::run_usage(conn, config, &route, None, None, None, local_log)?;
        record(&mut checks, &mut summary, "usage", &route, obs);
    }

    // Config checks for indexed constants and ports.
    let mut const_keys = distinct(&items, Category::Constant, 10);
    const_keys.extend(distinct(&items, Category::Port, 5));
    for key in const_keys {
        let obs = service::run_config(conn, &key)?;
        record(&mut checks, &mut summary, "config", &key, obs);
    }

    // Env var presence checks.
    for key in distinct(&items, Category::EnvVar, 10) {
        let obs = service::run_config(conn, &key)?;
        record(&mut checks, &mut summary, "config", &key, obs);
    }

    Ok(BaselineReport { summary, checks })
}

/// Distinct subjects in a category, preserving order, capped to `max`.
fn distinct(items: &[crate::inspect::InspectItem], cat: Category, max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter(|i| i.category == cat)
        .map(|i| i.subject.clone())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .take(max)
        .collect()
}

fn record(
    checks: &mut Vec<BaselineCheck>,
    summary: &mut BaselineSummary,
    kind: &str,
    subject: &str,
    obs: service::Observation,
) {
    let status_str = match obs.status {
        ObservationStatus::Observed => "observed",
        ObservationStatus::NotObserved => "not_observed",
        ObservationStatus::Inconclusive => "inconclusive",
        ObservationStatus::Found => "found",
        ObservationStatus::NotFound => "not_found",
    };
    match obs.status {
        ObservationStatus::Observed | ObservationStatus::Found => summary.observed += 1,
        ObservationStatus::Inconclusive | ObservationStatus::NotFound => summary.inconclusive += 1,
        ObservationStatus::NotObserved => summary.warnings += 1,
    }
    // For usage, the informative value is the log count; for config, the actual
    // defined value (not the number of matches).
    let value = if kind == "usage" {
        obs.count.map(serde_json::Value::from)
    } else {
        obs.evidence.iter().find_map(|e| e.value.clone())
    };
    checks.push(BaselineCheck {
        kind: kind.to_string(),
        subject: subject.to_string(),
        status: status_str.to_string(),
        value,
    });
}

/// `truth baseline [--local-log <path>] [--json]`.
pub fn baseline(local_log: Option<&str>, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let report = run_baseline(&conn, &config, local_log)?;

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    println!("Baseline checks\n");

    print_group(&report, "usage", "Usage");
    print_group(&report, "config", "Config");

    println!("\nSummary:");
    println!("{} observed/found", report.summary.observed);
    println!("{} inconclusive", report.summary.inconclusive);
    println!("{} warning(s)", report.summary.warnings);

    if report.checks.is_empty() {
        println!("\nNo indexed evidence to baseline. Run `truth index .` first.");
    }
    Ok(())
}

fn print_group(report: &BaselineReport, kind: &str, title: &str) {
    let group: Vec<&BaselineCheck> = report.checks.iter().filter(|c| c.kind == kind).collect();
    if group.is_empty() {
        return;
    }
    println!("{title}:");
    for c in group {
        let (mark, suffix) = match c.status.as_str() {
            "observed" => ("✓", count_suffix(c, "observed")),
            "found" => ("✓", value_suffix(c, "found")),
            "not_observed" => ("!", "not observed".to_string()),
            "not_found" => ("?", "not found".to_string()),
            _ => ("?", "inconclusive".to_string()),
        };
        println!("{mark} {} — {}", c.subject, suffix);
    }
    println!();
}

fn count_suffix(c: &BaselineCheck, label: &str) -> String {
    match &c.value {
        Some(v) if v.is_number() => format!("{label}, {v} request(s)"),
        _ => label.to_string(),
    }
}

fn value_suffix(c: &BaselineCheck, label: &str) -> String {
    match &c.value {
        Some(v) => format!("{label}, value = {v}"),
        None => label.to_string(),
    }
}
