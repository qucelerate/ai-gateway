//! Request-rate, token-rate and concurrency limits for an organization, a
//! team, a service account, or one API key: the levels budgets have.
//!
//! Every policy covering a request applies: a request is checked against its
//! key's policy, its service account's, its team's, and the organization's.
//! The request rate counts requests admitted in the last 60 seconds; the
//! token rate counts their tokens in the same window (each request its upper
//! bound while it runs, then what its usage reported), as an end user's plan
//! does; the concurrency limit counts admitted requests that have not
//! finished. All are enforced when a request's budget is reserved, so an
//! idempotent retry of an admitted request is never limited again.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::identity::{ApiKeyId, ServiceAccountId, TeamId};

/// Upper bound for the request rate and concurrency; the migration checks
/// the same range.
pub const MAX_LIMIT: u32 = 1_000_000;

/// Upper bound for the token rate, as for an end user's plan.
pub const MAX_TOKENS_PER_MINUTE: u32 = 1_000_000_000;

/// The rate window, in seconds.
pub const RATE_WINDOW_SECONDS: i64 = 60;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RateLimitError {
    #[error("set at least one of requests_per_minute, tokens_per_minute and max_concurrent")]
    Empty,
    #[error(
        "requests_per_minute and max_concurrent must be between 1 and 1000000, tokens_per_minute between 1 and 1000000000"
    )]
    OutOfRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLimitScope {
    Organization,
    Team(TeamId),
    ServiceAccount(ServiceAccountId),
    /// One API key, revoked or not.
    ApiKey(ApiKeyId),
}

impl RateLimitScope {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Organization => "organization",
            Self::Team(_) => "team",
            Self::ServiceAccount(_) => "service-account",
            Self::ApiKey(_) => "api-key",
        }
    }

    pub fn team_id(self) -> Option<TeamId> {
        match self {
            Self::Team(team) => Some(team),
            _ => None,
        }
    }

    pub fn service_account_id(self) -> Option<ServiceAccountId> {
        match self {
            Self::ServiceAccount(service_account) => Some(service_account),
            _ => None,
        }
    }

    pub fn api_key_id(self) -> Option<ApiKeyId> {
        match self {
            Self::ApiKey(key) => Some(key),
            _ => None,
        }
    }
}

/// A set request as the admin API accepts it. An absent limit is unlimited.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitInput {
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

impl RateLimitInput {
    pub fn validate(self) -> Result<Self, RateLimitError> {
        if self.requests_per_minute.is_none()
            && self.tokens_per_minute.is_none()
            && self.max_concurrent.is_none()
        {
            return Err(RateLimitError::Empty);
        }
        let within = |limit: Option<u32>, max: u32| limit.is_none_or(|l| (1..=max).contains(&l));
        if !within(self.requests_per_minute, MAX_LIMIT)
            || !within(self.max_concurrent, MAX_LIMIT)
            || !within(self.tokens_per_minute, MAX_TOKENS_PER_MINUTE)
        {
            return Err(RateLimitError::OutOfRange);
        }
        Ok(self)
    }
}

/// A stored policy as the admin API returns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RateLimitPolicyView {
    pub id: Uuid,
    /// `organization`, `team`, `service-account`, or `api-key`.
    pub scope: String,
    pub team_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// Set for an `api-key` policy. A revoked key's policy stays listed.
    #[serde(default)]
    pub api_key_id: Option<Uuid>,
    pub requests_per_minute: Option<u32>,
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
    pub max_concurrent: Option<u32>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Which limit rejected a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RateLimitKind {
    RequestsPerMinute,
    TokensPerMinute,
    Concurrency,
}

impl RateLimitKind {
    /// The policy field that sets the limit, as a refusal names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestsPerMinute => "requests_per_minute",
            Self::TokensPerMinute => "tokens_per_minute",
            Self::Concurrency => "max_concurrent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_needs_one_limit_within_range() {
        let input = |requests_per_minute, max_concurrent| RateLimitInput {
            requests_per_minute,
            tokens_per_minute: None,
            max_concurrent,
        };
        assert_eq!(input(None, None).validate(), Err(RateLimitError::Empty));
        assert_eq!(
            input(Some(0), None).validate(),
            Err(RateLimitError::OutOfRange)
        );
        assert_eq!(
            input(Some(10), Some(MAX_LIMIT + 1)).validate(),
            Err(RateLimitError::OutOfRange)
        );
        assert!(input(Some(MAX_LIMIT), None).validate().is_ok());
        assert!(input(None, Some(1)).validate().is_ok());
        let tokens = |tokens_per_minute| RateLimitInput {
            requests_per_minute: None,
            tokens_per_minute,
            max_concurrent: None,
        };
        assert!(tokens(Some(MAX_TOKENS_PER_MINUTE)).validate().is_ok());
        assert_eq!(
            tokens(Some(MAX_TOKENS_PER_MINUTE + 1)).validate(),
            Err(RateLimitError::OutOfRange)
        );
        assert_eq!(tokens(Some(0)).validate(), Err(RateLimitError::OutOfRange));
    }
}
