//! Code ownership resolution from authoritative files: GitHub-style `CODEOWNERS`
//! and the Linux-kernel `MAINTAINERS` format. These are the *intended* owners,
//! preferred over git-history heuristics.

use std::path::Path;

/// An owner entry resolved for a path.
#[derive(Debug, Clone, PartialEq)]
pub struct Owner {
    /// Display name or handle (e.g. "@team/payments", "Ingo Molnar <mingo@...>").
    pub who: String,
    /// "maintainer" | "reviewer" | "codeowner".
    pub role: String,
    /// Which file the ownership came from (for citation).
    pub source: String,
}

/// Parse and match ownership for a repo. Built once per repo root.
pub struct Ownership {
    codeowners: Vec<CodeownersRule>,
    maintainers: Vec<MaintainerSection>,
    root: std::path::PathBuf,
}

struct CodeownersRule {
    /// Glob pattern (CODEOWNERS syntax, gitignore-like).
    pattern: String,
    owners: Vec<String>,
}

struct MaintainerSection {
    maintainers: Vec<String>,
    reviewers: Vec<String>,
    /// `F:` file glob patterns.
    files: Vec<String>,
}

impl Ownership {
    /// Load ownership files from a repo root. Empty if none present.
    pub fn load(root: &Path) -> Self {
        let codeowners = load_codeowners(root);
        let maintainers = load_maintainers(root);
        Ownership { codeowners, maintainers, root: root.to_path_buf() }
    }

    /// Whether any authoritative ownership file was found.
    pub fn has_data(&self) -> bool {
        !self.codeowners.is_empty() || !self.maintainers.is_empty()
    }

    /// Resolve owners for a path (relative to the repo root). CODEOWNERS wins
    /// when present (last matching rule, like git); otherwise MAINTAINERS.
    pub fn owners_for(&self, path: &Path) -> Vec<Owner> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // CODEOWNERS: last matching rule wins (GitHub semantics).
        if !self.codeowners.is_empty() {
            if let Some(rule) = self.codeowners.iter().rev().find(|r| glob_match(&r.pattern, &rel_str)) {
                return rule
                    .owners
                    .iter()
                    .map(|o| Owner { who: o.clone(), role: "codeowner".into(), source: "CODEOWNERS".into() })
                    .collect();
            }
        }

        // MAINTAINERS: the most specific (longest matching F: pattern) section.
        let mut best: Option<(usize, &MaintainerSection)> = None;
        for sec in &self.maintainers {
            for f in &sec.files {
                if maintainers_match(f, &rel_str) {
                    let score = f.len();
                    if best.map(|(s, _)| score > s).unwrap_or(true) {
                        best = Some((score, sec));
                    }
                }
            }
        }
        if let Some((_, sec)) = best {
            let mut out: Vec<Owner> = sec
                .maintainers
                .iter()
                .map(|m| Owner { who: m.clone(), role: "maintainer".into(), source: "MAINTAINERS".into() })
                .collect();
            out.extend(sec.reviewers.iter().map(|r| Owner {
                who: r.clone(),
                role: "reviewer".into(),
                source: "MAINTAINERS".into(),
            }));
            return out;
        }
        Vec::new()
    }
}

fn load_codeowners(root: &Path) -> Vec<CodeownersRule> {
    // GitHub looks in these locations; first found wins.
    for rel in [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"] {
        let p = root.join(rel);
        if let Ok(text) = std::fs::read_to_string(&p) {
            return parse_codeowners(&text);
        }
    }
    Vec::new()
}

fn parse_codeowners(text: &str) -> Vec<CodeownersRule> {
    let mut rules = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(pattern) = parts.next() else { continue };
        let owners: Vec<String> = parts.map(|s| s.to_string()).collect();
        if !owners.is_empty() {
            rules.push(CodeownersRule { pattern: pattern.to_string(), owners });
        }
    }
    rules
}

fn load_maintainers(root: &Path) -> Vec<MaintainerSection> {
    let p = root.join("MAINTAINERS");
    match std::fs::read_to_string(&p) {
        Ok(text) => parse_maintainers(&text),
        Err(_) => Vec::new(),
    }
}

/// Parse the kernel MAINTAINERS format: blank-line-separated sections, each with
/// `M:` maintainers, `R:` reviewers, `F:` file patterns.
fn parse_maintainers(text: &str) -> Vec<MaintainerSection> {
    let mut sections = Vec::new();
    let mut cur = MaintainerSection { maintainers: vec![], reviewers: vec![], files: vec![] };
    let mut in_section = false;

    let flush = |cur: &mut MaintainerSection, out: &mut Vec<MaintainerSection>| {
        if !cur.files.is_empty() && (!cur.maintainers.is_empty() || !cur.reviewers.is_empty()) {
            out.push(MaintainerSection {
                maintainers: std::mem::take(&mut cur.maintainers),
                reviewers: std::mem::take(&mut cur.reviewers),
                files: std::mem::take(&mut cur.files),
            });
        } else {
            *cur = MaintainerSection { maintainers: vec![], reviewers: vec![], files: vec![] };
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("M:") {
            cur.maintainers.push(rest.trim().to_string());
            in_section = true;
        } else if let Some(rest) = line.strip_prefix("R:") {
            cur.reviewers.push(rest.trim().to_string());
            in_section = true;
        } else if let Some(rest) = line.strip_prefix("F:") {
            cur.files.push(rest.trim().to_string());
            in_section = true;
        } else if line.trim().is_empty() && in_section {
            flush(&mut cur, &mut sections);
            in_section = false;
        }
    }
    flush(&mut cur, &mut sections);
    sections
}

/// MAINTAINERS `F:` patterns are path prefixes / shell globs. A trailing `/`
/// or no wildcard means "this dir/file and everything under it".
fn maintainers_match(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('/') {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if pattern.contains('*') {
        return glob_match(pattern, path);
    }
    // Exact file, or directory prefix.
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

/// Minimal glob matcher with CODEOWNERS/gitignore semantics. A leading `/`
/// anchors to the repo root; without it, the pattern may match at any depth.
/// Supports `*` (within a segment), `**` (any depth), and trailing `/` (dir).
fn glob_match(pattern: &str, path: &str) -> bool {
    let anchored = pattern.starts_with('/');
    let mut pat = pattern.trim_start_matches('/');

    // Bare `*` matches everything.
    if pat == "*" {
        return true;
    }
    // Directory pattern: matches everything under it.
    if let Some(dir) = pat.strip_suffix('/') {
        if anchored || dir.contains('/') {
            return path == dir || path.starts_with(&format!("{dir}/"));
        }
        // Unanchored single-segment dir: match at any depth.
        return any_suffix(path, dir);
    }

    if anchored || pat.contains('/') {
        return glob_segments(pat, path);
    }
    // Unanchored file pattern (e.g. `*.rs`, `Makefile`): match at any depth by
    // testing the pattern against every path suffix that begins a segment.
    let pat_owned;
    if !pat.contains('*') {
        // exact name at any depth
        return path == pat || path.ends_with(&format!("/{pat}"));
    } else {
        pat_owned = pat.to_string();
        pat = &pat_owned;
    }
    if glob_segments(pat, path) {
        return true;
    }
    // Try matching the pattern against each "/"-suffix of the path.
    let mut rest = path;
    while let Some(idx) = rest.find('/') {
        rest = &rest[idx + 1..];
        if glob_segments(pat, rest) {
            return true;
        }
    }
    false
}

/// Whether `dir` appears as a directory at any depth in `path`.
fn any_suffix(path: &str, dir: &str) -> bool {
    let mut rest = path;
    loop {
        if rest == dir || rest.starts_with(&format!("{dir}/")) {
            return true;
        }
        match rest.find('/') {
            Some(i) => rest = &rest[i + 1..],
            None => return false,
        }
    }
}

/// Recursive `*` / `**` glob over the whole string (segment-agnostic for `**`).
fn glob_segments(pat: &str, text: &str) -> bool {
    let pb = pat.as_bytes();
    let tb = text.as_bytes();
    glob_rec(pb, 0, tb, 0)
}

fn glob_rec(p: &[u8], mut pi: usize, t: &[u8], mut ti: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            b'*' => {
                // `**` matches across `/`; single `*` stops at `/`.
                let double = pi + 1 < p.len() && p[pi + 1] == b'*';
                let next = if double { pi + 2 } else { pi + 1 };
                // Try to match the rest at every position.
                if glob_rec(p, next, t, ti) {
                    return true;
                }
                while ti < t.len() {
                    if !double && t[ti] == b'/' {
                        break;
                    }
                    ti += 1;
                    if glob_rec(p, next, t, ti) {
                        return true;
                    }
                }
                return false;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_maintainers_section_and_matches() {
        let text = "\
SCHEDULER\n\
M:\tIngo Molnar <mingo@redhat.com>\n\
M:\tPeter Zijlstra <peterz@infradead.org>\n\
R:\tBen Segall <bsegall@google.com>\n\
F:\tkernel/sched/\n\
F:\tinclude/linux/sched.h\n\
\n\
NETWORKING\n\
M:\tJakub Kicinski <kuba@kernel.org>\n\
F:\tnet/\n\
";
        let secs = parse_maintainers(text);
        assert_eq!(secs.len(), 2);

        let own = Ownership { codeowners: vec![], maintainers: secs, root: ".".into() };
        let o = own.owners_for(Path::new("kernel/sched/core.c"));
        assert!(o.iter().any(|x| x.who.contains("Ingo Molnar") && x.role == "maintainer"));
        assert!(o.iter().any(|x| x.who.contains("Ben Segall") && x.role == "reviewer"));

        let net = own.owners_for(Path::new("net/ipv4/tcp.c"));
        assert!(net.iter().any(|x| x.who.contains("Jakub")));
    }

    #[test]
    fn codeowners_last_match_wins() {
        let rules = parse_codeowners("* @org/default\n/src/payments/ @org/payments\n");
        let own = Ownership { codeowners: rules, maintainers: vec![], root: ".".into() };
        let o = own.owners_for(Path::new("src/payments/charge.rs"));
        assert_eq!(o.first().map(|x| x.who.as_str()), Some("@org/payments"));
        let other = own.owners_for(Path::new("src/other/x.rs"));
        assert_eq!(other.first().map(|x| x.who.as_str()), Some("@org/default"));
    }

    #[test]
    fn glob_matches_double_star() {
        assert!(glob_match("src/**/handlers.rs", "src/a/b/handlers.rs"));
        assert!(glob_match("*.rs", "deep/nested/file.rs"));
        assert!(!glob_match("/src/*.rs", "src/sub/x.rs"));
    }
}
