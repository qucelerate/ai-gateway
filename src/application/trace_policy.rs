// Tracing policy application service: read and set which of an
// organization's requests are traced, whether their content is stored, and
// for how long (D-017).
//
// Reading the policy needs `tracing_read` (every org-wide role); setting it
// needs `tracing_write` (owner and admin), runs through the admin mutation
// coordinator, and is idempotent, versioned and audited
// (`trace_policy.set`). Listed teams and service accounts must be the
// organization's.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    trace_policy::{TracePolicyError, TracePolicyInput, TracePolicyView},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    trace_policy::TracePolicyStore,
};

pub const ROUTE_TRACE_POLICY: &str = "/api/admin/v1/organizations/{org}/trace-policy";

#[derive(Debug)]
pub enum TracePolicyServiceError {
    Forbidden,
    NotFound,
    Invalid(TracePolicyError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for TracePolicyServiceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct TracePolicyWrite {
    pub policy: TracePolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct TracePolicyService {
    admin: AdminStore,
    store: TracePolicyStore,
}

impl TracePolicyService {
    pub fn new(admin: AdminStore, store: TracePolicyStore) -> Self {
        Self { admin, store }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), TracePolicyServiceError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(TracePolicyServiceError::Forbidden)
        }
    }

    /// The organization's policy; one never set traces nothing.
    pub async fn policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<TracePolicyView, TracePolicyServiceError> {
        self.authorize(actor, organization_id, AdminPermission::TracingRead)
            .await?;
        Ok(self.store.policy(organization_id).await?)
    }

    /// Replace the policy. It governs requests from the next one each
    /// process sees; content already stored keeps to the retention period
    /// now set.
    pub async fn set_policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: TracePolicyInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<TracePolicyWrite>, TracePolicyServiceError> {
        let input = input.validate().map_err(TracePolicyServiceError::Invalid)?;
        let target_id = policy_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::TracingWrite,
            route: ROUTE_TRACE_POLICY.into(),
            method: "PUT".into(),
            operation: operation("trace_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::TracePolicy,
            audit_action: audit_action("trace_policy.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "mode": input.mode,
                "team_ids": input.team_ids,
                "service_account_ids": input.service_account_ids,
                "content": input.content,
                "content_retention_days": input.content_retention_days,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the policy has since changed.
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(TracePolicyServiceError::NotFound)?;
                let policy = serde_json::from_value(snapshot).map_err(|_| {
                    TracePolicyServiceError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: TracePolicyWrite {
                        policy,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                TracePolicyStore::check_selection(tx, organization_id, &input)
                    .await?
                    .map_err(TracePolicyServiceError::Invalid)?;
                let existed = TracePolicyStore::view(tx, organization_id).await?.is_some();
                let created =
                    TracePolicyStore::upsert(tx, organization_id, target_id, &input, actor).await?;
                let stored = TracePolicyStore::view(tx, organization_id)
                    .await?
                    .ok_or(TracePolicyServiceError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = TracePolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    TracePolicyServiceError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        existed.then(|| AuditSnapshot {
                            version: previous(next),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: TracePolicyWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }
}

/// Stable per organization.
fn policy_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&["trace_policy", &organization_id.0.to_string()])
}

fn previous(next: ResourceVersion) -> Option<u64> {
    next.get().checked_sub(1).filter(|version| *version > 0)
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> TracePolicyServiceError {
    match error {
        AdminStoreError::Authorization(_) => TracePolicyServiceError::Forbidden,
        AdminStoreError::ResourceNotFound => TracePolicyServiceError::NotFound,
        AdminStoreError::VersionConflict(conflict) => {
            TracePolicyServiceError::VersionConflict(conflict)
        }
        AdminStoreError::IdempotencyConflict => TracePolicyServiceError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => TracePolicyServiceError::IdempotencyInProgress,
        other => TracePolicyServiceError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn tracing_is_set_by_owners_and_admins_and_read_org_wide() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, write) in [
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
            assert!(
                context.allows(AdminPermission::TracingRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::TracingWrite, scope),
                write,
                "{role:?}"
            );
        }
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-tracing-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = [
            ROUTE_TRACE_POLICY,
            crate::application::trace_content::ROUTE_TRACE_CONTENT,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
