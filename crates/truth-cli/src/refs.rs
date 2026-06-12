//! `truth uses <symbol>` — find references to a symbol/route/dependency across
//! the indexed code, the *code-side* complement to log-based usage.
//!
//! The product's headline is "nobody uses X". Until now we could only answer
//! that from runtime logs. This answers it from the code: is the symbol
//! referenced anywhere other than where it is defined?
//!
//! Deterministic and lazy: re-reads the indexed files at check time (we don't
//! store bodies), word-boundary matched to avoid substring noise, excludes the
//! definition site.

use crate::config_util::{load_config, print_json};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct RefHit {
    pub file: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefsReport {
    pub symbol: String,
    /// "referenced" | "unreferenced" | "definition_only"
    pub status: String,
    /// Reference count excluding the definition site.
    pub count: usize,
    /// Up to N sample reference sites.
    pub samples: Vec<RefHit>,
    /// Files scanned.
    pub files_scanned: usize,
    pub caveats: Vec<String>,
}

/// Whether `needle` occurs in `line` on identifier word boundaries (so `port`
/// does not match `support`). For path-like needles (containing `/`) we use a
/// plain substring match since `/` already bounds them.
fn line_references(line: &str, needle: &str) -> bool {
    if needle.contains('/') {
        return line.contains(needle);
    }
    let bytes = line.as_bytes();
    let nb = needle.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = line[start..].find(needle) {
        let i = start + pos;
        let before_ok = i == 0 || !is_word(bytes[i - 1]);
        let after = i + nb.len();
        let after_ok = after >= bytes.len() || !is_word(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
        if start >= line.len() {
            break;
        }
    }
    false
}

/// Scan `files` for references to `symbol`, excluding the definition site
/// (`def_file`:`def_line`). Returns (count, sample hits, files_scanned).
pub fn scan_references(
    files: &[String],
    symbol: &str,
    def_file: Option<&str>,
    def_line: Option<u32>,
    max_samples: usize,
) -> (usize, Vec<RefHit>, usize) {
    let mut count = 0;
    let mut samples = Vec::new();
    let mut scanned = 0;

    for file in files {
        let Ok(contents) = std::fs::read_to_string(file) else {
            continue;
        };
        scanned += 1;
        for (i, line) in contents.lines().enumerate() {
            let line_no = (i + 1) as u32;
            // Skip the definition site itself.
            if Some(file.as_str()) == def_file && Some(line_no) == def_line {
                continue;
            }
            if line_references(line, symbol) {
                count += 1;
                if samples.len() < max_samples {
                    samples.push(RefHit {
                        file: file.clone(),
                        line: line_no,
                        text: line.trim().chars().take(160).collect(),
                    });
                }
            }
        }
    }
    (count, samples, scanned)
}

/// Build a references report for a symbol, resolving its definition site from
/// the index when possible (to exclude it from the count).
/// Whether the index contains ANY `dependency_exists` fact — i.e. at least one
/// manifest (Cargo.toml/package.json/...) was parsed. Used by the verdict engine
/// to distinguish "X is not a dependency" from "no manifest was indexed".
pub fn dependency_index_populated(conn: &rusqlite::Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM evidence_items WHERE predicate = 'dependency_exists'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn build_report(conn: &rusqlite::Connection, symbol: &str) -> Result<RefsReport> {
    let files = truth_db::repo::repo_file_uris(conn)?;

    // Find the definition site (first evidence item whose subject matches), so
    // we can exclude it and distinguish "defined but unused" from "not found".
    let def = truth_db::repo::evidence_matching_key(conn, symbol)?
        .into_iter()
        .find(|i| i.subject_text.as_deref() == Some(symbol));
    let (def_file, def_line) = match &def {
        Some(it) => (
            it.metadata_json
                .get("uri")
                .and_then(|v| v.as_str())
                .map(String::from),
            it.metadata_json
                .get("line")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
        ),
        None => (None, None),
    };

    let (count, samples, files_scanned) =
        scan_references(&files, symbol, def_file.as_deref(), def_line, 5);

    let mut caveats = vec![
        "Text references in indexed files; excludes the definition site.".to_string(),
        "A reference is not proof of runtime use (and vice-versa) — pair with `truth usage`."
            .to_string(),
    ];

    let status = if count > 0 {
        "referenced"
    } else if def.is_some() {
        caveats.push(
            "Defined but never referenced in the indexed code — a strong dead-code signal."
                .to_string(),
        );
        "definition_only"
    } else {
        caveats.push(
            "Not found in the indexed code at all (check spelling or run `truth index .`)."
                .to_string(),
        );
        "unreferenced"
    };

    Ok(RefsReport {
        symbol: symbol.to_string(),
        status: status.to_string(),
        count,
        samples,
        files_scanned,
        caveats,
    })
}

/// `truth uses <symbol> [--json]`.
pub fn uses(symbol: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let report = build_report(&conn, symbol)?;

    if json {
        print_json(&serde_json::to_value(&report)?);
        return Ok(());
    }

    let headline = match report.status.as_str() {
        "referenced" => format!(
            "`{}` is referenced {} time(s) in code.",
            symbol, report.count
        ),
        "definition_only" => format!("`{}` is defined but never referenced in code.", symbol),
        _ => format!("`{}` was not found in the indexed code.", symbol),
    };
    println!("{headline}\n");

    if !report.samples.is_empty() {
        println!("References:");
        for h in &report.samples {
            println!("  {}:{}  {}", short_path(&h.file), h.line, h.text);
        }
        if report.count > report.samples.len() {
            println!("  … and {} more", report.count - report.samples.len());
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

fn short_path(p: &str) -> String {
    // Show the last 3 path components for readability.
    let parts: Vec<&str> = Path::new(p).iter().filter_map(|s| s.to_str()).collect();
    if parts.len() > 3 {
        format!(".../{}", parts[parts.len() - 3..].join("/"))
    } else {
        p.to_string()
    }
}
