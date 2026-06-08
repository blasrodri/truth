//! Deterministic query planner: turns a structured claim into a plan made only
//! of allowed safe templates (spec §11.2). No LLM is required here.

use truth_core::claim::{ClaimType, StructuredClaim};
use truth_core::enums::SourceKind;
use truth_core::query::{PlannedQuery, QueryPlan, QueryType};

/// Build a query plan for a claim. `loki_enabled` / `service` come from config.
pub fn plan_for(
    claim: &StructuredClaim,
    loki_enabled: bool,
    default_service: Option<&str>,
) -> QueryPlan {
    let mut queries = Vec::new();
    let subject = claim.subject.clone();
    let window = claim.time_window.clone();
    let environment = claim.environment.clone();
    let service = default_service.map(str::to_string);

    let log_source = if loki_enabled {
        SourceKind::Loki
    } else {
        SourceKind::LocalLogs
    };

    match claim.claim_type {
        ClaimType::UsageCount => {
            queries.push(log_query(
                log_source,
                QueryType::RouteCount,
                subject.clone(),
                window.clone(),
                environment.clone(),
                service.clone(),
            ));
            // Cross-check existence in the repo.
            queries.push(repo_query(QueryType::RouteExists, subject.clone()));
        }
        ClaimType::DependencyUsed => {
            // A dependency is answered by the manifest (declared?) plus code use.
            queries.push(repo_query(QueryType::DependencyExists, subject.clone()));
            queries.push(log_query(
                log_source,
                QueryType::RouteCount,
                subject.clone(),
                window.clone(),
                environment.clone(),
                service.clone(),
            ));
        }
        ClaimType::ErrorStillHappening => {
            queries.push(log_query(
                log_source,
                QueryType::ErrorCount,
                subject.clone(),
                window.clone(),
                environment.clone(),
                service.clone(),
            ));
        }
        ClaimType::LatestOccurrence | ClaimType::JobLastSuccess => {
            let qt = if matches!(claim.claim_type, ClaimType::JobLastSuccess) {
                QueryType::JobSuccess
            } else {
                QueryType::LatestOccurrence
            };
            queries.push(log_query(
                log_source,
                qt,
                subject.clone(),
                window.clone(),
                environment.clone(),
                service.clone(),
            ));
        }
        ClaimType::RouteExists => {
            queries.push(repo_query(QueryType::RouteExists, subject.clone()));
        }
        ClaimType::EnvVarExists => {
            queries.push(repo_query(QueryType::EnvVarExists, subject.clone()));
        }
        ClaimType::ConfigValue | ClaimType::FeatureFlagEnabled => {
            queries.push(repo_query(QueryType::ConfigValue, subject.clone()));
        }
        ClaimType::RetryCount | ClaimType::TimeoutValue | ClaimType::VersionRequired => {
            queries.push(repo_query(QueryType::ConstantValue, subject.clone()));
        }
        ClaimType::Unknown => {}
    }

    QueryPlan { queries }
}

fn log_query(
    source: SourceKind,
    query_type: QueryType,
    route: Option<String>,
    window: Option<String>,
    environment: Option<String>,
    service: Option<String>,
) -> PlannedQuery {
    PlannedQuery {
        source,
        query_type,
        route,
        pattern: None,
        name: None,
        window,
        environment,
        service,
    }
}

fn repo_query(query_type: QueryType, name: Option<String>) -> PlannedQuery {
    PlannedQuery {
        source: SourceKind::GitRepo,
        query_type,
        route: name.clone(),
        pattern: None,
        name,
        window: None,
        environment: None,
        service: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{ClaimExtractor, RegexExtractor};

    #[test]
    fn usage_claim_plans_logs_and_repo() {
        let claim = RegexExtractor.extract("Nobody uses /v1/checkout anymore.");
        let plan = plan_for(&claim, false, Some("api"));
        assert_eq!(plan.queries.len(), 2);
        assert!(plan
            .queries
            .iter()
            .any(|q| q.query_type == QueryType::RouteCount && q.source == SourceKind::LocalLogs));
        assert!(plan.queries.iter().any(|q| q.query_type == QueryType::RouteExists));
    }

    #[test]
    fn retry_claim_plans_constant_lookup() {
        let claim = RegexExtractor.extract("We retry payments 3 times.");
        let plan = plan_for(&claim, true, None);
        assert_eq!(plan.queries.len(), 1);
        assert_eq!(plan.queries[0].query_type, QueryType::ConstantValue);
    }
}
