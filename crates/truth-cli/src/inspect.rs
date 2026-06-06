//! `truth inspect` — show what was indexed, and the shared evidence
//! categorization used by `inspect`, `baseline`, and `doctor`.

use crate::config_util::{load_config, print_json};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use truth_core::models::EvidenceItem;

/// Categories of indexed evidence, derived from the extractor predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Route,
    Constant,
    EnvVar,
    Port,
    Dependency,
    Other,
}

impl Category {
    pub fn of(item: &EvidenceItem) -> Category {
        match item.predicate.as_deref() {
            Some("route_exists") => Category::Route,
            Some("env_var_exists") => Category::EnvVar,
            Some("dependency_exists") => Category::Dependency,
            Some("port") => Category::Port,
            // retry_count / timeout / generic numeric constants.
            Some(_) if item.value_json.as_ref().map(|v| v.is_number()).unwrap_or(false) => {
                Category::Constant
            }
            _ => Category::Other,
        }
    }

    fn plural(self) -> &'static str {
        match self {
            Category::Route => "routes",
            Category::Constant => "constants",
            Category::EnvVar => "env vars",
            Category::Port => "ports",
            Category::Dependency => "dependencies",
            Category::Other => "other",
        }
    }
}

/// One indexed item, flattened for display/JSON.
#[derive(Debug, Clone, Serialize)]
pub struct InspectItem {
    pub category: Category,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
    /// "regex" | "ast" | ... — which extractor produced this evidence.
    pub extraction_method: String,
}

/// Build the citation `uri:line` for an evidence item.
pub fn citation(item: &EvidenceItem) -> Option<String> {
    let uri = item.metadata_json.get("uri").and_then(|v| v.as_str())?;
    let line = item.metadata_json.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
    Some(if line > 0 { format!("{uri}:{line}") } else { uri.to_string() })
}

/// Load all indexed items, flattened and categorized.
pub fn load_items(conn: &Connection) -> Result<Vec<InspectItem>> {
    let mut items: Vec<InspectItem> = truth_db::repo::all_evidence(conn)?
        .iter()
        .map(|e| InspectItem {
            category: Category::of(e),
            subject: e.subject_text.clone().unwrap_or_default(),
            value: e.value_json.clone(),
            citation: citation(e),
            extraction_method: e.extraction_method.as_db_str().to_string(),
        })
        .collect();
    // Stable, readable ordering.
    items.sort_by(|a, b| a.subject.cmp(&b.subject));
    items.dedup_by(|a, b| {
        a.category == b.category
            && a.subject == b.subject
            && a.citation == b.citation
            && a.extraction_method == b.extraction_method
    });
    Ok(items)
}

fn parse_category(name: &str) -> Option<Category> {
    match name {
        "routes" | "route" => Some(Category::Route),
        "constants" | "constant" | "consts" => Some(Category::Constant),
        "env" | "envs" | "env_vars" | "envvars" => Some(Category::EnvVar),
        "ports" | "port" => Some(Category::Port),
        "deps" | "dependencies" | "dependency" => Some(Category::Dependency),
        _ => None,
    }
}

/// `truth inspect [category] [--source ast|regex|all] [--json]`.
pub fn inspect(category: Option<&str>, source: Option<&str>, json: bool) -> Result<()> {
    let config = load_config()?;
    let conn = truth_db::open(&config.database.path)?;
    let mut items = load_items(&conn)?;

    // Optional filter by extraction method (--source).
    if let Some(src) = source {
        let src = src.to_ascii_lowercase();
        if src != "all" {
            items.retain(|i| i.extraction_method == src);
        }
    }

    match category.map(str::to_lowercase).as_deref() {
        None | Some("evidence") => {
            if category == Some("evidence") {
                emit_items(&items, json);
            } else {
                emit_summary(&conn, &items, json)?;
            }
        }
        Some("extraction") => emit_extraction(&conn, &items, json)?,
        Some(other) => {
            let Some(cat) = parse_category(other) else {
                anyhow::bail!(
                    "unknown inspect category `{other}` (try: routes, constants, env, ports, deps, evidence, extraction)"
                );
            };
            let filtered: Vec<&InspectItem> = items.iter().filter(|i| i.category == cat).collect();
            emit_category(cat, &filtered, json);
        }
    }
    Ok(())
}

/// Counts of items by extraction method (regex vs ast).
fn method_counts(items: &[InspectItem]) -> std::collections::BTreeMap<String, usize> {
    let mut m = std::collections::BTreeMap::new();
    for it in items {
        *m.entry(it.extraction_method.clone()).or_insert(0) += 1;
    }
    m
}

/// `truth inspect extraction` — extraction summary by method.
fn emit_extraction(conn: &Connection, items: &[InspectItem], json: bool) -> Result<()> {
    let counts = truth_db::repo::index_counts(conn)?;
    let by_method = method_counts(items);
    let routes_by_method: std::collections::BTreeMap<String, usize> = {
        let mut m = std::collections::BTreeMap::new();
        for it in items.iter().filter(|i| i.category == Category::Route) {
            *m.entry(it.extraction_method.clone()).or_insert(0) += 1;
        }
        m
    };

    if json {
        print_json(&serde_json::json!({
            "files_indexed": counts.artifacts,
            "evidence_items": counts.evidence_items,
            "by_method": by_method,
            "routes_by_method": routes_by_method,
        }));
        return Ok(());
    }

    println!("Extraction summary\n");
    println!("Files indexed: {}", counts.artifacts);
    println!("Evidence items: {}\n", counts.evidence_items);
    println!("Evidence by extraction method:");
    for (m, n) in &by_method {
        println!("• {m}: {n}");
    }
    if !routes_by_method.is_empty() {
        println!("\nRoutes by method:");
        for (m, n) in &routes_by_method {
            println!("• {m}: {n}");
        }
    }
    Ok(())
}

fn emit_summary(conn: &Connection, items: &[InspectItem], json: bool) -> Result<()> {
    let counts = truth_db::repo::index_counts(conn)?;
    let by_cat = |c: Category| items.iter().filter(|i| i.category == c).count();
    let cats = [
        Category::Route,
        Category::Constant,
        Category::EnvVar,
        Category::Port,
        Category::Dependency,
    ];

    if json {
        let out = serde_json::json!({
            "artifacts": counts.artifacts,
            "spans": counts.spans,
            "evidence_items": counts.evidence_items,
            "by_category": cats.iter().map(|c| serde_json::json!({
                "category": c,
                "count": by_cat(*c),
            })).collect::<Vec<_>>(),
        });
        print_json(&out);
        return Ok(());
    }

    println!("Indexed repo summary\n");
    println!("Artifacts: {}", counts.artifacts);
    println!("Spans: {}", counts.spans);
    println!("Evidence items: {}\n", counts.evidence_items);
    println!("Evidence by type:");
    for c in cats {
        println!("• {}: {}", c.plural(), by_cat(c));
    }
    println!("\nTry:");
    println!("  truth inspect routes");
    println!("  truth inspect constants");
    println!("  truth config MAX_RETRIES");
    Ok(())
}

fn emit_category(cat: Category, items: &[&InspectItem], json: bool) {
    if json {
        print_json(&serde_json::to_value(items).expect("items serialize"));
        return;
    }
    let title = {
        let p = cat.plural();
        let mut c = p.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    println!("{title}\n");
    if items.is_empty() {
        println!("(none indexed)");
        return;
    }
    for it in items {
        match &it.value {
            Some(v) if !v.is_boolean() => println!("{} = {}", it.subject, scalar(v)),
            _ => println!("{}", it.subject),
        }
        if let Some(c) = &it.citation {
            println!("  {c}");
        }
        println!("  extraction: {}", it.extraction_method);
    }
}

fn emit_items(items: &[InspectItem], json: bool) {
    if json {
        print_json(&serde_json::to_value(items).expect("items serialize"));
        return;
    }
    println!("Indexed evidence\n");
    for it in items {
        let val = it.value.as_ref().map(|v| format!(" = {}", scalar(v))).unwrap_or_default();
        let cite = it.citation.as_deref().map(|c| format!(" ({c})")).unwrap_or_default();
        println!("• [{}] {}{}{}", category_label(it.category), it.subject, val, cite);
    }
}

fn category_label(c: Category) -> &'static str {
    match c {
        Category::Route => "route",
        Category::Constant => "const",
        Category::EnvVar => "env",
        Category::Port => "port",
        Category::Dependency => "dep",
        Category::Other => "other",
    }
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
