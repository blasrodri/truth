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

    // Register this repo in ~/.truth/registry.json so `truth stats --all` can
    // aggregate the ledger across repos (stores stay per-repo).
    if let Ok(cwd) = std::env::current_dir() {
        crate::stats::register_repo(&cwd);
    }
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

/// `truth setup` — the one-command per-repo onboarding: init the store, index
/// the repo, install the Claude Code hooks, register the MCP server, and — the
/// important part — end with a LIVE PROOF that truth is reading THIS repo, plus
/// a concrete statement of what changes now. A silent "you're all set" install
/// is why tools get forgotten; setup shows itself working on the user's own
/// code before it's done.
pub fn setup() -> Result<()> {
    println!("Setting up truth for this repo…\n");

    // 1. Store + config (init prints its own lines; keep them, they're real).
    init()?;

    // 2. Index — a real count the user can see.
    let config = load_config()?;
    if let Ok(conn) = truth_db::open(&config.database.path) {
        let _ = truth_indexer::index_repo(&conn, &config.repo, Some(Path::new(&config.repo.root)));
        if let Ok(status) = truth_db::repo::index_status(&conn) {
            println!("  ✓ Indexed        {} artifacts", status.artifacts);
        }
    }

    // 3. Hooks + MCP (each prints its own confirmation).
    crate::hook::install(false, false)?;
    register_mcp_with_claude();

    // 4. LIVE PROOF — verify a real fact derived from THIS repo, so the user
    //    sees truth deciding from their own evidence, not a canned example.
    setup_live_proof(&config);

    // 5. What changes now — concrete, not a vague "optional" tip.
    println!("\nYou're set. From now on, in this repo:");
    println!("  • Your coding agent's \"done\" is fact-checked before you read it.");
    println!("  • \"tests pass\" won't be accepted unless the tests actually ran");
    println!("    (run them via `truth run -- <cmd>`, or let the hook record them).");
    println!("  • See what truth has caught anytime:  truth stats");
    println!(
        "\ntruth also catches agent lies its own work disproves — e.g. claiming a file was\n\
         created when the diff never made it. How that's measured: docs/BENCHMARKS.md."
    );
    Ok(())
}

/// Run one real verification against the repo and print the verdict, so setup
/// ends by SHOWING truth work on the user's own code. Uses a fact truth derives
/// from git — the current HEAD sha — so it's grounded in their repo, never a
/// planted or fake claim. Best-effort: prints nothing on any failure (outside
/// git, empty repo) rather than a confusing half-result.
fn setup_live_proof(config: &Config) {
    let root = &config.repo.root;
    let head = git_short_sha(root);
    let Some(sha) = head else { return };

    let claim = format!("the current commit is {sha}");
    let proof = (|| -> Result<String> {
        let conn = truth_db::open(&config.database.path)?;
        let report =
            crate::verify_turn::verify_claims(&conn, config, &claim, Some(&[claim.clone()]), None)?;
        Ok(crate::verify_turn::render_text(&report))
    })();

    if let Ok(table) = proof {
        // Only show it if truth actually produced a Supported line — a grounded
        // win. If it refused (unlikely here), stay quiet rather than confuse.
        if table.contains("Supported") {
            println!(
                "\n  Trying it on THIS repo — truth just checked a claim from your own git:\n"
            );
            for line in table.lines() {
                let t = line.trim();
                if t.starts_with('✓') {
                    println!("    {t}");
                }
            }
            println!("\n  ↑ Decided from your repo's git state, not a model. Your agent gets the");
            println!("    same deterministic check on every claim it makes.");
        }
    }
}

/// Current HEAD short sha for `dir`, or None outside git / on any error.
/// The grounded live-proof at the end of `setup` verifies the claim
/// "the current commit is <sha>" against this, so it must never return a
/// fabricated value — outside git it stays None and setup shows no proof.
fn git_short_sha(dir: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Register `truth-mcp` with Claude Code (user scope) if the CLI is present
/// and the server isn't already registered. Never fails setup — prints the
/// manual command instead.
fn register_mcp_with_claude() {
    use std::process::Command;
    let manual = || {
        println!("MCP: register manually with `claude mcp add --scope user truth -- truth-mcp` (or your client's mcpServers config).")
    };
    match Command::new("claude")
        .args(["mcp", "get", "truth"])
        .output()
    {
        Ok(o) if o.status.success() => {
            println!("MCP: `truth` is already registered with Claude Code.");
        }
        Ok(_) => {
            let add = Command::new("claude")
                .args(["mcp", "add", "--scope", "user", "truth", "--", "truth-mcp"])
                .output();
            match add {
                Ok(o) if o.status.success() => {
                    println!("MCP: registered `truth` with Claude Code (user scope).");
                }
                _ => manual(),
            }
        }
        Err(_) => manual(),
    }
}

/// The crate version this binary was built from.
pub const TRUTH_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check GitHub for a newer release and, if found, print a one-line NOTICE with
/// the upgrade command. truth never downloads or runs anything — it only tells
/// you. Network failures are silent (returns Ok with no notice) so this never
/// blocks. `quiet` suppresses the "you're up to date" line.
pub fn upgrade_check(quiet: bool) -> Result<()> {
    let latest = match fetch_latest_release_tag() {
        Some(t) => t,
        None => {
            if !quiet {
                println!("Could not reach GitHub to check for updates.");
            }
            return Ok(());
        }
    };
    let latest_v = latest.trim_start_matches('v');
    if is_newer(latest_v, TRUTH_VERSION) {
        println!("⬆ truth {latest_v} is available (you have {TRUTH_VERSION}).");
        println!("  Upgrade: download from https://github.com/blasrodri/truth/releases/latest");
        println!("  or, if installed via cargo: cargo install --git https://github.com/blasrodri/truth truth-cli truth-mcp");
    } else if !quiet {
        println!("truth {TRUTH_VERSION} is up to date.");
    }
    Ok(())
}

/// GET the latest release tag from the GitHub API. None on any failure.
fn fetch_latest_release_tag() -> Option<String> {
    let resp = ureq::get("https://api.github.com/repos/blasrodri/truth/releases/latest")
        .set("User-Agent", "truth-cli")
        .timeout(std::time::Duration::from_secs(4))
        .call()
        .ok()?;
    let json: serde_json::Value = resp.into_json().ok()?;
    json.get("tag_name")?.as_str().map(|s| s.to_string())
}

/// Simple semver-ish "is `a` newer than `b`" by numeric component comparison.
fn is_newer(a: &str, b: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
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
    // The clap default "." means "the truth root", not the process CWD —
    // running `truth index` from a subdirectory must index the whole repo
    // into the root's store, not a slice of it into a new one.
    let path = if path == "." { &config.repo.root } else { path };
    // CLI flag overrides the `[indexer] extractor` config; default is regex.
    let mode = match extractor {
        Some(s) => truth_core::config::ExtractorMode::parse(s)
            .with_context(|| format!("unknown --extractor `{s}` (regex|ast|mixed)"))?,
        None => config.indexer.extractor,
    };
    // Incremental by default. Force a full rebuild when: `--full`; an explicit
    // `--extractor` (changes extraction policy without changing file contents,
    // so the incremental fast-path would skip everything and keep old evidence);
    // or the stored index format predates this binary (post-upgrade).
    let format_stale = truth_db::index_format_is_stale(&conn).unwrap_or(false);
    let force_full = full || extractor.is_some() || format_stale;
    if format_stale && !full {
        println!("Index was built by a different truth version — rebuilding in full.");
    }
    let stats = truth_indexer::index_repo_opts(
        &conn,
        &config.repo,
        Some(Path::new(path)),
        !force_full,
        mode,
    )?;
    truth_db::set_index_format_version(&conn)?;
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
    // Self-heal: bring the index up to date with the working tree first, so an
    // interactive `truth check` never contradicts a true claim against a stale
    // snapshot (same guarantee verify_turn / the hooks already give). Cheap and
    // incremental; opt out with TRUTH_NO_AUTOINDEX=1 for benchmarking.
    if std::env::var_os("TRUTH_NO_AUTOINDEX").is_none() {
        crate::verify_turn::auto_refresh_index(&conn, &config);
    }
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

/// Load `truth.toml` from the nearest truth root (see `config_util`).
fn load_config() -> Result<Config> {
    crate::config_util::load_config()
}

#[cfg(test)]
mod setup_tests {
    use super::git_short_sha;

    #[test]
    fn git_short_sha_none_outside_git() {
        // A non-git temp dir must yield None so setup shows no proof rather than
        // a fabricated sha.
        let dir = std::env::temp_dir().join(format!("truth_nogit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(git_short_sha(&dir.to_string_lossy()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_short_sha_returns_head_in_a_repo() {
        let dir = std::env::temp_dir().join(format!("truth_git_sha_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "b", "--no-gpg-sign"]);
        let sha = git_short_sha(&dir.to_string_lossy()).expect("sha in a repo");
        assert!(sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
