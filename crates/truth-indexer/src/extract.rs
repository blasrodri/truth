//! Deterministic evidence extraction from repo files (spec §12.4). Regex-based
//! detection of routes, ports, retry/timeout constants, env vars, config values
//! and dependencies across Rust / TypeScript / Python / Go and common manifests.
//! No full code understanding.

use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

use std::borrow::Cow;

/// One extracted fact, before it is turned into a stored `EvidenceItem`.
///
/// Borrows from the file contents wherever possible (`'a`): `subject` and `text`
/// are slices of the source line, and `predicate` is almost always a `&'static`
/// literal. The downstream consumer allocates owned `String`s once when building
/// the DB row, so extraction itself stays allocation-light (this is the hottest
/// loop in indexing).
#[derive(Debug, Clone, PartialEq)]
pub struct Extracted<'a> {
    /// Subject (route, env var, dependency name) or a constant key.
    pub subject: Cow<'a, str>,
    /// Predicate: route_exists, port, retry_count, timeout, env_var_exists, dependency_exists.
    pub predicate: Cow<'static, str>,
    /// JSON value: bool for existence, number for constants/ports.
    pub value: serde_json::Value,
    pub line: u32,
    pub text: &'a str,
}

// Pattern source strings, defined once and used for BOTH the individual
// capturing regexes and the combined `RegexSet`, so the two can never drift.
const RE_ROUTE: &str = r#"["'`](/[A-Za-z0-9_][A-Za-z0-9_/\-.:{}]*)["'`]"#;
const RE_PORT: &str = r#"(?i)\bport\b"?\s*[:=]\s*"?(\d{2,5})"#;
const RE_RETRY: &str =
    r#"(?i)([A-Za-z0-9_]*retr(?:y|ies)[A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*[:=]\s*(\d+)"#;
const RE_TIMEOUT: &str =
    r#"(?i)([A-Za-z0-9_]*timeout[A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*[:=]\s*(\d+)"#;
const RE_NAMED_CONST: &str =
    r#"(?:(?:pub\s+)?const|let|export\s+const|var|final|static)?\s*([A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*=\s*(\d+)\b"#;
const RE_ENV_VAR: &str = r#"(?x)
    (?:std::)?env::var\s*\(\s*["']([A-Z][A-Z0-9_]+)["']
    | (?i:os\.)?(?i:getenv)\s*\(\s*["']([A-Z][A-Z0-9_]+)["']
    | process\.env\.([A-Z][A-Z0-9_]+)
    | process\.env\[\s*["']([A-Z][A-Z0-9_]+)["']\s*\]
    | os\.environ\[\s*["']([A-Z][A-Z0-9_]+)["']\s*\]
    "#;
const RE_COMPOSE_PORT: &str = r#"^\s*-\s*["']?(\d{2,5}):\d{2,5}["']?\s*$"#;

struct Pats {
    route: Regex,
    port: Regex,
    retry: Regex,
    timeout: Regex,
    named_const: Regex,
    env_var: Regex,
    compose_port: Regex,
}

fn pats() -> &'static Pats {
    static P: OnceLock<Pats> = OnceLock::new();
    P.get_or_init(|| Pats {
        route: Regex::new(RE_ROUTE).unwrap(),
        port: Regex::new(RE_PORT).unwrap(),
        retry: Regex::new(RE_RETRY).unwrap(),
        timeout: Regex::new(RE_TIMEOUT).unwrap(),
        named_const: Regex::new(RE_NAMED_CONST).unwrap(),
        env_var: Regex::new(RE_ENV_VAR).unwrap(),
        compose_port: Regex::new(RE_COMPOSE_PORT).unwrap(),
    })
}

/// Cheap byte prefilter: a line can only match a pattern if it contains a digit
/// (every numeric pattern), a quote (route/env string literals), or the bytes
/// `env`/`getenv` (the quote-less env-var forms). Lines with none are skipped
/// before any regex runs — the vast majority of source lines.
fn line_may_match(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut has_signal = false;
    for &b in bytes {
        if b.is_ascii_digit() || b == b'"' || b == b'\'' || b == b'`' {
            has_signal = true;
            break;
        }
    }
    if has_signal {
        return true;
    }
    // Quote-less env forms: process.env.X / os.environ / getenv. Cheap contains.
    let lower_has = |needle: &str| line.as_bytes().windows(needle.len()).any(|w| {
        w.eq_ignore_ascii_case(needle.as_bytes())
    });
    lower_has("env") || lower_has("getenv")
}

/// Extract facts from one file's contents. Facts borrow from `contents`.
pub fn extract_file<'a>(path: &Path, contents: &'a str) -> Vec<Extracted<'a>> {
    let mut out = Vec::new();
    let p = pats();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

    for (i, line) in contents.lines().enumerate() {
        // Cheap byte prefilter skips the overwhelming majority of source lines
        // (no digit, quote, or env token → cannot match any pattern).
        if !line_may_match(line) {
            continue;
        }
        let line_no = (i + 1) as u32;

        // Gate each pattern with `is_match` (allocation-free — uses the regex
        // crate's internal pool) and only run `captures` (which allocates a
        // `Captures`) on lines that actually hit. This avoids the per-line
        // `RegexSet::matches` allocation, which dominated the heap profile.
        if p.route.is_match(line) {
            for c in p.route.captures_iter(line) {
                let route = group(line, &c, 1);
                out.push(fact(route, "route_exists", serde_json::Value::Bool(true), line_no, line));
            }
        }
        let mut specific = false;
        if p.retry.is_match(line) {
            if let Some(c) = p.retry.captures(line) {
                // Keyed by the canonical predicate (`retry_count`) so the verdict
                // engine finds it, and by the original identifier name as subject
                // so `truth config <NAME>` can find it. Group spans borrow `line`.
                let (name, val) = (group(line, &c, 1), group(line, &c, 2));
                push_num(&mut out, name, Cow::Borrowed("retry_count"), val, line_no, line);
                specific = true;
            }
        }
        if p.timeout.is_match(line) {
            if let Some(c) = p.timeout.captures(line) {
                let (name, val) = (group(line, &c, 1), group(line, &c, 2));
                push_num(&mut out, name, Cow::Borrowed("timeout"), val, line_no, line);
                specific = true;
            }
        }
        if p.port.is_match(line) {
            if let Some(c) = p.port.captures(line) {
                push_num(&mut out, "port", Cow::Borrowed("port"), group(line, &c, 1), line_no, line);
                specific = true;
            }
        } else if p.compose_port.is_match(line) {
            if let Some(c) = p.compose_port.captures(line) {
                // Published host port in a docker-compose `ports:` list.
                push_num(&mut out, "port", Cow::Borrowed("port"), group(line, &c, 1), line_no, line);
                specific = true;
            }
        }
        // Generic UPPER_SNAKE / Go-style named numeric constants, keyed by name.
        // Skipped when a more specific predicate (retry/timeout/port) already fired.
        if !specific && p.named_const.is_match(line) {
            if let Some(c) = p.named_const.captures(line) {
                let name = group(line, &c, 1);
                if is_constish(name) {
                    let val = group(line, &c, 2);
                    push_num(&mut out, name, Cow::Owned(name.to_uppercase()), val, line_no, line);
                }
            }
        }
        if p.env_var.is_match(line) {
            for c in p.env_var.captures_iter(line) {
                // Pick whichever alternative group matched.
                if let Some(g) = (1..=5).find(|g| c.get(*g).is_some()) {
                    let name = group(line, &c, g);
                    out.push(fact(name, "env_var_exists", serde_json::Value::Bool(true), line_no, line));
                }
            }
        }
    }

    // Dependencies from manifests.
    if fname == "Cargo.toml" {
        out.extend(extract_cargo_deps(contents));
    } else if fname == "package.json" {
        out.extend(extract_package_json_deps(contents));
    } else if fname == "requirements.txt" {
        out.extend(extract_requirements_txt(contents));
    } else if fname == "go.mod" {
        out.extend(extract_go_mod(contents));
    } else if ext == "toml" {
        // Generic TOML may still carry a [dependencies] table.
        out.extend(extract_cargo_deps(contents));
    }

    out
}

/// Capture group `n` as a slice that borrows the haystack `line` (not the
/// temporary `Captures`). Empty string if the group is absent.
fn group<'a>(line: &'a str, caps: &regex::Captures, n: usize) -> &'a str {
    match caps.get(n) {
        // `Match::range()` lets us re-slice `line` directly for the right lifetime.
        Some(m) => &line[m.start()..m.end()],
        None => "",
    }
}

/// Build a fact that borrows its subject/text from the source line. `predicate`
/// is a `&'static` literal. No heap allocation.
fn fact<'a>(
    subject: &'a str,
    predicate: &'static str,
    value: serde_json::Value,
    line: u32,
    text: &'a str,
) -> Extracted<'a> {
    Extracted {
        subject: Cow::Borrowed(subject),
        predicate: Cow::Borrowed(predicate),
        value,
        line,
        text: text.trim(),
    }
}

/// Treat names that look like constants (UPPER_SNAKE, or Go-style PascalCase
/// with a digit/word boundary) as config-ish, to avoid capturing `let x = 5`.
fn is_constish(name: &str) -> bool {
    let upper_snake = name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().any(|c| c.is_ascii_uppercase());
    // Go style: starts uppercase, has a lowercase somewhere (PascalCase like MaxRetries).
    let pascal = name.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        && name.chars().any(|c| c.is_ascii_lowercase());
    upper_snake || pascal
}

fn push_num<'a>(
    out: &mut Vec<Extracted<'a>>,
    subject: &'a str,
    predicate: Cow<'static, str>,
    raw: &str,
    line: u32,
    text: &'a str,
) {
    if let Ok(n) = raw.parse::<i64>() {
        // Avoid duplicate (subject,predicate) facts from overlapping patterns.
        if out
            .iter()
            .any(|e| e.subject == subject && e.predicate == predicate && e.line == line)
        {
            return;
        }
        out.push(Extracted {
            subject: Cow::Borrowed(subject),
            predicate,
            value: serde_json::Value::Number(n.into()),
            line,
            text: text.trim(),
        });
    }
}

fn extract_cargo_deps(contents: &str) -> Vec<Extracted<'_>> {
    let mut out = Vec::new();
    let mut in_deps = false;
    for (i, line) in contents.lines().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t.contains("dependencies");
            continue;
        }
        if in_deps {
            if let Some(name) = t.split('=').next().map(str::trim) {
                if !name.is_empty() && !name.starts_with('#') {
                    out.push(fact(name, "dependency_exists", serde_json::Value::Bool(true), (i + 1) as u32, t));
                }
            }
        }
    }
    out
}

fn extract_package_json_deps(contents: &str) -> Vec<Extracted<'static>> {
    // package.json parses to an owned Value, so these facts own their strings
    // (the only extractor that allocates per dependency — rare and small files).
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(contents) else {
        return out;
    };
    for key in ["dependencies", "devDependencies"] {
        if let Some(obj) = v.get(key).and_then(|d| d.as_object()) {
            for name in obj.keys() {
                out.push(Extracted {
                    subject: Cow::Owned(name.clone()),
                    predicate: Cow::Borrowed("dependency_exists"),
                    value: serde_json::Value::Bool(true),
                    line: 0,
                    text: "",
                });
            }
        }
    }
    out
}

fn extract_requirements_txt(contents: &str) -> Vec<Extracted<'_>> {
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t.starts_with('-') {
            continue;
        }
        // Name is up to the first version specifier / comparator / extras bracket.
        let name = t
            .split(['=', '<', '>', '~', '!', ';', '[', ' '])
            .next()
            .unwrap_or("")
            .trim();
        if !name.is_empty() {
            out.push(fact(name, "dependency_exists", serde_json::Value::Bool(true), (i + 1) as u32, t));
        }
    }
    out
}

fn extract_go_mod(contents: &str) -> Vec<Extracted<'_>> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (i, line) in contents.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && t == ")" {
            in_block = false;
            continue;
        }
        let dep_line = if let Some(rest) = t.strip_prefix("require ") {
            Some(rest.trim())
        } else if in_block && !t.is_empty() {
            Some(t)
        } else {
            None
        };
        if let Some(dl) = dep_line {
            if let Some(name) = dl.split_whitespace().next() {
                if name != "(" {
                    out.push(fact(name, "dependency_exists", serde_json::Value::Bool(true), (i + 1) as u32, t));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn has_route(facts: &[Extracted], route: &str) -> bool {
        facts.iter().any(|f| f.subject == route && f.predicate == "route_exists")
    }
    fn has_dep(facts: &[Extracted], name: &str) -> bool {
        facts.iter().any(|f| f.subject == name && f.predicate == "dependency_exists")
    }

    #[test]
    fn rust_route_and_const() {
        let src = r#"
            pub const MAX_RETRIES: u32 = 5;
            fn build() { Router::new().route("/v1/checkout", get(checkout)); }
            let port = 8080;
        "#;
        let f = extract_file(&PathBuf::from("main.rs"), src);
        assert!(has_route(&f, "/v1/checkout"));
        assert!(f.iter().any(|e| e.predicate == "retry_count" && e.value == serde_json::json!(5)));
        assert!(f.iter().any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
    }

    #[test]
    fn typescript_route_const_env() {
        let src = r#"
            export const MAX_RETRIES = 5;
            app.get("/v1/checkout", handler);
            router.post("/webhooks/stripe", handler);
            const k = process.env.STRIPE_SECRET;
        "#;
        let f = extract_file(&PathBuf::from("server.ts"), src);
        assert!(has_route(&f, "/v1/checkout"));
        assert!(has_route(&f, "/webhooks/stripe"));
        assert!(f.iter().any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MAX_RETRIES" && e.predicate == "retry_count" && e.value == serde_json::json!(5)));
    }

    #[test]
    fn python_route_const_env() {
        let src = r#"
MAX_RETRIES = 5

@app.get("/v1/checkout")
def checkout(): ...

secret = os.environ["STRIPE_SECRET"]
token = os.getenv("API_TOKEN")
"#;
        let f = extract_file(&PathBuf::from("app.py"), src);
        assert!(has_route(&f, "/v1/checkout"));
        assert!(f.iter().any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "API_TOKEN" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MAX_RETRIES" && e.predicate == "retry_count" && e.value == serde_json::json!(5)));
    }

    #[test]
    fn go_route_const_env() {
        let src = r#"
const MaxRetries = 5
func main() { r.HandleFunc("/v1/checkout", handler) }
secret := os.Getenv("STRIPE_SECRET")
"#;
        let f = extract_file(&PathBuf::from("main.go"), src);
        assert!(has_route(&f, "/v1/checkout"));
        assert!(f.iter().any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MaxRetries" && e.predicate == "retry_count" && e.value == serde_json::json!(5)));
    }

    #[test]
    fn deps_from_each_manifest() {
        assert!(has_dep(&extract_file(&PathBuf::from("Cargo.toml"), "[dependencies]\nredis = \"0.2\"\n"), "redis"));
        assert!(has_dep(
            &extract_file(&PathBuf::from("package.json"), r#"{"dependencies":{"express":"4"}}"#),
            "express"
        ));
        assert!(has_dep(&extract_file(&PathBuf::from("requirements.txt"), "fastapi==0.110\nredis>=4.0\n"), "fastapi"));
        assert!(has_dep(&extract_file(&PathBuf::from("requirements.txt"), "fastapi==0.110\nredis>=4.0\n"), "redis"));
        let gomod = "module x\n\nrequire (\n\tgithub.com/go-redis/redis v8\n)\n";
        assert!(has_dep(&extract_file(&PathBuf::from("go.mod"), gomod), "github.com/go-redis/redis"));
    }

    #[test]
    fn yaml_and_json_ports() {
        let yaml = "ports:\n  - \"8080:8080\"\n";
        let f = extract_file(&PathBuf::from("docker-compose.yml"), yaml);
        assert!(f.iter().any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
        let json = r#"{ "port": 8080 }"#;
        let f2 = extract_file(&PathBuf::from("config.json"), json);
        assert!(f2.iter().any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
    }
}
