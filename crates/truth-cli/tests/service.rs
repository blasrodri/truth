//! Tests for the deterministic observation commands (usage/errors/latest/config).

use std::path::{Path, PathBuf};
use truth_cli::service::{self, ObservationStatus};
use truth_core::config::Config;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(2).unwrap().to_path_buf()
}

fn sample_repo() -> PathBuf {
    repo_root().join("examples/sample-repo")
}

fn sample_log() -> String {
    repo_root().join("examples/sample-logs/api.log").to_string_lossy().into_owned()
}

fn setup() -> (rusqlite::Connection, Config) {
    let conn = truth_db::open_in_memory().unwrap();
    let mut config = Config::from_toml_str("").unwrap();
    config.loki.enabled = false;
    truth_indexer::index_repo(&conn, &config.repo, Some(&sample_repo())).unwrap();
    (conn, config)
}

#[test]
fn usage_observed_when_traffic_present() {
    let (conn, config) = setup();
    let obs = service::run_usage(&conn, &config, "/v1/checkout", None, None, None, Some(&sample_log())).unwrap();
    assert_eq!(obs.status, ObservationStatus::Observed);
    assert_eq!(obs.count, Some(4));
    assert!(obs.evidence.iter().any(|e| e.kind == "route_exists"));
}

#[test]
fn usage_not_observed_when_route_exists_but_no_traffic() {
    let (conn, config) = setup();
    // A repo route (/v1/checkout exists in repo) checked against an empty log:
    // no traffic but the route exists → NotObserved.
    let empty = std::env::temp_dir().join("truth_empty_usage.log");
    std::fs::write(&empty, "").unwrap();
    let obs = service::run_usage(
        &conn,
        &config,
        "/v1/checkout",
        None,
        None,
        None,
        Some(&empty.to_string_lossy()),
    )
    .unwrap();
    assert_eq!(obs.status, ObservationStatus::NotObserved);
    assert!(obs.caveats.iter().any(|c| c.contains("does not prove")));
}

#[test]
fn usage_inconclusive_when_no_route_and_no_logs() {
    let (conn, config) = setup();
    let empty = std::env::temp_dir().join("truth_empty_usage2.log");
    std::fs::write(&empty, "").unwrap();
    let obs = service::run_usage(&conn, &config, "/v9/foo", None, None, None, Some(&empty.to_string_lossy())).unwrap();
    assert_eq!(obs.status, ObservationStatus::Inconclusive);
}

#[test]
fn errors_observed() {
    let (_conn, config) = setup();
    let obs = service::run_errors(&config, "webhook", None, None, None, Some(&sample_log())).unwrap();
    assert_eq!(obs.status, ObservationStatus::Observed);
    assert!(obs.count.unwrap() >= 1);
}

#[test]
fn errors_not_observed_has_fix_caveat() {
    let (_conn, config) = setup();
    let obs = service::run_errors(&config, "nonexistent_error_xyz", None, None, None, Some(&sample_log())).unwrap();
    assert_eq!(obs.status, ObservationStatus::NotObserved);
    assert!(obs.caveats.iter().any(|c| c.contains("does not prove the issue is fixed")));
}

#[test]
fn latest_occurrence_found() {
    let (_conn, config) = setup();
    let obs = service::run_latest(&config, "/v1/checkout", None, None, None, Some(&sample_log())).unwrap();
    assert_eq!(obs.status, ObservationStatus::Observed);
    assert!(obs.latest_seen.is_some());
}

#[test]
fn config_lookup_finds_port_and_retry() {
    let (conn, _config) = setup();
    let port = service::run_config(&conn, "PORT").unwrap();
    assert_eq!(port.status, ObservationStatus::Found);
    assert!(port.evidence.iter().any(|e| e.value == Some(8080.into())));

    let retry = service::run_config(&conn, "RETRY").unwrap();
    assert_eq!(retry.status, ObservationStatus::Found);
    assert!(retry.evidence.iter().any(|e| e.value == Some(5.into())));
}

#[test]
fn config_lookup_not_found() {
    let (conn, _config) = setup();
    let obs = service::run_config(&conn, "NOPE_DOES_NOT_EXIST").unwrap();
    assert_eq!(obs.status, ObservationStatus::NotFound);
    assert!(obs.evidence.is_empty());
}
