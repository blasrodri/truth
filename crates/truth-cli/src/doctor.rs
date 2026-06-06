//! `truth doctor` — validate local setup and explain readiness.

use crate::config_util::{load_config, print_json};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::time::Duration;
use truth_core::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// "ready" or "not_ready".
    pub status: String,
    pub checks: Vec<DoctorCheck>,
    pub suggested_commands: Vec<String>,
}

fn chk(name: &str, status: CheckStatus, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck { name: name.into(), status, message: message.into() }
}

/// Run all diagnostics and build a report (no I/O side effects beyond reads and
/// best-effort network probes).
pub fn run(config: &Config, config_exists: bool) -> DoctorReport {
    let mut checks = Vec::new();
    let mut suggested = Vec::new();

    // Config.
    if config_exists {
        checks.push(chk("config", CheckStatus::Ok, "truth.toml found"));
    } else {
        checks.push(chk(
            "config",
            CheckStatus::Warn,
            "truth.toml not found; using built-in defaults (run `truth init`)",
        ));
        suggested.push("truth init".to_string());
    }

    // Database + migrations.
    let conn = truth_db::open(&config.database.path);
    let mut indexed_ok = false;
    match conn {
        Ok(conn) => {
            checks.push(chk("database", CheckStatus::Ok, format!("open at {}", config.database.path)));
            checks.push(chk("migrations", CheckStatus::Ok, "applied"));

            match truth_db::repo::index_counts(&conn) {
                Ok(counts) if counts.evidence_items > 0 => {
                    indexed_ok = true;
                    checks.push(chk(
                        "indexed_evidence",
                        CheckStatus::Ok,
                        format!(
                            "{} artifacts, {} spans, {} evidence items",
                            counts.artifacts, counts.spans, counts.evidence_items
                        ),
                    ));
                }
                Ok(_) => {
                    checks.push(chk("indexed_evidence", CheckStatus::Warn, "no indexed evidence yet"));
                    suggested.push("truth index .".to_string());
                }
                Err(e) => {
                    checks.push(chk("indexed_evidence", CheckStatus::Error, format!("count failed: {e}")));
                }
            }
        }
        Err(e) => {
            checks.push(chk("database", CheckStatus::Error, format!("cannot open {}: {e}", config.database.path)));
            checks.push(chk("migrations", CheckStatus::Error, "skipped (no database)"));
            suggested.push("truth init".to_string());
        }
    }

    // Repo root.
    if Path::new(&config.repo.root).exists() {
        checks.push(chk("repo_root", CheckStatus::Ok, config.repo.root.clone()));
    } else {
        checks.push(chk("repo_root", CheckStatus::Warn, format!("repo root `{}` not found", config.repo.root)));
    }

    // Indexer extractor mode.
    let mode = config.indexer.extractor;
    let ast_note = if mode.uses_ast() { "AST Rust routes: enabled" } else { "AST Rust routes: disabled" };
    checks.push(chk("extractor", CheckStatus::Info, format!("{} ({ast_note})", mode.as_str())));

    // Loki.
    checks.push(loki_check(config));

    // LLM.
    checks.push(llm_check(config));

    // Security.
    checks.push(chk(
        "redaction",
        if config.security.redact_pii { CheckStatus::Ok } else { CheckStatus::Warn },
        if config.security.redact_pii { "enabled" } else { "disabled" },
    ));
    checks.push(chk(
        "max_log_window",
        CheckStatus::Info,
        format!("{} days", config.security.max_log_window_days),
    ));

    // Sample fixtures, if present (helps first-run UX).
    let sample_repo = Path::new("examples/sample-repo");
    let sample_log = Path::new("examples/sample-logs/api.log");
    if sample_repo.exists() && sample_log.exists() {
        suggested.push(
            "truth usage /v1/checkout --local-log examples/sample-logs/api.log".to_string(),
        );
        suggested.push(
            "truth check \"nobody uses /v1/checkout anymore\" --local-log examples/sample-logs/api.log"
                .to_string(),
        );
        suggested.push("truth eval fixtures/eval/basic.yaml".to_string());
    } else if indexed_ok {
        suggested.push("truth inspect".to_string());
        suggested.push("truth baseline".to_string());
    }

    let ready = !checks.iter().any(|c| c.status == CheckStatus::Error);
    DoctorReport {
        status: if ready { "ready" } else { "not_ready" }.to_string(),
        checks,
        suggested_commands: suggested,
    }
}

fn loki_check(config: &Config) -> DoctorCheck {
    if !config.loki.enabled {
        return chk("loki", CheckStatus::Info, "disabled");
    }
    let url = format!("{}/ready", config.loki.base_url.trim_end_matches('/'));
    if probe(&url) {
        chk("loki", CheckStatus::Ok, format!("enabled and reachable at {}", config.loki.base_url))
    } else {
        chk("loki", CheckStatus::Warn, format!("enabled but unreachable at {}", config.loki.base_url))
    }
}

fn llm_check(config: &Config) -> DoctorCheck {
    if !config.llm.enabled {
        return chk("llm", CheckStatus::Info, "disabled; deterministic regex extractor active");
    }
    let url = format!("{}/models", config.llm.base_url.trim_end_matches('/'));
    if probe(&url) {
        chk("llm", CheckStatus::Ok, format!("configured and reachable at {}", config.llm.base_url))
    } else {
        chk("llm", CheckStatus::Warn, format!("configured but unreachable at {}; will fall back to regex", config.llm.base_url))
    }
}

/// Best-effort HTTP GET probe with a short timeout. Any 2xx/3xx/4xx response
/// counts as reachable; only connection errors count as unreachable.
fn probe(url: &str) -> bool {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(800))
        .build();
    match agent.get(url).call() {
        Ok(_) => true,
        // An HTTP error status still means the server answered → reachable.
        Err(ureq::Error::Status(_, _)) => true,
        Err(_) => false,
    }
}

/// `truth doctor [--json]`.
pub fn doctor(json: bool) -> Result<()> {
    let config_exists = Path::new("truth.toml").exists();
    let config = load_config()?;
    let report = run(&config, config_exists);

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    println!("truth doctor\n");
    for c in &report.checks {
        let label = match c.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Error => "error",
            CheckStatus::Info => "info",
        };
        println!("{}: {} — {}", pretty(&c.name), label, c.message);
    }

    println!();
    if report.status == "ready" {
        println!("Ready.");
    } else {
        println!("Not ready — resolve the errors above.");
    }

    if !report.suggested_commands.is_empty() {
        println!("\nSuggested next commands:");
        for cmd in &report.suggested_commands {
            println!("  {cmd}");
        }
    }
    Ok(())
}

fn pretty(name: &str) -> String {
    let words: Vec<String> = name
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect();
    words.join(" ")
}
