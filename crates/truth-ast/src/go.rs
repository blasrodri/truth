//! Go extraction. Routes from net/http (`mux.HandleFunc`, `http.Handle`,
//! including Go 1.22 `"GET /path"` patterns), gin (`r.GET`), chi/echo-style
//! (`r.Get`, `r.Mount`, `r.Route`) registration calls.

use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::{line_of, named_children, node_text, parse_int_lit, predicate_for, AstFact, Value};

/// Route-registration method names. Mixed-case covers net/http (HandleFunc),
/// gin (GET/POST), and chi/echo (Get/Post/Mount/Route).
const ROUTE_METHODS: &[&str] = &[
    "HandleFunc",
    "Handle",
    "GET",
    "POST",
    "PUT",
    "PATCH",
    "DELETE",
    "HEAD",
    "OPTIONS",
    "Any",
    "Get",
    "Post",
    "Put",
    "Patch",
    "Delete",
    "Head",
    "Options",
    "Group",
    "Mount",
    "Route",
];

/// HTTP verbs allowed as a Go 1.22 ServeMux pattern prefix (`"GET /items"`).
const HTTP_VERBS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

fn grammar() -> Language {
    tree_sitter_go::language()
}

/// Extract route + constant + symbol facts from Go source.
pub(crate) fn extract(source: &str) -> Result<Vec<AstFact>> {
    let lang = grammar();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .context("loading tree-sitter-go grammar")?;
    let tree = parser.parse(source, None).context("parsing go source")?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut out = Vec::new();
    extract_routes(&lang, &root, bytes, &mut out)?;
    extract_consts(&lang, &root, bytes, &mut out)?;
    extract_symbols(&lang, &root, bytes, &mut out)?;
    Ok(out)
}

/// The text of a Go string literal node without quotes/backticks.
fn string_lit_text<'a>(node: tree_sitter::Node, bytes: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "interpreted_string_literal" => Some(node_text(node, bytes).trim_matches('"')),
        "raw_string_literal" => Some(node_text(node, bytes).trim_matches('`')),
        _ => None,
    }
}

/// Resolve a route pattern to its path: either a plain `/path`, or a Go 1.22
/// ServeMux pattern `"VERB /path"` whose path portion we keep.
fn route_path(pattern: &str) -> Option<&str> {
    if pattern.starts_with('/') && pattern.len() > 1 {
        return Some(pattern);
    }
    let (verb, rest) = pattern.split_once(' ')?;
    if HTTP_VERBS.contains(&verb) && rest.starts_with('/') {
        return Some(rest);
    }
    None
}

/// Best-effort handler name: a bare identifier (`handleUsers`), a selector
/// (`api.ListUsers`), or the callee of a wrapper call (`promhttp.Handler()`).
/// Anonymous `func(...) {...}` literals yield no handler.
fn handler_ident(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "selector_expression" => Some(node_text(node, bytes).to_string()),
        "func_literal" => None,
        "call_expression" => node
            .child_by_field_name("function")
            .and_then(|f| handler_ident(f, bytes)),
        _ => named_children(node)
            .into_iter()
            .find_map(|c| handler_ident(c, bytes)),
    }
}

/// Routes: `recv.METHOD("…/path…", handler)` with a known registration method.
/// All methods require a 2nd argument (the handler / sub-router) except
/// `Group`, which legitimately takes just the prefix — this keeps 1-argument
/// lookups like `cache.Get("/key")` out structurally.
fn extract_routes(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (call_expression
          function: (selector_expression field: (field_identifier) @method)
          arguments: (argument_list) @args)
        "#,
    )
    .context("compiling go route query")?;
    let mi = q.capture_index_for_name("method").unwrap();
    let ai = q.capture_index_for_name("args").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let method = m.captures.iter().find(|c| c.index == mi).map(|c| c.node);
        let args = m.captures.iter().find(|c| c.index == ai).map(|c| c.node);
        let (Some(method), Some(args)) = (method, args) else {
            continue;
        };

        let method_name = node_text(method, bytes);
        if !ROUTE_METHODS.contains(&method_name) {
            continue;
        }

        let arg_nodes = named_children(args);
        let Some(first) = arg_nodes.first() else {
            continue;
        };
        let Some(pattern) = string_lit_text(*first, bytes) else {
            continue;
        };
        let Some(path) = route_path(pattern) else {
            continue;
        };
        if method_name != "Group" && arg_nodes.len() < 2 {
            continue;
        }

        let handler = arg_nodes.get(1).and_then(|n| handler_ident(*n, bytes));
        out.push(AstFact {
            subject: path.to_string(),
            predicate: "route_exists".into(),
            value: Value::Bool(true),
            line: line_of(*first),
            text: format!(".{method_name}(\"{pattern}\")"),
            route_builder: Some(method_name.to_string()),
            handler,
        });
    }
    Ok(())
}

/// Numeric constants: top-level `const Name = N`, including grouped
/// `const ( … )` blocks (each spec is a `const_spec` under the declaration).
/// `iota` and expression values are skipped — only literal ints are facts.
fn extract_consts(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (const_declaration
          (const_spec
            name: (identifier) @name
            value: (expression_list (int_literal) @val))) @decl
        "#,
    )
    .context("compiling go const query")?;
    let ni = q.capture_index_for_name("name").unwrap();
    let vi = q.capture_index_for_name("val").unwrap();
    let di = q.capture_index_for_name("decl").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let name_n = m.captures.iter().find(|c| c.index == ni).map(|c| c.node);
        let val_n = m.captures.iter().find(|c| c.index == vi).map(|c| c.node);
        let decl_n = m.captures.iter().find(|c| c.index == di).map(|c| c.node);
        let (Some(name_n), Some(val_n), Some(decl_n)) = (name_n, val_n, decl_n) else {
            continue;
        };
        // Top-level (package) constants only — function-local consts are
        // implementation detail, not configured values.
        if decl_n.parent().map(|p| p.kind()) != Some("source_file") {
            continue;
        }

        let name = node_text(name_n, bytes);
        let raw = node_text(val_n, bytes).replace('_', "");
        let Some(value) = parse_int_lit(&raw) else {
            continue;
        };
        out.push(AstFact {
            subject: name.to_string(),
            predicate: predicate_for(name),
            value: Value::Int(value),
            line: line_of(name_n),
            text: format!("const {name} = {value}"),
            route_builder: None,
            handler: None,
        });
    }
    Ok(())
}

/// Symbols: functions, methods, and type declarations (struct/interface/alias
/// all live under `type_spec`).
fn extract_symbols(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (function_declaration name: (identifier) @name)
        (method_declaration name: (field_identifier) @name)
        (type_spec name: (type_identifier) @name)
        "#,
    )
    .context("compiling go symbol query")?;
    let ni = q.capture_index_for_name("name").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let Some(name_n) = m.captures.iter().find(|c| c.index == ni).map(|c| c.node) else {
            continue;
        };
        let name = node_text(name_n, bytes);
        out.push(AstFact {
            subject: name.to_string(),
            predicate: "symbol_exists".into(),
            value: Value::Bool(true),
            line: line_of(name_n),
            text: format!("definition of `{name}`"),
            route_builder: None,
            handler: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes(facts: &[AstFact]) -> Vec<&str> {
        facts
            .iter()
            .filter(|f| f.predicate == "route_exists")
            .map(|f| f.subject.as_str())
            .collect()
    }

    #[test]
    fn go_extracts_stdlib_gin_and_chi_routes() {
        let src = r#"
package main

func routes() {
    mux.HandleFunc("/healthz", healthz)
    http.Handle("/metrics", promhttp.Handler())
    r.GET("/users", api.ListUsers)
    r.Mount("/admin", adminRouter)
}
"#;
        let f = extract(src).unwrap();
        let r = routes(&f);
        assert!(r.contains(&"/healthz"), "routes: {r:?}");
        assert!(r.contains(&"/metrics"), "routes: {r:?}");
        assert!(r.contains(&"/users"), "routes: {r:?}");
        assert!(r.contains(&"/admin"), "routes: {r:?}");
        let users = f.iter().find(|x| x.subject == "/users").unwrap();
        assert_eq!(users.handler.as_deref(), Some("api.ListUsers"));
        assert_eq!(users.route_builder.as_deref(), Some("GET"));
        let metrics = f.iter().find(|x| x.subject == "/metrics").unwrap();
        assert_eq!(metrics.handler.as_deref(), Some("promhttp.Handler"));
    }

    #[test]
    fn go_122_method_pattern() {
        // Go 1.22 ServeMux patterns embed the verb in the string.
        let src = r#"
package main

func routes() {
    mux.HandleFunc("GET /items/{id}", getItem)
}
"#;
        let f = extract(src).unwrap();
        assert!(routes(&f).contains(&"/items/{id}"), "facts: {f:?}");
    }

    #[test]
    fn go_rejects_non_route_strings() {
        // File operations, 1-argument lookups, and assignments are not routes.
        let src = r#"
package main

func notRoutes() {
    f, _ := os.Open("/dev/null")
    v := cache.Get("/key/lookup")
    p := "/etc/passwd"
    _ = filepath.Join("/usr", "bin")
}
"#;
        let f = extract(src).unwrap();
        assert!(routes(&f).is_empty(), "got: {:?}", routes(&f));
    }

    #[test]
    fn go_const_blocks_and_singles() {
        let src = r#"
package main

const MaxRetries = 5

const (
    DefaultPort = 8080
    BufMask     = 0xFF
    Mode        = iota
)

func f() {
    const local = 3
}
"#;
        let f = extract(src).unwrap();
        assert!(f.iter().any(|x| x.subject == "MaxRetries"
            && x.predicate == "retry_count"
            && x.value == Value::Int(5)));
        assert!(f.iter().any(|x| x.subject == "DefaultPort"
            && x.predicate == "port"
            && x.value == Value::Int(8080)));
        assert!(f
            .iter()
            .any(|x| x.subject == "BufMask" && x.value == Value::Int(255)));
        assert!(
            !f.iter().any(|x| x.subject == "Mode"),
            "iota constant leaked"
        );
        assert!(
            !f.iter().any(|x| x.subject == "local"),
            "function-local const leaked"
        );
    }

    #[test]
    fn go_symbols_funcs_methods_types() {
        let src = r#"
package main

// ghost_fn only exists in this comment
func RealFunction() {}

func (s *Server) Handle() {}

type User struct{ Name string }
type Reader interface{ Read() }

func uses() string { return "also_ghost in a string" }
"#;
        let syms: Vec<String> = extract(src)
            .unwrap()
            .iter()
            .filter(|x| x.predicate == "symbol_exists")
            .map(|x| x.subject.clone())
            .collect();
        let has = |n: &str| syms.iter().any(|s| s == n);
        assert!(has("RealFunction"));
        assert!(has("Handle"));
        assert!(has("User"));
        assert!(has("Reader"));
        assert!(!has("ghost_fn"), "comment name leaked: {syms:?}");
        assert!(!has("also_ghost"), "string name leaked: {syms:?}");
    }
}
