//! Basic PII/secret redaction applied before any log sample is stored or shown
//! in Slack/CLI output (spec §15.2).

use regex::Regex;
use std::sync::OnceLock;

struct Patterns {
    email: Regex,
    jwt: Regex,
    uuid: Regex,
    ip: Regex,
    token: Regex,
    phone: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        email: Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").unwrap(),
        // header.payload.signature — three base64url segments.
        jwt: Regex::new(r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+").unwrap(),
        uuid: Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
            .unwrap(),
        ip: Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").unwrap(),
        // Long opaque tokens / API keys (>=24 word chars), e.g. sk_live_...
        token: Regex::new(r"\b[A-Za-z0-9_\-]{24,}\b").unwrap(),
        phone: Regex::new(r"\+?\d[\d\-\s().]{7,}\d").unwrap(),
    })
}

/// Redact a single line. Order matters: structured patterns (JWT, UUID, email)
/// run before the generic long-token catch-all.
pub fn redact_line(input: &str) -> String {
    let p = patterns();
    let mut s = p.jwt.replace_all(input, "[REDACTED_JWT]").into_owned();
    s = p.email.replace_all(&s, "[REDACTED_EMAIL]").into_owned();
    s = p.uuid.replace_all(&s, "[REDACTED_UUID]").into_owned();
    s = p.ip.replace_all(&s, "[REDACTED_IP]").into_owned();
    s = p.token.replace_all(&s, "[REDACTED_TOKEN]").into_owned();
    s = p.phone.replace_all(&s, "[REDACTED_PHONE]").into_owned();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email() {
        assert!(redact_line("user alice@example.com hit /x").contains("[REDACTED_EMAIL]"));
    }

    #[test]
    fn redacts_jwt_and_uuid() {
        let line = "tok eyJhbGci.eyJzdWIi.abc123 id 550e8400-e29b-41d4-a716-446655440000";
        let out = redact_line(line);
        assert!(out.contains("[REDACTED_JWT]"));
        assert!(out.contains("[REDACTED_UUID]"));
    }

    #[test]
    fn redacts_ip() {
        assert!(redact_line("from 192.168.1.10 GET /x").contains("[REDACTED_IP]"));
    }

    #[test]
    fn keeps_route_readable() {
        // A short route should survive redaction so verdicts stay useful.
        let out = redact_line("GET /v1/checkout 200");
        assert!(out.contains("/v1/checkout"));
    }
}
