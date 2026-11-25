//! Notifications (Phase 10, D-024): what the gateway did, or needs from
//! people, in an organization.
//!
//! A kind fixes a notification's severity, whether it is an event or a
//! condition that stays open until resolved, and the permission a member
//! needs to see it. Raising one is idempotent per subject: an open
//! condition is updated rather than raised again, and an event is kept
//! once.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::admin::AdminPermission;

pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_BODY_CHARS: usize = 2000;
pub const MAX_SUBJECT_CHARS: usize = 256;
/// Chosen notifications one request may mark read.
pub const MAX_READ_IDS: usize = 100;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    /// Someone needs to act.
    Action,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Action => "action",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "action" => Some(Self::Action),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum NotificationKind {
    /// Published prices were adopted in a new release (event; subject: the
    /// release).
    #[serde(rename = "price.adopted")]
    PriceAdopted,
    /// A published price change was held for an admin to adopt or dismiss
    /// (subject: `{deployment}/{operation}`).
    #[serde(rename = "price.held")]
    PriceHeld,
    /// A price source's latest fetch failed (subject: the source).
    #[serde(rename = "price_source.failing")]
    PriceSourceFailing,
    /// A provider account's latest model listing failed (subject: the
    /// adapter instance key).
    #[serde(rename = "provider_account.sync_failing")]
    ProviderAccountSyncFailing,
    /// A budget's current window has used [`BUDGET_THRESHOLD_PERCENT`] of
    /// its limit or more (subject: the budget policy).
    #[serde(rename = "budget.threshold")]
    BudgetThreshold,
    /// An active API key expires within [`EXPIRY_NOTICE_DAYS`] (subject: the
    /// key).
    #[serde(rename = "api_key.expiring")]
    ApiKeyExpiring,
    /// An agent token expires within [`EXPIRY_NOTICE_DAYS`] (subject: the
    /// token).
    #[serde(rename = "agent_token.expiring")]
    AgentTokenExpiring,
    /// A webhook delivery to an endpoint was parked after its last retry
    /// (subject: the endpoint).
    #[serde(rename = "webhook.failing")]
    WebhookFailing,
    /// An app's end user has [`CREDITS_LOW_PERCENT`] or less left of the
    /// credits granted to them that are still live (subject: the end user's
    /// gateway id).
    #[serde(rename = "end_user.credits_low")]
    EndUserCreditsLow,
    /// Requests whose cost the gateway could not settle wait for the
    /// provider's amount (subject: `reconciliation`, one per organization).
    #[serde(rename = "reconciliation.open")]
    ReconciliationOpen,
}

/// The share of a budget's limit, in percent, past which it is reported.
pub const BUDGET_THRESHOLD_PERCENT: i64 = 80;
/// How long before an API key or agent token expires it is reported.
pub const EXPIRY_NOTICE_DAYS: i64 = 7;
/// The share of an end user's live credits, in percent, at or below which
/// they are reported.
pub const CREDITS_LOW_PERCENT: i64 = 20;

impl NotificationKind {
    pub const ALL: [Self; 10] = [
        Self::PriceAdopted,
        Self::PriceHeld,
        Self::PriceSourceFailing,
        Self::ProviderAccountSyncFailing,
        Self::BudgetThreshold,
        Self::ApiKeyExpiring,
        Self::AgentTokenExpiring,
        Self::WebhookFailing,
        Self::EndUserCreditsLow,
        Self::ReconciliationOpen,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PriceAdopted => "price.adopted",
            Self::PriceHeld => "price.held",
            Self::PriceSourceFailing => "price_source.failing",
            Self::ProviderAccountSyncFailing => "provider_account.sync_failing",
            Self::BudgetThreshold => "budget.threshold",
            Self::ApiKeyExpiring => "api_key.expiring",
            Self::AgentTokenExpiring => "agent_token.expiring",
            Self::WebhookFailing => "webhook.failing",
            Self::EndUserCreditsLow => "end_user.credits_low",
            Self::ReconciliationOpen => "reconciliation.open",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    pub const fn severity(self) -> Severity {
        match self {
            Self::PriceAdopted => Severity::Info,
            Self::PriceHeld | Self::ReconciliationOpen => Severity::Action,
            Self::PriceSourceFailing
            | Self::ProviderAccountSyncFailing
            | Self::BudgetThreshold
            | Self::ApiKeyExpiring
            | Self::AgentTokenExpiring
            | Self::WebhookFailing
            | Self::EndUserCreditsLow => Severity::Warning,
        }
    }

    /// A condition stays open until resolved; an event never is.
    pub const fn resolvable(self) -> bool {
        !matches!(self, Self::PriceAdopted)
    }

    /// What a member needs to see it.
    pub const fn permission(self) -> AdminPermission {
        match self {
            Self::PriceAdopted
            | Self::PriceHeld
            | Self::PriceSourceFailing
            | Self::ProviderAccountSyncFailing => AdminPermission::CatalogRead,
            Self::BudgetThreshold => AdminPermission::BudgetRead,
            Self::ApiKeyExpiring => AdminPermission::ApiKeysList,
            Self::AgentTokenExpiring => AdminPermission::MembersList,
            Self::WebhookFailing => AdminPermission::WebhookRead,
            // End-user ids are shown to those who can read usage (D-028).
            Self::EndUserCreditsLow => AdminPermission::UsageRead,
            Self::ReconciliationOpen => AdminPermission::ReconciliationRead,
        }
    }
}

/// The permission's API name, as notifications store it.
pub fn permission_name(permission: AdminPermission) -> String {
    match serde_json::to_value(permission) {
        Ok(serde_json::Value::String(name)) => name,
        _ => unreachable!("permissions serialize as strings"),
    }
}

/// A notification to raise.
#[derive(Clone, Debug, PartialEq)]
pub struct NewNotification {
    pub kind: NotificationKind,
    pub subject: String,
    pub title: String,
    pub body: String,
    /// A console path (`/admin/...`).
    pub link: Option<String>,
    pub data: serde_json::Value,
}

impl NewNotification {
    pub fn new(
        kind: NotificationKind,
        subject: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            title: title.into(),
            body: body.into(),
            link: None,
            data: serde_json::json!({}),
        }
    }

    pub fn with_link(mut self, link: impl Into<String>) -> Self {
        self.link = Some(link.into());
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    /// Fitted to what the store keeps: text cut to its limits (a title is
    /// never empty), and a link that is not a console path dropped.
    pub fn bounded(mut self) -> Self {
        self.subject = truncate(&self.subject, MAX_SUBJECT_CHARS);
        if self.subject.is_empty() {
            self.subject = "-".into();
        }
        self.title = truncate(&self.title, MAX_TITLE_CHARS);
        if self.title.trim().is_empty() {
            self.title = self.kind.as_str().into();
        }
        self.body = truncate(&self.body, MAX_BODY_CHARS);
        self.link = self
            .link
            .filter(|link| link.starts_with("/admin/") && link.len() <= 512);
        if !self.data.is_object() || self.data.to_string().len() > 16_384 {
            self.data = serde_json::json!({});
        }
        self
    }
}

fn truncate(value: &str, chars: usize) -> String {
    value.chars().take(chars).collect()
}

/// The `notification.created` webhook event: a notification was raised
/// (a condition opened, or an event recorded). A condition raised again
/// while open is not sent again. Its text is the gateway's own, naming the
/// resources it is about; it never carries inference content or secrets.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationCreated {
    pub notification_id: Uuid,
    pub kind: String,
    pub severity: Severity,
    pub subject: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub data: serde_json::Value,
    /// The permission a member needs to see it in the console.
    pub required_permission: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationView {
    pub id: Uuid,
    pub kind: String,
    pub severity: Severity,
    pub subject: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the condition it reports ended or was acted on; always `null`
    /// for an event.
    pub resolved_at: Option<DateTime<Utc>>,
    /// Whether the member reading the list has read it.
    pub read: bool,
}

/// Which notifications a list shows by read state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFilter {
    #[default]
    All,
    Unread,
    Read,
}

impl ReadFilter {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unread => "unread",
            Self::Read => "read",
        }
    }
}

/// A list's filters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotificationFilter {
    pub kind: Option<NotificationKind>,
    pub severity: Option<Severity>,
    pub read: ReadFilter,
}

impl NotificationFilter {
    /// Stable text a page token is bound to.
    pub fn normalized(&self) -> String {
        format!(
            "kind={};severity={};read={}",
            self.kind.map_or("", NotificationKind::as_str),
            self.severity.map_or("", Severity::as_str),
            self.read.as_str()
        )
    }
}

/// What to mark read: everything up to and including one notification (by
/// creation), or chosen ones.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MarkRead {
    #[serde(default)]
    pub through: Option<Uuid>,
    #[serde(default)]
    pub ids: Option<Vec<Uuid>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MarkReadError {
    #[error("give exactly one of `through` or `ids`")]
    Ambiguous,
    #[error("`ids` holds 1 to 100 notifications")]
    TooMany,
}

pub enum MarkReadTarget {
    Through(Uuid),
    Ids(Vec<Uuid>),
}

impl MarkRead {
    pub fn target(self) -> Result<MarkReadTarget, MarkReadError> {
        match (self.through, self.ids) {
            (Some(through), None) => Ok(MarkReadTarget::Through(through)),
            (None, Some(mut ids)) => {
                ids.sort_unstable();
                ids.dedup();
                if ids.is_empty() || ids.len() > MAX_READ_IDS {
                    return Err(MarkReadError::TooMany);
                }
                Ok(MarkReadTarget::Ids(ids))
            }
            _ => Err(MarkReadError::Ambiguous),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnreadCount {
    /// Unread notifications that are not resolved.
    pub unread: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_fix_their_rules() {
        for kind in NotificationKind::ALL {
            assert_eq!(NotificationKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
        }
        assert!(!NotificationKind::PriceAdopted.resolvable());
        assert!(NotificationKind::PriceHeld.resolvable());
        assert_eq!(NotificationKind::PriceHeld.severity(), Severity::Action);
        assert_eq!(
            permission_name(NotificationKind::PriceSourceFailing.permission()),
            "catalog_read"
        );
        assert_eq!(NotificationKind::parse("nope"), None);
        assert_eq!(
            permission_name(NotificationKind::BudgetThreshold.permission()),
            "budget_read"
        );
        assert_eq!(
            permission_name(NotificationKind::WebhookFailing.permission()),
            "webhook_read"
        );
    }

    #[test]
    fn notifications_are_fitted_to_what_is_stored() {
        let raised = NewNotification::new(
            NotificationKind::PriceSourceFailing,
            "s".repeat(300),
            "  ",
            "b".repeat(2100),
        )
        .with_link("https://elsewhere.example")
        .with_data(serde_json::json!([1]))
        .bounded();
        assert_eq!(raised.subject.chars().count(), MAX_SUBJECT_CHARS);
        assert_eq!(raised.title, "price_source.failing");
        assert_eq!(raised.body.chars().count(), MAX_BODY_CHARS);
        assert_eq!(raised.link, None);
        assert_eq!(raised.data, serde_json::json!({}));
        let kept = NewNotification::new(NotificationKind::PriceHeld, "d/chat", "Held", "")
            .with_link("/admin/models/pricing")
            .bounded();
        assert_eq!(kept.link.as_deref(), Some("/admin/models/pricing"));
    }

    #[test]
    fn mark_read_takes_one_target() {
        let id = Uuid::new_v4();
        assert!(matches!(
            MarkRead { through: Some(id), ids: None }.target(),
            Ok(MarkReadTarget::Through(found)) if found == id
        ));
        assert!(matches!(
            MarkRead { through: None, ids: Some(vec![id, id]) }.target(),
            Ok(MarkReadTarget::Ids(ids)) if ids == [id]
        ));
        assert_eq!(
            MarkRead {
                through: Some(id),
                ids: Some(vec![id])
            }
            .target()
            .err(),
            Some(MarkReadError::Ambiguous)
        );
        assert_eq!(
            MarkRead {
                through: None,
                ids: None
            }
            .target()
            .err(),
            Some(MarkReadError::Ambiguous)
        );
        assert_eq!(
            MarkRead {
                through: None,
                ids: Some(vec![])
            }
            .target()
            .err(),
            Some(MarkReadError::TooMany)
        );
        let many = (0..=MAX_READ_IDS).map(|_| Uuid::new_v4()).collect();
        assert_eq!(
            MarkRead {
                through: None,
                ids: Some(many)
            }
            .target()
            .err(),
            Some(MarkReadError::TooMany)
        );
    }

    #[test]
    fn filters_normalize_for_page_tokens() {
        assert_eq!(
            NotificationFilter::default().normalized(),
            "kind=;severity=;read=all"
        );
        assert_eq!(
            NotificationFilter {
                kind: Some(NotificationKind::PriceHeld),
                severity: Some(Severity::Action),
                read: ReadFilter::Unread,
            }
            .normalized(),
            "kind=price.held;severity=action;read=unread"
        );
    }
}
