//! Spending budgets for an organization, a team, a service account, or one
//! API key.
//!
//! A budget is an amount requests may spend, in money units of one currency
//! (10⁻⁶ of its smallest unit, as every amount in accounting). The gateway
//! never admits a request its budgets cannot cover: each request reserves
//! its largest possible cost, as the gateway prices it, before it is sent,
//! and settles at its actual cost afterwards. That bounds the gateway's own
//! accounting, not the provider's invoice, which the provider meters itself
//! (a model may overrun its cap, a price may be stale). A budget repeats
//! each calendar month in UTC, or covers one fixed window. Every budget
//! covering a request applies: a request made with an API key must fit
//! that key's budget, its service account's, its team's and the
//! organization's. A request no budget covers is refused, so spending is never
//! unlimited by omission.

use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::identity::{ApiKeyId, ServiceAccountId, TeamId};
use super::rate_limit::RateLimitScope;

/// What one budget covers: the scopes rate limits have, and one API key, so
/// apps sharing a service account can each have a limit of their own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetScope {
    Organization,
    Team(TeamId),
    ServiceAccount(ServiceAccountId),
    ApiKey(ApiKeyId),
}

impl BudgetScope {
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
            Self::ServiceAccount(account) => Some(account),
            _ => None,
        }
    }

    pub fn api_key_id(self) -> Option<ApiKeyId> {
        match self {
            Self::ApiKey(key) => Some(key),
            _ => None,
        }
    }

    /// The rate-limit scope with the same subject: budgets and rate limits
    /// have the same levels.
    pub fn rate_limit_scope(self) -> RateLimitScope {
        match self {
            Self::Organization => RateLimitScope::Organization,
            Self::Team(team) => RateLimitScope::Team(team),
            Self::ServiceAccount(account) => RateLimitScope::ServiceAccount(account),
            Self::ApiKey(key) => RateLimitScope::ApiKey(key),
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BudgetError {
    #[error("currency must be three uppercase letters")]
    Currency,
    #[error("limit_minor must be zero or more")]
    Limit,
    #[error("a monthly budget takes no starts_at or ends_at")]
    MonthlyWindow,
    #[error("a fixed budget needs starts_at before ends_at")]
    FixedWindow,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetPeriod {
    /// Each calendar month in UTC, from a fresh balance.
    Monthly,
    /// One window, `[starts_at, ends_at)`.
    Fixed,
}

impl BudgetPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Fixed => "fixed",
        }
    }
}

/// A set request as the admin API accepts it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetInput {
    #[serde(default = "default_currency")]
    pub currency: String,
    pub limit_minor: i64,
    pub period: BudgetPeriod,
    #[serde(default)]
    pub starts_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ends_at: Option<DateTime<Utc>>,
}

fn default_currency() -> String {
    "USD".to_owned()
}

impl BudgetInput {
    pub fn validate(self) -> Result<Self, BudgetError> {
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(BudgetError::Currency);
        }
        if self.limit_minor < 0 {
            return Err(BudgetError::Limit);
        }
        match (self.period, self.starts_at, self.ends_at) {
            (BudgetPeriod::Monthly, None, None) => Ok(self),
            (BudgetPeriod::Monthly, _, _) => Err(BudgetError::MonthlyWindow),
            (BudgetPeriod::Fixed, Some(start), Some(end)) if end > start => Ok(self),
            (BudgetPeriod::Fixed, _, _) => Err(BudgetError::FixedWindow),
        }
    }

    /// The window this budget charges at `now`, if any: the UTC month for a
    /// monthly budget, its own window for a fixed one that has begun and not
    /// ended.
    pub fn window_at(&self, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match self.period {
            BudgetPeriod::Monthly => Some(month_of(now)),
            BudgetPeriod::Fixed => match (self.starts_at, self.ends_at) {
                (Some(start), Some(end)) if start <= now && now < end => Some((start, end)),
                _ => None,
            },
        }
    }
}

/// The UTC calendar month containing `at`.
pub fn month_of(at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = Utc
        .with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .single()
        .expect("the first of a month at midnight UTC exists");
    let (year, month) = if at.month() == 12 {
        (at.year() + 1, 1)
    } else {
        (at.year(), at.month() + 1)
    };
    let end = Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("the first of a month at midnight UTC exists");
    (start, end)
}

/// What the current window of a budget has used.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetUsage {
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    /// Settled spending in the window.
    pub consumed_minor: i64,
    /// Held by requests still in flight.
    pub reserved_minor: i64,
}

/// A stored budget as the admin API returns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetPolicyView {
    pub id: Uuid,
    /// `organization`, `team`, `service-account`, or `api-key`.
    pub scope: String,
    pub team_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// Set for an `api-key` budget. A revoked key's budget stays listed.
    #[serde(default)]
    pub api_key_id: Option<Uuid>,
    pub currency: String,
    pub limit_minor: i64,
    pub period: BudgetPeriod,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    /// The window in force now; `null` when none is (a fixed budget that
    /// has not begun or has ended).
    pub current: Option<BudgetUsage>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn input(period: BudgetPeriod, starts_at: Option<&str>, ends_at: Option<&str>) -> BudgetInput {
        BudgetInput {
            currency: "USD".into(),
            limit_minor: 10_000_000_000,
            period,
            starts_at: starts_at.map(at),
            ends_at: ends_at.map(at),
        }
    }

    #[test]
    fn months_are_utc_calendar_months() {
        assert_eq!(
            month_of(at("2026-10-31T23:59:59Z")),
            (at("2026-10-01T00:00:00Z"), at("2026-11-01T00:00:00Z"))
        );
        assert_eq!(
            month_of(at("2026-12-15T08:00:00-08:00")),
            (at("2026-12-01T00:00:00Z"), at("2027-01-01T00:00:00Z"))
        );
    }

    #[test]
    fn inputs_are_checked() {
        assert!(input(BudgetPeriod::Monthly, None, None).validate().is_ok());
        assert_eq!(
            input(BudgetPeriod::Monthly, Some("2026-10-01T00:00:00Z"), None).validate(),
            Err(BudgetError::MonthlyWindow)
        );
        assert_eq!(
            input(
                BudgetPeriod::Fixed,
                Some("2026-10-02T00:00:00Z"),
                Some("2026-10-01T00:00:00Z")
            )
            .validate(),
            Err(BudgetError::FixedWindow)
        );
        assert_eq!(
            BudgetInput {
                currency: "usd".into(),
                ..input(BudgetPeriod::Monthly, None, None)
            }
            .validate(),
            Err(BudgetError::Currency)
        );
        assert_eq!(
            BudgetInput {
                limit_minor: -1,
                ..input(BudgetPeriod::Monthly, None, None)
            }
            .validate(),
            Err(BudgetError::Limit)
        );
        // Currency defaults to USD.
        let parsed: BudgetInput =
            serde_json::from_str(r#"{"limit_minor": 1, "period": "monthly"}"#).unwrap();
        assert_eq!(parsed.currency, "USD");
    }

    #[test]
    fn the_window_in_force_depends_on_the_period() {
        let now = at("2026-10-08T12:00:00Z");
        assert_eq!(
            input(BudgetPeriod::Monthly, None, None).window_at(now),
            Some(month_of(now))
        );
        let fixed = input(
            BudgetPeriod::Fixed,
            Some("2026-10-01T00:00:00Z"),
            Some("2026-10-15T00:00:00Z"),
        );
        assert_eq!(
            fixed.window_at(now),
            Some((at("2026-10-01T00:00:00Z"), at("2026-10-15T00:00:00Z")))
        );
        assert_eq!(fixed.window_at(at("2026-10-15T00:00:00Z")), None);
        assert_eq!(fixed.window_at(at("2026-09-30T00:00:00Z")), None);
    }
}
