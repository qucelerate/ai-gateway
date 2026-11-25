use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    admin::{AdminPermission, AdminScope, authorize},
    audit::{AuditAction, AuditOutcome, AuditTargetResource},
    auth::AuthCrypto,
    identity::{OrganizationId, TeamId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
};
use crate::persistence::{
    AdminStore, AdminStoreError, AuditDimension, AuditEventRecord, AuditListFilters, AuditStore,
    AuditStoreError,
};

pub const AUDIT_COLLECTION: &str = "audit_events";
pub const AUDIT_ORDER: &str = "occurred_at_desc,id_desc";

#[derive(Debug, Error)]
pub enum AuditServiceError {
    #[error("administrative permission denied")]
    Forbidden,
    #[error("authentication unavailable")]
    AuthenticationUnavailable,
    #[error("invalid audit filter: {0}")]
    InvalidRequest(String),
    #[error("pagination failed: {0}")]
    Pagination(#[from] PaginationError),
    #[error("database operation failed: {0}")]
    Store(#[from] AuditStoreError),
    #[error("administrative operation failed: {0}")]
    Admin(#[from] AdminStoreError),
}

/// `action`, `target_type`, `outcome` and `actor_type` take one value or a
/// comma-separated list, matching any of them.
#[derive(Clone, Debug, Default)]
pub struct AuditFilterInput {
    pub actor: Option<String>,
    pub action: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub outcome: Option<String>,
    pub actor_type: Option<String>,
    pub q: Option<String>,
    pub occurred_after: Option<String>,
    pub occurred_before: Option<String>,
    /// Leaves out views: the events reading data records (`*.read`,
    /// `*.list`), so changes stand out.
    pub exclude_views: bool,
}

/// The most groups a summary returns.
pub const AUDIT_SUMMARY_LIMIT: u32 = 500;
/// The most dimensions a summary groups by.
const AUDIT_SUMMARY_DIMENSIONS: usize = 2;
/// The longest search text.
const AUDIT_SEARCH_MAX: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct AuditGroupView {
    /// The group's value for each `group_by` dimension, by name; null where
    /// the event has none (an actor-less system event).
    pub values: serde_json::Map<String, serde_json::Value>,
    pub count: i64,
    pub first_occurred_at: DateTime<Utc>,
    pub last_occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuditSummaryView {
    pub group_by: Vec<&'static str>,
    pub groups: Vec<AuditGroupView>,
    /// Every matching event, across all groups returned or not.
    pub total: i64,
    /// More groups exist than were returned.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuditActorView {
    pub actor_type: String,
    pub user_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// The person's, agent's or service account's name, if known.
    pub name: Option<String>,
}

/// Public audit representation. Metadata is intentionally absent: callers
/// must not receive stored request bodies, secrets, prompts, or completions.
#[derive(Clone, Debug, Serialize)]
pub struct AuditEventView {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub actor: AuditActorView,
    pub action: String,
    pub target_type: String,
    pub target_id: Uuid,
    /// A team's or service account's name, while it exists.
    pub target_name: Option<String>,
    pub outcome: String,
    pub request_id: Option<Uuid>,
}

#[derive(Clone)]
pub struct AuditApplicationService {
    admin: AdminStore,
    store: AuditStore,
    codec: Option<SignedCursorCodec>,
}

impl AuditApplicationService {
    pub fn new(admin: AdminStore, store: AuditStore, crypto: Option<AuthCrypto>) -> Self {
        Self {
            admin,
            store,
            codec: crypto.map(SignedCursorCodec::new),
        }
    }

    pub async fn list(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: AuditFilterInput,
        page: PageRequest,
        request_id: Uuid,
    ) -> Result<Page<AuditEventView>, AuditServiceError> {
        page.validate()?;
        let filters = normalize_filters(input)?;

        self.authorize_read(actor, organization_id).await?;

        let codec = self
            .codec
            .as_ref()
            .ok_or(AuditServiceError::AuthenticationUnavailable)?;
        let normalized = filters.normalized_string();
        let (upper_watermark, after) = if let Some(token) = page.page_token.as_deref() {
            let claims = codec.decode_and_validate(
                token,
                actor,
                AdminScope::Organization(organization_id),
                AUDIT_COLLECTION,
                &normalized,
                AUDIT_ORDER,
            )?;
            let watermark = claims
                .upper_watermark
                .ok_or(PaginationError::InvalidToken)?;
            let after = parse_keyset(&claims.last_keyset)?;
            let watermark = parse_timestamp(&watermark)?;
            (Some(watermark), Some(after))
        } else {
            (None, None)
        };

        let query = filters.store_filters();
        let (records, has_more, watermark) = self
            .store
            .list(
                organization_id,
                &query,
                page.page_size,
                upper_watermark,
                after,
            )
            .await?;

        // A successful read is itself an administrative access event. Keep
        // this event write on the existing AdminStore write path and expose
        // no client-controlled metadata.
        self.record_read(actor, organization_id, request_id).await?;

        let next_page_token = if has_more {
            let last = records
                .last()
                .ok_or_else(|| AuditServiceError::InvalidRequest("empty audit page".into()))?;
            let claims = CursorClaims::new(
                actor,
                AdminScope::Organization(organization_id),
                AUDIT_COLLECTION,
                normalized,
                AUDIT_ORDER,
                keyset(last.occurred_at, last.id),
                Some(watermark.to_rfc3339()),
            )?;
            Some(codec.encode(&claims)?)
        } else {
            None
        };

        Ok(Page::new(
            records.into_iter().map(AuditEventView::from).collect(),
            next_page_token,
        ))
    }

    /// Matching events counted by up to two dimensions (`group_by`,
    /// comma-separated), largest groups first. Reading it is audited like
    /// reading the events.
    pub async fn summarize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: AuditFilterInput,
        group_by: Option<String>,
        request_id: Uuid,
    ) -> Result<AuditSummaryView, AuditServiceError> {
        let filters = normalize_filters(input)?;
        let dimensions = parse_dimensions(group_by.as_deref())?;
        self.authorize_read(actor, organization_id).await?;
        let query = filters.store_filters();
        let (records, total, truncated) = self
            .store
            .summarize(organization_id, &query, &dimensions, AUDIT_SUMMARY_LIMIT)
            .await?;
        self.record_read(actor, organization_id, request_id).await?;
        Ok(AuditSummaryView {
            group_by: dimensions
                .iter()
                .map(|dimension| dimension.name())
                .collect(),
            groups: records
                .into_iter()
                .map(|group| AuditGroupView {
                    values: dimensions
                        .iter()
                        .zip(group.values)
                        .map(|(dimension, value)| {
                            (
                                dimension.name().to_owned(),
                                value.map_or(serde_json::Value::Null, serde_json::Value::String),
                            )
                        })
                        .collect(),
                    count: group.count,
                    first_occurred_at: group.first_occurred_at,
                    last_occurred_at: group.last_occurred_at,
                })
                .collect(),
            total,
            truncated,
        })
    }

    // AuditRead is deliberately checked only at organization scope. A
    // team-admin binding can therefore never authorize these reads.
    async fn authorize_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<(), AuditServiceError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await?;
        authorize(
            &context,
            AdminPermission::AuditRead,
            AdminScope::Organization(organization_id),
        )
        .map_err(|_| AuditServiceError::Forbidden)
    }

    /// A successful read is itself an administrative access event. Keep
    /// this event write on the existing AdminStore write path and expose no
    /// client-controlled metadata.
    async fn record_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        request_id: Uuid,
    ) -> Result<(), AuditServiceError> {
        let action = AuditAction::new("audit.read")
            .map_err(|_| AuditServiceError::InvalidRequest("audit action invalid".into()))?;
        self.admin
            .record_audit(crate::domain::audit::AdminAuditEvent::new(
                crate::domain::audit::AuditActor::User(actor),
                organization_id,
                None,
                AuditTargetResource::Organization,
                action,
                organization_id.0,
                AuditOutcome::Succeeded,
                request_id,
                None,
                None,
            ))
            .await?;
        Ok(())
    }
}

fn parse_dimensions(value: Option<&str>) -> Result<Vec<AuditDimension>, AuditServiceError> {
    let mut dimensions = Vec::new();
    for name in value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let dimension = AuditDimension::parse(name).ok_or_else(|| invalid("group_by"))?;
        if dimensions.contains(&dimension) {
            return Err(invalid("group_by"));
        }
        dimensions.push(dimension);
    }
    if dimensions.is_empty() || dimensions.len() > AUDIT_SUMMARY_DIMENSIONS {
        return Err(invalid("group_by"));
    }
    Ok(dimensions)
}

#[derive(Clone, Debug)]
struct NormalizedFilters {
    actor: Option<Uuid>,
    actions: Vec<String>,
    target_types: Vec<String>,
    target_id: Option<Uuid>,
    outcomes: Vec<String>,
    actor_types: Vec<String>,
    search: Option<String>,
    occurred_after: Option<DateTime<Utc>>,
    occurred_before: Option<DateTime<Utc>>,
    exclude_views: bool,
}

impl NormalizedFilters {
    /// Bound into page tokens, so a token only continues the same query: a
    /// digest, so long filter lists never outgrow the token.
    fn normalized_string(&self) -> String {
        use sha2::{Digest, Sha256};
        let query = format!(
            "actor={};action={};target_type={};target_id={};outcome={};actor_type={};q={};after={};before={}",
            option_uuid(self.actor),
            self.actions.join(","),
            self.target_types.join(","),
            option_uuid(self.target_id),
            self.outcomes.join(","),
            self.actor_types.join(","),
            self.search.as_deref().map(urlencode).unwrap_or_default(),
            option_time(self.occurred_after),
            option_time(self.occurred_before),
        );
        // Only when set, so tokens issued before the filter existed still
        // continue their query.
        let query = if self.exclude_views {
            format!("{query};views=excluded")
        } else {
            query
        };
        let digest = Sha256::digest(query.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        format!("sha256:{hex}")
    }

    fn store_filters(&self) -> AuditListFilters {
        AuditListFilters {
            actor_id: self.actor,
            actions: self.actions.clone(),
            target_types: self.target_types.clone(),
            target_id: self.target_id,
            outcomes: self.outcomes.clone(),
            actor_types: self.actor_types.clone(),
            search: self.search.clone(),
            occurred_after: self.occurred_after,
            occurred_before: self.occurred_before,
            exclude_views: self.exclude_views,
        }
    }
}

/// Search text with the token's separators escaped.
fn urlencode(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(';', "%3B")
        .replace('=', "%3D")
}

fn normalize_filters(input: AuditFilterInput) -> Result<NormalizedFilters, AuditServiceError> {
    let actor = parse_uuid(input.actor, "actor")?;
    let actions = normalize_list(input.action, "action")?;
    for value in &actions {
        AuditAction::new(value).map_err(|_| invalid("action"))?;
    }
    let target_types = normalize_list(input.target_type, "target_type")?;
    if !target_types.iter().all(|value| {
        AuditTargetResource::ALL
            .iter()
            .any(|target| target.as_str() == value)
    }) {
        return Err(invalid("target_type"));
    }
    let target_id = parse_uuid(input.target_id, "target_id")?;
    let outcomes = normalize_list(input.outcome, "outcome")?;
    if !outcomes
        .iter()
        .all(|value| matches!(value.as_str(), "succeeded" | "failed" | "denied"))
    {
        return Err(invalid("outcome"));
    }
    let actor_types = normalize_list(input.actor_type, "actor_type")?;
    if !actor_types
        .iter()
        .all(|value| matches!(value.as_str(), "user" | "service_account" | "system"))
    {
        return Err(invalid("actor_type"));
    }
    let search = match input.q.map(|value| value.trim().to_owned()) {
        Some(value) if value.chars().count() > AUDIT_SEARCH_MAX => return Err(invalid("q")),
        Some(value) if !value.is_empty() => Some(value),
        _ => None,
    };
    let occurred_after = parse_time(input.occurred_after, "occurred_after")?;
    let occurred_before = parse_time(input.occurred_before, "occurred_before")?;
    if occurred_after
        .zip(occurred_before)
        .is_some_and(|(a, b)| a > b)
    {
        return Err(invalid("time range"));
    }
    Ok(NormalizedFilters {
        actor,
        actions,
        target_types,
        target_id,
        outcomes,
        actor_types,
        search,
        occurred_after,
        occurred_before,
        exclude_views: input.exclude_views,
    })
}

/// A comma-separated list, lowercased, sorted and without repeats (so the
/// same set always makes the same page token); an empty element is invalid.
fn normalize_list(value: Option<String>, name: &str) -> Result<Vec<String>, AuditServiceError> {
    let Some(value) = normalize_optional(value, name)? else {
        return Ok(Vec::new());
    };
    let mut values = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(invalid(name));
        }
        values.push(item.to_owned());
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn normalize_optional(
    value: Option<String>,
    name: &str,
) -> Result<Option<String>, AuditServiceError> {
    value
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            if value.is_empty() {
                Err(invalid(name))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

fn parse_uuid(value: Option<String>, name: &str) -> Result<Option<Uuid>, AuditServiceError> {
    value
        .map(|value| Uuid::parse_str(value.trim()).map_err(|_| invalid(name)))
        .transpose()
}

fn parse_time(
    value: Option<String>,
    name: &str,
) -> Result<Option<DateTime<Utc>>, AuditServiceError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value.trim())
                .map(|time| time.with_timezone(&Utc))
                .map_err(|_| invalid(name))
        })
        .transpose()
}

fn invalid(name: &str) -> AuditServiceError {
    AuditServiceError::InvalidRequest(format!("invalid {name}"))
}

fn option_uuid(value: Option<Uuid>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn option_time(value: Option<DateTime<Utc>>) -> String {
    value.map(|value| value.to_rfc3339()).unwrap_or_default()
}

fn keyset(occurred_at: DateTime<Utc>, id: Uuid) -> String {
    format!("{}|{id}", occurred_at.to_rfc3339())
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, PaginationError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| PaginationError::InvalidToken)
}

fn parse_keyset(value: &str) -> Result<(DateTime<Utc>, Uuid), PaginationError> {
    let (timestamp, id) = value.split_once('|').ok_or(PaginationError::InvalidToken)?;
    let id = Uuid::parse_str(id).map_err(|_| PaginationError::InvalidToken)?;
    Ok((parse_timestamp(timestamp)?, id))
}

impl From<AuditEventRecord> for AuditEventView {
    fn from(value: AuditEventRecord) -> Self {
        Self {
            id: value.id,
            occurred_at: value.occurred_at,
            organization_id: OrganizationId(value.organization_id),
            team_id: value.team_id.map(TeamId),
            actor: AuditActorView {
                actor_type: value.actor_type,
                user_id: value.actor_user_id,
                service_account_id: value.actor_service_account_id,
                name: value.actor_name,
            },
            action: value.action,
            target_type: value.entity_type,
            target_id: value.entity_id,
            target_name: value.target_name,
            outcome: value.outcome,
            request_id: value.request_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditFilterInput, AuditServiceError, normalize_filters};

    #[test]
    fn reporting_target_types_are_valid_audit_filters() {
        for target_type in [
            "usage_report",
            "inference_trace",
            "reconciliation_item",
            "catalog_release",
            "catalog_activation",
            "provider_credential",
            "price_override",
            "rate_limit_policy",
            "budget_policy",
            "webhook_endpoint",
            "webhook_delivery",
            "catalog_sync_job",
            "price_mapping",
            "price_source_fetch",
            "retention_policy",
            "trace_policy",
            "data_export",
            "cloud_principal_binding",
            "invitation",
            "config_pin",
            "usage_export",
            "catalog_auto_offer",
            "agent",
            "agent_token",
            "model_card_note",
            "model_evaluation_set",
            "model_master",
            "model_master_link",
            "batch",
        ] {
            let filters = normalize_filters(AuditFilterInput {
                target_type: Some(target_type.to_owned()),
                ..AuditFilterInput::default()
            })
            .expect("reporting target type should be accepted");

            assert_eq!(filters.target_types, [target_type]);
        }
    }

    #[test]
    fn unknown_target_types_remain_invalid_audit_filters() {
        let error = normalize_filters(AuditFilterInput {
            target_type: Some("unknown_report".to_owned()),
            ..AuditFilterInput::default()
        })
        .expect_err("unknown target type should remain rejected");

        assert!(matches!(error, AuditServiceError::InvalidRequest(_)));
    }
}
