//! `truth ci` — run checks from a claim file and exit per a fail policy.
//!
//! Exit codes: 0 = pass, 1 = CI policy failed, 2 = operational error.

use crate::config_util::load_config;
use crate::report::{load_claim_file, render, run_report, truth_core_now, Format};
use anyhow::Result;
use truth_core::report::{Report, ReportResult, Severity};

/// Parsed CI fail policy.
pub struct Policy {
    /// Verdict statuses that count as failing (db_str form).
    pub fail_on: Vec<String>,
    /// Minimum severity that participates in failure.
    pub fail_severity: Severity,
}

impl Default for Policy {
    fn default() -> Self {
        // Default: fail on contradicted with severity error.
        Policy { fail_on: vec!["contradicted".to_string()], fail_severity: Severity::Error }
    }
}

fn parse_severity(s: &str) -> Result<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "info" => Ok(Severity::Info),
        "warning" | "warn" => Ok(Severity::Warning),
        "error" => Ok(Severity::Error),
        other => anyhow::bail!("unknown severity `{other}` (info|warning|error)"),
    }
}

/// A claim result fails policy if its status is in `fail_on` AND its severity is
/// at least `fail_severity`.
pub fn is_failing(result: &ReportResult, policy: &Policy) -> bool {
    policy.fail_on.iter().any(|s| s == &result.status)
        && result.severity.rank() >= policy.fail_severity.rank()
}

/// Failing results under a policy.
pub fn failing<'a>(report: &'a Report, policy: &Policy) -> Vec<&'a ReportResult> {
    report.results.iter().filter(|r| is_failing(r, policy)).collect()
}

/// CLI entry point. Returns the process exit code (0/1/2). It does not call
/// `process::exit` itself so it stays testable; `main` maps the code.
pub fn ci(
    claim_path: &str,
    local_log: Option<&str>,
    fail_on: Option<&str>,
    fail_severity: Option<&str>,
    format: Option<&str>,
    out: Option<&str>,
) -> i32 {
    match ci_inner(claim_path, local_log, fail_on, fail_severity, format, out) {
        Ok(passed) => {
            if passed {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("truth ci: {e}");
            2
        }
    }
}

fn ci_inner(
    claim_path: &str,
    local_log: Option<&str>,
    fail_on: Option<&str>,
    fail_severity: Option<&str>,
    format: Option<&str>,
    out: Option<&str>,
) -> Result<bool> {
    let mut config = load_config()?;
    config.loki.enabled = false;

    let mut claim_file = load_claim_file(claim_path)?;
    if let Some(ll) = local_log {
        claim_file.defaults.local_log = Some(ll.to_string());
    }

    let policy = Policy {
        fail_on: fail_on
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
            .unwrap_or_else(|| Policy::default().fail_on),
        fail_severity: match fail_severity {
            Some(s) => parse_severity(s)?,
            None => Policy::default().fail_severity,
        },
    };

    let report = run_report(&config, &claim_file, &truth_core_now())?;

    // Optional rendered artifact (does not affect exit code).
    if let Some(fmt) = format {
        let rendered = render(&report, Format::parse(fmt)?)?;
        if let Some(path) = out {
            std::fs::write(path, format!("{rendered}\n"))?;
        }
    }

    let failures = failing(&report, &policy);
    print_summary(&report, &failures);
    Ok(failures.is_empty())
}

fn print_summary(report: &Report, failures: &[&ReportResult]) {
    let s = &report.summary;
    println!("truth ci\n");
    println!("Claims checked: {}", report.claims_checked);
    println!("Supported: {}", s.supported);
    println!("Contradicted: {}", s.contradicted);
    println!("Inconclusive: {}", s.inconclusive);

    if failures.is_empty() {
        println!("\nCI result: passed");
    } else {
        println!("\nFailing:");
        for f in failures {
            println!("• {} — {} — severity={}", f.id, title(&f.status), f.severity.as_str());
        }
        println!("\nCI result: failed");
    }
}

fn title(db_status: &str) -> String {
    db_status
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
