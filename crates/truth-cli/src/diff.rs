//! `truth diff` — compare two reports or recorded eval outputs, surfacing
//! changed verdicts, new claims, and removed claims.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct VerdictChange {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub changed: Vec<VerdictChange>,
    pub added: Vec<NamedStatus>,
    pub removed: Vec<NamedStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NamedStatus {
    pub id: String,
    pub status: String,
}

/// Compute the diff between two id→status maps.
pub fn diff_maps(
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> DiffReport {
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    for (id, new_status) in new {
        match old.get(id) {
            Some(old_status) if old_status != new_status => changed.push(VerdictChange {
                id: id.clone(),
                from: old_status.clone(),
                to: new_status.clone(),
            }),
            Some(_) => {}
            None => added.push(NamedStatus { id: id.clone(), status: new_status.clone() }),
        }
    }
    for (id, old_status) in old {
        if !new.contains_key(id) {
            removed.push(NamedStatus { id: id.clone(), status: old_status.clone() });
        }
    }
    DiffReport { changed, added, removed }
}

/// Parse a report-JSON or recorded-YAML file into an id→status map.
pub fn load_status_map(path: &str) -> Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let trimmed = text.trim_start();

    // Report JSON: { "results": [ { "id", "status" } ] }
    if trimmed.starts_with('{') {
        let v: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parsing JSON {path}"))?;
        return Ok(status_map_from_json(&v));
    }

    // Recorded YAML: { cases: [ { name, expected_status } ] } or a claim file.
    let v: serde_yaml::Value =
        serde_yaml::from_str(&text).with_context(|| format!("parsing YAML {path}"))?;
    status_map_from_yaml(&v)
        .with_context(|| format!("{path} is not a recognized report or recorded fixture"))
}

fn status_map_from_json(v: &serde_json::Value) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(results) = v.get("results").and_then(|r| r.as_array()) {
        for r in results {
            if let (Some(id), Some(status)) = (
                r.get("id").and_then(|x| x.as_str()),
                r.get("status").and_then(|x| x.as_str()),
            ) {
                map.insert(id.to_string(), status.to_string());
            }
        }
    }
    map
}

fn status_map_from_yaml(v: &serde_yaml::Value) -> Option<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    // Recorded eval fixture: cases[].{name, expected_status}.
    if let Some(cases) = v.get("cases").and_then(|c| c.as_sequence()) {
        for c in cases {
            let id = c.get("name").and_then(|x| x.as_str());
            let status = c.get("expected_status").and_then(|x| x.as_str());
            if let (Some(id), Some(status)) = (id, status) {
                map.insert(id.to_string(), status.to_string());
            }
        }
        return Some(map);
    }
    // Claim file with recorded statuses: claims[].{id, expected_status}.
    if let Some(claims) = v.get("claims").and_then(|c| c.as_sequence()) {
        for c in claims {
            let id = c.get("id").and_then(|x| x.as_str());
            let status = c.get("expected_status").and_then(|x| x.as_str());
            if let (Some(id), Some(status)) = (id, status) {
                map.insert(id.to_string(), status.to_string());
            }
        }
        return Some(map);
    }
    None
}

fn title(db_status: &str) -> String {
    db_status
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// CLI entry point for `truth diff`.
pub fn diff(old_path: &str, new_path: &str, json: bool) -> Result<()> {
    let old = load_status_map(old_path)?;
    let new = load_status_map(new_path)?;
    let report = diff_maps(&old, &new);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("truth diff\n");
    if report.changed.is_empty() && report.added.is_empty() && report.removed.is_empty() {
        println!("No changes.");
        return Ok(());
    }

    if !report.changed.is_empty() {
        println!("Changed verdicts:");
        for c in &report.changed {
            println!("• {}: {} → {}", c.id, title(&c.from), title(&c.to));
        }
        println!();
    }
    if !report.added.is_empty() {
        println!("New claims:");
        for a in &report.added {
            println!("• {} — {}", a.id, title(&a.status));
        }
        println!();
    }
    if !report.removed.is_empty() {
        println!("Removed claims:");
        for r in &report.removed {
            println!("• {}", r.id);
        }
    }
    Ok(())
}
