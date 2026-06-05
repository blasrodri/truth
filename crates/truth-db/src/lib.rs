//! `truth-db` — SQLite persistence (rusqlite) and migrations.

pub mod migrate;
pub mod repo;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// Open (creating parent dirs if needed), enable foreign keys, and migrate.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db dir {}", parent.display()))?;
        }
    }
    let conn = Connection::open(path)
        .with_context(|| format!("opening sqlite at {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate::run(&conn)?;
    Ok(conn)
}

/// In-memory connection (tests).
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate::run(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use truth_core::enums::*;
    use truth_core::models::*;

    #[test]
    fn migrations_create_schema_and_roundtrip() {
        let conn = open_in_memory().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let check = Check {
            id: "c1".into(),
            trigger: Trigger::Cli,
            question: "test".into(),
            question_type: Some(QuestionType::Usage),
            status: CheckStatus::Completed,
            created_at: 1,
            metadata_json: json!({}),
        };
        repo::insert_check(&conn, &check).unwrap();

        let v = Verdict {
            id: "v1".into(),
            check_id: "c1".into(),
            status: VerdictStatus::Contradicted,
            confidence: 0.9,
            summary: "nope".into(),
            caveats_json: json!(["c"]),
            evidence_ids_json: json!(["e"]),
            suggested_action: None,
            created_at: 2,
        };
        repo::insert_verdict(&conn, &v).unwrap();

        let got = repo::get_verdict_for_check(&conn, "c1").unwrap().unwrap();
        assert_eq!(got.status, VerdictStatus::Contradicted);
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = open_in_memory().unwrap();
        migrate::run(&conn).unwrap();
        migrate::run(&conn).unwrap();
    }
}
