//! Documentation coverage + drift detection.

use truth_cli::docs::build_report;

fn indexed_repo() -> (rusqlite::Connection, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("truth_docs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "fn reg(r:&mut R){ r.post(\"/v1/checkout\", h); r.post(\"/v1/secret\", h); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("docs/api.md"),
        "# API\nThe /v1/checkout endpoint handles payments.\nThe /v1/refund endpoint processes refunds.\n",
    )
    .unwrap();

    let conn = truth_db::open_in_memory().unwrap();
    let config = truth_core::config::Config::from_toml_str("").unwrap();
    truth_indexer::index_repo(&conn, &config.repo, Some(&dir)).unwrap();
    (conn, dir)
}

#[test]
fn classifies_documented_undocumented_and_drift() {
    let (conn, dir) = indexed_repo();

    // In code AND docs.
    assert_eq!(build_report(&conn, "/v1/checkout").unwrap().status, "documented");

    // In code, not docs.
    assert_eq!(build_report(&conn, "/v1/secret").unwrap().status, "undocumented");

    // In docs, NOT in code — the drift case.
    let drift = build_report(&conn, "/v1/refund").unwrap();
    assert_eq!(drift.status, "drift", "{drift:?}");
    assert!(drift.doc_count >= 1 && drift.code_count == 0);

    let _ = std::fs::remove_dir_all(&dir);
}
