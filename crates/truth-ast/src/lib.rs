//! `truth-ast` — tree-sitter-based extraction (Rust, TypeScript, Python, Go).
//!
//! Structural extraction that regex-over-lines cannot do: routes matched by AST
//! shape (a method call / decorator with a string-literal `/path` argument),
//! numeric constants carrying name + value, and real symbol definitions. See
//! `docs/TREESITTER_SPIKE.md`.

mod go;
mod python;
mod typescript;

use anyhow::{Context, Result};
use tree_sitter::{Parser, Query, QueryCursor};

/// Languages with AST extraction support. Each maps to a tree-sitter grammar
/// and a per-language extractor producing the same `AstFact` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Rust,
    /// `.ts` and plain JS (`.js`/`.mjs`/`.cjs`) — the TypeScript grammar is a
    /// superset of JS, so one grammar covers both.
    TypeScript,
    /// `.tsx`/`.jsx` — JSX needs the dedicated TSX grammar variant.
    Tsx,
    Python,
    Go,
}

impl Lang {
    /// Map a file extension (lowercase, no dot) to its AST language, if
    /// supported. This is the indexer's "is this file AST-supported?" gate.
    pub fn from_extension(ext: &str) -> Option<Lang> {
        match ext {
            "rs" => Some(Lang::Rust),
            "ts" | "js" | "mjs" | "cjs" => Some(Lang::TypeScript),
            "tsx" | "jsx" => Some(Lang::Tsx),
            "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }
}

/// Extract route + constant + symbol facts from source in any supported language.
pub fn extract(lang: Lang, source: &str) -> Result<Vec<AstFact>> {
    match lang {
        Lang::Rust => extract_rust(source),
        Lang::TypeScript => typescript::extract(source, false),
        Lang::Tsx => typescript::extract(source, true),
        Lang::Python => python::extract(source),
        Lang::Go => go::extract(source),
    }
}

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
    /// For routes: the registration method (`route`, `post`, ...).
    pub route_builder: Option<String>,
    /// For routes: the handler identifier, if recoverable.
    pub handler: Option<String>,
}

/// A minimal value type (avoids a serde_json dep in this crate).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
}

/// Route-registration method names whose string-literal arg is a route.
const ROUTE_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "route", "at", "mount", "nest",
    "service", "handle", "resource",
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
    extract_functions(&root, bytes, &mut out)?;
    Ok(out)
}

pub(crate) fn line_of(node: tree_sitter::Node) -> u32 {
    (node.start_position().row + 1) as u32
}

pub(crate) fn node_text<'a>(node: tree_sitter::Node, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

/// Recover a handler name from a route's 2nd argument: an identifier directly
/// (`handler`), or the inner identifier of a wrapper call like `get(checkout)`
/// / `web::get().to(handler)` (best-effort: the first identifier-like token).
fn handler_ident(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "scoped_identifier" => Some(node_text(node, bytes).to_string()),
        "call_expression" => {
            // get(checkout) -> the first identifier in the arguments.
            let args = node.child_by_field_name("arguments")?;
            named_children(args)
                .into_iter()
                .find_map(|c| handler_ident(c, bytes))
        }
        _ => named_children(node)
            .into_iter()
            .find_map(|c| handler_ident(c, bytes)),
    }
}

/// Collect a node's named children into a Vec (avoids cursor-lifetime issues).
pub(crate) fn named_children(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
    let mut w = node.walk();
    node.named_children(&mut w).collect()
}

/// Routes: a method call `recv.METHOD("…/path…", …)` where METHOD is a known
/// route-registration name and the first argument is a string literal starting
/// with `/`. Structurally precise — no file paths / derivation paths / format
/// strings sneaking in.
fn extract_routes(root: &tree_sitter::Node, bytes: &[u8], out: &mut Vec<AstFact>) -> Result<()> {
    // Match the whole arguments node so we can read the route literal AND the
    // handler argument (the 2nd arg).
    let q = Query::new(
        &tree_sitter_rust::language(),
        r#"
        (call_expression
          function: (field_expression field: (field_identifier) @method)
          arguments: (arguments) @args)
        "#,
    )
    .context("compiling route query")?;
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

        // First argument must be a string literal that is a route path.
        let mut walker = args.walk();
        let arg_nodes: Vec<_> = args.named_children(&mut walker).collect();
        let Some(first) = arg_nodes.first() else {
            continue;
        };
        if first.kind() != "string_literal" {
            continue;
        }
        let lit = node_text(*first, bytes);
        let path = lit.trim_matches('"');
        if !(path.starts_with('/') && path.len() > 1) {
            continue;
        }

        // Handler: the 2nd argument's leading identifier (e.g. `get(checkout)`
        // -> "checkout"; bare `handler` -> "handler").
        let handler = arg_nodes.get(1).and_then(|n| handler_ident(*n, bytes));

        out.push(AstFact {
            subject: path.to_string(),
            predicate: "route_exists".into(),
            value: Value::Bool(true),
            line: line_of(*first),
            text: format!(".{method_name}({lit})"),
            route_builder: Some(method_name.to_string()),
            handler,
        });
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
        let (Some(name_n), Some(val_n)) = (name_n, val_n) else {
            continue;
        };

        let name = node_text(name_n, bytes);
        let raw = node_text(val_n, bytes).replace('_', "");
        let Some(value) = parse_int_lit(&raw) else {
            continue;
        };

        let predicate = predicate_for(name);
        out.push(AstFact {
            subject: name.to_string(),
            predicate,
            value: Value::Int(value),
            line: line_of(name_n),
            text: format!("const {name} = {value}"),
            route_builder: None,
            handler: None,
        });
    }
    Ok(())
}

/// Functions/methods as `symbol_exists` facts. AST-precise: only real `fn`
/// definitions match — a function name in a doc comment, string, or macro arg
/// does not — which a regex "fn NAME" scan cannot guarantee. Also covers
/// struct/enum/trait/type definitions so symbol claims resolve against actual
/// declarations.
fn extract_functions(root: &tree_sitter::Node, bytes: &[u8], out: &mut Vec<AstFact>) -> Result<()> {
    let q = Query::new(
        &tree_sitter_rust::language(),
        r#"
        (function_item name: (identifier) @name)
        (struct_item name: (type_identifier) @name)
        (enum_item name: (type_identifier) @name)
        (trait_item name: (type_identifier) @name)
        (type_item name: (type_identifier) @name)
        "#,
    )
    .context("compiling symbol query")?;
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

pub(crate) fn predicate_for(name: &str) -> String {
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

pub(crate) fn parse_int_lit(raw: &str) -> Option<i64> {
    if let Some(h) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(o) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
        i64::from_str_radix(o, 8).ok()
    } else if let Some(b) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
        i64::from_str_radix(b, 2).ok()
    } else {
        raw.parse::<i64>().ok()
    }
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
    fn extracts_simple_route_and_const() {
        let src = r#"
            pub const MAX_RETRIES: u32 = 5;
            fn build(r: &mut Router) { r.post("/v1/checkout", handle); }
        "#;
        let f = extract_rust(src).unwrap();
        assert!(routes(&f).contains(&"/v1/checkout"));
        assert!(f.iter().any(|x| x.subject == "MAX_RETRIES"
            && x.predicate == "retry_count"
            && x.value == Value::Int(5)));
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
        assert!(
            routes(&f).is_empty(),
            "should extract no routes, got: {:?}",
            routes(&f)
        );
    }

    #[test]
    fn typed_and_hex_constants() {
        let src = "const BUF_MASK: u32 = 0xFF;\nstatic DEFAULT_PORT: u16 = 8080;\n";
        let f = extract_rust(src).unwrap();
        assert!(f
            .iter()
            .any(|x| x.subject == "BUF_MASK" && x.value == Value::Int(255)));
        assert!(f.iter().any(|x| x.subject == "DEFAULT_PORT"
            && x.predicate == "port"
            && x.value == Value::Int(8080)));
    }

    #[test]
    fn extracts_symbols_but_not_names_in_comments_or_strings() {
        // AST precision: only real declarations become `symbol_exists`. A name in
        // a comment or string literal must NOT — which a regex `fn NAME` can't
        // guarantee.
        let src = r#"
            // ghost_fn is only mentioned here in a comment
            pub fn real_function() {}
            struct RealStruct;
            enum RealEnum { A }
            fn uses_string() { let _ = "also_ghost is in a string"; }
        "#;
        let syms: Vec<String> = extract_rust(src)
            .unwrap()
            .iter()
            .filter(|x| x.predicate == "symbol_exists")
            .map(|x| x.subject.clone())
            .collect();
        let has = |n: &str| syms.iter().any(|s| s == n);
        assert!(has("real_function"));
        assert!(has("RealStruct"));
        assert!(has("RealEnum"));
        assert!(has("uses_string"));
        assert!(!has("ghost_fn"), "comment name leaked: {syms:?}");
        assert!(!has("also_ghost"), "string name leaked: {syms:?}");
    }
}
