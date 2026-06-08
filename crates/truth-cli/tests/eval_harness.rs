//! Tests for the eval framework: fixture parsing and pass/fail accounting.

use std::path::{Path, PathBuf};
use truth_cli::eval::{run_eval, Fixture};
use truth_core::config::Config;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn offline_config() -> Config {
    let mut c = Config::from_toml_str("").unwrap();
    c.loki.enabled = false;
    c
}

/// Build a fixture whose paths are absolute (so it runs regardless of cwd).
fn fixture_yaml(expected_unused: &str) -> String {
    let repo = repo_root().join("examples/sample-repo");
    let log = repo_root().join("examples/sample-logs/api.log");
    format!(
        r#"cases:
  - name: unused endpoint
    question: nobody uses /v1/checkout anymore
    repo: {repo}
    local_log: {log}
    expected_status: {expected_unused}
  - name: retry contradicted
    question: we retry payments 3 times
    repo: {repo}
    expected_status: contradicted
"#,
        repo = repo.display(),
        log = log.display(),
    )
}

#[test]
fn fixture_parses() {
    let f: Fixture = serde_yaml::from_str(&fixture_yaml("contradicted")).unwrap();
    assert_eq!(f.cases.len(), 2);
    assert_eq!(f.cases[0].name, "unused endpoint");
}

#[test]
fn all_cases_pass_with_correct_expectations() {
    let f: Fixture = serde_yaml::from_str(&fixture_yaml("contradicted")).unwrap();
    let report = run_eval(&offline_config(), &f).unwrap();
    assert_eq!(report.passed, 2);
    assert_eq!(report.failed, 0);
    assert!(report.cases.iter().all(|c| c.passed));
}

#[test]
fn wrong_expectation_is_reported_as_failure() {
    // Expect "supported" for the unused-endpoint case, which is actually contradicted.
    let f: Fixture = serde_yaml::from_str(&fixture_yaml("supported")).unwrap();
    let report = run_eval(&offline_config(), &f).unwrap();
    assert_eq!(report.failed, 1);
    let failing = report.cases.iter().find(|c| !c.passed).unwrap();
    assert_eq!(failing.expected_status, "supported");
    assert_eq!(failing.actual_status, "contradicted");
}

#[test]
fn shipped_basic_fixture_is_green() {
    // The repo-relative fixture is exercised against absolute paths here.
    let text = std::fs::read_to_string(repo_root().join("fixtures/eval/basic.yaml")).unwrap();
    let mut f: Fixture = serde_yaml::from_str(&text).unwrap();
    // Rewrite relative paths to absolute so the test is cwd-independent.
    for case in &mut f.cases {
        if let Some(r) = &case.repo {
            case.repo = Some(repo_root().join(r).to_string_lossy().into_owned());
        }
        if let Some(l) = &case.local_log {
            case.local_log = Some(repo_root().join(l).to_string_lossy().into_owned());
        }
    }
    let report = run_eval(&offline_config(), &f).unwrap();
    assert_eq!(
        report.failed, 0,
        "shipped fixture should be green: {:?}",
        report.cases
    );
}
