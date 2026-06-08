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

    // The index DB and config are local runtime state — never commit them.
    // Auto-ignore so users don't accidentally check in `.truth/` (a real
    // footgun: the SQLite store can be large and is machine-specific).
    if let Some(added) = ensure_gitignored(&[".truth/", "truth.toml"])? {
        println!("Added to .gitignore: {}", added.join(", "));
    }

    let config = load_config()?;
    let _conn = truth_db::open(&config.database.path)?;
    println!("Initialized database at {}", config.database.path);
    Ok(())
}

/// Ensure each pattern is present in `./.gitignore`, creating the file if
/// needed. Idempotent — only appends patterns not already covered. Returns the
/// patterns actually added (empty `None` if the file/entries already had them).
fn ensure_gitignored(patterns: &[&str]) -> Result<Option<Vec<String>>> {
    let path = Path::new(".gitignore");
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let present: std::collections::HashSet<&str> = existing.lines().map(|l| l.trim()).collect();

    let to_add: Vec<String> = patterns
        .iter()
        .filter(|p| {
            // Treat "foo/" and "foo" as equivalent for the presence check.
            let bare = p.trim_end_matches('/');
            !present.contains(**p) && !present.contains(bare)
        })
        .map(|p| p.to_string())
        .collect();

    if to_add.is_empty() {
        return Ok(None);
    }

    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# truth — local index/config (machine-specific, do not commit)\n");
    for p in &to_add {
        out.push_str(p);
        out.push('\n');
    }
    std::fs::write(path, out).context("writing .gitignore")?;
    Ok(Some(to_add))
}

/// `truth serve` — informational placeholder (exits zero).
pub fn serve() -> Result<()> {
    println!("`truth serve` is not implemented yet.\n");
    println!("The core verifier is available through:");
    println!("  truth check \"...\"");
    println!("  truth usage ...");
    println!("  truth errors ...");
    println!("  truth eval ...");
    println!("\nSlack/HTTP mode is planned for a later phase.");
    Ok(())
}

/// `truth db migrate`
pub fn db_migrate() -> Result<()> {
    let config = load_config()?;
    let _conn = truth_db::open(&config.database.path)?;
    println!("Migrations applied at {}", config.database.path);
    Ok(())
}

/// `truth index <path> [--stats] [--full] [--extractor regex|ast|mixed]`
pub fn index(path: &str, stats_flag: bool, full: bool, extractor: Option<&str>) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    // CLI flag overrides the `[indexer] extractor` config; default is regex.
    let mode = match extractor {
        Some(s) => truth_core::config::ExtractorMode::parse(s)
            .with_context(|| format!("unknown --extractor `{s}` (regex|ast|mixed)"))?,
        None => config.indexer.extractor,
    };
    // Incremental by default. But an explicit `--extractor` changes extraction
    // policy without changing file contents, so the incremental fast-path would
    // skip everything and silently keep the old evidence — force a full rebuild.
    let force_full = full || extractor.is_some();
    let stats = truth_indexer::index_repo_opts(
        &conn,
        &config.repo,
        Some(Path::new(path)),
        !force_full,
        mode,
    )?;
    println!(
        "Indexed {} files → {} artifacts, {} evidence items (extractor: {}).",
        stats.files,
        stats.artifacts,
        stats.evidence_items,
        mode.as_str()
    );
    if stats_flag {
        println!();
        println!("Files selected:  {}", stats.files);
        println!("Files read:      {}", stats.files_read);
        println!("Evidence items:  {}", stats.evidence_items);
        println!("Evidence/file:   {:.2}", stats.evidence_per_file());
        println!(
            "Elapsed:         {:.1} ms",
            stats.elapsed.as_secs_f64() * 1000.0
        );
        println!("Throughput:      {:.0} files/sec", stats.files_per_sec());
    }
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
        // Add actionable guidance when the result was inconclusive, based on
        // the most likely cause (no index / no logs / unresolved subject).
        if let Some(hint) = crate::diagnostics::check_hint(&conn, &config, &outcome, local_log)? {
            println!("\n{hint}");
        }
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
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("json serializes")
    );
}

/// Open the database, failing with a friendly hint if it is missing.
pub(crate) fn open_db(config: &Config) -> Result<Connection> {
    truth_db::open(&config.database.path).with_context(|| {
        format!(
            "opening database at {} (run `truth init` first?)",
            config.database.path
        )
    })
}

/// Load `truth.toml`, falling back to defaults if absent.
fn load_config() -> Result<Config> {
    if Path::new("truth.toml").exists() {
        Config::load("truth.toml")
    } else {
        Ok(Config::from_toml_str("")?)
    }
}
