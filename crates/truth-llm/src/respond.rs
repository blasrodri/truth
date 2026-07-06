//! Deterministic response generation. Produces the Slack-style verdict block
//! (spec §14.3) from structured verdict + evidence + caveats. No LLM is needed;
//! the response generator only ever sees structured data, never raw logs.

use truth_core::enums::VerdictStatus;
use truth_core::verdict::VerdictDecision;

pub struct ResponseInput<'a> {
    pub claim_text: &'a str,
    pub decision: &'a VerdictDecision,
    /// Human-readable evidence lines (already formatted/cited).
    pub evidence_lines: &'a [String],
}

fn headline(status: VerdictStatus) -> &'static str {
    match status {
        VerdictStatus::Supported => "Supported.",
        VerdictStatus::Contradicted => "Contradicted.",
        VerdictStatus::PartiallySupported => "Partially supported.",
        VerdictStatus::Inconclusive => "Inconclusive.",
        VerdictStatus::NeedsMoreContext => "Needs more context.",
    }
}

/// Render the final response text.
pub fn render(input: &ResponseInput) -> String {
    let d = input.decision;
    let mut out = String::new();
    out.push_str(headline(d.status));
    out.push_str("\n\n");

    // Short explanation line.
    out.push_str(&one_line_explanation(input));
    out.push('\n');

    if !input.evidence_lines.is_empty() {
        out.push_str("\nEvidence:\n");
        for line in input.evidence_lines {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }

    let caveats: Vec<&String> = d.caveats.iter().collect();
    if !caveats.is_empty() {
        out.push_str("\nCaveats:\n");
        for c in caveats {
            out.push_str("- ");
            out.push_str(c);
            out.push('\n');
        }
    }

    if let Some(action) = &d.suggested_action {
        out.push_str("\nSuggested action:\n- ");
        out.push_str(action);
        out.push('\n');
    }

    out.trim_end().to_string()
}

fn one_line_explanation(input: &ResponseInput) -> String {
    let claim = input.claim_text.trim_end_matches('.');
    match input.decision.status {
        VerdictStatus::Contradicted => {
            format!("The claim \u{201c}{claim}\u{201d} does not match the evidence I found.")
        }
        VerdictStatus::Supported => {
            format!("The claim \u{201c}{claim}\u{201d} is consistent with the evidence I found.")
        }
        VerdictStatus::PartiallySupported => {
            format!("The claim \u{201c}{claim}\u{201d} is partly consistent with the evidence.")
        }
        VerdictStatus::Inconclusive => {
            format!("I could not verify \u{201c}{claim}\u{201d} from the configured sources.")
        }
        VerdictStatus::NeedsMoreContext => {
            format!("I need more detail to check \u{201c}{claim}\u{201d}.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_contradicted_block() {
        let d = VerdictDecision {
            status: VerdictStatus::Contradicted,
            confidence: 0.94,
            evidence_ids: vec![],
            caveats: vec!["This only checks the configured Loki source.".into()],
            suggested_action: None,
            structured: true,
            unproven: false,
        };
        let out = render(&ResponseInput {
            claim_text: "Nobody uses /v1/checkout anymore.",
            decision: &d,
            evidence_lines: &["Loki route_count for `/v1/checkout`: 12481".to_string()],
        });
        assert!(out.starts_with("Contradicted."));
        assert!(out.contains("Evidence:"));
        assert!(out.contains("Caveats:"));
    }
}
