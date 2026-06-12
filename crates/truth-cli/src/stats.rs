//! `truth stats` — the lie ledger. Summarizes the audit trail already stored
//! in SQLite: how many claims were checked, how many the evidence contradicted,
//! what kinds of claims fail most, and the recent contradictions verbatim.
//!
//! `--all` aggregates across every repo registered in `~/.truth/registry.json`
//! (written by `truth init`). The per-repo stores stay separate — evidence is
//! repo-scoped by design — only the ledger is read across them.

use crate::config_util::load_config;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::PathBuf;
use truth_core::now_secs;
use truth_db::repo::CheckVerdictRow;
use truth_logs::window::window_secs;

#[derive(Debug, Default, serde::Serialize)]
pub struct RepoStats {
    pub repo: String,
    pub claims: usize,
    pub supported: usize,
    pub contradicted: usize,
    pub partial: usize,
    pub refused: usize,
    pub runs: usize,
    pub runs_green: usize,
    /// claim_type → contradicted count.
    pub contradicted_by_type: BTreeMap<String, usize>,
    /// (question, status, unix ts) — newest first, contradictions only.
    pub recent_contradictions: Vec<(String, i64)>,
}

/// Aggregate one repo's ledger over the window.
fn collect(conn: &Connection, repo: &str, since: i64) -> Result<RepoStats> {
    let rows = truth_db::repo::check_verdicts_since(conn, since)?;
    let runs = truth_db::repo::runs_since(conn, since)?;
    let mut s = RepoStats {
        repo: repo.to_string(),
        runs: runs.len(),
        runs_green: runs.iter().filter(|r| r.exit_code == 0).count(),
        ..Default::default()
    };
    for row in &rows {
        s.claims += 1;
        match row.status.as_str() {
            "supported" => s.supported += 1,
            "contradicted" => {
                s.contradicted += 1;
                *s.contradicted_by_type
                    .entry(claim_type_of(row))
                    .or_default() += 1;
                if s.recent_contradictions.len() < 5 {
                    s.recent_contradictions
                        .push((row.question.clone(), row.created_at));
                }
            }
            "partially_supported" => s.partial += 1,
            // inconclusive / needs_more_context — refusals by design.
            _ => s.refused += 1,
        }
    }
    Ok(s)
}

fn claim_type_of(row: &CheckVerdictRow) -> String {
    row.metadata_json
        .get("claim")
        .and_then(|c| c.get("claim_type"))
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// `~/.truth/registry.json` — the list of repos `truth init` has set up, so
/// `stats --all` can aggregate without a host-level store.
pub fn registry_path() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".truth").join("registry.json"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Record a repo root in the host registry (idempotent, best-effort: stats
/// aggregation is a convenience, never a reason for init to fail).
pub fn register_repo(root: &std::path::Path) {
    let Some(path) = registry_path() else { return };
    let mut repos: Vec<String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("repos").and_then(|r| {
                r.as_array().map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect()
                })
            })
        })
        .unwrap_or_default();
    let root = root.to_string_lossy().into_owned();
    if repos.contains(&root) {
        return;
    }
    repos.push(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!({ "repos": repos })).unwrap_or_default(),
    );
}

fn registered_repos() -> Vec<String> {
    registry_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("repos").and_then(|r| {
                r.as_array().map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .collect()
                })
            })
        })
        .unwrap_or_default()
}

/// One re-evaluated past contradiction.
#[derive(Debug, serde::Serialize)]
struct ReviewItem {
    question: String,
    when: String,
    /// Verdict the CURRENT engine gives the same question.
    now: String,
    /// True if it no longer contradicts — a phantom FP a fix has resolved.
    resolved: bool,
}

/// `truth stats --review` — re-run every past contradiction through the CURRENT
/// engine and report which ones no longer contradict. The ledger is the
/// false-positive review queue; this closes the loop by auto-detecting the
/// phantoms an engine fix has already retired (so they can stop scaring anyone)
/// while leaving the genuine catches flagged.
pub fn review(window: Option<&str>, json: bool) -> Result<()> {
    use truth_core::enums::Trigger;

    let since = now_secs() - window_secs(window);
    let label = window.unwrap_or("7d");
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;

    // De-dup questions (the ledger repeats meta-discussion sentences) and keep
    // the most recent timestamp for each.
    let rows = truth_db::repo::check_verdicts_since(&conn, since)?;
    let mut seen: BTreeMap<String, i64> = BTreeMap::new();
    for row in &rows {
        if row.status == "contradicted" {
            let e = seen.entry(row.question.clone()).or_insert(row.created_at);
            *e = (*e).max(row.created_at);
        }
    }

    // Re-index once so re-evaluation reflects the current working tree.
    if std::env::var_os("TRUTH_NO_AUTOINDEX").is_none() {
        crate::verify_turn::auto_refresh_index(&conn, &config);
    }

    let mut items: Vec<ReviewItem> = Vec::new();
    for (question, ts) in seen {
        // Re-run through the current engine. Trigger::Cli, no logs — we only
        // care whether the verdict still lands on "contradicted".
        let now = match crate::check::run_check(&conn, &config, &question, Trigger::Cli, None) {
            Ok(out) => out.decision.status.as_db_str().to_string(),
            Err(_) => "error".to_string(),
        };
        let resolved = now != "contradicted";
        let when = chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        items.push(ReviewItem {
            question,
            when,
            now,
            resolved,
        });
    }
    // Phantoms (now-resolved) first — they're the actionable cleanup.
    items.sort_by_key(|i| !i.resolved);

    let resolved = items.iter().filter(|i| i.resolved).count();
    let still = items.len() - resolved;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "window": label,
                "reviewed": items.len(),
                "resolved": resolved,
                "still_contradicted": still,
                "items": items,
            }))?
        );
        return Ok(());
    }

    println!("truth stats --review — re-checking past contradictions ({label})\n");
    if items.is_empty() {
        println!("  no contradictions recorded in this window.");
        return Ok(());
    }
    println!(
        "  reviewed {} distinct · {} now resolved (phantom FPs) · {} still contradicted\n",
        items.len(),
        resolved,
        still
    );
    for it in &items {
        let mark = if it.resolved {
            "✓ resolved"
        } else {
            "✗ stands "
        };
        let q: String = it.question.chars().take(64).collect();
        println!("  {mark}  [{:<13}] {q}  ({})", it.now, it.when);
    }
    if resolved > 0 {
        println!(
            "\n  {resolved} past contradiction(s) no longer fire — engine fixes retired them."
        );
    }
    println!();
    Ok(())
}

/// `truth stats [--window 7d] [--all] [--json]`
pub fn stats(window: Option<&str>, all: bool, json: bool) -> Result<()> {
    let since = now_secs() - window_secs(window);
    let label = window.unwrap_or("7d");

    let mut per_repo: Vec<RepoStats> = Vec::new();
    if all {
        for repo in registered_repos() {
            let db = std::path::Path::new(&repo)
                .join(".truth")
                .join("truth.sqlite");
            if !db.is_file() {
                continue;
            }
            match truth_db::open(&db) {
                Ok(conn) => per_repo.push(collect(&conn, &repo, since)?),
                Err(_) => continue,
            }
        }
        if per_repo.is_empty() {
            println!(
                "No registered repos with a .truth store found. Run `truth init` in a repo first."
            );
            return Ok(());
        }
    } else {
        let config = load_config()?;
        let conn = truth_db::open(&config.database.path)?;
        per_repo.push(collect(&conn, &config.repo.root, since)?);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "window": label,
                "repos": per_repo,
            }))?
        );
        return Ok(());
    }

    println!("truth stats — last {label}\n");
    for s in &per_repo {
        if per_repo.len() > 1 {
            println!("{}", s.repo);
        }
        if s.claims == 0 {
            println!("  no claims checked in this window\n");
            continue;
        }
        let pct = |n: usize| (n as f64 / s.claims as f64 * 100.0).round();
        println!("  claims checked    {:>5}", s.claims);
        println!(
            "  supported         {:>5} ({}%)",
            s.supported,
            pct(s.supported)
        );
        println!(
            "  contradicted      {:>5} ({}%)",
            s.contradicted,
            pct(s.contradicted)
        );
        if s.partial > 0 {
            println!("  partial           {:>5} ({}%)", s.partial, pct(s.partial));
        }
        println!("  refused           {:>5} ({}%)", s.refused, pct(s.refused));
        println!(
            "  runs recorded     {:>5} ({} green, {} failing)",
            s.runs,
            s.runs_green,
            s.runs - s.runs_green
        );
        if !s.contradicted_by_type.is_empty() {
            println!("\n  contradictions by claim type:");
            let mut by_count: Vec<(&String, &usize)> = s.contradicted_by_type.iter().collect();
            by_count.sort_by(|a, b| b.1.cmp(a.1));
            for (ty, n) in by_count {
                println!("    {ty:<18} {n}");
            }
        }
        if !s.recent_contradictions.is_empty() {
            println!("\n  recent contradictions:");
            for (q, ts) in &s.recent_contradictions {
                let date = chrono::DateTime::from_timestamp(*ts, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let q_short: String = q.chars().take(70).collect();
                println!("    ✗ {q_short}  ({date})");
            }
        }
        println!();
    }
    Ok(())
}
