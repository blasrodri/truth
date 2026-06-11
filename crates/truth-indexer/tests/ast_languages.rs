//! AST extraction wired through the indexer for TypeScript / Python / Go
//! (mirrors the Rust coverage in `truth-cli/tests/ast_extraction.rs`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use truth_core::config::{Config, ExtractorMode};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Index a single file (named `fname`, under `src/`) with the given mode.
fn index(fname: &str, src: &str, mode: ExtractorMode) -> (rusqlite::Connection, PathBuf) {
    // Unique per call so parallel tests never share a directory.
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "truth_ast_lang_it_{}_{:?}_{n}",
        std::process::id(),
        mode
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src").join(fname), src).unwrap();

    let conn = truth_db::open_in_memory().unwrap();
    let config = Config::from_toml_str("").unwrap();
    truth_indexer::index_repo_opts(&conn, &config.repo, Some(&dir), false, mode).unwrap();
    (conn, dir)
}

/// (subject, extraction_method) for all evidence with the given predicate.
fn by_predicate(conn: &rusqlite::Connection, predicate: &str) -> Vec<(String, String)> {
    truth_db::repo::all_evidence(conn)
        .unwrap()
        .into_iter()
        .filter(|e| e.predicate.as_deref() == Some(predicate))
        .map(|e| {
            (
                e.subject_text.clone().unwrap_or_default(),
                e.extraction_method.as_db_str().to_string(),
            )
        })
        .collect()
}

#[test]
fn ast_extracts_typescript_route_and_symbol() {
    let src = r#"
export const handler = async () => {};
app.post('/v1/checkout', handler);
"#;
    let (conn, dir) = index("routes.ts", src, ExtractorMode::Ast);
    let r = by_predicate(&conn, "route_exists");
    assert!(
        r.iter().any(|(s, m)| s == "/v1/checkout" && m == "ast"),
        "routes: {r:?}"
    );
    let s = by_predicate(&conn, "symbol_exists");
    assert!(
        s.iter().any(|(n, m)| n == "handler" && m == "ast"),
        "symbols: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ast_extracts_tsx_component_symbol() {
    let src = r#"
export function Checkout() {
    return <button>Buy</button>;
}
"#;
    let (conn, dir) = index("checkout.tsx", src, ExtractorMode::Ast);
    let s = by_predicate(&conn, "symbol_exists");
    assert!(
        s.iter().any(|(n, m)| n == "Checkout" && m == "ast"),
        "symbols: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ast_extracts_python_decorator_route() {
    let src = r#"
@app.route("/login", methods=["POST"])
def login():
    pass
"#;
    let (conn, dir) = index("app.py", src, ExtractorMode::Ast);
    let r = by_predicate(&conn, "route_exists");
    assert!(
        r.iter().any(|(s, m)| s == "/login" && m == "ast"),
        "routes: {r:?}"
    );
    let s = by_predicate(&conn, "symbol_exists");
    assert!(
        s.iter().any(|(n, m)| n == "login" && m == "ast"),
        "symbols: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ast_extracts_go_route_and_type() {
    let src = r#"
package main

type Server struct{}

func routes() {
    mux.HandleFunc("/healthz", healthz)
}
"#;
    let (conn, dir) = index("main.go", src, ExtractorMode::Ast);
    let r = by_predicate(&conn, "route_exists");
    assert!(
        r.iter().any(|(s, m)| s == "/healthz" && m == "ast"),
        "routes: {r:?}"
    );
    let s = by_predicate(&conn, "symbol_exists");
    assert!(
        s.iter().any(|(n, m)| n == "Server" && m == "ast"),
        "symbols: {s:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mixed_mode_dedupes_regex_routes_for_ast_languages() {
    // One real route + one noise string the regex extractor would match: in
    // mixed mode the AST result must win, with no regex duplicate.
    let src = r#"
const fixture = "/v1/should-not-count";
app.get('/v1/users', listUsers);
"#;
    let (conn, dir) = index("routes.ts", src, ExtractorMode::Mixed);
    let r = by_predicate(&conn, "route_exists");
    assert_eq!(r.len(), 1, "expected 1 route, got: {r:?}");
    assert_eq!(r[0], ("/v1/users".to_string(), "ast".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn regex_mode_unchanged_for_new_languages() {
    // The conservative default keeps its regex routes untouched.
    let src = "app.get('/v1/users', listUsers);\n";
    let (conn, dir) = index("routes.ts", src, ExtractorMode::Regex);
    let r = by_predicate(&conn, "route_exists");
    assert!(
        r.iter().any(|(s, m)| s == "/v1/users" && m == "regex"),
        "routes: {r:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
