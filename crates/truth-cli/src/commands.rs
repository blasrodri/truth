//! CLI command implementations.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use truth_core::config::Config;
use truth_core::enums::Trigger;

use crate::check::run_check;
use crate::service::{self, render_observation};

const DEFAULT_CONFIG: &str = include_str!("../../../truth.toml.example");

/// `truth init` — scaffold `.truth/`, write `truth.toml`, run migrations.
pub fn init() -> Result<()> {
    std::fs::create_dir_all(".truth").context("creating .truth/")?;

    if Path::new("truth.toml").exists() {
        println!("truth.toml already exists, leaving it untouched.");
    } else {
        std::fs::write("truth.toml", DEFAULT_CONFIG).context("writing truth.toml")?;
        println!("Wrote truth.toml");
    }

    let config = load_config()?;
    let _conn = truth_db::open(&config.database.path)?;
    println!("Initialized database at {}", config.database.path);
    Ok(())
}

/// `truth db migrate`
pub fn db_migrate() -> Result<()> {
    let config = load_config()?;
    let _conn = truth_db::open(&config.database.path)?;
    println!("Migrations applied at {}", config.database.path);
    Ok(())
}

/// `truth index <path>`
pub fn index(path: &str) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let stats = truth_indexer::index_repo(&conn, &config.repo, Some(Path::new(path)))?;
    println!(
        "Indexed {} files → {} artifacts, {} evidence items.",
        stats.files, stats.artifacts, stats.evidence_items
    );
    Ok(())
}

/// `truth check "<claim>"`
pub fn check(question: &str, local_log: Option<&str>, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = open_db(&config)?;
    let outcome = run_check(&conn, &config, question, Trigger::Cli, local_log)?;
    if json {
        print_json(&outcome.to_json());
    } else {
        println!("{}", outcome.response_text);
        println!("\n(check id: {})", outcome.check_id);
    }
    Ok(())
}

/// `truth usage <subject>`
pub fn usage(
    subject: &str,
    window: Option<&str>,
    env: Option<&str>,
    svc: Option<&str>,
    local_log: Option<&str>,
    json: bool,
) -> Result<()> {
    let config = load_config()?;
    let conn = open_db(&config)?;
    let obs = service::run_usage(&conn, &config, subject, window, env, svc, local_log)?;
    emit_observation(&obs, json);
    Ok(())
}

/// `truth errors <pattern>`
pub fn errors(
    pattern: &str,
    window: Option<&str>,
    env: Option<&str>,
    svc: Option<&str>,
    local_log: Option<&str>,
    json: bool,
) -> Result<()> {
    let config = load_config()?;
    let obs = service::run_errors(&config, pattern, window, env, svc, local_log)?;
    emit_observation(&obs, json);
    Ok(())
}

/// `truth latest <pattern>`
pub fn latest(
    pattern: &str,
    window: Option<&str>,
    env: Option<&str>,
    svc: Option<&str>,
    local_log: Option<&str>,
    json: bool,
) -> Result<()> {
    let config = load_config()?;
    let obs = service::run_latest(&config, pattern, window, env, svc, local_log)?;
    emit_observation(&obs, json);
    Ok(())
}

/// `truth config <key>`
pub fn config(key: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = open_db(&config)?;
    let obs = service::run_config(&conn, key)?;
    emit_observation(&obs, json);
    Ok(())
}

fn emit_observation(obs: &service::Observation, json: bool) {
    if json {
        print_json(&serde_json::to_value(obs).expect("observation serializes"));
    } else {
        println!("{}", render_observation(obs));
    }
}

fn print_json(v: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(v).expect("json serializes"));
}

/// Open the database, failing with a friendly hint if it is missing.
fn open_db(config: &Config) -> Result<Connection> {
    truth_db::open(&config.database.path)
        .with_context(|| format!("opening database at {} (run `truth init` first?)", config.database.path))
}

/// Load `truth.toml`, falling back to defaults if absent.
fn load_config() -> Result<Config> {
    if Path::new("truth.toml").exists() {
        Config::load("truth.toml")
    } else {
        Ok(Config::from_toml_str("")?)
    }
}
