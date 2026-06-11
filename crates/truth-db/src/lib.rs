//! `truth-db` — SQLite persistence (rusqlite) and migrations.

pub mod migrate;
pub mod repo;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

/// Version of the **index format** — the extraction/evidence logic, NOT the
/// crate semver. Bump this whenever a release changes how code is extracted into
/// evidence (new claim types, sharper relevance gates, AST changes, ...), so an
/// index built by an older binary is detected as stale and rebuilt. Stored in
/// SQLite's `PRAGMA user_version`.
pub const INDEX_FORMAT_VERSION: i64 = 4;

/// Read the index-format version stamped on this DB (0 if never stamped).
pub fn index_format_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Stamp the current `INDEX_FORMAT_VERSION` onto the DB (call after a full index).
pub fn set_index_format_version(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "user_version", INDEX_FORMAT_VERSION)?;
    Ok(())
}

/// True when the DB was built by a binary with a different index format than the
/// running one — its evidence may be extracted by stale logic and should be
/// fully rebuilt, not incrementally patched.
pub fn index_format_is_stale(conn: &Connection) -> Result<bool> {
    let stored = index_format_version(conn)?;
    // 0 = legacy/never-stamped index → treat as stale so it gets rebuilt once.
    Ok(stored != INDEX_FORMAT_VERSION)
}

/// Open (creating parent dirs if needed), enable foreign keys, and migrate.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db dir {}", parent.display()))?;
        }
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening sqlite at {}", path.display()))?;
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
        assert_eq!(count, 2);

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
