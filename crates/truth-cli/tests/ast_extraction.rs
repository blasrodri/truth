//! Phase 7A: AST route extraction wired through the indexer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use truth_core::config::{Config, ExtractorMode};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn index(src: &str, mode: ExtractorMode) -> (rusqlite::Connection, PathBuf) {
    // Unique per call so parallel tests never share a directory.
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "truth_ast_it_{}_{:?}_{n}",
        std::process::id(),
        mode
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), src).unwrap();

    let conn = truth_db::open_in_memory().unwrap();
    let config = Config::from_toml_str("").unwrap();
    truth_indexer::index_repo_opts(&conn, &config.repo, Some(&dir), false, mode).unwrap();
    (conn, dir)
}

/// (subject, extraction_method) for all route_exists evidence.
fn routes(conn: &rusqlite::Connection) -> Vec<(String, String)> {
    truth_db::repo::all_evidence(conn)
        .unwrap()
        .into_iter()
        .filter(|e| e.predicate.as_deref() == Some("route_exists"))
        .map(|e| {
            (
                e.subject_text.clone().unwrap_or_default(),
                e.extraction_method.as_db_str().to_string(),
            )
        })
        .collect()
}

const REAL_ROUTE: &str = r#"
use axum::{Router, routing::get};
fn router() -> Router {
    Router::new().route("/v1/checkout", get(checkout))
}
async fn checkout() {}
"#;

#[test]
fn ast_extracts_real_rust_route() {
    let (conn, dir) = index(REAL_ROUTE, ExtractorMode::Ast);
    let r = routes(&conn);
    assert!(
        r.iter().any(|(s, m)| s == "/v1/checkout" && m == "ast"),
        "routes: {r:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ast_ignores_assert_eq_string() {
    let src = r#"
#[test]
fn test_route() {
    assert_eq!("/v1/checkout", route);
}
"#;
    let (conn, dir) = index(src, ExtractorMode::Ast);
    let r = routes(&conn);
    assert!(
        r.is_empty(),
        "AST should extract no routes from assert_eq, got: {r:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_mode_prefers_ast_no_duplicate() {
    // A file with a real route + a noise string regex would falsely match.
    let src = r#"
fn router(r: &mut Router) {
    r.post("/v1/checkout", handle);
}
fn noise() { assert_eq!("/v1/should-not-count", x); }
"#;
    let (conn, dir) = index(src, ExtractorMode::Mixed);
    let r = routes(&conn);
    // Exactly one route, from AST, no duplicate, no noise.
    assert_eq!(r.len(), 1, "expected 1 route, got: {r:?}");
    assert_eq!(r[0], ("/v1/checkout".to_string(), "ast".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn regex_mode_unchanged_behavior() {
    // Regex mode keeps its routes tagged as regex (and would catch the noise the
    // precision gate lets through).
    let (conn, dir) = index(REAL_ROUTE, ExtractorMode::Regex);
    let r = routes(&conn);
    assert!(
        r.iter().any(|(s, m)| s == "/v1/checkout" && m == "regex"),
        "routes: {r:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ast_route_carries_handler_metadata() {
    let (conn, dir) = index(REAL_ROUTE, ExtractorMode::Ast);
    let ev = truth_db::repo::all_evidence(&conn)
        .unwrap()
        .into_iter()
        .find(|e| e.subject_text.as_deref() == Some("/v1/checkout"))
        .expect("route evidence");
    assert_eq!(
        ev.metadata_json.get("extraction").and_then(|v| v.as_str()),
        Some("ast")
    );
    assert_eq!(
        ev.metadata_json.get("handler").and_then(|v| v.as_str()),
        Some("checkout")
    );
    assert_eq!(
        ev.metadata_json
            .get("route_builder")
            .and_then(|v| v.as_str()),
        Some("route")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
