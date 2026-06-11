//! `truth settings` — read and write `truth.toml` keys without hand-editing.
//!
//! Exposes a curated, validated set of dotted keys (e.g. `indexer.extractor`)
//! so users — and agents — can tweak behavior programmatically. Editing is
//! structure-preserving: only the targeted key changes; comments and other keys
//! in `truth.toml` are left intact by re-serializing the parsed document.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// A configurable key: its dotted path and how its value is validated.
struct Setting {
    key: &'static str,
    /// Human description shown by `settings list`.
    help: &'static str,
    /// Allowed values, or empty for free-form (still type-checked below).
    allowed: &'static [&'static str],
    /// Value kind for parsing/validation.
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    /// A comma-separated list of strings → a TOML array.
    StrList,
}

/// The curated settings surface. Keep this small and meaningful — these are the
/// knobs that actually change verification behavior.
const SETTINGS: &[Setting] = &[
    Setting {
        key: "indexer.extractor",
        help:
            "Extraction backend: regex (fast) | ast | mixed (AST-precise symbols/routes for Rust/TS/Python/Go)",
        allowed: &["regex", "ast", "mixed"],
        kind: Kind::Str,
    },
    Setting {
        key: "repo.include",
        help: "Comma-separated paths to index (e.g. src,lib,app)",
        allowed: &[],
        kind: Kind::StrList,
    },
    Setting {
        key: "repo.exclude",
        help: "Comma-separated paths to skip (e.g. target,node_modules,testdata)",
        allowed: &[],
        kind: Kind::StrList,
    },
    Setting {
        key: "llm.enabled",
        help: "Use an LLM to extract claims from prose (engine still decides verdicts): true|false",
        allowed: &["true", "false"],
        kind: Kind::Bool,
    },
    Setting {
        key: "llm.base_url",
        help: "OpenAI-compatible endpoint for claim extraction (e.g. http://localhost:11434/v1)",
        allowed: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "llm.model",
        help: "Model name for claim extraction (e.g. qwen3:1.7b)",
        allowed: &[],
        kind: Kind::Str,
    },
    Setting {
        key: "security.redact_pii",
        help: "Redact emails/JWTs/UUIDs/IPs in stored log samples: true|false",
        allowed: &["true", "false"],
        kind: Kind::Bool,
    },
];

fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

const PATH: &str = "truth.toml";

fn load_doc() -> Result<toml::Table> {
    if !Path::new(PATH).exists() {
        bail!("no truth.toml here — run `truth init` first.");
    }
    let text = std::fs::read_to_string(PATH).context("reading truth.toml")?;
    text.parse::<toml::Table>().context("parsing truth.toml")
}

/// `truth settings list` — show every configurable key, its current value, and help.
pub fn list(json: bool) -> Result<()> {
    let doc = load_doc().unwrap_or_default();
    if json {
        let items: Vec<_> = SETTINGS
            .iter()
            .map(|s| {
                serde_json::json!({
                    "key": s.key,
                    "value": get_value(&doc, s.key).map(toml_to_json),
                    "help": s.help,
                    "allowed": s.allowed,
                })
            })
            .collect();
        crate::config_util::print_json(&serde_json::json!({ "settings": items }));
        return Ok(());
    }
    println!("truth settings\n");
    for s in SETTINGS {
        let cur = get_value(&doc, s.key)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(default)".into());
        println!("  {:<22} = {}", s.key, cur);
        println!("      {}", s.help);
        if !s.allowed.is_empty() {
            println!("      allowed: {}", s.allowed.join(" | "));
        }
    }
    Ok(())
}

/// `truth settings get <key>`.
pub fn get(key: &str, json: bool) -> Result<()> {
    if find(key).is_none() {
        bail!("unknown setting `{key}` — run `truth settings list`.");
    }
    let doc = load_doc().unwrap_or_default();
    let val = get_value(&doc, key);
    if json {
        crate::config_util::print_json(&serde_json::json!({
            "key": key, "value": val.map(toml_to_json),
        }));
    } else {
        match val {
            Some(v) => println!("{key} = {v}"),
            None => println!("{key} = (default)"),
        }
    }
    Ok(())
}

/// `truth settings set <key> <value>` — validate and write, preserving the rest.
pub fn set(key: &str, value: &str, json: bool) -> Result<()> {
    let Some(setting) = find(key) else {
        bail!("unknown setting `{key}` — run `truth settings list` to see valid keys.");
    };
    if !setting.allowed.is_empty() && !setting.allowed.contains(&value) {
        bail!(
            "invalid value `{value}` for {key}; allowed: {}",
            setting.allowed.join(" | ")
        );
    }
    let toml_val = parse_value(setting.kind, value)?;

    let mut doc = load_doc()?;
    set_value(&mut doc, key, toml_val);
    let serialized = toml::to_string_pretty(&doc).context("serializing truth.toml")?;
    std::fs::write(PATH, serialized).context("writing truth.toml")?;

    if json {
        crate::config_util::print_json(&serde_json::json!({ "key": key, "set": value }));
    } else {
        println!("Set {key} = {value}");
        if key == "indexer.extractor" {
            println!("Re-index to apply: truth index .");
        }
    }
    Ok(())
}

fn parse_value(kind: Kind, value: &str) -> Result<toml::Value> {
    Ok(match kind {
        Kind::Str => toml::Value::String(value.to_string()),
        Kind::Bool => toml::Value::Boolean(
            value
                .parse()
                .with_context(|| format!("`{value}` is not a bool (true|false)"))?,
        ),
        Kind::StrList => toml::Value::Array(
            value
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        ),
    })
}

/// Read a dotted key from the document.
fn get_value<'a>(doc: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let (table, leaf) = key.rsplit_once('.')?;
    let mut cur = doc;
    for part in table.split('.') {
        cur = cur.get(part)?.as_table()?;
    }
    cur.get(leaf)
}

/// Set a dotted key, creating intermediate tables as needed.
fn set_value(doc: &mut toml::Table, key: &str, val: toml::Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let (leaf, tables) = parts.split_last().unwrap();
    let mut cur = doc;
    for part in tables {
        cur = cur
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("config path is a table");
    }
    cur.insert(leaf.to_string(), val);
}

fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::json!(s),
        toml::Value::Boolean(b) => serde_json::json!(b),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        other => serde_json::json!(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_get_set_roundtrip() {
        let mut doc = toml::Table::new();
        set_value(
            &mut doc,
            "indexer.extractor",
            toml::Value::String("mixed".into()),
        );
        assert_eq!(
            get_value(&doc, "indexer.extractor").and_then(|v| v.as_str()),
            Some("mixed")
        );
    }

    #[test]
    fn parse_strlist_splits_and_trims() {
        let v = parse_value(Kind::StrList, "src, lib , app").unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[1].as_str(), Some("lib"));
    }

    #[test]
    fn parse_bool_rejects_garbage() {
        assert!(parse_value(Kind::Bool, "yes").is_err());
        assert!(parse_value(Kind::Bool, "true").is_ok());
    }
}
