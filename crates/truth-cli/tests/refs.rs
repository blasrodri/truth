//! Reference finder: dead-code and unused-dependency detection.

use truth_cli::refs::{build_report, scan_references};

#[test]
fn word_boundary_scan_excludes_substrings_and_def_site() {
    // `port` must not match `support`. Lines: 1=support, 2=def, 3=use.
    let f = tmp(
        "scan",
        "let support = 1;\nlet port = 8080;\nuse_of(port);\n",
    );
    let (count, samples, scanned) =
        scan_references(std::slice::from_ref(&f), "port", None, None, 5);
    assert_eq!(scanned, 1);
    assert_eq!(
        count, 2,
        "def + use, support excluded; samples: {samples:?}"
    );

    // Excluding the definition site (line 2) leaves just the real use.
    let (count_excl, _, _) = scan_references(&[f], "port", Some(&tmp_path("scan")), Some(2), 5);
    assert_eq!(count_excl, 1);
}

#[test]
fn detects_dead_code_and_unused_dependency() {
    let dir = std::env::temp_dir().join(format!("truth_refs_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub const MAX_RETRIES: u32 = 5;\npub const DEAD_TIMEOUT: u32 = 9;\nfn r(){ for _ in 0..MAX_RETRIES {} }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname=\"x\"\n[dependencies]\nredis = \"0\"\nserde = \"1\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/db.rs"), "use redis::Client;\n").unwrap();

    let conn = truth_db::open_in_memory().unwrap();
    let config = truth_core::config::Config::from_toml_str("").unwrap();
    truth_indexer::index_repo(&conn, &config.repo, Some(&dir)).unwrap();

    // MAX_RETRIES: referenced (def line excluded).
    let used = build_report(&conn, "MAX_RETRIES").unwrap();
    assert_eq!(used.status, "referenced", "{used:?}");

    // DEAD_TIMEOUT: defined, never referenced.
    let dead = build_report(&conn, "DEAD_TIMEOUT").unwrap();
    assert_eq!(dead.status, "definition_only", "{dead:?}");

    // redis: used via `use redis::Client`.
    let redis = build_report(&conn, "redis").unwrap();
    assert_eq!(redis.status, "referenced");

    // serde: in Cargo.toml but never imported — unused dependency.
    let serde = build_report(&conn, "serde").unwrap();
    assert_eq!(
        serde.status, "definition_only",
        "unused dep should be definition_only: {serde:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn tmp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("truth_refs_{name}_{}.rs", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn tmp(name: &str, contents: &str) -> String {
    let p = tmp_path(name);
    std::fs::write(&p, contents).unwrap();
    p
}
