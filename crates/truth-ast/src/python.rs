//! Python extraction. Routes come from decorators only (`@app.route("/x")`,
//! `@router.get("/x")` — Flask/FastAPI style); a bare call like `d.get("/k")`
//! is never a route, which kills the dict-lookup false-positive class outright.

use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::{line_of, named_children, node_text, parse_int_lit, predicate_for, AstFact, Value};

/// Decorator method names that register a route (Flask `route`, FastAPI verbs).
const ROUTE_METHODS: &[&str] = &[
    "route",
    "get",
    "post",
    "put",
    "patch",
    "delete",
    "head",
    "options",
    "websocket",
];

fn grammar() -> Language {
    tree_sitter_python::language()
}

/// Extract route + constant + symbol facts from Python source.
pub(crate) fn extract(source: &str) -> Result<Vec<AstFact>> {
    let lang = grammar();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .context("loading tree-sitter-python grammar")?;
    let tree = parser
        .parse(source, None)
        .context("parsing python source")?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut out = Vec::new();
    extract_routes(&lang, &root, bytes, &mut out)?;
    extract_consts(&lang, &root, bytes, &mut out)?;
    extract_symbols(&lang, &root, bytes, &mut out)?;
    Ok(out)
}

/// The content of a plain Python string literal. `None` for f-strings (any
/// `interpolation` child) — those are not literal paths.
fn string_lit_text(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let children = named_children(node);
    if children.iter().any(|c| c.kind() == "interpolation") {
        return None;
    }
    // The grammar splits strings into string_start / string_content / string_end;
    // concatenate the content parts (usually exactly one).
    let content: String = children
        .iter()
        .filter(|c| c.kind() == "string_content")
        .map(|c| node_text(*c, bytes))
        .collect();
    Some(content)
}

/// Routes: a decorator whose call is `recv.METHOD("/path", …)` with METHOD a
/// known route-registration name. The handler is the decorated `def`'s name.
fn extract_routes(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (decorator
          (call
            function: (attribute attribute: (identifier) @method)
            arguments: (argument_list) @args)) @dec
        "#,
    )
    .context("compiling python route query")?;
    let mi = q.capture_index_for_name("method").unwrap();
    let ai = q.capture_index_for_name("args").unwrap();
    let di = q.capture_index_for_name("dec").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let method = m.captures.iter().find(|c| c.index == mi).map(|c| c.node);
        let args = m.captures.iter().find(|c| c.index == ai).map(|c| c.node);
        let dec = m.captures.iter().find(|c| c.index == di).map(|c| c.node);
        let (Some(method), Some(args), Some(dec)) = (method, args, dec) else {
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
        let Some(path) = string_lit_text(*first, bytes) else {
            continue;
        };
        if !(path.starts_with('/') && path.len() > 1) {
            continue;
        }

        // Handler: the decorated function's name (the decorator's parent is a
        // decorated_definition wrapping the def).
        let handler = dec.parent().and_then(|dd| {
            named_children(dd).into_iter().find_map(|c| {
                if matches!(c.kind(), "function_definition" | "class_definition") {
                    c.child_by_field_name("name")
                        .map(|n| node_text(n, bytes).to_string())
                } else {
                    None
                }
            })
        });

        out.push(AstFact {
            subject: path.clone(),
            predicate: "route_exists".into(),
            value: Value::Bool(true),
            line: line_of(*first),
            text: format!("@.{method_name}(\"{path}\")"),
            route_builder: Some(method_name.to_string()),
            handler,
        });
    }
    Ok(())
}

/// Numeric constants: module-level `NAME = N` assignments, UPPER_SNAKE names
/// only — Python has no `const`, so the naming convention is the constness
/// signal. Anything lowercase is a variable, not a configured constant.
fn extract_consts(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    // Anchoring under (module …) restricts matches to top level structurally.
    let q = Query::new(
        lang,
        r#"
        (module
          (expression_statement
            (assignment left: (identifier) @name right: (integer) @val)))
        "#,
    )
    .context("compiling python const query")?;
    let ni = q.capture_index_for_name("name").unwrap();
    let vi = q.capture_index_for_name("val").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let name_n = m.captures.iter().find(|c| c.index == ni).map(|c| c.node);
        let val_n = m.captures.iter().find(|c| c.index == vi).map(|c| c.node);
        let (Some(name_n), Some(val_n)) = (name_n, val_n) else {
            continue;
        };

        let name = node_text(name_n, bytes);
        if !is_upper_snake(name) {
            continue;
        }
        let raw = node_text(val_n, bytes).replace('_', "");
        let Some(value) = parse_int_lit(&raw) else {
            continue;
        };
        out.push(AstFact {
            subject: name.to_string(),
            predicate: predicate_for(name),
            value: Value::Int(value),
            line: line_of(name_n),
            text: format!("{name} = {value}"),
            route_builder: None,
            handler: None,
        });
    }
    Ok(())
}

/// UPPER_SNAKE_CASE with at least one letter (rejects `_`, `__`, `A1b`).
fn is_upper_snake(name: &str) -> bool {
    name.chars().any(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Symbols: every `def` and `class` definition (methods included, mirroring the
/// Rust extractor which also captures impl methods).
fn extract_symbols(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (function_definition name: (identifier) @name)
        (class_definition name: (identifier) @name)
        "#,
    )
    .context("compiling python symbol query")?;
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
    fn py_extracts_flask_and_fastapi_decorators() {
        let src = r#"
@app.route("/login", methods=["POST"])
def login():
    pass

@router.get("/items/{item_id}")
async def read_item(item_id: int):
    pass
"#;
        let f = extract(src).unwrap();
        let r = routes(&f);
        assert!(r.contains(&"/login"), "routes: {r:?}");
        assert!(r.contains(&"/items/{item_id}"), "routes: {r:?}");
        let login = f.iter().find(|x| x.subject == "/login").unwrap();
        assert_eq!(login.handler.as_deref(), Some("login"));
        assert_eq!(login.route_builder.as_deref(), Some("route"));
    }

    #[test]
    fn py_rejects_plain_calls_and_fstrings() {
        // dict.get, open(), assignments, and f-string decorators are not routes.
        let src = r#"
f = open("/dev/null")
v = d.get("/key/lookup")
path = "/etc/passwd"

@app.get(f"/users/{prefix}")
def dynamic():
    pass
"#;
        let f = extract(src).unwrap();
        assert!(routes(&f).is_empty(), "got: {:?}", routes(&f));
    }

    #[test]
    fn py_module_level_upper_snake_consts_only() {
        let src = r#"
MAX_RETRIES = 5
DEFAULT_PORT = 8080
request_count = 3

def f():
    LOCAL_TIMEOUT = 30
"#;
        let f = extract(src).unwrap();
        assert!(f.iter().any(|x| x.subject == "MAX_RETRIES"
            && x.predicate == "retry_count"
            && x.value == Value::Int(5)));
        assert!(f.iter().any(|x| x.subject == "DEFAULT_PORT"
            && x.predicate == "port"
            && x.value == Value::Int(8080)));
        assert!(
            !f.iter().any(|x| x.subject == "request_count"),
            "lowercase variable leaked"
        );
        assert!(
            !f.iter().any(|x| x.subject == "LOCAL_TIMEOUT"),
            "function-local assignment leaked"
        );
    }

    #[test]
    fn py_symbols_not_comments_or_strings() {
        let src = r#"
# ghost_fn is only mentioned in this comment
def real_function():
    return "also_ghost is in a string"

class RealClass:
    def method(self):
        pass
"#;
        let syms: Vec<String> = extract(src)
            .unwrap()
            .iter()
            .filter(|x| x.predicate == "symbol_exists")
            .map(|x| x.subject.clone())
            .collect();
        let has = |n: &str| syms.iter().any(|s| s == n);
        assert!(has("real_function"));
        assert!(has("RealClass"));
        assert!(has("method"));
        assert!(!has("ghost_fn"), "comment name leaked: {syms:?}");
        assert!(!has("also_ghost"), "string name leaked: {syms:?}");
    }
}
