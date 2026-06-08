//! `truth eval <fixture.yaml>` — the product's quality harness. Each case
//! indexes a repo into a fresh in-memory DB, runs a check, and compares the
//! resulting verdict status against the expected status.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use truth_core::config::Config;
use truth_core::enums::Trigger;

use crate::check::run_check;
use truth_core::report::ClaimFile;

#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub cases: Vec<Case>,
}

impl Fixture {
    /// Convert a v1 claim file into eval cases (only claims with an
    /// `expected_status` are checkable by eval; others are skipped).
    pub fn from_claim_file(cf: &ClaimFile) -> Fixture {
        let cases = cf
            .claims
            .iter()
            .filter_map(|c| {
                let expected = c.expected_status.clone()?;
                Some(Case {
                    name: c.id.clone(),
                    question: c.text.clone(),
                    repo: c.repo.clone().or_else(|| cf.defaults.repo.clone()),
                    local_log: c
                        .local_log
                        .clone()
                        .or_else(|| cf.defaults.local_log.clone()),
                    expected_status: expected,
                })
            })
            .collect();
        Fixture { cases }
    }
}

/// Parse a fixture file, accepting either the old `cases:` format or the new
/// `claims:` claim-file format.
pub fn load_fixture(text: &str) -> Result<Fixture> {
    // Prefer the explicit `cases:` format; fall back to the claim-file format.
    if let Ok(f) = serde_yaml::from_str::<Fixture>(text) {
        return Ok(f);
    }
    let cf =
        ClaimFile::from_yaml(text).context("fixture is neither a `cases:` nor `claims:` file")?;
    Ok(Fixture::from_claim_file(&cf))
}

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub name: String,
    pub question: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub local_log: Option<String>,
    pub expected_status: String,
}

/// A case with its captured outputs, written by `eval --record`.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedCase {
    pub name: String,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_log: Option<String>,
    /// Set to the actual status observed during recording.
    pub expected_status: String,
    pub recorded: RecordedDetail,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordedDetail {
    pub summary: String,
    pub evidence_count: usize,
    pub caveats: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordedFixture {
    pub cases: Vec<RecordedCase>,
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

    Ok(EvalReport {
        passed,
        failed,
        cases,
    })
}

/// Run all cases and capture their actual outputs as a recorded fixture.
pub fn record_eval(config: &Config, fixture: &Fixture) -> Result<RecordedFixture> {
    let mut cases = Vec::new();
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
        cases.push(RecordedCase {
            name: case.name.clone(),
            question: case.question.clone(),
            repo: case.repo.clone(),
            local_log: case.local_log.clone(),
            expected_status: outcome.decision.status.as_db_str().to_string(),
            recorded: RecordedDetail {
                summary: concise_summary(&outcome.response_text),
                evidence_count: outcome.evidence.len(),
                caveats: outcome.decision.caveats.clone(),
            },
        });
    }
    Ok(RecordedFixture { cases })
}

/// Pull the one-line explanation out of a rendered response block. The block is
/// "Headline.\n\n<explanation>\n\nEvidence: ...", so the explanation is the
/// first non-empty line after the headline.
fn concise_summary(response: &str) -> String {
    let mut lines = response.lines().filter(|l| !l.trim().is_empty());
    let _headline = lines.next();
    lines
        .next()
        .or_else(|| response.lines().next())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// CLI entry point: parse fixture, run, print, exit non-zero on any failure.
///
/// With `record`, captures actual outputs to a YAML file instead of asserting
/// (it never exits non-zero). `force` allows overwriting an existing file.
pub fn eval(fixture_path: &str, json: bool, record: Option<&str>, force: bool) -> Result<()> {
    let config = load_config()?;
    let text = std::fs::read_to_string(fixture_path)
        .with_context(|| format!("reading fixture {fixture_path}"))?;
    let fixture = load_fixture(&text).with_context(|| format!("parsing fixture {fixture_path}"))?;

    // Eval forces the offline log path (deterministic, no network).
    let mut config = config;
    config.loki.enabled = false;

    if let Some(out_path) = record {
        return record_to_file(&config, &fixture, out_path, force, json);
    }

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

/// Record actual outputs to `out_path`, refusing to overwrite without `force`.
fn record_to_file(
    config: &Config,
    fixture: &Fixture,
    out_path: &str,
    force: bool,
    json: bool,
) -> Result<()> {
    if Path::new(out_path).exists() && !force {
        anyhow::bail!("{out_path} already exists; pass --force to overwrite");
    }
    let recorded = record_eval(config, fixture)?;
    let yaml = serde_yaml::to_string(&recorded).context("serializing recorded fixture")?;
    std::fs::write(out_path, &yaml).with_context(|| format!("writing {out_path}"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&recorded)?);
    } else {
        println!("Recorded {} case(s) to {out_path}", recorded.cases.len());
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
