//! TypeScript / JavaScript / TSX extraction. One grammar pair covers all of
//! `.ts`/`.js` (the TypeScript grammar is a JS superset) and `.tsx`/`.jsx`
//! (the TSX variant, which additionally parses JSX).

use anyhow::{Context, Result};
use tree_sitter::{Language, Parser, Query, QueryCursor};

use crate::{line_of, named_children, node_text, parse_int_lit, predicate_for, AstFact, Value};

/// Route-registration method names (Express/Koa/Fastify/NestJS-router style)
/// whose first string-literal argument is a route path.
const ROUTE_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "all", "use", "route",
];

fn grammar(tsx: bool) -> Language {
    if tsx {
        tree_sitter_typescript::language_tsx()
    } else {
        tree_sitter_typescript::language_typescript()
    }
}

/// Extract route + constant + symbol facts from TS/JS source.
pub(crate) fn extract(source: &str, tsx: bool) -> Result<Vec<AstFact>> {
    let lang = grammar(tsx);
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .context("loading tree-sitter-typescript grammar")?;
    let tree = parser
        .parse(source, None)
        .context("parsing typescript source")?;
    let root = tree.root_node();
    let bytes = source.as_bytes();

    let mut out = Vec::new();
    extract_routes(&lang, &root, bytes, &mut out)?;
    extract_consts(&lang, &root, bytes, &mut out)?;
    extract_symbols(&lang, &root, bytes, &mut out)?;
    Ok(out)
}

/// The literal text of a TS `string` node without quotes, or `None` if it is
/// not a plain string (template strings with substitutions are not literal).
fn string_lit_text<'a>(node: tree_sitter::Node, bytes: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "string" => Some(node_text(node, bytes).trim_matches(|c| c == '"' || c == '\'')),
        // A template string with no `${}` substitution is still a literal path.
        "template_string" => {
            let has_subst = named_children(node)
                .iter()
                .any(|c| c.kind() == "template_substitution");
            if has_subst {
                None
            } else {
                Some(node_text(node, bytes).trim_matches('`'))
            }
        }
        _ => None,
    }
}

/// Best-effort handler name from a route call's trailing argument: a bare
/// identifier (`listUsers`), a member chain (`controller.list`), or the first
/// identifier inside a wrapper.
fn handler_ident(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "member_expression" => Some(node_text(node, bytes).to_string()),
        "arrow_function" | "function" | "function_expression" => None, // anonymous
        _ => named_children(node)
            .into_iter()
            .find_map(|c| handler_ident(c, bytes)),
    }
}

/// Routes: `recv.METHOD("/path", handler, …)`. Precision rule: HTTP-verb
/// methods (`get`, `post`, …) require a 2nd argument (the handler) so that
/// `Map.get("/key")`-style single-argument calls never count; `route` is the
/// one chaining method that legitimately takes only the path.
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
          function: (member_expression property: (property_identifier) @method)
          arguments: (arguments) @args)
        "#,
    )
    .context("compiling ts route query")?;
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
        let Some(path) = string_lit_text(*first, bytes) else {
            continue;
        };
        if !(path.starts_with('/') && path.len() > 1) {
            continue;
        }
        // Verb methods need a handler argument; `route` chains and may not.
        if method_name != "route" && arg_nodes.len() < 2 {
            continue;
        }

        let handler = arg_nodes.get(1).and_then(|n| handler_ident(*n, bytes));
        out.push(AstFact {
            subject: path.to_string(),
            predicate: "route_exists".into(),
            value: Value::Bool(true),
            line: line_of(*first),
            text: format!(".{method_name}(\"{path}\")"),
            route_builder: Some(method_name.to_string()),
            handler,
        });
    }
    Ok(())
}

/// Numeric constants: top-level `const NAME = N` / `export const NAME = N`.
/// `let`/`var` and function-local consts are deliberately excluded — only
/// module-level `const` carries the "configured value" meaning claims cite.
fn extract_consts(
    lang: &Language,
    root: &tree_sitter::Node,
    bytes: &[u8],
    out: &mut Vec<AstFact>,
) -> Result<()> {
    let q = Query::new(
        lang,
        r#"
        (lexical_declaration
          (variable_declarator name: (identifier) @name value: (number) @val)) @decl
        "#,
    )
    .context("compiling ts const query")?;
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

        // Must be `const` (not `let`) — the keyword is the declaration's first token.
        if decl_n.child(0).map(|c| c.kind()) != Some("const") {
            continue;
        }
        // Must be module-level: parent is the program, or an `export` statement
        // directly under the program.
        let Some(parent) = decl_n.parent() else {
            continue;
        };
        let top_level = parent.kind() == "program"
            || (parent.kind() == "export_statement"
                && parent.parent().map(|p| p.kind()) == Some("program"));
        if !top_level {
            continue;
        }

        let name = node_text(name_n, bytes);
        let raw = node_text(val_n, bytes).replace('_', "");
        let Some(value) = parse_int_lit(&raw) else {
            continue; // floats / bigint stay out of scope
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

/// Symbols: function/class/interface/type/enum declarations, plus consts bound
/// to a function value (`const handler = () => {}` — the dominant TS style).
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
        (generator_function_declaration name: (identifier) @name)
        (class_declaration name: (type_identifier) @name)
        (abstract_class_declaration name: (type_identifier) @name)
        (interface_declaration name: (type_identifier) @name)
        (type_alias_declaration name: (type_identifier) @name)
        (enum_declaration name: (identifier) @name)
        (variable_declarator name: (identifier) @fn_name value: (_) @fn_val)
        "#,
    )
    .context("compiling ts symbol query")?;
    let ni = q.capture_index_for_name("name").unwrap();
    let fni = q.capture_index_for_name("fn_name").unwrap();
    let fvi = q.capture_index_for_name("fn_val").unwrap();

    let mut cursor = QueryCursor::new();
    for m in cursor.matches(&q, *root, bytes) {
        let name_n = if let Some(c) = m.captures.iter().find(|c| c.index == ni) {
            c.node
        } else {
            // The variable_declarator pattern: only count it as a symbol when
            // the bound value is a function (kind check in code — the function
            // expression node name varies across grammar versions).
            let Some(val) = m.captures.iter().find(|c| c.index == fvi).map(|c| c.node) else {
                continue;
            };
            if !matches!(
                val.kind(),
                "arrow_function" | "function" | "function_expression" | "generator_function"
            ) {
                continue;
            }
            let Some(n) = m.captures.iter().find(|c| c.index == fni).map(|c| c.node) else {
                continue;
            };
            n
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

    fn symbols(facts: &[AstFact]) -> Vec<&str> {
        facts
            .iter()
            .filter(|f| f.predicate == "symbol_exists")
            .map(|f| f.subject.as_str())
            .collect()
    }

    #[test]
    fn ts_extracts_routes_with_handlers() {
        let src = r#"
            app.get('/v1/users', listUsers);
            router.post(
                "/v1/checkout",
                handleCheckout,
            );
            router.route('/v1/orders').get(listOrders);
        "#;
        let f = extract(src, false).unwrap();
        let r = routes(&f);
        assert!(r.contains(&"/v1/users"), "routes: {r:?}");
        assert!(r.contains(&"/v1/checkout"), "routes: {r:?}");
        assert!(r.contains(&"/v1/orders"), "routes: {r:?}");
        let checkout = f.iter().find(|x| x.subject == "/v1/checkout").unwrap();
        assert_eq!(checkout.handler.as_deref(), Some("handleCheckout"));
        assert_eq!(checkout.route_builder.as_deref(), Some("post"));
    }

    #[test]
    fn ts_rejects_non_route_strings() {
        // None of these are route registrations: a plain fs call, a 1-argument
        // Map-style `.get`, an assignment, and a non-route method.
        let src = r#"
            fs.readFileSync('/dev/null');
            cache.get('/dev/null');
            const p = '/etc/passwd';
            log.info('/v1/looks-like-a-route');
        "#;
        let f = extract(src, false).unwrap();
        assert!(routes(&f).is_empty(), "got: {:?}", routes(&f));
    }

    #[test]
    fn ts_module_level_consts_only() {
        let src = r#"
            const MAX_RETRIES = 5;
            export const DEFAULT_PORT = 8080;
            let counter = 3;
            function f() { const LOCAL_TIMEOUT = 30; }
        "#;
        let f = extract(src, false).unwrap();
        assert!(f.iter().any(|x| x.subject == "MAX_RETRIES"
            && x.predicate == "retry_count"
            && x.value == Value::Int(5)));
        assert!(f.iter().any(|x| x.subject == "DEFAULT_PORT"
            && x.predicate == "port"
            && x.value == Value::Int(8080)));
        assert!(!f.iter().any(|x| x.subject == "counter"), "let leaked");
        assert!(
            !f.iter().any(|x| x.subject == "LOCAL_TIMEOUT"),
            "function-local const leaked"
        );
    }

    #[test]
    fn ts_symbols_cover_declarations_not_comments() {
        let src = r#"
            // ghost_fn is only in this comment
            export function realFunction() {}
            class RealClass {}
            interface RealInterface { x: number }
            type RealAlias = string;
            enum RealEnum { A }
            export const arrowHandler = async (req: Request) => {};
            const notAFunction = 42;
            const s = "also_ghost lives in a string";
        "#;
        let syms = symbols(&extract(src, false).unwrap())
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let has = |n: &str| syms.iter().any(|s| s == n);
        assert!(has("realFunction"), "syms: {syms:?}");
        assert!(has("RealClass"));
        assert!(has("RealInterface"));
        assert!(has("RealAlias"));
        assert!(has("RealEnum"));
        assert!(has("arrowHandler"));
        assert!(!has("notAFunction"), "non-function const leaked: {syms:?}");
        assert!(!has("ghost_fn"), "comment name leaked");
        assert!(!has("also_ghost"), "string name leaked");
    }

    #[test]
    fn tsx_parses_jsx_and_extracts() {
        let src = r#"
            export const MAX_ITEMS = 20;
            export function ItemList() {
                return <ul>{items.map((i) => <li key={i}>{i}</li>)}</ul>;
            }
        "#;
        let f = extract(src, true).unwrap();
        assert!(symbols(&f).contains(&"ItemList"), "facts: {f:?}");
        assert!(f
            .iter()
            .any(|x| x.subject == "MAX_ITEMS" && x.value == Value::Int(20)));
    }
}
