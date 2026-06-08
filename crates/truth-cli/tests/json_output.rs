//! Tests that JSON output is valid, stable, and prose-free.

use std::path::{Path, PathBuf};
use truth_cli::check::run_check;
use truth_cli::service;
use truth_core::config::Config;
use truth_core::enums::Trigger;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn setup() -> (rusqlite::Connection, Config) {
    let conn = truth_db::open_in_memory().unwrap();
    let mut config = Config::from_toml_str("").unwrap();
    config.loki.enabled = false;
    truth_indexer::index_repo(
        &conn,
        &config.repo,
        Some(&repo_root().join("examples/sample-repo")),
    )
    .unwrap();
    (conn, config)
}

fn sample_log() -> String {
    repo_root()
        .join("examples/sample-logs/api.log")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn check_json_is_valid_and_has_stable_keys() {
    let (conn, config) = setup();
    let out = run_check(
        &conn,
        &config,
        "nobody uses /v1/checkout anymore",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    let j = out.to_json();
    // Re-serialize and parse to confirm it's valid JSON.
    let s = serde_json::to_string(&j).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
    for key in [
        "status",
        "confidence",
        "summary",
        "evidence",
        "caveats",
        "check_id",
    ] {
        assert!(parsed.get(key).is_some(), "missing key {key}");
    }
    assert_eq!(parsed["status"], "contradicted");
    assert!(parsed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["source"] == "local_logs"));
}

#[test]
fn observation_json_serializes_cleanly() {
    let (conn, _config) = setup();
    let obs = service::run_config(&conn, "PORT").unwrap();
    let v = serde_json::to_value(&obs).unwrap();
    assert_eq!(v["status"], "found");
    // Every evidence item must carry source + kind.
    for e in v["evidence"].as_array().unwrap() {
        assert!(e["source"].is_string());
        assert!(e["kind"].is_string());
    }
}
