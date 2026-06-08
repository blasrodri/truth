//! Structured claim produced by claim extraction (spec §11.1) plus the closed
//! set of claim types the MVP supports.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::str::FromStr;

/// Allowed `claim_type` values for v0.1 (spec §5 / §11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimType {
    UsageCount,
    ErrorStillHappening,
    LatestOccurrence,
    RouteExists,
    /// A code symbol (function/method/struct/type/helper) exists or was
    /// added/removed/renamed.
    SymbolExists,
    ConfigValue,
    EnvVarExists,
    DependencyUsed,
    RetryCount,
    TimeoutValue,
    VersionRequired,
    JobLastSuccess,
    FeatureFlagEnabled,
    Unknown,
}

/// Comparison operator extracted from a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOperator {
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    Exists,
    NotExists,
    #[default]
    Unknown,
}

/// A structured, checkable claim (spec §11.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredClaim {
    pub is_checkable: bool,
    pub claim_type: ClaimType,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    #[serde(default)]
    pub operator: ClaimOperator,
    /// Expected value, if the claim asserts one. JSON to allow numbers/strings/bools.
    pub value: Option<Value>,
    pub unit: Option<String>,
    /// Free-form time window hint ("recent", "7d", ...). Resolved by adapters.
    pub time_window: Option<String>,
    pub environment: Option<String>,
    pub confidence: f32,
    #[serde(default)]
    pub needs_clarification: bool,
    pub clarification_question: Option<String>,
}

impl StructuredClaim {
    /// A claim that could not be turned into anything checkable.
    pub fn unknown(reason: impl Into<String>) -> Self {
        StructuredClaim {
            is_checkable: false,
            claim_type: ClaimType::Unknown,
            subject: None,
            predicate: None,
            operator: ClaimOperator::Unknown,
            value: None,
            unit: None,
            time_window: None,
            environment: None,
            confidence: 0.0,
            needs_clarification: true,
            clarification_question: Some(reason.into()),
        }
    }

    /// Numeric expected value if present.
    pub fn expected_number(&self) -> Option<f64> {
        self.value.as_ref().and_then(|v| v.as_f64())
    }
}

impl FromStr for ClaimType {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_value(Value::String(s.to_string())).map_err(|_| ())
    }
}
