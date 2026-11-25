// Usage export application service: read, set and delete an organization's
// export to its own object storage (D-014).
//
// Reading needs `usage_export_read` (every org-wide role); setting and
// deleting need `usage_export_write` (owner and admin) and run through the
// admin mutation coordinator, so they are idempotent, versioned and audited.
// Setting a destination first writes `_gateway-verify.json` to it through the
// named connection, outside any transaction; a destination the gateway
// cannot write is refused and nothing is saved. A replayed request is
// verified again before its recorded response is returned.

use std::sync::Arc;

use chrono::Utc;
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
    usage_export::{
        DestinationError, DestinationInput, ObjectWriter, UsageExportView, VERIFY_OBJECT,
        WriteError,
    },
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    usage_export::UsageExportStore,
};

pub const ROUTE_USAGE_EXPORT: &str = "/api/admin/v1/organizations/{org}/usage-export";

#[derive(Debug)]
pub enum UsageExportError {
    Forbidden,
    NotFound,
    Invalid(DestinationError),
    /// The destination could not be written through the connection.
    Unverified(WriteError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for UsageExportError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct UsageExportWrite {
    pub export: UsageExportView,
    pub created: bool,
}

#[derive(Clone)]
pub struct UsageExportService {
    admin: AdminStore,
    store: UsageExportStore,
    writer: Arc<dyn ObjectWriter>,
}

impl UsageExportService {
    pub fn new(admin: AdminStore, store: UsageExportStore, writer: Arc<dyn ObjectWriter>) -> Self {
        Self {
            admin,
            store,
            writer,
        }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), UsageExportError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(UsageExportError::Forbidden)
        }
    }

    pub async fn get(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<UsageExportView, UsageExportError> {
        self.authorize(actor, organization_id, AdminPermission::UsageExportRead)
            .await?;
        self.store
            .get(organization_id)
            .await?
            .ok_or(UsageExportError::NotFound)
    }

    /// Verifies the destination, then saves it.
    pub async fn set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: DestinationInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<UsageExportWrite>, UsageExportError> {
        let destination = input.validate().map_err(UsageExportError::Invalid)?;
        // Before writing anything on the actor's behalf. A denial is
        // audited as the coordinator would audit it.
        if let Err(error) = self
            .authorize(actor, organization_id, AdminPermission::UsageExportWrite)
            .await
        {
            if matches!(error, UsageExportError::Forbidden) {
                let denied = AdminAuditEvent::new(
                    AuditActor::User(actor),
                    organization_id,
                    None,
                    AuditTargetResource::UsageExport,
                    audit_action("usage_export.set"),
                    export_id(organization_id),
                    AuditOutcome::Denied,
                    request_id,
                    None,
                    None,
                );
                if let Err(audit) = self.admin.record_audit(denied).await {
                    tracing::warn!(error = ?audit, "usage export denial not audited");
                }
            }
            return Err(error);
        }
        let verified_at = Utc::now();
        let marker = serde_json::json!({
            "gateway": "qucelerate",
            "organization_id": organization_id.0,
            "verified_at": verified_at,
        });
        self.writer
            .write(
                organization_id,
                &destination,
                &destination.object(VERIFY_OBJECT),
                "application/json",
                marker.to_string().into_bytes(),
            )
            .await
            .map_err(UsageExportError::Unverified)?;
        let target_id = export_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::UsageExportWrite,
            route: ROUTE_USAGE_EXPORT.into(),
            method: "PUT".into(),
            operation: operation("usage_export.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::UsageExport,
            audit_action: audit_action("usage_export.set"),
            if_match,
            request_id,
            body: serde_json::to_value(&destination)
                .map_err(|_| UsageExportError::Store("destination is not serializable".into()))?,
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(UsageExportError::NotFound)?;
                let export = serde_json::from_value(snapshot).map_err(|_| {
                    UsageExportError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: UsageExportWrite {
                        export,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let created = UsageExportStore::upsert(
                    tx,
                    organization_id,
                    target_id,
                    &destination,
                    actor,
                    verified_at,
                )
                .await?;
                let stored = UsageExportStore::view(tx, organization_id)
                    .await?
                    .ok_or(UsageExportError::NotFound)?;
                let next = mutation.next_version();
                let export = UsageExportView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&export).map_err(|_| {
                    UsageExportError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        (!created).then(|| AuditSnapshot {
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
                    item: UsageExportWrite { export, created },
                    replayed: false,
                })
            }
        }
    }

    /// Deletes the export and discards records not yet written.
    pub async fn delete(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, UsageExportError> {
        let target_id = export_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::UsageExportWrite,
            route: ROUTE_USAGE_EXPORT.into(),
            method: "DELETE".into(),
            operation: operation("usage_export.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::UsageExport,
            audit_action: audit_action("usage_export.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(_) => Ok(MutationOutcome {
                item: (),
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                if !UsageExportStore::delete(tx, organization_id).await? {
                    return Err(UsageExportError::NotFound);
                }
                let next = mutation.next_version();
                mutation
                    .commit(
                        Some(AuditSnapshot {
                            version: previous(next),
                            ..Default::default()
                        }),
                        None,
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: (),
                    replayed: false,
                })
            }
        }
    }
}

/// Stable per organization, so a deleted export set again keeps its
/// version history.
fn export_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&["usage_export", &organization_id.0.to_string()])
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

fn map_admin_error(error: AdminStoreError) -> UsageExportError {
    match error {
        AdminStoreError::Authorization(_) => UsageExportError::Forbidden,
        AdminStoreError::ResourceNotFound => UsageExportError::NotFound,
        AdminStoreError::VersionConflict(conflict) => UsageExportError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => UsageExportError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => UsageExportError::IdempotencyInProgress,
        other => UsageExportError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn exports_are_set_by_owners_and_admins_and_read_org_wide() {
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
                context.allows(AdminPermission::UsageExportRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::UsageExportWrite, scope),
                write,
                "{role:?}"
            );
        }
        assert!(!AdminPermission::UsageExportRead.is_team_scoped());
    }

    #[test]
    fn operations_and_actions_are_valid() {
        for value in [
            "usage_export.set",
            "usage_export.delete",
            "usage_export.deleted",
        ] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }

    fn contract() -> serde_json::Value {
        serde_json::from_str(include_str!("../../openapi/admin-usage-export-v1.json")).unwrap()
    }

    /// The schema's properties, which must all be required.
    fn properties(schema: &str) -> Vec<String> {
        let contract = contract();
        let schema = &contract["components"]["schemas"][schema];
        let mut names: Vec<String> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut required: Vec<String> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect();
        names.sort_unstable();
        required.sort_unstable();
        assert_eq!(names, required);
        names
    }

    fn keys(value: impl serde::Serialize) -> Vec<String> {
        let mut keys: Vec<String> = serde_json::to_value(value)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn openapi_contract_matches_the_route_and_payloads() {
        let contract = contract();
        let paths: Vec<&String> = contract["paths"].as_object().unwrap().keys().collect();
        assert_eq!(paths, [ROUTE_USAGE_EXPORT]);
        let now = Utc::now();
        let view = UsageExportView {
            id: Uuid::nil(),
            kind: crate::domain::usage_export::StorageKind::Gcs,
            connection: "gemini-main".into(),
            bucket: "acme-usage".into(),
            account: None,
            region: None,
            prefix: String::new(),
            enabled_at: now,
            verified_at: now,
            last_success_at: None,
            failing: false,
            failing_since: None,
            last_error: None,
            queued_records: 0,
            oldest_queued_at: None,
            version: 1,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(keys(&view), properties("UsageExport"));
        let record = crate::domain::usage_export::UsageRecord {
            schema: crate::domain::usage_export::RECORD_SCHEMA.into(),
            event_id: Uuid::nil(),
            trace_id: Uuid::nil(),
            reservation_id: Uuid::nil(),
            organization_id: Uuid::nil(),
            team_id: None,
            service_account_id: None,
            end_user: None,
            operation: "chat".into(),
            deployment: None,
            model: "model".into(),
            provider_family: None,
            location: None,
            outcome: "settled".into(),
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            reasoning_tokens: None,
            other_tokens: None,
            usage_confidence: None,
            usage_completeness: None,
            pricing_confidence: None,
            amount_minor: None,
            currency: "USD".into(),
            recorded_at: now,
            finalized_at: now,
            batch_id: None,
            custom_id: None,
            model_master: None,
        };
        assert_eq!(keys(&record), properties("UsageRecord"));
        let failing = crate::domain::usage_export::UsageExportFailing {
            export_id: Uuid::nil(),
            kind: crate::domain::usage_export::StorageKind::S3,
            failing_since: now,
            last_error: None,
            queued_records: 0,
        };
        assert_eq!(keys(&failing), properties("UsageExportFailing"));
    }
}
