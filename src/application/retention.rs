// Data retention application service: read and set an organization's
// retention policy, and authorize and audit exports.
//
// Reading the policy needs `retention_read` (every org-wide role); setting it
// needs `retention_write` (owner and admin), runs through the admin mutation
// coordinator, and is idempotent, versioned and audited. An export needs the
// dataset's read permission org-wide (`usage_read`, `trace_read` or
// `audit_read`) and is recorded (dataset, format, range, requester) and
// audited in one transaction before the first row is sent; an export that fails or is abandoned
// adds a `failed` audit event naming the same record. A team-scoped binding
// cannot export. Export records are listed with `audit_read`. The purge
// itself runs in the worker (`jobs::retention`).

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    retention::{
        ExportDataset, ExportQuery, ExportRecord, ExportStatus, RetentionPolicyError,
        RetentionPolicyInput, RetentionPolicyView,
    },
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    retention::RetentionStore,
};

pub const ROUTE_RETENTION_POLICY: &str = "/api/admin/v1/organizations/{org}/retention-policy";
pub const ROUTE_EXPORTS: &str = "/api/admin/v1/organizations/{org}/exports";
pub const ROUTE_EXPORT: &str = "/api/admin/v1/organizations/{org}/exports/{dataset}";

const EXPORT_COLLECTION: &str = "data_exports";
const ORDER_EXPORT: &str = "started_at desc,id desc";

#[derive(Debug)]
pub enum RetentionError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Invalid(RetentionPolicyError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Pagination(PaginationError),
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for RetentionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct RetentionWrite {
    pub policy: RetentionPolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct RetentionService {
    admin: AdminStore,
    store: RetentionStore,
    codec: Option<SignedCursorCodec>,
}

/// An authorized, recorded export about to stream.
#[derive(Clone, Copy, Debug)]
pub struct StartedExport {
    pub id: Uuid,
    actor: UserAccountId,
    organization_id: OrganizationId,
    dataset: ExportDataset,
    request_id: Uuid,
}

impl RetentionService {
    pub fn new(admin: AdminStore, store: RetentionStore, codec: Option<SignedCursorCodec>) -> Self {
        Self {
            admin,
            store,
            codec,
        }
    }

    pub fn store(&self) -> &RetentionStore {
        &self.store
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), RetentionError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(RetentionError::Forbidden)
        }
    }

    /// The organization's policy; unset periods keep data indefinitely.
    pub async fn policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<RetentionPolicyView, RetentionError> {
        self.authorize(actor, organization_id, AdminPermission::RetentionRead)
            .await?;
        Ok(self.store.policy(organization_id).await?)
    }

    /// Replace the policy. Shortening a period lets the next purge delete
    /// what is now older than it.
    pub async fn set_policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: RetentionPolicyInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<RetentionWrite>, RetentionError> {
        let input = input.validate().map_err(RetentionError::Invalid)?;
        let target_id = policy_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::RetentionWrite,
            route: ROUTE_RETENTION_POLICY.into(),
            method: "PUT".into(),
            operation: operation("retention_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::RetentionPolicy,
            audit_action: audit_action("retention_policy.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "trace_days": input.trace_days,
                "usage_days": input.usage_days,
                "audit_days": input.audit_days,
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
                    .ok_or(RetentionError::NotFound)?;
                let policy = serde_json::from_value(snapshot).map_err(|_| {
                    RetentionError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: RetentionWrite {
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
                let existed = RetentionStore::view(tx, organization_id).await?.is_some();
                let created =
                    RetentionStore::upsert(tx, organization_id, target_id, input, actor).await?;
                let stored = RetentionStore::view(tx, organization_id)
                    .await?
                    .ok_or(RetentionError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = RetentionPolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    RetentionError::Store("response snapshot is not serializable".into())
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
                    item: RetentionWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }

    /// Authorizes an export, records what it covers, and audits it. The
    /// audit write fails closed: an export that cannot be audited is not
    /// sent.
    pub async fn begin_export(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &ExportQuery,
        request_id: Uuid,
    ) -> Result<StartedExport, RetentionError> {
        let permission = match query.dataset {
            ExportDataset::Usage => AdminPermission::UsageRead,
            ExportDataset::Traces => AdminPermission::TraceRead,
            ExportDataset::AuditEvents => AdminPermission::AuditRead,
        };
        self.authorize(actor, organization_id, permission).await?;
        let export = StartedExport {
            id: Uuid::new_v4(),
            actor,
            organization_id,
            dataset: query.dataset,
            request_id,
        };
        self.store
            .start_export(
                export.id,
                organization_id,
                actor,
                query,
                request_id,
                &export_event(&export, AuditOutcome::Succeeded),
            )
            .await
            .map_err(RetentionError::Admin)?;
        Ok(export)
    }

    /// Records how an export ended; one that did not complete is audited
    /// again as `failed`. Errors are logged: the response is already sent.
    pub async fn finish_export(&self, export: StartedExport, status: ExportStatus, rows: u64) {
        if let Err(error) = self.store.finish_export(export.id, status, rows).await {
            tracing::warn!(export_id = %export.id, error = ?error, "export record not finished");
        }
        if status != ExportStatus::Completed {
            if let Err(error) = self.audit_export(&export, AuditOutcome::Failed).await {
                tracing::warn!(export_id = %export.id, error = ?error, "export failure not audited");
            }
        }
    }

    async fn audit_export(
        &self,
        export: &StartedExport,
        outcome: AuditOutcome,
    ) -> Result<(), RetentionError> {
        self.admin
            .record_audit(export_event(export, outcome))
            .await
            .map_err(RetentionError::Admin)
    }

    /// The organization's exports, newest first: who asked for which slice,
    /// and how each ended.
    pub async fn exports(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<ExportRecord>, RetentionError> {
        self.authorize(actor, organization_id, AdminPermission::AuditRead)
            .await?;
        let scope = AdminScope::Organization(organization_id);
        let codec = self
            .codec
            .as_ref()
            .ok_or(RetentionError::AuthenticationUnavailable)?;
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(token, actor, scope, EXPORT_COLLECTION, "", ORDER_EXPORT)
                    .map_err(RetentionError::Pagination)?;
                // Only a validly signed token gets here, so a bad keyset is
                // a server-side inconsistency.
                Some(
                    serde_json::from_str(&claims.last_keyset).map_err(|_| {
                        RetentionError::Store("export page keyset is invalid".into())
                    })?,
                )
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list_exports(organization_id, page.page_size, after)
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let claims = CursorClaims::new(
                    actor,
                    scope,
                    EXPORT_COLLECTION,
                    "",
                    ORDER_EXPORT,
                    serde_json::json!([last.started_at, last.id]).to_string(),
                    None,
                )
                .map_err(RetentionError::Pagination)?;
                Some(codec.encode(&claims).map_err(RetentionError::Pagination)?)
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }
}

/// The audit event of an export's start or failure; it names the export
/// record.
fn export_event(export: &StartedExport, outcome: AuditOutcome) -> AdminAuditEvent {
    AdminAuditEvent::new(
        AuditActor::User(export.actor),
        export.organization_id,
        None,
        AuditTargetResource::DataExport,
        audit_action(export.dataset.audit_action()),
        export.id,
        outcome,
        export.request_id,
        None,
        None,
    )
}

/// Stable per organization.
fn policy_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&["retention_policy", &organization_id.0.to_string()])
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

fn map_admin_error(error: AdminStoreError) -> RetentionError {
    match error {
        AdminStoreError::Authorization(_) => RetentionError::Forbidden,
        AdminStoreError::ResourceNotFound => RetentionError::NotFound,
        AdminStoreError::VersionConflict(conflict) => RetentionError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => RetentionError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => RetentionError::IdempotencyInProgress,
        other => RetentionError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding},
        identity::TeamId,
    };

    #[test]
    fn retention_is_set_by_owners_and_admins_and_read_org_wide() {
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
                context.allows(AdminPermission::RetentionRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::RetentionWrite, scope),
                write,
                "{role:?}"
            );
        }
        let team_admin = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::team(
                org,
                TeamId::new(),
                user,
                AdminRole::TeamAdmin,
            )],
        };
        assert!(!team_admin.allows(AdminPermission::RetentionRead, scope));
        assert!(!team_admin.allows(AdminPermission::UsageRead, scope));
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-data-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![ROUTE_RETENTION_POLICY, ROUTE_EXPORTS, ROUTE_EXPORT];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
