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
    /// Humanized, searchable description derived from the surrounding code
    /// (path words + handler symbol + nearby doc comment). Used for concept
    /// resolution / embeddings, which need human words, not raw identifiers.
    /// `None` for facts where the identifier is already human-meaningful.
    pub label: Option<String>,
}

// Pattern source strings, defined once and used for BOTH the individual
// capturing regexes and the combined `RegexSet`, so the two can never drift.
const RE_ROUTE: &str = r#"["'`](/[A-Za-z0-9_][A-Za-z0-9_/\-.:{}]*)["'`]"#;
const RE_PORT: &str = r#"(?i)\bport\b"?\s*[:=]\s*"?(\d{2,5})"#;
const RE_RETRY: &str = r#"(?i)([A-Za-z0-9_]*retr(?:y|ies)[A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*[:=]\s*(\d+)"#;
const RE_TIMEOUT: &str =
    r#"(?i)([A-Za-z0-9_]*timeout[A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*[:=]\s*(\d+)"#;
const RE_NAMED_CONST: &str = r#"(?:(?:pub\s+)?const|let|export\s+const|var|final|static)?\s*([A-Za-z_][A-Za-z0-9_]*)\s*(?::\s*[A-Za-z0-9_:<>\[\]]+)?\s*=\s*(\d+)\b"#;
const RE_ENV_VAR: &str = r#"(?x)
    (?:std::)?env::var\s*\(\s*["']([A-Z][A-Z0-9_]+)["']
    | (?i:os\.)?(?i:getenv)\s*\(\s*["']([A-Z][A-Z0-9_]+)["']
    | process\.env\.([A-Z][A-Z0-9_]+)
    | process\.env\[\s*["']([A-Z][A-Z0-9_]+)["']\s*\]
    | os\.environ\[\s*["']([A-Z][A-Z0-9_]+)["']\s*\]
    "#;
const RE_COMPOSE_PORT: &str = r#"^\s*-\s*["']?(\d{2,5}):\d{2,5}["']?\s*$"#;
// C/C++/Obj-C preprocessor numeric constant: `#define NAME 5` / `#define NAME 0x1F`.
// Group 1 = name, group 2 = value. No `=` (the C idiom the other patterns miss).
const RE_CDEFINE: &str =
    r#"^\s*#\s*define\s+([A-Za-z_][A-Za-z0-9_]*)\s+\(?\s*(\d+|0[xX][0-9a-fA-F]+)\b"#;

struct Pats {
    route: Regex,
    port: Regex,
    retry: Regex,
    timeout: Regex,
    named_const: Regex,
    env_var: Regex,
    compose_port: Regex,
    cdefine: Regex,
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
        cdefine: Regex::new(RE_CDEFINE).unwrap(),
    })
}

/// Classify a `#define`/const name into a canonical predicate by keyword, so a
/// retry/timeout/port macro is keyed the same way as its `=`-form siblings.
fn predicate_for_name(name: &str) -> Cow<'static, str> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("retr") {
        Cow::Borrowed("retry_count")
    } else if lower.contains("timeout") {
        Cow::Borrowed("timeout")
    } else if lower.contains("port") {
        Cow::Borrowed("port")
    } else {
        Cow::Owned(name.to_uppercase())
    }
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
    let lower_has = |needle: &str| {
        line.as_bytes()
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
    };
    lower_has("env") || lower_has("getenv")
}

/// Extract facts from one file's contents. Facts borrow from `contents`.
pub fn extract_file<'a>(path: &Path, contents: &'a str) -> Vec<Extracted<'a>> {
    let mut out = Vec::new();
    let p = pats();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

    // Materialize lines once so route enrichment can look back for doc comments.
    let lines: Vec<&str> = contents.lines().collect();

    for (i, line) in lines.iter().copied().enumerate() {
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
        // C/C++/Obj-C `#define NAME value`. Handled first and exclusively for
        // the line, since the `=`-form patterns below don't match `#define`.
        if line.as_bytes().first() == Some(&b'#') || line.trim_start().starts_with('#') {
            if let Some(c) = p.cdefine.captures(line) {
                let name = group(line, &c, 1);
                let raw = group(line, &c, 2);
                let value = parse_int_lit(raw);
                if let Some(n) = value {
                    let predicate = predicate_for_name(name);
                    // Keep only claimable config: a specific predicate
                    // (retry/timeout/port) is inherently relevant; a generic
                    // #define must pass the config-relevance gate, else it's
                    // hardware/register noise (dominant in C — the kernel has
                    // millions of these).
                    // A tunable SUFFIX (`..._TIMEOUT`, `..._RETRIES`, `..._PORT`)
                    // is a strong config signal that overrides the hardware veto
                    // (so `DMA_TIMEOUT` survives) — but a mere substring match
                    // (`PORT_ENA_RX_SHIFT`) does not.
                    let tunable_suffix = ends_with_tunable(name);
                    let keep = if tunable_suffix {
                        !looks_like_chip_macro(name)
                    } else {
                        !is_hardware_artifact(name)
                            && !looks_like_chip_macro(name)
                            && is_config_relevant(name)
                    };
                    if keep {
                        // Subject is the macro name (so `truth config NAME` finds it).
                        push_extracted(&mut out, name, predicate, n, line_no, line);
                    }
                }
                continue;
            }
        }

        if p.route.is_match(line) {
            let has_signal = line_has_route_signal(line);
            for c in p.route.captures_iter(line) {
                let route = group(line, &c, 1);
                // Precision gate: a quoted `/x` is only a route if the line
                // registers one (framework verb) or the path is clearly
                // route-shaped. Rejects file paths, derivation paths (`/0`,
                // `/637`), and format strings that dominated false positives.
                if has_signal || route_shaped(route) {
                    let mut f = fact(
                        route,
                        "route_exists",
                        serde_json::Value::Bool(true),
                        line_no,
                        line,
                    );
                    f.label = Some(route_label(route, line, &lines, i));
                    out.push(f);
                }
            }
        }
        let mut specific = false;
        if p.retry.is_match(line) {
            if let Some(c) = p.retry.captures(line) {
                // Keyed by the canonical predicate (`retry_count`) so the verdict
                // engine finds it, and by the original identifier name as subject
                // so `truth config <NAME>` can find it. Group spans borrow `line`.
                let (name, val) = (group(line, &c, 1), group(line, &c, 2));
                push_num(
                    &mut out,
                    name,
                    Cow::Borrowed("retry_count"),
                    val,
                    line_no,
                    line,
                );
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
                push_num(
                    &mut out,
                    "port",
                    Cow::Borrowed("port"),
                    group(line, &c, 1),
                    line_no,
                    line,
                );
                specific = true;
            }
        } else if p.compose_port.is_match(line) {
            if let Some(c) = p.compose_port.captures(line) {
                // Published host port in a docker-compose `ports:` list.
                push_num(
                    &mut out,
                    "port",
                    Cow::Borrowed("port"),
                    group(line, &c, 1),
                    line_no,
                    line,
                );
                specific = true;
            }
        }
        // Generic UPPER_SNAKE / Go-style named numeric constants, keyed by name.
        // Skipped when a more specific predicate (retry/timeout/port) already fired.
        if !specific && p.named_const.is_match(line) {
            if let Some(c) = p.named_const.captures(line) {
                let name = group(line, &c, 1);
                // Must look like a constant AND be claimable config (not a
                // hardware/register/enum/chip artifact).
                if is_constish(name) && is_config_relevant(name) && !looks_like_chip_macro(name) {
                    let val = group(line, &c, 2);
                    push_num(
                        &mut out,
                        name,
                        Cow::Owned(name.to_uppercase()),
                        val,
                        line_no,
                        line,
                    );
                }
            }
        }
        if p.env_var.is_match(line) {
            for c in p.env_var.captures_iter(line) {
                // Pick whichever alternative group matched.
                if let Some(g) = (1..=5).find(|g| c.get(*g).is_some()) {
                    let name = group(line, &c, g);
                    out.push(fact(
                        name,
                        "env_var_exists",
                        serde_json::Value::Bool(true),
                        line_no,
                        line,
                    ));
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
        label: None,
    }
}

/// Framework HTTP route-registration signals. If a line contains one of these,
/// a quoted `/x` on it is very likely a real route.
const ROUTE_SIGNALS: &[&str] = &[
    ".get(",
    ".post(",
    ".put(",
    ".patch(",
    ".delete(",
    ".head(",
    ".options(",
    ".route(",
    ".routes(",
    ".handle(",
    ".at(",
    ".mount(",
    ".nest(",
    ".service(",
    "handlefunc(",
    "handle(",
    "@get",
    "@post",
    "@put",
    "@patch",
    "@delete",
    "@app.",
    "@router.",
    "@route",
    "@requestmapping",
    "@getmapping",
    "@postmapping",
    "router.",
    "route!",
    "web::resource",
    "addroute",
    "register",
    "endpoint",
    "path(",
    "url(",
    "uri(",
    "r.get",
    "r.post",
    "app.get",
    "app.post",
];

/// Whether a line contains a framework route-registration signal (ASCII-ci).
fn line_has_route_signal(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ROUTE_SIGNALS.iter().any(|s| lower.contains(s))
}

/// Common filesystem-path roots that are never HTTP routes.
const FS_ROOTS: &[&str] = &[
    "usr",
    "etc",
    "var",
    "tmp",
    "bin",
    "lib",
    "opt",
    "home",
    "dev",
    "proc",
    "sys",
    "mnt",
    "users",
    "applications",
    "library",
    "private",
    "volumes",
];

/// File extensions that mark a path literal as a filename, not a route.
const FILE_EXTS: &[&str] = &[
    "rs", "go", "py", "ts", "js", "c", "h", "cpp", "java", "rb", "txt", "md", "json", "yaml",
    "yml", "toml", "lock", "dll", "so", "dylib", "exe", "sh", "cfg", "ini", "xml", "html", "css",
    "png", "jpg", "svg", "csv", "sql", "proto", "pdf", "log", "tmp", "bak", "pem", "key", "crt",
];

/// Whether a path *looks* like an HTTP route on its own (used only when there's
/// NO framework signal on the line, where we must be conservative). It must
/// have a real "word" segment (≥2 chars with a letter, rejecting `/0` / `/637`),
/// not be an absolute filesystem path (`/Users/...`, `/etc/...`), and not end in
/// a known file extension (`/spec.yaml`, `/Restler.dll`). Path params (`:id`,
/// `{id}`) are allowed.
fn route_shaped(path: &str) -> bool {
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.is_empty() {
        return false;
    }
    let has_word = segments.iter().any(|s| {
        let s = s
            .trim_start_matches(':')
            .trim_matches(|c| c == '{' || c == '}');
        s.len() >= 2 && s.chars().any(|c| c.is_ascii_alphabetic())
    });
    if !has_word {
        return false;
    }
    // Absolute filesystem path?
    if FS_ROOTS.contains(&segments[0].to_ascii_lowercase().as_str()) {
        return false;
    }
    // Filename (last segment has a known file extension)?
    if let Some(last) = segments.last() {
        if let Some((_, ext)) = last.rsplit_once('.') {
            if FILE_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                return false;
            }
        }
    }
    true
}

/// Build a humanized, searchable label for a route from its surrounding code:
/// path words + the handler symbol on the line + words from the nearest doc
/// comment above. This is what concept resolution / embeddings match against,
/// because they need human words, not raw identifiers like `/v1/checkout`.
///
/// e.g. `/v1/checkout` with handler `handle_checkout` under a `/// Legacy
/// checkout flow` comment → "checkout handle checkout legacy checkout flow".
fn route_label(route: &str, line: &str, lines: &[&str], idx: usize) -> String {
    let mut words: Vec<String> = Vec::new();

    // 1) Path segments as words (skip version tokens and numerics).
    for seg in route.split('/').filter(|s| !s.is_empty()) {
        let seg = seg.trim_matches(|c| c == ':' || c == '{' || c == '}');
        if seg.is_empty() || seg.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if seg.len() == 2 && seg.starts_with('v') && seg[1..].chars().all(|c| c.is_ascii_digit()) {
            continue; // v1, v2
        }
        push_identifier_words(&mut words, seg);
    }

    // 2) Handler symbol: the identifier just after the route literal, e.g.
    //    `.post("/x", handle_checkout)` → handle_checkout.
    if let Some(pos) = line.find(route) {
        let after = &line[pos + route.len()..];
        if let Some(handler) = after
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .find(|t| {
                t.len() >= 3
                    && t.chars()
                        .next()
                        .map(|c| c.is_ascii_alphabetic())
                        .unwrap_or(false)
            })
        {
            push_identifier_words(&mut words, handler);
        }
    }

    // 3) Nearest doc comment above (//, ///, #, *), up to 3 lines back.
    for back in 1..=3 {
        let Some(prev_idx) = idx.checked_sub(back) else {
            break;
        };
        let prev = lines[prev_idx].trim();
        let comment = prev
            .strip_prefix("///")
            .or_else(|| prev.strip_prefix("//!"))
            .or_else(|| prev.strip_prefix("//"))
            .or_else(|| prev.strip_prefix("* "))
            .or_else(|| prev.strip_prefix("# "));
        match comment {
            Some(text) => {
                for w in text.split(|c: char| !c.is_ascii_alphanumeric()) {
                    if w.len() >= 3 && w.chars().any(|c| c.is_ascii_alphabetic()) {
                        words.push(w.to_lowercase());
                    }
                }
            }
            None => break, // stop at the first non-comment line
        }
    }

    dedup_keep_order(&mut words);
    words.join(" ")
}

/// Split an identifier (`handle_checkout`, `handleCheckout`, `HandleCheckout`)
/// into lowercase words and push them.
fn push_identifier_words(out: &mut Vec<String>, ident: &str) {
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.len() >= 2 {
            out.push(cur.to_lowercase());
        }
        cur.clear();
    };
    for ch in ident.chars() {
        if ch == '_' || ch == '-' || ch == '.' {
            flush(&mut cur, out);
        } else if ch.is_ascii_uppercase() && !cur.is_empty() {
            flush(&mut cur, out);
            cur.push(ch);
        } else {
            cur.push(ch);
        }
    }
    flush(&mut cur, out);
}

fn dedup_keep_order(words: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    words.retain(|w| seen.insert(w.clone()));
}

/// Parse a C integer literal: decimal or `0x`-hex.
fn parse_int_lit(raw: &str) -> Option<i64> {
    if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        raw.parse::<i64>().ok()
    }
}

/// Push a fully-formed numeric fact (used by the `#define` path, which has an
/// already-parsed value and a possibly-owned predicate).
fn push_extracted<'a>(
    out: &mut Vec<Extracted<'a>>,
    subject: &'a str,
    predicate: Cow<'static, str>,
    value: i64,
    line: u32,
    text: &'a str,
) {
    if out
        .iter()
        .any(|e| e.subject == subject && e.predicate == predicate && e.line == line)
    {
        return;
    }
    out.push(Extracted {
        subject: Cow::Borrowed(subject),
        predicate,
        value: serde_json::Value::Number(value.into()),
        line,
        text: text.trim(),
        label: None,
    });
}

/// Treat names that look like *meaningful* constants (UPPER_SNAKE, or Go-style
/// PascalCase) as config-ish, to avoid capturing `let x = 5` or enum spam.
///
/// Precision filters: reject names shorter than 3 chars (`A`, `N`, `OK`) and
/// single-token all-caps without an underscore (`ABORTED`, `TRUE`) which are
/// almost always enum discriminants / flags, not config knobs. A real config
/// constant reads like `MAX_RETRIES`, `DEFAULT_PORT`, `MaxConnections`.
/// Keyword stems that mark a constant as *tunable/policy config* — the kind of
/// thing a person makes a claim about ("the timeout is 30s", "max retries is 5").
const CONFIG_KEYWORDS: &[&str] = &[
    "retr",
    "timeout",
    "port",
    "max",
    "min",
    "limit",
    "size",
    "len",
    "count",
    "interval",
    "threshold",
    "enable",
    "disable",
    "default",
    "capacity",
    "buffer",
    "batch",
    "window",
    "ttl",
    "delay",
    "backoff",
    "concurren",
    "pool",
    "quota",
    "rate",
    "deadline",
    "expire",
    "version",
    "level",
];

/// Substrings that mark a constant as a *hardware / register / wiring* artifact —
/// never a claimable config value (these dominate C codebases like the kernel).
const HARDWARE_MARKERS: &[&str] = &[
    "offset", "_ofst", "_off", "addr", "_reg", "register", "irq", "_mask", "_bit", "_shift",
    "_pin", "gpio", "dma", "_phys", "vaddr", "opcode", "_cmd", "_dev", "vendor", "_id", "magic",
    "_hz", "mhz", "khz", "_clk", "clock", "voltage", "_mv", "_uv", "_ohm", "errno", "ioctl", "_fd",
];

/// A strong, unambiguous config suffix: the name *ends* with a tunable word, so
/// it's a real knob even if a hardware word appears earlier (`DMA_TIMEOUT`,
/// `RX_BUFFER_SIZE`). Distinct from a substring match (`PORT_ENA_RX_SHIFT`).
fn ends_with_tunable(name: &str) -> bool {
    const TUNABLE_SUFFIXES: &[&str] = &[
        "_timeout",
        "_retries",
        "_retry",
        "_port",
        "_max",
        "_min",
        "_limit",
        "_size",
        "_count",
        "_interval",
        "_threshold",
        "_ttl",
        "_delay",
        "_backoff",
        "_capacity",
        "_deadline",
    ];
    let lower = name.to_ascii_lowercase();
    TUNABLE_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// A hard veto: the name contains a hardware/register/wiring marker, so it is
/// never claimable config even if it also contains a config-ish word (e.g.
/// `A5PSW_PORT_ENA_RX_SHIFT` has "port" but is a register shift). Applied to ALL
/// constants, including ones routed to specific predicates.
fn is_hardware_artifact(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HARDWARE_MARKERS.iter().any(|m| lower.contains(m))
}

/// Whether a numeric constant name looks like *config a person would claim about*
/// rather than a hardware/register/enum artifact. Conservative: must hit a config
/// keyword and miss all hardware markers. This is what turns the kernel's ~2.3M
/// raw constants (register offsets, reset IDs) into the few hundred real ones.
fn is_config_relevant(name: &str) -> bool {
    if is_hardware_artifact(name) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    CONFIG_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Heuristic: the name starts with a chip/part/vendor code, so it's
/// device-specific register/wiring data, not general config. Matches a leading
/// segment like `A2065_`, `A3D_`, `A3700_`, `MT7621_` — a short token that mixes
/// letters and digits before the first `_`.
fn looks_like_chip_macro(name: &str) -> bool {
    let Some((head, _rest)) = name.split_once('_') else {
        return false;
    };
    if head.len() < 2 || head.len() > 8 {
        return false;
    }
    let has_digit = head.chars().any(|c| c.is_ascii_digit());
    let has_alpha = head.chars().any(|c| c.is_ascii_alphabetic());
    // A leading alnum code (letters+digits) is a part number / chip prefix.
    has_digit && has_alpha
}

fn is_constish(name: &str) -> bool {
    if name.len() < 3 {
        return false;
    }
    let upper_snake = name
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().any(|c| c.is_ascii_uppercase());
    // PascalCase / camelCase compound (has a lowercase AND an uppercase).
    let mixed = name.chars().any(|c| c.is_ascii_lowercase())
        && name.chars().any(|c| c.is_ascii_uppercase());

    if upper_snake {
        // Require a multi-segment UPPER_SNAKE (`MAX_RETRIES`), which filters bare
        // single-word enum tokens like `ABORTED`, `TRUE`, `OK`.
        name.contains('_')
    } else {
        // PascalCase like `MaxRetries` is fine; require >1 capital so single-cap
        // lowercase ids (`Foo`-ish) still count but `Bar` plain words are weak.
        mixed
    }
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
            label: None,
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
            if let Some(key) = t.split('=').next().map(str::trim) {
                // Dotted keys (`anyhow.workspace = true`, `serde.features = [...]`)
                // and quoted keys (`"some-crate" = ...`) name the same package.
                // The crate name is the first dotted segment, unquoted.
                let name = key
                    .split('.')
                    .next()
                    .unwrap_or(key)
                    .trim()
                    .trim_matches('"');
                if !name.is_empty() && !name.starts_with('#') {
                    out.push(fact(
                        name,
                        "dependency_exists",
                        serde_json::Value::Bool(true),
                        (i + 1) as u32,
                        t,
                    ));
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
                    label: None,
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
            out.push(fact(
                name,
                "dependency_exists",
                serde_json::Value::Bool(true),
                (i + 1) as u32,
                t,
            ));
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
                    out.push(fact(
                        name,
                        "dependency_exists",
                        serde_json::Value::Bool(true),
                        (i + 1) as u32,
                        t,
                    ));
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
        facts
            .iter()
            .any(|f| f.subject == route && f.predicate == "route_exists")
    }
    fn has_dep(facts: &[Extracted], name: &str) -> bool {
        facts
            .iter()
            .any(|f| f.subject == name && f.predicate == "dependency_exists")
    }

    #[test]
    fn cargo_workspace_and_quoted_dep_keys() {
        // Regression: `anyhow.workspace = true` indexed the crate as
        // `anyhow.workspace`, so "depends on anyhow" never matched. The crate
        // name is the first dotted segment, unquoted.
        let src = r#"
            [dependencies]
            anyhow.workspace = true
            serde = { version = "1", features = ["derive"] }
            "tree-sitter-typescript" = "0.21"
            tokio.features = ["full"]
        "#;
        let f = extract_cargo_deps(src);
        assert!(has_dep(&f, "anyhow"));
        assert!(has_dep(&f, "serde"));
        assert!(has_dep(&f, "tree-sitter-typescript"));
        assert!(has_dep(&f, "tokio"));
        assert!(!has_dep(&f, "anyhow.workspace"));
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
        assert!(f
            .iter()
            .any(|e| e.predicate == "retry_count" && e.value == serde_json::json!(5)));
        assert!(f
            .iter()
            .any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
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
        assert!(f
            .iter()
            .any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MAX_RETRIES"
            && e.predicate == "retry_count"
            && e.value == serde_json::json!(5)));
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
        assert!(f
            .iter()
            .any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f
            .iter()
            .any(|e| e.subject == "API_TOKEN" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MAX_RETRIES"
            && e.predicate == "retry_count"
            && e.value == serde_json::json!(5)));
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
        assert!(f
            .iter()
            .any(|e| e.subject == "STRIPE_SECRET" && e.predicate == "env_var_exists"));
        assert!(f.iter().any(|e| e.subject == "MaxRetries"
            && e.predicate == "retry_count"
            && e.value == serde_json::json!(5)));
    }

    #[test]
    fn route_enrichment_builds_human_label() {
        let src = "/// Legacy checkout flow. Deprecated.\nrouter.post(\"/v1/checkout\", handle_checkout);\n";
        let f = extract_file(&PathBuf::from("routes.rs"), src);
        let route = f
            .iter()
            .find(|e| e.subject == "/v1/checkout")
            .expect("route");
        let label = route.label.as_deref().unwrap_or("");
        // Path word + handler words + doc-comment words, humanized.
        assert!(label.contains("checkout"), "label: {label}");
        assert!(label.contains("handle"), "label: {label}");
        assert!(label.contains("legacy"), "label: {label}");
        // Version token v1 and the numeric are dropped.
        assert!(!label.contains("v1"), "label: {label}");
    }

    #[test]
    fn route_precision_keeps_real_drops_junk() {
        // Framework-signal lines → routes kept.
        let signaled = "app.get(\"/v1/checkout\", h); router.post(\"/webhooks/stripe\", h);";
        let f = extract_file(&PathBuf::from("server.ts"), signaled);
        assert!(has_route(&f, "/v1/checkout"));
        assert!(has_route(&f, "/webhooks/stripe"));

        // No signal: derivation paths, file paths, absolute paths → dropped.
        let junk = "let p = \"/0\"; let q = \"/637\"; let f = \"/Users/x/main.py\"; let d = \"/spec.yaml\";";
        let f = extract_file(&PathBuf::from("x.rs"), junk);
        assert!(
            !f.iter().any(|e| e.predicate == "route_exists"),
            "junk routes: {:?}",
            f.iter()
                .filter(|e| e.predicate == "route_exists")
                .map(|e| e.subject.as_ref())
                .collect::<Vec<_>>()
        );

        // No signal but clearly route-shaped → kept.
        let bare = "const path = \"/api/v1/users\";";
        let f = extract_file(&PathBuf::from("x.rs"), bare);
        assert!(has_route(&f, "/api/v1/users"));
    }

    #[test]
    fn constant_precision_drops_single_letter_and_enum_tokens() {
        let f = extract_file(
            &PathBuf::from("x.rs"),
            "let A = 1;\nconst ABORTED = 4016;\nconst MAX_RETRIES = 5;\n",
        );
        // Single-letter and bare-uppercase enum tokens are dropped...
        assert!(!f.iter().any(|e| e.subject == "A"));
        assert!(!f.iter().any(|e| e.subject == "ABORTED"));
        // ...but a real UPPER_SNAKE config constant survives.
        assert!(f
            .iter()
            .any(|e| e.subject == "MAX_RETRIES" && e.value == serde_json::json!(5)));
    }

    #[test]
    fn c_define_constants_ports_and_hex() {
        let src = r#"
#define MAX_RETRIES 5
#define DMA_TIMEOUT 3000
#define DEFAULT_PORT 8080
#define LISTEN_PORT 443
#define BUF_MASK 0xFF
char *e = getenv("KERNEL_DEBUG");
"#;
        let f = extract_file(&PathBuf::from("net.c"), src);
        // retry/timeout/port keywords route to canonical predicates...
        assert!(f.iter().any(|e| e.subject == "MAX_RETRIES"
            && e.predicate == "retry_count"
            && e.value == serde_json::json!(5)));
        assert!(f.iter().any(|e| e.subject == "DMA_TIMEOUT"
            && e.predicate == "timeout"
            && e.value == serde_json::json!(3000)));
        assert!(f
            .iter()
            .any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
        assert!(f
            .iter()
            .any(|e| e.predicate == "port" && e.value == serde_json::json!(443)));
        // BUF_MASK is a register/hardware artifact (`_mask`) — filtered out, not
        // a claimable config value. This is the relevance gate in action.
        assert!(!f.iter().any(|e| e.subject == "BUF_MASK"));
        // getenv works in C too.
        assert!(f
            .iter()
            .any(|e| e.subject == "KERNEL_DEBUG" && e.predicate == "env_var_exists"));
    }

    #[test]
    fn c_define_with_spacing_variants() {
        // `# define` with extra spaces, and a parenthesized value.
        let src = "#  define   FOO_TIMEOUT  900\n# define BAR (12)\n";
        let f = extract_file(&PathBuf::from("x.h"), src);
        // A timeout keyword routes + survives the relevance gate.
        assert!(f.iter().any(|e| e.subject == "FOO_TIMEOUT"
            && e.predicate == "timeout"
            && e.value == serde_json::json!(900)));
        // `BAR` is a generic name with no config signal — filtered (not claimable).
        assert!(!f.iter().any(|e| e.subject == "BAR"));
    }

    #[test]
    fn relevance_gate_keeps_config_drops_hardware() {
        // The kernel-noise problem: register/offset macros must be dropped, real
        // tunables kept.
        let src = "\
#define A10_DERRADDR_OFST 44
#define A10SR_RESET_USB 4
#define GPIO_IRQ_BASE 32
#define MAX_BATCH_SIZE 16
#define WORKER_POOL_LIMIT 8
";
        let f = extract_file(&PathBuf::from("drv.c"), src);
        let kept: Vec<&str> = f.iter().map(|e| e.subject.as_ref()).collect();
        assert!(
            !kept.contains(&"A10_DERRADDR_OFST"),
            "offset noise kept: {kept:?}"
        );
        assert!(
            !kept.contains(&"A10SR_RESET_USB"),
            "reset-id noise kept: {kept:?}"
        );
        assert!(!kept.contains(&"GPIO_IRQ_BASE"), "irq noise kept: {kept:?}");
        assert!(
            kept.contains(&"MAX_BATCH_SIZE"),
            "real config dropped: {kept:?}"
        );
        assert!(
            kept.contains(&"WORKER_POOL_LIMIT"),
            "real config dropped: {kept:?}"
        );
    }

    #[test]
    fn deps_from_each_manifest() {
        assert!(has_dep(
            &extract_file(
                &PathBuf::from("Cargo.toml"),
                "[dependencies]\nredis = \"0.2\"\n"
            ),
            "redis"
        ));
        assert!(has_dep(
            &extract_file(
                &PathBuf::from("package.json"),
                r#"{"dependencies":{"express":"4"}}"#
            ),
            "express"
        ));
        assert!(has_dep(
            &extract_file(
                &PathBuf::from("requirements.txt"),
                "fastapi==0.110\nredis>=4.0\n"
            ),
            "fastapi"
        ));
        assert!(has_dep(
            &extract_file(
                &PathBuf::from("requirements.txt"),
                "fastapi==0.110\nredis>=4.0\n"
            ),
            "redis"
        ));
        let gomod = "module x\n\nrequire (\n\tgithub.com/go-redis/redis v8\n)\n";
        assert!(has_dep(
            &extract_file(&PathBuf::from("go.mod"), gomod),
            "github.com/go-redis/redis"
        ));
    }

    #[test]
    fn yaml_and_json_ports() {
        let yaml = "ports:\n  - \"8080:8080\"\n";
        let f = extract_file(&PathBuf::from("docker-compose.yml"), yaml);
        assert!(f
            .iter()
            .any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
        let json = r#"{ "port": 8080 }"#;
        let f2 = extract_file(&PathBuf::from("config.json"), json);
        assert!(f2
            .iter()
            .any(|e| e.predicate == "port" && e.value == serde_json::json!(8080)));
    }
}
