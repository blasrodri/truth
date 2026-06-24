//! End-to-end pipeline test: index the sample repo into an in-memory DB, then
//! run checks through the offline (local-file) log path and assert verdicts.

use std::path::Path;
use truth_cli::check;
use truth_core::config::Config;
use truth_core::enums::{Trigger, VerdictStatus};

fn sample_repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(|root| Box::leak(root.join("examples/sample-repo").into_boxed_path()))
        .unwrap()
}

fn sample_log() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .join("examples/sample-logs/api.log")
        .to_string_lossy()
        .into_owned()
}

fn setup() -> (rusqlite::Connection, Config) {
    let conn = truth_db::open_in_memory().unwrap();
    let mut config = Config::from_toml_str("").unwrap();
    config.loki.enabled = false; // force offline local-file path
    truth_indexer::index_repo(&conn, &config.repo, Some(sample_repo())).unwrap();
    (conn, config)
}

#[test]
fn checkout_usage_count_is_inconclusive_not_contradicted() {
    // PHASE 2: "nobody uses X" vs observed log traffic is a COUNT signal — it
    // informs but never contradicts. Heavy traffic is surfaced at Inconclusive
    // (the user still sees it) rather than accusing the agent of lying on a
    // count. Only structured binary facts (diff/AST/value/exit-code) contradict.
    let (conn, config) = setup();
    let out = check::run_check(
        &conn,
        &config,
        "nobody uses /v1/checkout anymore",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    assert_eq!(out.decision.status, VerdictStatus::Inconclusive);
    // The traffic evidence is preserved, not discarded.
    assert!(!out.decision.caveats.is_empty());
}

#[test]
fn retry_count_is_contradicted() {
    let (conn, config) = setup();
    let out = check::run_check(
        &conn,
        &config,
        "we retry payments 3 times",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    assert_eq!(out.decision.status, VerdictStatus::Contradicted);
}

#[test]
fn port_is_supported() {
    let (conn, config) = setup();
    let out = check::run_check(
        &conn,
        &config,
        "the service runs on port 8080",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    assert_eq!(out.decision.status, VerdictStatus::Supported);
}

#[test]
fn webhook_errors_count_is_inconclusive_not_contradicted() {
    // PHASE 2: "errors are fixed" vs an error log COUNT — the observed errors may
    // predate the fix or stem from another cause, so a count can't prove the
    // claim false. Surfaced at Inconclusive for the user to investigate, never a
    // contradiction.
    let (conn, config) = setup();
    let out = check::run_check(
        &conn,
        &config,
        "webhook errors are fixed",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    assert_eq!(out.decision.status, VerdictStatus::Inconclusive);
}

#[test]
fn unknown_route_is_inconclusive() {
    let (conn, config) = setup();
    let out = check::run_check(
        &conn,
        &config,
        "does anyone still use /v9/foo?",
        Trigger::Cli,
        Some(&sample_log()),
    )
    .unwrap();
    assert_eq!(out.decision.status, VerdictStatus::Inconclusive);
}
