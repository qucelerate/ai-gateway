// Batch administration (D-021, Phase 7 milestone 6): an organization's
// batch settings, its provider connections' batch staging, and the
// organization's batches as the console's Usage -> Batches shows them.
//
// Reading the settings and staging needs `catalog_read`; setting the
// settings needs `catalog_write`, and setting or deleting a staging
// `credential_write`, as the connection's credential does. Listing batches
// and their detail needs `usage_read`; downloading a batch's results needs
// `trace_content_read`, since results carry the model's answers. Cancelling
// a batch needs `service_account_mutate`, as the service account's own
// cancel does. Every write runs through the admin mutation coordinator:
// idempotent, versioned and audited (`batch_settings.set`,
// `batch_staging.set`, `batch_staging.delete`, `batch.cancel`).

use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::application::trace_objects::TraceObjects;
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    batch::{BatchSettingsInput, BatchStagingInput},
    catalog::ProviderFamily,
    identity::{OrganizationId, UserAccountId},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    batch_settings::{BatchSettingsStore, BatchSettingsView, BatchStagingView},
    batches::{BatchCost, BatchRow, BatchStore, BatchStoreError, JobSummary},
};

pub const ROUTE_BATCH_SETTINGS: &str = "/api/admin/v1/organizations/{org}/batch-settings";
pub const ROUTE_BATCH_STAGINGS: &str = "/api/admin/v1/organizations/{org}/batch-staging";
pub const ROUTE_BATCH_STAGING: &str =
    "/api/admin/v1/organizations/{org}/batch-staging/{connection}";
pub const ROUTE_ADMIN_BATCHES: &str = "/api/admin/v1/organizations/{org}/batches";
pub const ROUTE_ADMIN_BATCH: &str = "/api/admin/v1/organizations/{org}/batches/{batch}";
pub const ROUTE_ADMIN_BATCH_CANCEL: &str =
    "/api/admin/v1/organizations/{org}/batches/{batch}/cancel";
pub const ROUTE_ADMIN_BATCH_OUTPUT: &str =
    "/api/admin/v1/organizations/{org}/batches/{batch}/output";
pub const ROUTE_ADMIN_BATCH_ERRORS: &str =
    "/api/admin/v1/organizations/{org}/batches/{batch}/errors";

/// Most batches one page lists.
pub const MAX_PAGE: i64 = 100;

#[derive(Debug)]
pub enum BatchAdminError {
    Forbidden,
    NotFound,
    Invalid(&'static str),
    /// The connection runs no batch jobs staged in a bucket.
    NotStaged,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    /// The results are not written yet, or were purged.
    ResultsUnavailable,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for BatchAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

impl From<BatchStoreError> for BatchAdminError {
    fn from(error: BatchStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct SettingsWrite {
    pub settings: BatchSettingsView,
    pub created: bool,
}

#[derive(Clone, Debug)]
pub struct StagingWrite {
    pub staging: BatchStagingView,
    pub created: bool,
}

/// A batch with its jobs and what it has cost, as the admin detail shows it.
#[derive(Clone, Debug)]
pub struct BatchDetail {
    pub batch: BatchRow,
    pub jobs: Vec<JobSummary>,
    pub costs: Vec<BatchCost>,
}

/// Which of a batch's result files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultFile {
    Output,
    Errors,
}

#[derive(Clone)]
pub struct BatchAdminService {
    admin: AdminStore,
    settings: BatchSettingsStore,
    batches: BatchStore,
    objects: Option<Arc<dyn TraceObjects>>,
}

impl BatchAdminService {
    pub fn new(
        admin: AdminStore,
        settings: BatchSettingsStore,
        batches: BatchStore,
        objects: Option<Arc<dyn TraceObjects>>,
    ) -> Self {
        Self {
            admin,
            settings,
            batches,
            objects,
        }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), BatchAdminError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(BatchAdminError::Forbidden)
        }
    }

    /// The organization's settings; never set, emulated batches are off.
    pub async fn settings(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<BatchSettingsView, BatchAdminError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.settings.settings(organization_id).await?)
    }

    /// Replace the settings. Batches already made keep the mode each job
    /// was given; the concurrency applies from each job's next run.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_settings(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: BatchSettingsInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<SettingsWrite>, BatchAdminError> {
        let (allowed, concurrency) = input.validate().map_err(BatchAdminError::Invalid)?;
        let target_id =
            deterministic_target_id(&["batch_settings", &organization_id.0.to_string()]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_BATCH_SETTINGS.into(),
            method: "PUT".into(),
            operation: operation("batch_settings.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::BatchSettings,
            audit_action: audit_action("batch_settings.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "emulated_allowed": allowed,
                "emulated_concurrency": concurrency,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: SettingsWrite {
                    settings: replayed(record.response_metadata)?,
                    created: false,
                },
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let existed = BatchSettingsStore::settings_view(tx, organization_id)
                    .await?
                    .is_some();
                let created = BatchSettingsStore::upsert_settings(
                    tx,
                    organization_id,
                    target_id,
                    allowed,
                    concurrency,
                    actor,
                )
                .await?;
                let stored = BatchSettingsStore::settings_view(tx, organization_id)
                    .await?
                    .ok_or(BatchAdminError::NotFound)?;
                let next = mutation.next_version();
                let settings = BatchSettingsView {
                    version: next.get(),
                    ..stored
                };
                commit(mutation, existed, next, target_id, &settings).await?;
                Ok(MutationOutcome {
                    item: SettingsWrite { settings, created },
                    replayed: false,
                })
            }
        }
    }

    /// The connections' staging.
    pub async fn stagings(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<Vec<BatchStagingView>, BatchAdminError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.settings.staging_list(organization_id).await?)
    }

    /// Set where `connection`'s batch jobs stage, reviving a deleted
    /// staging. The connection must be the organization's and of a family
    /// whose batch jobs are staged (Bedrock: S3; Gemini: Cloud Storage).
    #[allow(clippy::too_many_arguments)]
    pub async fn set_staging(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        connection: &str,
        input: BatchStagingInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<StagingWrite>, BatchAdminError> {
        crate::domain::credentials::validate_adapter_instance_key(connection)
            .map_err(|_| BatchAdminError::NotFound)?;
        let target_id = staging_id(organization_id, connection);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CredentialWrite,
            route: ROUTE_BATCH_STAGING.into(),
            method: "PUT".into(),
            operation: operation("batch_staging.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::BatchStaging,
            audit_action: audit_action("batch_staging.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "connection": connection,
                "bucket": input.bucket,
                "prefix": input.prefix,
                "region": input.region,
                "role_arn": input.role_arn,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: StagingWrite {
                    staging: replayed(record.response_metadata)?,
                    created: false,
                },
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let kind = BatchSettingsStore::connection_family(tx, organization_id, connection)
                    .await?
                    .ok_or(BatchAdminError::NotFound)?
                    .parse::<ProviderFamily>()
                    .ok()
                    .and_then(crate::provider::batch::staging_kind)
                    .ok_or(BatchAdminError::NotStaged)?;
                let valid = input.validate(kind).map_err(BatchAdminError::Invalid)?;
                let existed = BatchSettingsStore::staging_view(tx, organization_id, connection)
                    .await?
                    .is_some();
                let created = BatchSettingsStore::upsert_staging(
                    tx,
                    organization_id,
                    target_id,
                    connection,
                    kind,
                    &valid,
                    actor,
                )
                .await?;
                let stored = BatchSettingsStore::staging_view(tx, organization_id, connection)
                    .await?
                    .ok_or(BatchAdminError::NotFound)?;
                let next = mutation.next_version();
                let staging = BatchStagingView {
                    version: next.get(),
                    ..stored
                };
                commit(mutation, existed, next, target_id, &staging).await?;
                Ok(MutationOutcome {
                    item: StagingWrite { staging, created },
                    replayed: false,
                })
            }
        }
    }

    /// Hide `connection`'s staging: its native batch jobs are no longer
    /// made, and new lines for it run emulated where allowed, else are
    /// refused. Jobs already sent still collect their output from it.
    pub async fn delete_staging(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        connection: &str,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, BatchAdminError> {
        crate::domain::credentials::validate_adapter_instance_key(connection)
            .map_err(|_| BatchAdminError::NotFound)?;
        let target_id = staging_id(organization_id, connection);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CredentialWrite,
            route: ROUTE_BATCH_STAGING.into(),
            method: "DELETE".into(),
            operation: operation("batch_staging.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::BatchStaging,
            audit_action: audit_action("batch_staging.delete"),
            if_match,
            request_id,
            body: serde_json::json!({ "connection": connection }),
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
                let shown = BatchSettingsStore::delete_staging(
                    mutation.transaction(),
                    organization_id,
                    connection,
                    actor,
                )
                .await?;
                if !shown {
                    return Err(BatchAdminError::NotFound);
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

    /// The organization's batches, newest first: at most `limit`, which is
    /// a page of up to [`MAX_PAGE`] and one row more, to tell whether
    /// another page follows.
    pub async fn batches(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: Option<Uuid>,
        status: Option<&str>,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<BatchRow>, BatchAdminError> {
        self.authorize(actor, organization_id, AdminPermission::UsageRead)
            .await?;
        Ok(self
            .batches
            .admin_list(
                organization_id.0,
                service_account_id,
                status,
                after,
                limit.clamp(1, MAX_PAGE + 1),
            )
            .await?)
    }

    /// A batch, its jobs and its cost so far.
    pub async fn batch(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<BatchDetail, BatchAdminError> {
        self.authorize(actor, organization_id, AdminPermission::UsageRead)
            .await?;
        self.detail(organization_id, id).await
    }

    async fn detail(
        &self,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<BatchDetail, BatchAdminError> {
        let batch = self
            .batches
            .admin_batch(organization_id.0, id)
            .await?
            .ok_or(BatchAdminError::NotFound)?;
        let (jobs, costs) = tokio::try_join!(
            self.batches.job_summaries(organization_id.0, id),
            self.batches.costs(organization_id.0, id)
        )?;
        Ok(BatchDetail { batch, jobs, costs })
    }

    /// A batch's output (each line's answer) or errors (each line's
    /// failure), as JSONL.
    pub async fn results(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: Uuid,
        which: ResultFile,
    ) -> Result<Vec<u8>, BatchAdminError> {
        self.authorize(actor, organization_id, AdminPermission::TraceContentRead)
            .await?;
        let batch = self
            .batches
            .admin_batch(organization_id.0, id)
            .await?
            .ok_or(BatchAdminError::NotFound)?;
        let file_id = match which {
            ResultFile::Output => batch.output_file_id,
            ResultFile::Errors => batch.error_file_id,
        }
        .ok_or(BatchAdminError::ResultsUnavailable)?;
        let file = self
            .batches
            .admin_file(organization_id.0, file_id)
            .await?
            .filter(|file| file.content_purged_at.is_none())
            .ok_or(BatchAdminError::ResultsUnavailable)?;
        let objects = self
            .objects
            .as_ref()
            .ok_or_else(|| BatchAdminError::Store("no object store".into()))?;
        objects
            .get(&file.object_key)
            .await
            .map_err(|_| BatchAdminError::Store("object store unavailable".into()))?
            .ok_or(BatchAdminError::ResultsUnavailable)
    }

    /// Ask a batch to stop, as its service account's cancel does: lines
    /// not yet run end at no charge, and lines already with the provider
    /// settle as it reports them. A batch already ending is returned as it
    /// is.
    pub async fn cancel(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: Uuid,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<BatchDetail>, BatchAdminError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::ServiceAccountMutate,
            route: ROUTE_ADMIN_BATCH_CANCEL.into(),
            method: "POST".into(),
            operation: operation("batch.cancel"),
            idempotency_key,
            target_id: id,
            parent_id: None,
            target: AuditTargetResource::Batch,
            audit_action: audit_action("batch.cancelled"),
            if_match: IfMatch::Any,
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
            // The batch as it is now: a cancel is only ever asked once.
            MutationStart::Replay(_) => Ok(MutationOutcome {
                item: self.detail(organization_id, id).await?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                BatchStore::cancel_in(
                    mutation.transaction(),
                    organization_id.0,
                    None,
                    id,
                    Utc::now(),
                )
                .await?
                .ok_or(BatchAdminError::NotFound)?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        previous(next).map(|version| AuditSnapshot {
                            version: Some(version),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: self.detail(organization_id, id).await?,
                    replayed: false,
                })
            }
        }
    }
}

/// Stable per organization and connection, so a revived staging keeps its
/// version.
fn staging_id(organization_id: OrganizationId, connection: &str) -> Uuid {
    deterministic_target_id(&["batch_staging", &organization_id.0.to_string(), connection])
}

/// Commits a set: audited with the versions before and after, and the
/// view recorded for a replay.
async fn commit<T: serde::Serialize>(
    mutation: crate::persistence::admin::AdminMutation<'_>,
    existed: bool,
    next: ResourceVersion,
    target_id: Uuid,
    view: &T,
) -> Result<(), BatchAdminError> {
    let snapshot = serde_json::to_value(view)
        .map_err(|_| BatchAdminError::Store("response snapshot is not serializable".into()))?;
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
        .map_err(map_admin_error)
}

/// The recorded response of a replayed set, even if it has changed since.
fn replayed<T: serde::de::DeserializeOwned>(
    metadata: Option<IdempotencyResultMetadata>,
) -> Result<T, BatchAdminError> {
    let snapshot = metadata
        .and_then(|metadata| metadata.snapshot)
        .ok_or(BatchAdminError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| BatchAdminError::Store("recorded response snapshot is invalid".into()))
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

fn map_admin_error(error: AdminStoreError) -> BatchAdminError {
    match error {
        AdminStoreError::Authorization(_) => BatchAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => BatchAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => BatchAdminError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => BatchAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => BatchAdminError::IdempotencyInProgress,
        other => BatchAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};
    use crate::domain::batch::StagingKind;

    #[test]
    fn batches_are_read_by_usage_readers_and_staged_by_credential_writers() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, settings, staging) in [
            (AdminRole::OrganizationOwner, true, true),
            (AdminRole::OrganizationAdmin, true, true),
            (AdminRole::ReadOnlyMember, false, false),
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![AdminRoleBinding::organization(org, user, role)],
            };
            assert!(
                context.allows(AdminPermission::UsageRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::CatalogWrite, scope),
                settings,
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::CredentialWrite, scope),
                staging,
                "{role:?}"
            );
        }
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-batches-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = [
            ROUTE_BATCH_SETTINGS,
            ROUTE_BATCH_STAGINGS,
            ROUTE_BATCH_STAGING,
            ROUTE_ADMIN_BATCHES,
            ROUTE_ADMIN_BATCH,
            ROUTE_ADMIN_BATCH_CANCEL,
            ROUTE_ADMIN_BATCH_OUTPUT,
            ROUTE_ADMIN_BATCH_ERRORS,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }

    #[test]
    fn a_stagings_id_is_its_connections() {
        let org = OrganizationId::new();
        assert_eq!(
            staging_id(org, "bedrock-main"),
            staging_id(org, "bedrock-main")
        );
        assert_ne!(
            staging_id(org, "bedrock-main"),
            staging_id(org, "gemini-main")
        );
        assert_ne!(
            staging_id(org, "bedrock-main"),
            staging_id(OrganizationId::new(), "bedrock-main")
        );
    }

    #[test]
    fn staging_kinds_follow_the_family() {
        assert_eq!(
            crate::provider::batch::staging_kind(ProviderFamily::Bedrock),
            Some(StagingKind::S3)
        );
        assert_eq!(
            crate::provider::batch::staging_kind(ProviderFamily::OpenAi),
            None
        );
    }
}
