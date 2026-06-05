//! `truth eval <fixture.yaml>` — the product's quality harness. Each case
//! indexes a repo into a fresh in-memory DB, runs a check, and compares the
//! resulting verdict status against the expected status.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use truth_core::config::Config;
use truth_core::enums::Trigger;

use crate::check::run_check;

#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
pub struct Case {
    pub name: String,
    pub question: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub local_log: Option<String>,
    pub expected_status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub name: String,
    pub expected_status: String,
    pub actual_status: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub passed: usize,
    pub failed: usize,
    pub cases: Vec<CaseResult>,
}

/// Run all cases in a fixture. Each case is fully isolated (own in-memory DB).
pub fn run_eval(config: &Config, fixture: &Fixture) -> Result<EvalReport> {
    let mut cases = Vec::new();
    let mut passed = 0;
    let mut failed = 0;

    for case in &fixture.cases {
        let conn = truth_db::open_in_memory()?;
        if let Some(repo) = &case.repo {
            truth_indexer::index_repo(&conn, &config.repo, Some(Path::new(repo)))
                .with_context(|| format!("indexing repo for case `{}`", case.name))?;
        }
        let outcome = run_check(
            &conn,
            config,
            &case.question,
            Trigger::Cli,
            case.local_log.as_deref(),
        )?;
        let actual = outcome.decision.status.as_db_str().to_string();
        let ok = actual == case.expected_status;
        if ok {
            passed += 1;
        } else {
            failed += 1;
        }
        cases.push(CaseResult {
            name: case.name.clone(),
            expected_status: case.expected_status.clone(),
            actual_status: actual,
            passed: ok,
        });
    }

    Ok(EvalReport { passed, failed, cases })
}

/// CLI entry point: parse fixture, run, print, exit non-zero on any failure.
pub fn eval(fixture_path: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let text = std::fs::read_to_string(fixture_path)
        .with_context(|| format!("reading fixture {fixture_path}"))?;
    let fixture: Fixture =
        serde_yaml::from_str(&text).with_context(|| format!("parsing fixture {fixture_path}"))?;

    // Eval forces the offline log path (deterministic, no network).
    let mut config = config;
    config.loki.enabled = false;

    let report = run_eval(&config, &fixture)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}\n", fixture_path);
        println!("{} passed, {} failed\n", report.passed, report.failed);
        for c in &report.cases {
            let mark = if c.passed { "✓" } else { "✗" };
            if c.passed {
                println!("{mark} {}", c.name);
            } else {
                println!(
                    "{mark} {} (expected {}, got {})",
                    c.name, c.expected_status, c.actual_status
                );
            }
        }
    }

    if report.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn load_config() -> Result<Config> {
    if Path::new("truth.toml").exists() {
        Config::load("truth.toml")
    } else {
        Ok(Config::from_toml_str("")?)
    }
}
