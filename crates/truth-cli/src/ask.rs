//! `truth ask <file>` — one fused answer to "what's the deal with this file?".
//!
//! Composes the proven per-file signals into a single human report:
//! - who owns it (declared owners, or the clearest recent git lead),
//! - how active it is (commits, last change, active/stale),
//! - whether its key symbols look used or dead (best-effort).
//!
//! Everything is deterministic and cited; nothing here is an LLM judgement.

use crate::config_util::{load_config, print_json};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use truth_git::GitHistory;

#[derive(Debug, Clone, Serialize)]
pub struct AskReport {
    pub file: String,
    pub resolved: bool,
    /// Declared owners and/or the clear git lead, human-formatted.
    pub owners: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivitySummary>,
    /// How many other files reference this file's module (by stem). None if not
    /// computable. 0 = likely an orphan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<usize>,
    /// Documentation mentions of this file's module name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_mentions: Option<usize>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivitySummary {
    pub commits: usize,
    pub last_changed: Option<String>,
    pub first_changed: Option<String>,
    /// "active" | "maintained" | "stale"
    pub status: String,
}

/// Days since a unix timestamp, given "now". Pure so it stays testable.
fn days_since(now: i64, ts: i64) -> i64 {
    (now - ts).max(0) / 86_400
}

/// Classify activity from last-change recency.
fn activity_status(now: i64, last_ts: i64) -> &'static str {
    match days_since(now, last_ts) {
        0..=90 => "active",
        91..=730 => "maintained",
        _ => "stale",
    }
}

pub fn build_report(conn: &rusqlite::Connection, file: &str, now: i64) -> Result<AskReport> {
    let owners_report = crate::owners::build_report(conn, file)?;
    let resolved = !owners_report.files.is_empty();

    let mut notes = Vec::new();
    let mut owners = Vec::new();
    let mut lead = None;

    // Declared owners first; otherwise the recent git committers.
    for o in &owners_report.owners {
        if o.kind == "recent_committer" {
            continue;
        }
        owners.push(format!("{} ({})", o.who, o.kind));
    }
    // The clear-lead caveat (if any) names the dominant recent committer.
    for c in &owners_report.caveats {
        if c.contains("clearest recent owner") {
            lead = Some(c.clone());
        }
    }
    if owners.is_empty() {
        // Fall back to top committers as the ownership signal.
        for o in owners_report
            .owners
            .iter()
            .filter(|o| o.kind == "recent_committer")
            .take(3)
        {
            let share = o
                .share
                .map(|s| format!(", {:.0}%", s * 100.0))
                .unwrap_or_default();
            owners.push(format!("{}{}", o.who, share));
        }
    }

    // Activity from git, against the resolved file path.
    let mut activity = None;
    if let Some(path) = owners_report.files.first() {
        let p = Path::new(path);
        let history = GitHistory::for_file(p);
        if let Some(a) = history.file_activity(p) {
            activity = Some(ActivitySummary {
                commits: a.commits,
                last_changed: fmt_date(a.last_ts),
                first_changed: fmt_date(a.first_ts),
                status: activity_status(now, a.last_ts).to_string(),
            });
        }
    }

    // Module-level usage: is this file referenced by OTHER files (by its module
    // stem, e.g. `mutex` for `mutex.rs`)? 0 hints at an orphan/leaf. And doc
    // coverage of that module name.
    let mut referenced_by = None;
    let mut doc_mentions = None;
    if let Some(path) = owners_report.files.first() {
        if let Some(stem) = module_stem(path) {
            let code = truth_db::repo::code_file_uris(conn)?;
            let others: Vec<String> = code.into_iter().filter(|u| !same_file(u, path)).collect();
            let (count, _, scanned) = crate::refs::scan_references(&others, &stem, None, None, 0);
            if scanned > 0 {
                referenced_by = Some(count);
            }
            let docs = truth_db::repo::doc_file_uris(conn)?;
            let (dcount, _, dscanned) = crate::refs::scan_references(&docs, &stem, None, None, 0);
            if dscanned > 0 {
                doc_mentions = Some(dcount);
            }
            if referenced_by == Some(0) {
                notes.push(format!(
                    "No other file references the `{stem}` module — it may be an entry point or unused."
                ));
            }
        }
    }

    if !resolved {
        notes.push(format!(
            "Could not resolve `{file}` — pass a real path or run `truth index .`."
        ));
    }

    Ok(AskReport {
        file: file.to_string(),
        resolved,
        owners,
        lead,
        activity,
        referenced_by,
        doc_mentions,
        notes,
    })
}

/// The module stem of a file path (`.../sync/mutex.rs` -> "mutex"), for a
/// module-level "is this referenced elsewhere?" scan. Skips index-style stems
/// (`mod`, `lib`, `main`, `index`) that aren't meaningful module names.
fn module_stem(path: &str) -> Option<String> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    if stem.len() < 3 || matches!(stem, "mod" | "lib" | "main" | "index") {
        return None;
    }
    Some(stem.to_string())
}

/// Whether two URIs point at the same file, tolerating a `./` prefix.
fn same_file(a: &str, b: &str) -> bool {
    a.trim_start_matches("./") == b.trim_start_matches("./")
}

/// The module stem of a path for display (best-effort).
fn stem_of(path: &str) -> String {
    module_stem(path).unwrap_or_else(|| path.to_string())
}

fn fmt_date(ts: i64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

/// `truth ask <file> [--json]`.
pub fn ask(file: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let now = chrono::Utc::now().timestamp();
    let report = build_report(&conn, file, now)?;

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    println!("About `{}`\n", report.file);
    if !report.resolved {
        for n in &report.notes {
            println!("• {n}");
        }
        return Ok(());
    }

    if !report.owners.is_empty() {
        println!("Owners / who to ask:");
        for o in &report.owners {
            println!("  • {o}");
        }
    }
    if let Some(lead) = &report.lead {
        println!("  {}", lead.trim_start_matches('•').trim());
    }
    if let Some(a) = &report.activity {
        let last = a.last_changed.as_deref().unwrap_or("?");
        let first = a.first_changed.as_deref().unwrap_or("?");
        println!(
            "\nActivity: {} ({} commits, last changed {last}, since {first}).",
            a.status, a.commits
        );
    }
    if let Some(n) = report.referenced_by {
        // The zero case is the reliable signal (likely orphan). A nonzero count
        // is a coarse mention-count (a common stem like `worker` over-counts), so
        // bucket it rather than imply precision.
        let s = match n {
            0 => "Referenced elsewhere: none found — likely an entry point or unused.".to_string(),
            1..=5 => format!(
                "Referenced elsewhere: a few files mention `{}` (~{n}).",
                stem_of(&report.file)
            ),
            _ => format!(
                "Referenced elsewhere: widely mentioned (`{}` appears across the codebase).",
                stem_of(&report.file)
            ),
        };
        println!("{s}");
    }
    if let Some(d) = report.doc_mentions {
        if d > 0 {
            println!("Docs: mentioned in {d} doc location(s).");
        }
    }
    for n in &report.notes {
        println!("\n• {n}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    #[test]
    fn activity_classification() {
        let now = 1_000_000 * DAY;
        assert_eq!(activity_status(now, now - 10 * DAY), "active");
        assert_eq!(activity_status(now, now - 200 * DAY), "maintained");
        assert_eq!(activity_status(now, now - 1000 * DAY), "stale");
        // Future / equal timestamps don't go negative.
        assert_eq!(activity_status(now, now + DAY), "active");
        assert_eq!(days_since(now, now + DAY), 0);
    }

    #[test]
    fn module_stem_skips_index_files() {
        assert_eq!(module_stem("a/b/mutex.rs").as_deref(), Some("mutex"));
        assert_eq!(module_stem("a/b/mod.rs"), None);
        assert_eq!(module_stem("a/b/lib.rs"), None);
        assert_eq!(module_stem("a/io.rs"), None); // too short (<3)
        assert!(same_file("./src/x.rs", "src/x.rs"));
        assert!(!same_file("src/x.rs", "src/y.rs"));
    }
}
