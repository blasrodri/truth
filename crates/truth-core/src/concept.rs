//! Concept resolution: map a fuzzy claim subject ("old checkout", "stripe
//! webhook") to the closest indexed concept/route ("/v1/checkout",
//! "/webhooks/stripe").
//!
//! Resolution is deterministic and dependency-free by default (`FuzzyResolver`,
//! token-overlap + light synonyms). A semantic embedding resolver can be added
//! behind the same `ConceptResolver` trait as a low-confidence fallback.

/// A candidate concept the resolver can match against (an indexed route, env
/// var, dependency name, ...). `label` is what gets matched and returned.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub label: String,
}

impl Candidate {
    pub fn new(label: impl Into<String>) -> Self {
        Candidate { label: label.into() }
    }
}

/// A resolution result: the chosen candidate label and a confidence in [0, 1].
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    pub label: String,
    pub confidence: f32,
}

/// Resolve a free-text subject to the best candidate, if any clears the bar.
pub trait ConceptResolver {
    fn resolve(&self, subject: &str, candidates: &[Candidate]) -> Option<Resolution>;
}

/// Split a string into lowercase alphanumeric word tokens (len ≥ 2).
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

/// Deterministic, dependency-free resolver: Jaccard token overlap between the
/// subject and each candidate, plus a small curated synonym boost for the
/// common engineering vocabulary mismatches (old/legacy → v1, etc.).
pub struct FuzzyResolver {
    /// Minimum score to accept a match (avoid resolving to noise).
    pub threshold: f32,
}

impl Default for FuzzyResolver {
    fn default() -> Self {
        FuzzyResolver { threshold: 0.15 }
    }
}

/// (subject keyword, candidate substring, boost) — cheap curated synonyms.
const SYNONYMS: &[(&str, &str, f32)] = &[
    ("old", "v1", 0.4),
    ("legacy", "v1", 0.4),
    ("previous", "v1", 0.3),
    ("new", "v2", 0.3),
    ("webhook", "webhook", 0.3),
    ("hook", "webhook", 0.25),
    ("signin", "login", 0.3),
    ("sign", "login", 0.2),
    ("auth", "login", 0.2),
    ("health", "health", 0.3),
    ("liveness", "health", 0.3),
    ("readiness", "health", 0.3),
];

impl FuzzyResolver {
    fn score(&self, subject_lower: &str, subject_tokens: &[String], candidate: &str) -> f32 {
        let cand_tokens = tokens(candidate);
        let inter = subject_tokens.iter().filter(|t| cand_tokens.contains(t)).count() as f32;
        let union = (subject_tokens.len() + cand_tokens.len()) as f32 - inter;
        let mut score = if union > 0.0 { inter / union } else { 0.0 };

        let cand_lower = candidate.to_lowercase();
        for (kw, sub, boost) in SYNONYMS {
            if subject_lower.contains(kw) && cand_lower.contains(sub) {
                score += boost;
            }
        }
        score
    }
}

impl ConceptResolver for FuzzyResolver {
    fn resolve(&self, subject: &str, candidates: &[Candidate]) -> Option<Resolution> {
        if candidates.is_empty() {
            return None;
        }
        let subject_lower = subject.to_lowercase();
        let subject_tokens = tokens(subject);

        let mut best: Option<(f32, &str)> = None;
        for c in candidates {
            let s = self.score(&subject_lower, &subject_tokens, &c.label);
            if best.map(|(bs, _)| s > bs).unwrap_or(true) {
                best = Some((s, &c.label));
            }
        }
        match best {
            Some((score, label)) if score >= self.threshold => Some(Resolution {
                label: label.to_string(),
                confidence: score.min(1.0),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(v: &[&str]) -> Vec<Candidate> {
        v.iter().map(|s| Candidate::new(*s)).collect()
    }

    fn routes() -> Vec<Candidate> {
        cands(&[
            "/auth/login",
            "/health",
            "/users/profile",
            "/v1/checkout",
            "/v2/checkout",
            "/webhooks/stripe",
        ])
    }

    #[test]
    fn resolves_token_overlap_cases() {
        let r = FuzzyResolver::default();
        let cs = routes();
        for (q, expected) in [
            ("old checkout", "/v1/checkout"),
            ("legacy checkout endpoint", "/v1/checkout"),
            ("stripe webhook", "/webhooks/stripe"),
            ("login", "/auth/login"),
            ("user profile", "/users/profile"),
            ("the checkout v2 route", "/v2/checkout"),
        ] {
            let got = r.resolve(q, &cs).map(|x| x.label);
            assert_eq!(got.as_deref(), Some(expected), "query: {q}");
        }
    }

    #[test]
    fn returns_none_below_threshold() {
        let r = FuzzyResolver::default();
        assert!(r.resolve("completely unrelated gibberish xyz", &routes()).is_none());
    }

    #[test]
    fn empty_candidates_is_none() {
        let r = FuzzyResolver::default();
        assert!(r.resolve("checkout", &[]).is_none());
    }
}
