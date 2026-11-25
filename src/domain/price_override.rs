//! Organization price overrides: a price that replaces the catalog price of
//! one deployment (by name) and operation for a validity window.
//!
//! Overrides are matched by deployment name rather than release rows, so one
//! survives catalog upgrades. They are immutable except for a one-time
//! revocation, and never apply retroactively: a reservation snapshots the
//! price it was made with, so creating, expiring or revoking an override only
//! changes what later requests are charged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::{
    accounting::{Currency, PriceSnapshot, PricingConfidence},
    catalog::CatalogOperation,
};

const MAX_DEPLOYMENT_LENGTH: usize = 255;
const MAX_REASON_LENGTH: usize = 1024;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PriceOverrideError {
    #[error("deployment must be between 1 and 255 characters")]
    Deployment,
    #[error("operation must be one of chat, chat_stream, embeddings")]
    Operation,
    #[error("prices must be non-negative")]
    Price,
    #[error("currency must be a three-letter uppercase ISO 4217 code")]
    Currency,
    #[error("confidence must be known or conservative")]
    Confidence,
    #[error("reason must be between 1 and 1024 characters")]
    Reason,
    #[error("effective_until must be later than effective_from and in the future")]
    Window,
}

/// A create request as the admin API accepts it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceOverrideInput {
    pub deployment: String,
    pub operation: String,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    #[serde(default)]
    pub cached_input_minor_per_token: i64,
    #[serde(default)]
    pub reasoning_minor_per_token: i64,
    #[serde(default)]
    pub other_minor_per_token: i64,
    pub currency: String,
    pub confidence: String,
    pub reason: String,
    /// Defaults to now. A start in the past starts now: an override never
    /// applies retroactively.
    #[serde(default)]
    pub effective_from: Option<DateTime<Utc>>,
    /// Open-ended when absent.
    #[serde(default)]
    pub effective_until: Option<DateTime<Utc>>,
}

/// A validated create request. `effective_from` is at least `now`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewPriceOverride {
    pub deployment: String,
    pub operation: CatalogOperation,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub currency: Currency,
    pub confidence: PricingConfidence,
    pub reason: String,
    pub effective_from: DateTime<Utc>,
    pub effective_until: Option<DateTime<Utc>>,
}

impl PriceOverrideInput {
    pub fn validate(self, now: DateTime<Utc>) -> Result<NewPriceOverride, PriceOverrideError> {
        if self.deployment.is_empty() || self.deployment.len() > MAX_DEPLOYMENT_LENGTH {
            return Err(PriceOverrideError::Deployment);
        }
        let operation = self
            .operation
            .parse::<CatalogOperation>()
            .map_err(|_| PriceOverrideError::Operation)?;
        // The snapshot is the accounting path's own price check; the version
        // here is a placeholder for the id-derived one.
        PriceSnapshot::configured_with_breakdown(
            self.input_minor_per_token,
            self.output_minor_per_token,
            self.cached_input_minor_per_token,
            self.reasoning_minor_per_token,
            self.other_minor_per_token,
            "override",
            PricingConfidence::Known,
        )
        .map_err(|_| PriceOverrideError::Price)?;
        let currency = Currency::new(self.currency).map_err(|_| PriceOverrideError::Currency)?;
        let confidence = match self.confidence.as_str() {
            "known" => PricingConfidence::Known,
            "conservative" => PricingConfidence::Conservative,
            _ => return Err(PriceOverrideError::Confidence),
        };
        let reason = self.reason.trim();
        if reason.is_empty() || reason.chars().count() > MAX_REASON_LENGTH {
            return Err(PriceOverrideError::Reason);
        }
        let effective_from = self.effective_from.map_or(now, |from| from.max(now));
        if self
            .effective_until
            .is_some_and(|until| until <= effective_from)
        {
            return Err(PriceOverrideError::Window);
        }
        Ok(NewPriceOverride {
            deployment: self.deployment,
            operation,
            input_minor_per_token: self.input_minor_per_token,
            output_minor_per_token: self.output_minor_per_token,
            cached_input_minor_per_token: self.cached_input_minor_per_token,
            reasoning_minor_per_token: self.reasoning_minor_per_token,
            other_minor_per_token: self.other_minor_per_token,
            currency,
            confidence,
            reason: reason.to_owned(),
            effective_from,
            effective_until: self.effective_until,
        })
    }
}

/// A revocation request.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceOverrideRevocation {
    pub reason: String,
}

impl PriceOverrideRevocation {
    pub fn validate(self) -> Result<String, PriceOverrideError> {
        let reason = self.reason.trim();
        if reason.is_empty() || reason.chars().count() > MAX_REASON_LENGTH {
            return Err(PriceOverrideError::Reason);
        }
        Ok(reason.to_owned())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceOverrideStatus {
    /// Not yet effective.
    Scheduled,
    Active,
    Expired,
    Revoked,
}

impl PriceOverrideStatus {
    pub fn at(
        now: DateTime<Utc>,
        effective_from: DateTime<Utc>,
        effective_until: Option<DateTime<Utc>>,
        revoked_at: Option<DateTime<Utc>>,
    ) -> Self {
        if revoked_at.is_some() {
            Self::Revoked
        } else if now < effective_from {
            Self::Scheduled
        } else if effective_until.is_some_and(|until| now >= until) {
            Self::Expired
        } else {
            Self::Active
        }
    }
}

/// What the admin API returns for an override.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceOverrideView {
    pub id: Uuid,
    pub deployment: String,
    pub operation: String,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub currency: String,
    pub confidence: String,
    /// The price version reservations made with this override record.
    pub price_version: String,
    pub reason: String,
    pub effective_from: DateTime<Utc>,
    pub effective_until: Option<DateTime<Utc>>,
    /// Status when the response was produced.
    pub status: PriceOverrideStatus,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by: Option<Uuid>,
    pub revoke_reason: Option<String>,
    /// Resource version; also the `ETag`.
    pub version: u64,
}

/// The price version recorded for reservations priced by override `id`. The
/// database requires the same value.
pub fn price_version(id: Uuid) -> String {
    format!("override:{id}")
}

/// Optional list filters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PriceOverrideQuery {
    pub deployment: Option<String>,
    pub operation: Option<CatalogOperation>,
}

impl PriceOverrideQuery {
    pub fn new(
        deployment: Option<String>,
        operation: Option<String>,
    ) -> Result<Self, PriceOverrideError> {
        if deployment
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.len() > MAX_DEPLOYMENT_LENGTH)
        {
            return Err(PriceOverrideError::Deployment);
        }
        let operation = operation
            .map(|value| value.parse::<CatalogOperation>())
            .transpose()
            .map_err(|_| PriceOverrideError::Operation)?;
        Ok(Self {
            deployment,
            operation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn input() -> PriceOverrideInput {
        PriceOverrideInput {
            deployment: "gpt-main".into(),
            operation: "chat".into(),
            input_minor_per_token: 3,
            output_minor_per_token: 5,
            cached_input_minor_per_token: 0,
            reasoning_minor_per_token: 0,
            other_minor_per_token: 0,
            currency: "USD".into(),
            confidence: "known".into(),
            reason: "  negotiated rate  ".into(),
            effective_from: None,
            effective_until: None,
        }
    }

    #[test]
    fn validation_defaults_start_to_now_and_trims_reason() {
        let now = Utc::now();
        let valid = input().validate(now).unwrap();
        assert_eq!(valid.effective_from, now);
        assert_eq!(valid.reason, "negotiated rate");
        assert_eq!(valid.operation, CatalogOperation::Chat);
    }

    #[test]
    fn past_starts_are_clamped_to_now() {
        let now = Utc::now();
        let valid = PriceOverrideInput {
            effective_from: Some(now - Duration::days(3)),
            ..input()
        }
        .validate(now)
        .unwrap();
        assert_eq!(valid.effective_from, now);
        let later = now + Duration::hours(1);
        let scheduled = PriceOverrideInput {
            effective_from: Some(later),
            ..input()
        }
        .validate(now)
        .unwrap();
        assert_eq!(scheduled.effective_from, later);
    }

    #[test]
    fn invalid_requests_are_rejected() {
        let now = Utc::now();
        let cases = [
            (
                PriceOverrideInput {
                    deployment: String::new(),
                    ..input()
                },
                PriceOverrideError::Deployment,
            ),
            (
                PriceOverrideInput {
                    deployment: "d".repeat(256),
                    ..input()
                },
                PriceOverrideError::Deployment,
            ),
            (
                PriceOverrideInput {
                    operation: "completions".into(),
                    ..input()
                },
                PriceOverrideError::Operation,
            ),
            (
                PriceOverrideInput {
                    reasoning_minor_per_token: -1,
                    ..input()
                },
                PriceOverrideError::Price,
            ),
            (
                PriceOverrideInput {
                    currency: "usd".into(),
                    ..input()
                },
                PriceOverrideError::Currency,
            ),
            (
                PriceOverrideInput {
                    confidence: "estimated".into(),
                    ..input()
                },
                PriceOverrideError::Confidence,
            ),
            (
                PriceOverrideInput {
                    reason: "   ".into(),
                    ..input()
                },
                PriceOverrideError::Reason,
            ),
            (
                PriceOverrideInput {
                    reason: "r".repeat(1025),
                    ..input()
                },
                PriceOverrideError::Reason,
            ),
            // An end in the past, or not after the (clamped) start.
            (
                PriceOverrideInput {
                    effective_until: Some(now - Duration::seconds(1)),
                    ..input()
                },
                PriceOverrideError::Window,
            ),
            (
                PriceOverrideInput {
                    effective_from: Some(now + Duration::hours(2)),
                    effective_until: Some(now + Duration::hours(1)),
                    ..input()
                },
                PriceOverrideError::Window,
            ),
        ];
        for (request, expected) in cases {
            assert_eq!(request.validate(now).unwrap_err(), expected);
        }
    }

    #[test]
    fn status_follows_window_and_revocation() {
        let now = Utc::now();
        let hour = Duration::hours(1);
        assert_eq!(
            PriceOverrideStatus::at(now, now + hour, None, None),
            PriceOverrideStatus::Scheduled
        );
        assert_eq!(
            PriceOverrideStatus::at(now, now - hour, None, None),
            PriceOverrideStatus::Active
        );
        assert_eq!(
            PriceOverrideStatus::at(now, now - hour, Some(now), None),
            PriceOverrideStatus::Expired
        );
        assert_eq!(
            PriceOverrideStatus::at(now, now - hour, Some(now + hour), Some(now)),
            PriceOverrideStatus::Revoked
        );
    }

    #[test]
    fn query_filters_are_validated() {
        assert!(
            PriceOverrideQuery::new(Some("gpt-main".into()), Some("embeddings".into())).is_ok()
        );
        assert_eq!(
            PriceOverrideQuery::new(Some(String::new()), None).unwrap_err(),
            PriceOverrideError::Deployment
        );
        assert_eq!(
            PriceOverrideQuery::new(None, Some("chat-stream".into())).unwrap_err(),
            PriceOverrideError::Operation
        );
    }
}
