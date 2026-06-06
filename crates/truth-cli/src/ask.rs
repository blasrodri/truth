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
        for o in owners_report.owners.iter().filter(|o| o.kind == "recent_committer").take(3) {
            let share = o.share.map(|s| format!(", {:.0}%", s * 100.0)).unwrap_or_default();
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

    if !resolved {
        notes.push(format!("Could not resolve `{file}` — pass a real path or run `truth index .`."));
    }

    Ok(AskReport { file: file.to_string(), resolved, owners, lead, activity, notes })
}

fn fmt_date(ts: i64) -> Option<String> {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0).single().map(|dt| dt.format("%Y-%m-%d").to_string())
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
}
