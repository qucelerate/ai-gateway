//! Outbox event vocabulary.
//!
//! Every event is a fixed-shape record, built from identifiers, amounts and
//! closed vocabularies. None can carry inference content or provider
//! credentials. Only `notification.created` carries text: the gateway's own
//! notification text, which names the resources it is about (a team, a
//! service account, a webhook host) as members see it in the console. Consumers receive
//! each event at least once and deduplicate by event id.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::identity::{ServiceAccountId, TeamId, UserAccountId};
pub use super::notifications::NotificationCreated;
pub use super::usage_export::UsageExportFailing;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum OutboxEventType {
    #[serde(rename = "inference.finalized")]
    InferenceFinalized,
    #[serde(rename = "reconciliation.opened")]
    ReconciliationOpened,
    #[serde(rename = "reconciliation.updated")]
    ReconciliationUpdated,
    #[serde(rename = "reconciliation.resolved")]
    ReconciliationResolved,
    #[serde(rename = "usage_export.failing")]
    UsageExportFailing,
    #[serde(rename = "notification.created")]
    NotificationCreated,
}

impl OutboxEventType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InferenceFinalized => "inference.finalized",
            Self::ReconciliationOpened => "reconciliation.opened",
            Self::ReconciliationUpdated => "reconciliation.updated",
            Self::ReconciliationResolved => "reconciliation.resolved",
            Self::UsageExportFailing => "usage_export.failing",
            Self::NotificationCreated => "notification.created",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inference.finalized" => Some(Self::InferenceFinalized),
            "reconciliation.opened" => Some(Self::ReconciliationOpened),
            "reconciliation.updated" => Some(Self::ReconciliationUpdated),
            "reconciliation.resolved" => Some(Self::ReconciliationResolved),
            "usage_export.failing" => Some(Self::UsageExportFailing),
            "notification.created" => Some(Self::NotificationCreated),
            _ => None,
        }
    }
}

/// An event payload; the variant determines the event type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OutboxPayload {
    InferenceFinalized(InferenceFinalized),
    ReconciliationOpened(ReconciliationOpened),
    ReconciliationUpdated(ReconciliationUpdated),
    ReconciliationResolved(ReconciliationResolved),
    UsageExportFailing(UsageExportFailing),
    NotificationCreated(NotificationCreated),
}

impl OutboxPayload {
    pub fn event_type(&self) -> OutboxEventType {
        match self {
            Self::InferenceFinalized(_) => OutboxEventType::InferenceFinalized,
            Self::ReconciliationOpened(_) => OutboxEventType::ReconciliationOpened,
            Self::ReconciliationUpdated(_) => OutboxEventType::ReconciliationUpdated,
            Self::ReconciliationResolved(_) => OutboxEventType::ReconciliationResolved,
            Self::UsageExportFailing(_) => OutboxEventType::UsageExportFailing,
            Self::NotificationCreated(_) => OutboxEventType::NotificationCreated,
        }
    }

    /// Parses a stored payload as the given type.
    pub fn from_stored(
        event_type: OutboxEventType,
        payload: serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        Ok(match event_type {
            OutboxEventType::InferenceFinalized => {
                Self::InferenceFinalized(serde_json::from_value(payload)?)
            }
            OutboxEventType::ReconciliationOpened => {
                Self::ReconciliationOpened(serde_json::from_value(payload)?)
            }
            OutboxEventType::ReconciliationUpdated => {
                Self::ReconciliationUpdated(serde_json::from_value(payload)?)
            }
            OutboxEventType::ReconciliationResolved => {
                Self::ReconciliationResolved(serde_json::from_value(payload)?)
            }
            OutboxEventType::UsageExportFailing => {
                Self::UsageExportFailing(serde_json::from_value(payload)?)
            }
            OutboxEventType::NotificationCreated => {
                Self::NotificationCreated(serde_json::from_value(payload)?)
            }
        })
    }
}

/// A trace reached a final outcome. A trace finalized as `unknown` may be
/// finalized again once its settlement arrives or an operator resolves it;
/// the latest event per `trace_id` is authoritative.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceFinalized {
    pub trace_id: Uuid,
    pub reservation_id: Uuid,
    pub team_id: Option<TeamId>,
    pub service_account_id: Option<ServiceAccountId>,
    pub model: String,
    pub currency: String,
    /// `settled`, `unknown`, `bound-breach`, `expired`, `input-count-drift`,
    /// `operator-resolved`, `failed-over` (an attempt released when its
    /// request failed over to another deployment, D-023), or `refused`
    /// (released at zero: every deployment tried refused it before any
    /// work).
    pub outcome: String,
    pub amount_minor: Option<i64>,
    pub finalized_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationOpened {
    pub item_id: Uuid,
    pub reservation_id: Uuid,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

/// An open item's reason changed: an item opened by expiry recovery
/// (`expired-after-claim`) takes the reason of a finalization that arrives
/// later. The item stays open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationUpdated {
    pub item_id: Uuid,
    pub reservation_id: Uuid,
    pub previous_reason: String,
    pub reason: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationResolved {
    pub item_id: Uuid,
    pub reservation_id: Uuid,
    /// `settlement` or `operator`.
    pub resolution: String,
    pub resolved_amount_minor: Option<i64>,
    pub resolved_by: Option<UserAccountId>,
    pub resolved_at: DateTime<Utc>,
}
