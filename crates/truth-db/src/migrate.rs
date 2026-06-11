//! Embedded migration runner. Migrations are applied in order; applied versions
//! are tracked in `schema_migrations`.

use anyhow::Result;
use rusqlite::Connection;

/// (version, name, sql). Add new tuples here as migrations are introduced.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "init", include_str!("../../../migrations/0001_init.sql")),
    (2, "runs", include_str!("../../../migrations/0002_runs.sql")),
];

pub fn run(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let applied: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    for (version, name, sql) in MIGRATIONS {
        if *version <= applied {
            continue;
        }
        conn.execute_batch(sql)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![version, name, truth_core::now_secs()],
        )?;
    }
    Ok(())
}
