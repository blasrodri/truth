//! Tests for Phase 6B: claim files, claims extraction, report, ci, diff.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use truth_cli::ci::{failing, is_failing, Policy};
use truth_cli::claims::extract_claims;
use truth_cli::diff::diff_maps;
use truth_cli::eval::{load_fixture, Fixture};
use truth_cli::report::{render, run_report, Format};
use truth_core::config::Config;
use truth_core::report::{ClaimFile, Severity};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}
fn sample_repo() -> PathBuf {
    repo_root().join("examples/sample-repo")
}
fn sample_log() -> String {
    repo_root()
        .join("examples/sample-logs/api.log")
        .to_string_lossy()
        .into_owned()
}
fn offline() -> Config {
    let mut c = Config::from_toml_str("").unwrap();
    c.loki.enabled = false;
    c
}

fn claim_file_for(extra_status: bool) -> ClaimFile {
    let status = |s: &str| {
        if extra_status {
            format!("    expected_status: {s}\n")
        } else {
            String::new()
        }
    };
    // PHASE 2: the contradicting claim must rest on a STRUCTURED fact, since
    // usage/error log counts no longer contradict. A wrong retry count
    // contradicts via exact value comparison against the indexed source — the
    // sound, binary kind of contradiction `report`/`ci` should count. (The id is
    // kept as `checkout-unused` so the assertions referencing it still resolve.)
    let yaml = format!(
        "version: 1\ndefaults:\n  repo: {repo}\n  local_log: {log}\nclaims:\n  - id: checkout-unused\n    text: we retry payments 3 times\n    severity: warning\n{s1}  - id: service-port\n    text: the service runs on port 8080\n    severity: info\n{s2}",
        repo = sample_repo().display(),
        log = sample_log(),
        s1 = status("contradicted"),
        s2 = status("supported"),
    );
    ClaimFile::from_yaml(&yaml).unwrap()
}

// ---------- claim-file parser ----------

#[test]
fn parses_claims_v1_with_severity_tags_defaults() {
    let cf = claim_file_for(true);
    assert_eq!(cf.claims.len(), 2);
    assert_eq!(cf.claims[0].severity, Severity::Warning);
    assert_eq!(cf.claims[1].severity, Severity::Info);
    assert!(cf.defaults.repo.is_some());
}

#[test]
fn eval_reads_old_cases_format() {
    let yaml = "cases:\n  - name: a\n    question: the service runs on port 8080\n    expected_status: supported\n";
    let f: Fixture = load_fixture(yaml).unwrap();
    assert_eq!(f.cases.len(), 1);
    assert_eq!(f.cases[0].name, "a");
}

#[test]
fn eval_reads_new_claims_format_and_skips_unstated() {
    let yaml = "version: 1\nclaims:\n  - id: a\n    text: x\n    expected_status: supported\n  - id: b\n    text: y\n";
    let f: Fixture = load_fixture(yaml).unwrap();
    // Only the claim with an expected_status becomes an eval case.
    assert_eq!(f.cases.len(), 1);
    assert_eq!(f.cases[0].name, "a");
}

// ---------- truth claims ----------

#[test]
fn claims_extracts_port_retry_usage_and_ignores_vague() {
    let readme = sample_repo()
        .join("README.md")
        .to_string_lossy()
        .into_owned();
    let specs = extract_claims(&[readme], &offline()).unwrap();
    let joined: Vec<&str> = specs.iter().map(|s| s.text.as_str()).collect();
    assert!(
        joined.iter().any(|t| t.contains("8080")),
        "port claim: {joined:?}"
    );
    assert!(
        joined.iter().any(|t| t.to_lowercase().contains("retry")),
        "retry claim: {joined:?}"
    );
    assert!(
        joined.iter().any(|t| t.contains("/v1/checkout")),
        "usage claim: {joined:?}"
    );
    // Vague prose must not be extracted.
    assert!(!joined.iter().any(|t| t.contains("architecture is simple")));
    // Every spec carries source + extraction metadata.
    for s in &specs {
        assert!(s.source.is_some());
        assert!(s.extraction.is_some());
    }
}

#[test]
fn claims_yaml_reparses() {
    let readme = sample_repo()
        .join("README.md")
        .to_string_lossy()
        .into_owned();
    let specs = extract_claims(&[readme], &offline()).unwrap();
    let cf = ClaimFile {
        version: 1,
        metadata: Default::default(),
        defaults: Default::default(),
        claims: specs,
    };
    let yaml = cf.to_yaml().unwrap();
    let reparsed = ClaimFile::from_yaml(&yaml).unwrap();
    assert_eq!(reparsed.claims.len(), cf.claims.len());
}

// ---------- truth report ----------

#[test]
fn report_runs_and_does_not_fail_on_contradicted() {
    let cf = claim_file_for(false);
    let report = run_report(&offline(), &cf, "2026-06-04T00:00:00Z").unwrap();
    assert_eq!(report.claims_checked, 2);
    assert_eq!(report.summary.contradicted, 1);
    assert_eq!(report.summary.supported, 1);
    // Results carry evidence + caveats.
    let checkout = report
        .results
        .iter()
        .find(|r| r.id == "checkout-unused")
        .unwrap();
    assert_eq!(checkout.status, "contradicted");
    assert!(!checkout.evidence.is_empty());
    assert!(!checkout.caveats.is_empty());
}

#[test]
fn report_renders_all_formats() {
    let cf = claim_file_for(false);
    let report = run_report(&offline(), &cf, "t").unwrap();

    let text = render(&report, Format::Text).unwrap();
    assert!(text.contains("truth report"));

    let md = render(&report, Format::Markdown).unwrap();
    assert!(md.starts_with("# truth report"));
    assert!(md.contains("| Status | Count |"));

    let json = render(&report, Format::Json).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["claims_checked"], 2);
    assert!(v["results"].is_array());
}

// ---------- truth ci ----------

#[test]
fn ci_passes_when_no_failing_severity() {
    let cf = claim_file_for(false);
    let report = run_report(&offline(), &cf, "t").unwrap();
    // Default policy: contradicted + severity error. checkout-unused is a warning.
    let policy = Policy::default();
    assert!(failing(&report, &policy).is_empty());
}

#[test]
fn ci_fails_on_contradicted_warning_with_warning_severity() {
    let cf = claim_file_for(false);
    let report = run_report(&offline(), &cf, "t").unwrap();
    let policy = Policy {
        fail_on: vec!["contradicted".into()],
        fail_severity: Severity::Warning,
    };
    let fails = failing(&report, &policy);
    assert_eq!(fails.len(), 1);
    assert_eq!(fails[0].id, "checkout-unused");
}

#[test]
fn ci_is_failing_respects_status_and_severity() {
    use truth_core::report::ReportResult;
    let r = ReportResult {
        id: "x".into(),
        text: "t".into(),
        severity: Severity::Info,
        tags: vec![],
        status: "contradicted".into(),
        confidence: 1.0,
        check_id: "c".into(),
        summary: "s".into(),
        evidence: vec![],
        caveats: vec![],
    };
    // Info severity below the warning threshold → not failing.
    let policy = Policy {
        fail_on: vec!["contradicted".into()],
        fail_severity: Severity::Warning,
    };
    assert!(!is_failing(&r, &policy));
    // Lower the threshold to info → now failing.
    let policy = Policy {
        fail_on: vec!["contradicted".into()],
        fail_severity: Severity::Info,
    };
    assert!(is_failing(&r, &policy));
}

// ---------- truth diff ----------

#[test]
fn diff_detects_changed_added_removed() {
    let mut old = BTreeMap::new();
    old.insert("checkout-unused".to_string(), "supported".to_string());
    old.insert("old-port".to_string(), "supported".to_string());
    let mut new = BTreeMap::new();
    new.insert("checkout-unused".to_string(), "contradicted".to_string());
    new.insert("webhook-fixed".to_string(), "contradicted".to_string());

    let d = diff_maps(&old, &new);
    assert_eq!(d.changed.len(), 1);
    assert_eq!(d.changed[0].id, "checkout-unused");
    assert_eq!(d.added.len(), 1);
    assert_eq!(d.added[0].id, "webhook-fixed");
    assert_eq!(d.removed.len(), 1);
    assert_eq!(d.removed[0].id, "old-port");
}

#[test]
fn diff_same_map_has_no_changes() {
    let mut m = BTreeMap::new();
    m.insert("a".to_string(), "supported".to_string());
    let d = diff_maps(&m, &m);
    assert!(d.changed.is_empty() && d.added.is_empty() && d.removed.is_empty());
    let v = serde_json::to_value(&d).unwrap();
    assert!(v["changed"].as_array().unwrap().is_empty());
}
