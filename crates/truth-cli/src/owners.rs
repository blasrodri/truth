//! `truth owners <subject>` — who has worked on the code behind a subject.
//!
//! Resolves the subject (route, constant, env var, or a file path) to the
//! file(s) it lives in via the index, then reports ownership from the
//! authoritative source (CODEOWNERS / MAINTAINERS) and, as a heuristic signal,
//! recent git committers.
//!
//! Framing is deliberate and conservative: this is "who has worked on this
//! code", with cited evidence — not a claim of who is responsible. Git history
//! answers "who touched it", a proxy for ownership, so we never assert "X should
//! fix it"; we surface signal and let humans judge.

use crate::config_util::{load_config, print_json};
use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};
use truth_git::owners::Ownership;
use truth_git::GitHistory;

#[derive(Debug, Clone, Serialize)]
pub struct OwnerEntry {
    pub who: String,
    /// codeowner | maintainer | reviewer | recent_committer
    pub kind: String,
    /// Where the signal came from (CODEOWNERS, MAINTAINERS, git).
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OwnersReport {
    pub subject: String,
    pub files: Vec<String>,
    pub owners: Vec<OwnerEntry>,
    pub caveats: Vec<String>,
}

/// Resolve the files a subject maps to, using the index. A subject that is a
/// path is used directly; otherwise we look up evidence and read its `uri`.
fn files_for_subject(conn: &rusqlite::Connection, subject: &str) -> Result<Vec<String>> {
    // Direct file path?
    if Path::new(subject).exists() {
        return Ok(vec![subject.to_string()]);
    }
    let mut files: Vec<String> = truth_db::repo::evidence_matching_key(conn, subject)?
        .into_iter()
        .filter_map(|i| {
            i.metadata_json
                .get("uri")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

fn fmt_date(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d").to_string())
}

/// Build the owners report for a subject.
pub fn build_report(conn: &rusqlite::Connection, subject: &str) -> Result<OwnersReport> {
    let files = files_for_subject(conn, subject)?;
    let mut owners: Vec<OwnerEntry> = Vec::new();
    let mut caveats: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if files.is_empty() {
        caveats.push(format!(
            "Could not resolve `{subject}` to any indexed file. Try `truth index .` or a file path."
        ));
        return Ok(OwnersReport { subject: subject.to_string(), files, owners, caveats });
    }

    let mut any_explicit = false;
    for file in &files {
        let path = PathBuf::from(file);
        // Authoritative ownership: search upward for the repo root's ownership
        // files by walking parents until one has data.
        let ownership = find_ownership(&path);
        if let Some((own, root)) = &ownership {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            for o in own.owners_for(rel) {
                let key = (o.who.clone(), o.role.clone());
                if seen.insert(key) {
                    any_explicit = true;
                    owners.push(OwnerEntry {
                        who: o.who,
                        kind: o.role,
                        source: o.source,
                        last_active: None,
                    });
                }
            }
        }

        // Git committer signal (always shown as supporting evidence).
        let history = GitHistory::for_file(&path);
        if history.available() {
            for (author, _score, ts) in history.recent_committers(&path, 30).into_iter().take(3) {
                let key = (author.clone(), "recent_committer".to_string());
                if seen.insert(key) {
                    owners.push(OwnerEntry {
                        who: author,
                        kind: "recent_committer".into(),
                        source: "git".into(),
                        last_active: fmt_date(ts),
                    });
                }
            }
        }
    }

    if any_explicit {
        caveats.push("Maintainers/code-owners are the intended owners.".to_string());
    } else if owners.iter().any(|o| o.kind == "recent_committer") {
        caveats.push(
            "No CODEOWNERS/MAINTAINERS found; showing recent git committers (who touched the code, a proxy for ownership — not a claim of responsibility)."
                .to_string(),
        );
    } else {
        caveats.push("No ownership signal found for the resolved file(s).".to_string());
    }

    Ok(OwnersReport { subject: subject.to_string(), files, owners, caveats })
}

/// Walk up from a file to find the nearest ancestor with ownership data, so a
/// deep file (`kernel/sched/core.c`) resolves against the repo-root MAINTAINERS.
fn find_ownership(file: &Path) -> Option<(Ownership, PathBuf)> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        let own = Ownership::load(d);
        if own.has_data() {
            return Some((own, d.to_path_buf()));
        }
        dir = d.parent();
    }
    None
}

/// `truth owners <subject> [--json]`.
pub fn owners(subject: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let report = build_report(&conn, subject)?;

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    if report.files.is_empty() {
        println!("Could not resolve `{}` to any indexed file.", subject);
        for c in &report.caveats {
            println!("• {c}");
        }
        return Ok(());
    }

    println!("Owners for `{}`\n", subject);
    println!("Resolved to:");
    for f in &report.files {
        println!("  {f}");
    }

    let explicit: Vec<&OwnerEntry> = report
        .owners
        .iter()
        .filter(|o| o.kind != "recent_committer")
        .collect();
    let committers: Vec<&OwnerEntry> = report
        .owners
        .iter()
        .filter(|o| o.kind == "recent_committer")
        .collect();

    if !explicit.is_empty() {
        println!("\nDeclared owners:");
        for o in explicit {
            println!("  • {} — {} ({})", o.who, o.kind, o.source);
        }
    }
    if !committers.is_empty() {
        println!("\nRecently worked on this code (git):");
        for o in committers {
            let when = o.last_active.as_deref().map(|d| format!(", last {d}")).unwrap_or_default();
            println!("  • {}{}", o.who, when);
        }
    }
    if !report.caveats.is_empty() {
        println!("\nCaveats:");
        for c in &report.caveats {
            println!("• {c}");
        }
    }
    Ok(())
}
