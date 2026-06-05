//! Tests for Phase 6A: doctor, inspect, baseline, eval --record, diagnostics.

use std::path::{Path, PathBuf};
use truth_cli::baseline::run_baseline;
use truth_cli::check::run_check;
use truth_cli::diagnostics::check_hint;
use truth_cli::doctor;
use truth_cli::eval::{record_eval, Fixture};
use truth_cli::inspect::{self, Category};
use truth_core::config::Config;
use truth_core::enums::Trigger;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}
fn sample_repo() -> PathBuf {
    repo_root().join("examples/sample-repo")
}
fn sample_log() -> String {
    repo_root().join("examples/sample-logs/api.log").to_string_lossy().into_owned()
}

fn offline_config() -> Config {
    let mut c = Config::from_toml_str("").unwrap();
    c.loki.enabled = false;
    c
}

fn indexed() -> (rusqlite::Connection, Config) {
    let conn = truth_db::open_in_memory().unwrap();
    let config = offline_config();
    truth_indexer::index_repo(&conn, &config.repo, Some(&sample_repo())).unwrap();
    (conn, config)
}

// ---------- doctor ----------

#[test]
fn doctor_ready_when_indexed() {
    // doctor::run opens the configured DB; point it at an in-memory-equivalent by
    // indexing a temp DB file.
    let dir = std::env::temp_dir().join("truth_doctor_ready");
    std::fs::create_dir_all(&dir).unwrap();
    let dbpath = dir.join("d.sqlite");
    let _ = std::fs::remove_file(&dbpath);
    let mut config = offline_config();
    config.database.path = dbpath.to_string_lossy().into_owned();
    let conn = truth_db::open(&config.database.path).unwrap();
    truth_indexer::index_repo(&conn, &config.repo, Some(&sample_repo())).unwrap();
    drop(conn);

    let report = doctor::run(&config, true);
    assert_eq!(report.status, "ready");
    assert!(report.checks.iter().any(|c| c.name == "indexed_evidence"
        && matches!(c.status, doctor::CheckStatus::Ok)));
}

#[test]
fn doctor_warns_when_not_indexed_and_suggests_index() {
    let dir = std::env::temp_dir().join("truth_doctor_empty");
    std::fs::create_dir_all(&dir).unwrap();
    let dbpath = dir.join("d.sqlite");
    let _ = std::fs::remove_file(&dbpath);
    let mut config = offline_config();
    config.database.path = dbpath.to_string_lossy().into_owned();

    let report = doctor::run(&config, false);
    // No index yet → there is a warn for indexed_evidence and an index suggestion.
    assert!(report.checks.iter().any(|c| c.name == "indexed_evidence"
        && matches!(c.status, doctor::CheckStatus::Warn)));
    assert!(report.suggested_commands.iter().any(|c| c.contains("index")));
    // Missing config is reported.
    assert!(report.checks.iter().any(|c| c.name == "config"
        && matches!(c.status, doctor::CheckStatus::Warn)));
}

#[test]
fn doctor_report_serializes_to_json() {
    let report = doctor::run(&offline_config(), true);
    let v = serde_json::to_value(&report).unwrap();
    assert!(v["status"].is_string());
    assert!(v["checks"].is_array());
    assert!(v["suggested_commands"].is_array());
}

// ---------- inspect ----------

#[test]
fn inspect_loads_routes_constants_and_ports() {
    let (conn, _config) = indexed();
    let items = inspect::load_items(&conn).unwrap();
    assert!(items.iter().any(|i| i.category == Category::Route && i.subject == "/v1/checkout"));
    assert!(items.iter().any(|i| i.category == Category::Constant));
    assert!(items.iter().any(|i| i.category == Category::Port && i.value == Some(8080.into())));
}

#[test]
fn inspect_items_serialize_to_json() {
    let (conn, _config) = indexed();
    let items = inspect::load_items(&conn).unwrap();
    let v = serde_json::to_value(&items).unwrap();
    let arr = v.as_array().unwrap();
    assert!(!arr.is_empty());
    for it in arr {
        assert!(it["category"].is_string());
        assert!(it["subject"].is_string());
    }
}

// ---------- baseline ----------

#[test]
fn baseline_detects_routes_and_runs_usage() {
    let (conn, config) = indexed();
    let report = run_baseline(&conn, &config, Some(&sample_log())).unwrap();
    let usage = report.checks.iter().find(|c| c.kind == "usage" && c.subject == "/v1/checkout");
    let usage = usage.expect("usage check for /v1/checkout");
    assert_eq!(usage.status, "observed");
    assert_eq!(usage.value, Some(4.into()));
}

#[test]
fn baseline_reports_config_values_not_counts() {
    let (conn, config) = indexed();
    let report = run_baseline(&conn, &config, Some(&sample_log())).unwrap();
    let port = report.checks.iter().find(|c| c.subject == "port").expect("port config");
    assert_eq!(port.value, Some(8080.into()));
}

#[test]
fn baseline_is_observational_and_serializes() {
    let (conn, config) = indexed();
    let report = run_baseline(&conn, &config, Some(&sample_log())).unwrap();
    // It always produces a report; never an error because errors were observed.
    let v = serde_json::to_value(&report).unwrap();
    assert!(v["summary"]["observed"].as_u64().unwrap() >= 1);
    assert!(v["checks"].is_array());
}

// ---------- eval --record ----------

#[test]
fn record_eval_captures_actual_status() {
    let yaml = format!(
        "cases:\n  - name: c1\n    question: nobody uses /v1/checkout anymore\n    repo: {}\n    local_log: {}\n    expected_status: supported\n",
        sample_repo().display(),
        sample_log(),
    );
    let fixture: Fixture = serde_yaml::from_str(&yaml).unwrap();
    let recorded = record_eval(&offline_config(), &fixture).unwrap();
    assert_eq!(recorded.cases.len(), 1);
    // Recorded expected_status is the ACTUAL status (contradicted), not the input.
    assert_eq!(recorded.cases[0].expected_status, "contradicted");
    assert!(recorded.cases[0].recorded.evidence_count >= 1);
    assert!(!recorded.cases[0].recorded.summary.is_empty());
}

#[test]
fn recorded_fixture_yaml_is_reparseable_as_fixture() {
    let yaml = format!(
        "cases:\n  - name: c1\n    question: the service runs on port 8080\n    repo: {}\n    expected_status: supported\n",
        sample_repo().display(),
    );
    let fixture: Fixture = serde_yaml::from_str(&yaml).unwrap();
    let recorded = record_eval(&offline_config(), &fixture).unwrap();
    let out = serde_yaml::to_string(&recorded).unwrap();
    // The recorded YAML carries the same shape (plus a `recorded` block) and
    // re-parses as a Fixture (extra fields are ignored).
    let reparsed: Fixture = serde_yaml::from_str(&out).unwrap();
    assert_eq!(reparsed.cases.len(), 1);
    assert_eq!(reparsed.cases[0].expected_status, "supported");
}

// ---------- diagnostics ----------

#[test]
fn check_without_index_hints_to_index() {
    let conn = truth_db::open_in_memory().unwrap(); // empty, not indexed
    let config = offline_config();
    let outcome = run_check(&conn, &config, "we retry payments 3 times", Trigger::Cli, None).unwrap();
    let hint = check_hint(&conn, &config, &outcome, None).unwrap();
    assert!(hint.expect("hint").contains("truth index ."));
}

#[test]
fn usage_claim_without_logs_hints_to_local_log() {
    // Index repo so the route is known, but provide no log source.
    let (conn, config) = indexed();
    // Remove the route from the equation by asking about a usage claim with no logs.
    let outcome = run_check(&conn, &config, "does anyone still use /v9/foo?", Trigger::Cli, None).unwrap();
    let hint = check_hint(&conn, &config, &outcome, None).unwrap().expect("hint");
    assert!(hint.contains("--local-log") || hint.contains("[loki]"));
}

#[test]
fn supported_check_produces_no_hint() {
    let (conn, config) = indexed();
    let outcome = run_check(&conn, &config, "the service runs on port 8080", Trigger::Cli, None).unwrap();
    assert!(check_hint(&conn, &config, &outcome, None).unwrap().is_none());
}
