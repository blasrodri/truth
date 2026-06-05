//! `truth explain <check-id>` — reconstruct a previous check from the SQLite
//! audit trail: question, extracted claim, evidence queries, and verdict.

use anyhow::{anyhow, Result};
use std::path::Path;
use truth_core::config::Config;

pub fn explain(check_id: &str, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;

    let check = truth_db::repo::get_check(&conn, check_id)?
        .ok_or_else(|| anyhow!("no check found with id {check_id}"))?;
    let queries = truth_db::repo::get_evidence_queries_for_check(&conn, check_id)?;
    let verdict = truth_db::repo::get_verdict_for_check(&conn, check_id)?;

    let claim = check.metadata_json.get("claim").cloned();

    if json {
        let out = serde_json::json!({
            "check_id": check.id,
            "question": check.question,
            "trigger": check.trigger.as_db_str(),
            "question_type": check.question_type.map(|q| q.as_db_str()),
            "created_at": check.created_at,
            "claim": claim,
            "evidence_queries": queries.iter().map(|q| serde_json::json!({
                "source": q.source.as_db_str(),
                "query_type": q.query_type,
                "query_text": q.query_text,
                "time_from": q.time_from,
                "time_to": q.time_to,
                "result": q.result_summary_json,
                "executed_at": q.executed_at,
            })).collect::<Vec<_>>(),
            "verdict": verdict.as_ref().map(|v| serde_json::json!({
                "status": v.status.as_db_str(),
                "confidence": v.confidence,
                "caveats": v.caveats_json,
                "summary": v.summary,
            })),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("Check: {}", check.id);
    println!("Question: {}", check.question);
    if let Some(v) = &verdict {
        println!("Verdict: {}", title(v.status.as_db_str()));
        println!("Confidence: {:.2}", v.confidence);
    }
    println!();

    if let Some(claim) = &claim {
        println!("Extracted claim:");
        for (k, label) in [
            ("claim_type", "type"),
            ("subject", "subject"),
            ("operator", "operator"),
            ("value", "expected"),
            ("time_window", "window"),
            ("environment", "env"),
        ] {
            if let Some(val) = claim.get(k) {
                if !val.is_null() {
                    println!("• {label}: {}", scalar(val));
                }
            }
        }
        println!();
    }

    if !queries.is_empty() {
        println!("Evidence queries:");
        for q in &queries {
            println!("• {} {}", title(q.source.as_db_str()), q.query_type);
            println!("  query: {}", q.query_text);
            let summary = &q.result_summary_json;
            if let Some(count) = summary.get("count").and_then(|c| c.as_i64()) {
                println!("  result: {count}");
            }
            if let Some(latest) = summary.get("latest_seen") {
                if !latest.is_null() {
                    let shown = latest
                        .as_i64()
                        .map(fmt_ts)
                        .unwrap_or_else(|| scalar(latest));
                    println!("  latest: {shown}");
                }
            }
        }
        println!();
    }

    if let Some(v) = &verdict {
        if let Some(caveats) = v.caveats_json.as_array() {
            if !caveats.is_empty() {
                println!("Caveats:");
                for c in caveats {
                    if let Some(s) = c.as_str() {
                        println!("• {s}");
                    }
                }
            }
        }
    }

    Ok(())
}

fn fmt_ts(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| secs.to_string())
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn title(db_str: &str) -> String {
    let mut chars = db_str.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn load_config() -> Result<Config> {
    if Path::new("truth.toml").exists() {
        Config::load("truth.toml")
    } else {
        Ok(Config::from_toml_str("")?)
    }
}
