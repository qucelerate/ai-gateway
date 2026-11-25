// Reading a traced request's content (D-017).
//
// Content is a separate permission from request records:
// `trace_content_read`, held by owners and admins, organization-wide only.
// Every read is audited (`trace_content.read`, naming the trace), including
// one that is refused, and the audit write fails closed: content that
// cannot be audited is not returned. A trace outside the organization is
// `NotFound`. A trace without stored content says why (`pending`,
// `not_traced`, `off`, `unavailable`, `failed`, `expired`) with whatever
// timing it kept.
//
// A refusal is audited best effort, as the mutation coordinator audits
// one: an actor outside the organization cannot be recorded against it,
// and that must not turn a 403 into a 503.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    application::trace_objects::TraceObjects,
    domain::{
        admin::{AdminPermission, AdminScope},
        audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
        credentials::CredentialCipher,
        identity::{OrganizationId, TeamId, UserAccountId},
        trace_content::{TraceDocument, Truncated, content_aad, open_content},
    },
    persistence::{
        admin::{AdminStore, AdminStoreError},
        trace_content::{ContentRecord, TraceContentStore},
    },
};

pub const ROUTE_TRACE_CONTENT: &str = "/api/admin/v1/organizations/{org}/traces/{id}/content";

/// How long after its request finishes a trace without a content row is
/// still `pending`: capture records the row moments after the response.
pub const PENDING_GRACE: Duration = Duration::minutes(2);

#[derive(Debug)]
pub enum TraceContentError {
    Forbidden,
    NotFound,
    /// The content is recorded as stored but cannot be read now: the trace
    /// store or keyring is not configured, unreachable, or the object is
    /// gone or does not open.
    Unavailable,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for TraceContentError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

/// What a traced request kept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TraceContentView {
    pub trace_id: Uuid,
    /// `stored`, `pending`, `off`, `unavailable`, `failed`, `expired`, or
    /// `not_traced`.
    pub state: String,
    pub started_at: Option<DateTime<Utc>>,
    pub provider_started_at: Option<DateTime<Utc>>,
    pub first_event_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub truncated: Truncated,
    /// The request as sent; null unless `stored`.
    pub request: Value,
    /// The response as it came back; null unless `stored`, or when the
    /// provider returned nothing usable.
    pub response: Value,
}

#[derive(Clone)]
pub struct TraceContentService {
    admin: AdminStore,
    store: TraceContentStore,
    objects: Option<Arc<dyn TraceObjects>>,
    cipher: Option<CredentialCipher>,
}

impl TraceContentService {
    pub fn new(
        admin: AdminStore,
        store: TraceContentStore,
        objects: Option<Arc<dyn TraceObjects>>,
        cipher: Option<CredentialCipher>,
    ) -> Self {
        Self {
            admin,
            store,
            objects,
            cipher,
        }
    }

    pub async fn read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        trace_id: Uuid,
        request_id: Uuid,
    ) -> Result<TraceContentView, TraceContentError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(
            AdminPermission::TraceContentRead,
            AdminScope::Organization(organization_id),
        ) {
            if let Err(error) = self
                .audit(
                    actor,
                    organization_id,
                    None,
                    trace_id,
                    request_id,
                    AuditOutcome::Denied,
                )
                .await
            {
                tracing::warn!(error = ?error, "trace content denial not audited");
            }
            return Err(TraceContentError::Forbidden);
        }
        let record = self
            .store
            .read(organization_id, trace_id)
            .await?
            .ok_or(TraceContentError::NotFound)?;
        let team = record.team_id.map(TeamId);
        // Opened before the audit, so a read that fails is recorded as
        // failed; audited before anything is returned.
        let view = self
            .view(organization_id, trace_id, record, Utc::now())
            .await;
        let outcome = if view.is_ok() {
            AuditOutcome::Succeeded
        } else {
            AuditOutcome::Failed
        };
        self.audit(actor, organization_id, team, trace_id, request_id, outcome)
            .await?;
        view
    }

    async fn view(
        &self,
        organization_id: OrganizationId,
        trace_id: Uuid,
        record: ContentRecord,
        now: DateTime<Utc>,
    ) -> Result<TraceContentView, TraceContentError> {
        let mut view = TraceContentView {
            trace_id,
            state: state(&record, now).into(),
            started_at: record.started_at,
            provider_started_at: record.provider_started_at,
            first_event_at: record.first_event_at,
            finished_at: record.finished_at,
            expired_at: record.expired_at,
            request_bytes: record.request_bytes,
            response_bytes: record.response_bytes,
            truncated: Truncated {
                request: record.request_truncated.unwrap_or(false),
                response: record.response_truncated.unwrap_or(false),
            },
            request: Value::Null,
            response: Value::Null,
        };
        if view.state != "stored" {
            return Ok(view);
        }
        let (Some(name), Some(key_id), Some(sealed_key)) =
            (record.object_name, record.key_id, record.sealed_key)
        else {
            return Err(TraceContentError::Unavailable);
        };
        let (Some(objects), Some(cipher)) = (&self.objects, &self.cipher) else {
            return Err(TraceContentError::Unavailable);
        };
        let sealed = objects
            .get(&name)
            .await
            .map_err(|error| {
                tracing::warn!(trace_id = %trace_id, error = %error, "trace content not read");
                TraceContentError::Unavailable
            })?
            .ok_or(TraceContentError::Unavailable)?;
        let key = cipher
            .open_trace_content_key(organization_id, trace_id, &key_id, &sealed_key)
            .map_err(|_| TraceContentError::Unavailable)?;
        let plaintext = open_content(&key, &content_aad(organization_id, trace_id), &sealed)
            .map_err(|_| TraceContentError::Unavailable)?;
        let document: TraceDocument =
            serde_json::from_slice(&plaintext).map_err(|_| TraceContentError::Unavailable)?;
        view.request = document.request;
        view.response = document.response;
        view.truncated = document.truncated;
        Ok(view)
    }

    async fn audit(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        team_id: Option<TeamId>,
        trace_id: Uuid,
        request_id: Uuid,
        outcome: AuditOutcome,
    ) -> Result<(), TraceContentError> {
        self.admin
            .record_audit(AdminAuditEvent::new(
                AuditActor::User(actor),
                organization_id,
                team_id,
                AuditTargetResource::InferenceTrace,
                AuditAction::new("trace_content.read").expect("audit action constant is valid"),
                trace_id,
                outcome,
                request_id,
                None,
                None,
            ))
            .await
            .map_err(TraceContentError::Admin)
    }
}

/// What the read reports: the row's state, `pending` while its content is
/// being written or its request is in flight or just finished, else
/// `not_traced`.
fn state(record: &ContentRecord, now: DateTime<Utc>) -> &str {
    match (record.state.as_deref(), record.trace_finalized_at) {
        (Some("writing"), _) => "pending",
        (Some(state), _) => state,
        (None, None) => "pending",
        (None, Some(finalized)) if now - finalized < PENDING_GRACE => "pending",
        (None, Some(_)) => "not_traced",
    }
}

fn map_admin_error(error: AdminStoreError) -> TraceContentError {
    match error {
        AdminStoreError::Authorization(_) => TraceContentError::Forbidden,
        AdminStoreError::ResourceNotFound => TraceContentError::NotFound,
        other => TraceContentError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn content_still_coming_is_pending() {
        let now = Utc::now();
        let record = |state: Option<&str>, finalized: Option<DateTime<Utc>>| ContentRecord {
            team_id: None,
            trace_finalized_at: finalized,
            state: state.map(str::to_owned),
            object_name: None,
            key_id: None,
            sealed_key: None,
            request_bytes: None,
            response_bytes: None,
            request_truncated: None,
            response_truncated: None,
            started_at: None,
            provider_started_at: None,
            first_event_at: None,
            finished_at: None,
            expired_at: None,
        };
        let long_ago = Some(now - Duration::minutes(10));
        for (record, expected) in [
            (record(None, None), "pending"),
            (record(None, Some(now)), "pending"),
            (record(None, long_ago), "not_traced"),
            (record(Some("writing"), long_ago), "pending"),
            (record(Some("stored"), long_ago), "stored"),
        ] {
            assert_eq!(state(&record, now), expected);
        }
    }

    #[test]
    fn only_owners_and_admins_read_content() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, allowed) in [
            (AdminRole::OrganizationOwner, true),
            (AdminRole::OrganizationAdmin, true),
            (AdminRole::Operator, false),
            (AdminRole::Auditor, false),
            (AdminRole::ReadOnlyMember, false),
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![AdminRoleBinding::organization(org, user, role)],
            };
            assert_eq!(
                context.allows(AdminPermission::TraceContentRead, scope),
                allowed,
                "{role:?}"
            );
        }
    }
}
