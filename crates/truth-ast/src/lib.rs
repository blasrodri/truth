//! `truth-ast` — tree-sitter-based extraction (spike: Rust only).
//!
//! Structural extraction that regex-over-lines cannot do: routes matched by AST
//! shape (a method call with a string-literal `/path` argument), and numeric
//! constants carrying name + value + type + visibility. See
//! `docs/TREESITTER_SPIKE.md`.

use anyhow::{Context, Result};
use tree_sitter::{Parser, Query, QueryCursor};

/// A structurally-extracted fact, intentionally shaped like the indexer's
/// `Extracted` so it can plug into the existing pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AstFact {
    /// Subject: the route path, or the constant name.
    pub subject: String,
    /// Predicate: "route_exists", "retry_count", "timeout", "port", or the
    /// uppercased const name for a generic numeric constant.
    pub predicate: String,
    /// Bool for existence, number for constants/ports.
    pub value: Value,
    /// 1-based line number.
    pub line: u32,
    /// The matched source text (trimmed).
    pub text: String,
}

/// A minimal value type (avoids a serde_json dep in this crate).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
}

/// Route-registration method names whose string-literal arg is a route.
const ROUTE_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "route", "at",
    "mount", "nest", "service", "handle", "resource",
];

/// Extract route + constant facts from Rust source via tree-sitter.
pub fn extract_rust(source: &str) -> Result<Vec<AstFact>> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::language())
        .context("loading tree-sitter-rust grammar")?;
    let tree = parser.parse(source, None).context("parsing rust source")?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut out = Vec::new();
    extract_routes(&root, bytes, &mut out)?;
    extract_consts(&root, bytes, &mut out)?;
    Ok(out)
}

fn line_of(node: tree_sitter::Node) -> u32 {
    (node.start_position().row + 1) as u32
}

fn node_text<'a>(node: tree_sitter::Node, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

/// Routes: a method call `recv.METHOD("…/path…", …)` where METHOD is a known
/// route-registration name and the first argument is a string literal starting
/// with `/`. Structurally precise — no file paths / derivation paths / format
/// strings sneaking in.
fn extract_routes(root: &tree_sitter::Node, bytes: &[u8], out: &mut Vec<AstFact>) -> Result<()> {
    // (call_expression function:(field_expression field:(field_identifier)@m)
    //   arguments:(arguments (string_literal)@s))
    let q = Query::new(
        &tree_sitter_rust::language(),
        r#"
        (call_expression
          function: (field_expression field: (field_identifier) @method)
          arguments: (arguments (string_literal) @arg))
        "#,
    )
    .context("compiling route query")?;
    let mi = q.capture_index_for_name("method").unwrap();
    let ai = q.capture_index_for_name("arg").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let method = m.captures.iter().find(|c| c.index == mi).map(|c| c.node);
        let arg = m.captures.iter().find(|c| c.index == ai).map(|c| c.node);
        let (Some(method), Some(arg)) = (method, arg) else { continue };

        let method_name = node_text(method, bytes);
        if !ROUTE_METHODS.contains(&method_name) {
            continue;
        }
        let lit = node_text(arg, bytes);
        let path = lit.trim_matches('"');
        if path.starts_with('/') && path.len() > 1 {
            out.push(AstFact {
                subject: path.to_string(),
                predicate: "route_exists".into(),
                value: Value::Bool(true),
                line: line_of(arg),
                text: format!(".{method_name}({lit})"),
            });
        }
    }
    Ok(())
}

/// Numeric constants: `const NAME: TYPE = N;` and `static NAME: TYPE = N;`.
/// Captures the name, integer value, and routes retry/timeout/port by keyword.
fn extract_consts(root: &tree_sitter::Node, bytes: &[u8], out: &mut Vec<AstFact>) -> Result<()> {
    let q = Query::new(
        &tree_sitter_rust::language(),
        r#"
        (const_item name: (identifier) @name value: (integer_literal) @val)
        (static_item name: (identifier) @name value: (integer_literal) @val)
        "#,
    )
    .context("compiling const query")?;
    let ni = q.capture_index_for_name("name").unwrap();
    let vi = q.capture_index_for_name("val").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let name_n = m.captures.iter().find(|c| c.index == ni).map(|c| c.node);
        let val_n = m.captures.iter().find(|c| c.index == vi).map(|c| c.node);
        let (Some(name_n), Some(val_n)) = (name_n, val_n) else { continue };

        let name = node_text(name_n, bytes);
        let raw = node_text(val_n, bytes).replace('_', "");
        let Some(value) = parse_int_lit(&raw) else { continue };

        let predicate = predicate_for(name);
        out.push(AstFact {
            subject: name.to_string(),
            predicate,
            value: Value::Int(value),
            line: line_of(name_n),
            text: format!("const {name} = {value}"),
        });
    }
    Ok(())
}

fn predicate_for(name: &str) -> String {
    let l = name.to_ascii_lowercase();
    if l.contains("retr") {
        "retry_count".into()
    } else if l.contains("timeout") {
        "timeout".into()
    } else if l.contains("port") {
        "port".into()
    } else {
        name.to_uppercase()
    }
}

fn parse_int_lit(raw: &str) -> Option<i64> {
    if let Some(h) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else {
        raw.parse::<i64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes(facts: &[AstFact]) -> Vec<&str> {
        facts.iter().filter(|f| f.predicate == "route_exists").map(|f| f.subject.as_str()).collect()
    }

    #[test]
    fn extracts_simple_route_and_const() {
        let src = r#"
            pub const MAX_RETRIES: u32 = 5;
            fn build(r: &mut Router) { r.post("/v1/checkout", handle); }
        "#;
        let f = extract_rust(src).unwrap();
        assert!(routes(&f).contains(&"/v1/checkout"));
        assert!(f.iter().any(|x| x.subject == "MAX_RETRIES" && x.predicate == "retry_count" && x.value == Value::Int(5)));
    }

    #[test]
    fn handles_multiline_call_that_regex_misses() {
        // The route literal is on a different line from the method — a line-based
        // regex with method-signal gating would miss this.
        let src = r#"
            router
                .post(
                    "/v1/checkout",
                    handle_checkout,
                );
        "#;
        let f = extract_rust(src).unwrap();
        assert!(routes(&f).contains(&"/v1/checkout"), "facts: {f:?}");
    }

    #[test]
    fn rejects_non_route_string_literals_structurally() {
        // A plain string literal that is not a route-method argument must NOT be
        // extracted (no precision gate needed — it's structural).
        let src = r#"
            fn f() {
                let p = "/etc/passwd";        // file path, not a route
                log("/0");                    // not a route method
                let path = "/v1/real";        // assignment, not a call arg
            }
        "#;
        let f = extract_rust(src).unwrap();
        assert!(routes(&f).is_empty(), "should extract no routes, got: {:?}", routes(&f));
    }

    #[test]
    fn typed_and_hex_constants() {
        let src = "const BUF_MASK: u32 = 0xFF;\nstatic DEFAULT_PORT: u16 = 8080;\n";
        let f = extract_rust(src).unwrap();
        assert!(f.iter().any(|x| x.subject == "BUF_MASK" && x.value == Value::Int(255)));
        assert!(f.iter().any(|x| x.subject == "DEFAULT_PORT" && x.predicate == "port" && x.value == Value::Int(8080)));
    }
}
