//! All domain enums and their stable string mapping used for the SQLite `TEXT`
//! columns. This module is the single place where the wire/DB string form of
//! every enum is defined, so it must stay in sync with the migration in
//! `truth-db`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Generate an enum with a 1:1 mapping to a stable snake_case-ish DB string.
///
/// Produces:
/// * `serde` (de)serialization using the given string for each variant
/// * `Display` returning the DB string
/// * `FromStr` parsing the DB string (error = `EnumParseError`)
/// * `as_db_str(&self) -> &'static str`
macro_rules! db_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $variant:ident => $s:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $( #[serde(rename = $s)] $variant ),+ }

        impl $name {
            pub fn as_db_str(&self) -> &'static str {
                match self { $( $name::$variant => $s ),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_db_str())
            }
        }

        impl FromStr for $name {
            type Err = EnumParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $s => Ok($name::$variant), )+
                    other => Err(EnumParseError {
                        enum_name: stringify!($name),
                        value: other.to_string(),
                    }),
                }
            }
        }
    };
}

#[derive(Debug, thiserror::Error)]
#[error("invalid value `{value}` for enum {enum_name}")]
pub struct EnumParseError {
    pub enum_name: &'static str,
    pub value: String,
}

db_enum! {
    /// Where an artifact / evidence originated.
    pub enum SourceKind {
        Slack => "slack",
        GitRepo => "git_repo",
        Loki => "loki",
        LocalLogs => "local_logs",
        Github => "github",
        Manual => "manual",
    }
}

db_enum! {
    pub enum ArtifactKind {
        SlackMessage => "slack_message",
        SlackThread => "slack_thread",
        SourceFile => "source_file",
        MarkdownDoc => "markdown_doc",
        ConfigFile => "config_file",
        LogQueryResult => "log_query_result",
        LocalLogFile => "local_log_file",
        PullRequest => "pull_request",
        Issue => "issue",
        Commit => "commit",
    }
}

db_enum! {
    pub enum EvidenceType {
        Assertion => "assertion",
        Definition => "definition",
        Observation => "observation",
        Change => "change",
        Decision => "decision",
        TestEvidence => "test_evidence",
    }
}

db_enum! {
    /// Authority of a piece of evidence. Ordering for verdicts is handled in
    /// `crate::verdict`, not by the discriminant here.
    pub enum Authority {
        RuntimeLogs => "runtime_logs",
        Metrics => "metrics",
        ProductionConfig => "production_config",
        Test => "test",
        Code => "code",
        Config => "config",
        OfficialDoc => "official_doc",
        Adr => "adr",
        PullRequest => "pull_request",
        Issue => "issue",
        SlackMessage => "slack_message",
        Unknown => "unknown",
    }
}

db_enum! {
    pub enum Trigger {
        SlackMention => "slack_mention",
        Cli => "cli",
        ManualApi => "manual_api",
        Scheduled => "scheduled",
    }
}

db_enum! {
    pub enum QuestionType {
        Usage => "usage",
        IncidentStatus => "incident_status",
        CurrentImplementation => "current_implementation",
        CurrentRuntimeState => "current_runtime_state",
        DocumentationAccuracy => "documentation_accuracy",
        ConfigValue => "config_value",
        Unknown => "unknown",
    }
}

db_enum! {
    pub enum VerdictStatus {
        Supported => "supported",
        Contradicted => "contradicted",
        PartiallySupported => "partially_supported",
        Inconclusive => "inconclusive",
        NeedsMoreContext => "needs_more_context",
    }
}

db_enum! {
    pub enum CheckStatus {
        Queued => "queued",
        Running => "running",
        Completed => "completed",
        Failed => "failed",
    }
}

db_enum! {
    /// How an `EvidenceItem` was produced.
    pub enum ExtractionMethod {
        Deterministic => "deterministic",
        Regex => "regex",
        Ast => "ast",
        Llm => "llm",
        Manual => "manual",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_db_string() {
        for s in [
            VerdictStatus::Supported,
            VerdictStatus::PartiallySupported,
            VerdictStatus::NeedsMoreContext,
        ] {
            let parsed: VerdictStatus = s.as_db_str().parse().unwrap();
            assert_eq!(parsed, s);
        }
        assert_eq!(Authority::RuntimeLogs.as_db_str(), "runtime_logs");
        assert!("nope".parse::<VerdictStatus>().is_err());
    }

    #[test]
    fn serde_uses_db_string() {
        let j = serde_json::to_string(&EvidenceType::Observation).unwrap();
        assert_eq!(j, "\"observation\"");
        let back: EvidenceType = serde_json::from_str(&j).unwrap();
        assert_eq!(back, EvidenceType::Observation);
    }
}
