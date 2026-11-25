//! An organization's own evaluations (Phase 8 milestone 4): evaluation
//! sets, and runs scored from a completed batch. Sets and runs need
//! `catalog_write` to create and `catalog_read` to read; both are
//! idempotent and audited (`model_evaluation_set.create`,
//! `model_evaluation_run.create`). Neither can change once made. A set can
//! be deleted (`model_evaluation_set.delete`, versioned): softly, so it is
//! hidden from lists and cards while its items, runs and scores are kept.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::{
    application::{admin_identities::deterministic_target_id, trace_objects::TraceObjects},
    domain::{
        admin::{
            AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope,
            IdempotencyResultMetadata,
        },
        audit::{AuditAction, AuditSnapshot, AuditTargetResource},
        batch::MAX_FILE_BYTES,
        identity::{OrganizationId, UserAccountId},
        model_evaluation::{self, EvaluationItem, EvaluationRun, EvaluationSet},
        version::IfMatch,
    },
    persistence::{
        AdminStore, AdminStoreError,
        admin::{AdminMutationRequest, MutationStart},
        model_evaluations::ModelEvaluationStore,
    },
};

pub const ROUTE_EVALUATION_SETS: &str = "/api/admin/v1/organizations/{org}/catalog/evaluation-sets";
pub const ROUTE_EVALUATION_SET: &str =
    "/api/admin/v1/organizations/{org}/catalog/evaluation-sets/{set}";
pub const ROUTE_EVALUATION_RUNS: &str =
    "/api/admin/v1/organizations/{org}/catalog/evaluation-sets/{set}/runs";
/// The most sets, and runs of a set, listed.
pub const MAX_LISTED: i64 = 200;

#[derive(Debug)]
pub enum EvaluationError {
    Forbidden,
    NotFound,
    Invalid(&'static str),
    BatchNotFound,
    /// The batch is not completed, is not a chat batch, or its output is
    /// gone.
    BatchUnusable(&'static str),
    StoreUnavailable,
    VersionConflict(crate::domain::version::VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(sqlx::Error),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for EvaluationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error)
    }
}

fn map_admin_error(error: AdminStoreError) -> EvaluationError {
    match error {
        AdminStoreError::Authorization(_) => EvaluationError::Forbidden,
        AdminStoreError::ResourceNotFound => EvaluationError::NotFound,
        AdminStoreError::VersionConflict(conflict) => EvaluationError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => EvaluationError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => EvaluationError::IdempotencyInProgress,
        other => EvaluationError::Admin(other),
    }
}

fn snapshot_error() -> EvaluationError {
    EvaluationError::Store(sqlx::Error::Protocol(
        "recorded response snapshot is invalid".into(),
    ))
}

#[derive(Clone)]
pub struct ModelEvaluationService {
    admin: AdminStore,
    store: ModelEvaluationStore,
    objects: Option<Arc<dyn TraceObjects>>,
}

impl ModelEvaluationService {
    pub fn new(
        admin: AdminStore,
        store: ModelEvaluationStore,
        objects: Option<Arc<dyn TraceObjects>>,
    ) -> Self {
        Self {
            admin,
            store,
            objects,
        }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), EvaluationError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(EvaluationError::Forbidden)
        }
    }

    pub async fn sets(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<Vec<EvaluationSet>, EvaluationError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.store.sets(organization_id, MAX_LISTED).await?)
    }

    pub async fn set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<EvaluationSet, EvaluationError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        self.store
            .set(organization_id, id)
            .await?
            .ok_or(EvaluationError::NotFound)
    }

    pub async fn runs(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        set_id: Uuid,
    ) -> Result<Vec<EvaluationRun>, EvaluationError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        if self.store.set(organization_id, set_id).await?.is_none() {
            return Err(EvaluationError::NotFound);
        }
        Ok(self.store.runs(organization_id, set_id, MAX_LISTED).await?)
    }

    /// Creates a set. Returns it and whether this was a replay.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        name: &str,
        description: Option<&str>,
        items: &[EvaluationItem],
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(EvaluationSet, bool), EvaluationError> {
        let name = name.trim();
        let description = description.map(str::trim).filter(|d| !d.is_empty());
        if let Some(why) = model_evaluation::invalid_set(name, description, items) {
            return Err(EvaluationError::Invalid(why));
        }
        let id = deterministic_target_id(&[
            "model_evaluation_set.create",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_EVALUATION_SETS.into(),
            method: "POST".into(),
            operation: AdminOperation::new("model_evaluation_set.create")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id: id,
            parent_id: None,
            target: AuditTargetResource::ModelEvaluationSet,
            audit_action: AuditAction::new("model_evaluation_set.create")
                .expect("audit action constant is valid"),
            if_match: IfMatch::Any,
            request_id,
            // The items stay out of the request record: only their digest
            // and count.
            body: serde_json::json!({
                "name": name,
                "description": description,
                "item_count": items.len(),
                "items_sha256": format!(
                    "{:x}",
                    <sha2::Sha256 as sha2::Digest>::digest(
                        serde_json::to_vec(items).unwrap_or_default()
                    )
                ),
            }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(record) => {
                let set = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .and_then(|snapshot| serde_json::from_value(snapshot).ok())
                    .ok_or_else(snapshot_error)?;
                return Ok((set, true));
            }
            MutationStart::New(mutation) => mutation,
        };
        let next = mutation.next_version();
        let tx = mutation.transaction();
        ModelEvaluationStore::create_set(
            tx,
            organization_id,
            id,
            name,
            description,
            items,
            actor.0,
        )
        .await?;
        let set = ModelEvaluationStore::set_in(tx, organization_id, id)
            .await?
            .ok_or(EvaluationError::NotFound)?;
        let snapshot = serde_json::to_value(&set).map_err(|_| snapshot_error())?;
        mutation
            .commit(
                None,
                Some(AuditSnapshot {
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: Some(snapshot),
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok((set, false))
    }

    /// Deletes set `id`, softly: it leaves lists, reads and model cards,
    /// and no run can be recorded for it, while its items, runs and scores
    /// are kept. Needs `catalog_write` and the set's version (`If-Match`);
    /// idempotent and audited (`model_evaluation_set.delete`). Returns
    /// whether this was a replay.
    pub async fn delete_set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: Uuid,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<bool, EvaluationError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_EVALUATION_SET.into(),
            method: "DELETE".into(),
            operation: AdminOperation::new("model_evaluation_set.delete")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id: id,
            parent_id: None,
            target: AuditTargetResource::ModelEvaluationSet,
            audit_action: AuditAction::new("model_evaluation_set.delete")
                .expect("audit action constant is valid"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(_) => return Ok(true),
            MutationStart::New(mutation) => mutation,
        };
        let next = mutation.next_version();
        let tx = mutation.transaction();
        // A set deleted before is not found: dropping the mutation rolls
        // its version bump back.
        if !ModelEvaluationStore::delete_set(tx, organization_id, id, actor.0).await? {
            return Err(EvaluationError::NotFound);
        }
        mutation
            .commit(
                Some(AuditSnapshot {
                    enabled: Some(true),
                    version: next.get().checked_sub(1).filter(|v| *v > 0),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    // Whether the set is in use: false once deleted.
                    enabled: Some(false),
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
        Ok(false)
    }

    /// Scores set `set_id` from the completed chat batch `batch_id` of the
    /// organization, and records the run. Returns it and whether this was a
    /// replay.
    pub async fn create_run(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        set_id: Uuid,
        batch_id: Uuid,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(EvaluationRun, bool), EvaluationError> {
        // Checked before the batch is read, so a caller who may not record
        // a run learns nothing of the organization's batches.
        self.authorize(actor, organization_id, AdminPermission::CatalogWrite)
            .await?;
        let set = self
            .store
            .set(organization_id, set_id)
            .await?
            .ok_or(EvaluationError::NotFound)?;
        // Read before the transaction opens: the output can be large. A
        // failure is answered after the idempotency check below, so a retry
        // of a recorded run replays even once the files are unreadable or
        // gone.
        let scored = self.score(organization_id, &set, batch_id).await;
        let id = deterministic_target_id(&[
            "model_evaluation_run.create",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_EVALUATION_RUNS.into(),
            method: "POST".into(),
            operation: AdminOperation::new("model_evaluation_run.create")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id: id,
            parent_id: Some(set_id),
            target: AuditTargetResource::ModelEvaluationRun,
            audit_action: AuditAction::new("model_evaluation_run.create")
                .expect("audit action constant is valid"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({"set_id": set_id, "batch_id": batch_id}),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(record) => {
                let run = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .and_then(|snapshot| serde_json::from_value(snapshot).ok())
                    .ok_or_else(snapshot_error)?;
                return Ok((run, true));
            }
            MutationStart::New(mutation) => mutation,
        };
        // Not a replay: a run needs its scores (dropping the mutation rolls
        // its transaction back).
        let (scores, unmatched) = match scored {
            Ok(scored) => scored,
            Err(error) => {
                drop(mutation);
                return Err(error);
            }
        };
        let next = mutation.next_version();
        let tx = mutation.transaction();
        // Checked again under a lock: the set may have been deleted since
        // it was read.
        if !ModelEvaluationStore::lock_live_set(tx, organization_id, set_id).await? {
            return Err(EvaluationError::NotFound);
        }
        ModelEvaluationStore::create_run(
            tx,
            organization_id,
            id,
            set_id,
            batch_id,
            unmatched,
            &scores,
            actor.0,
        )
        .await?;
        let run = ModelEvaluationStore::runs_in(tx, organization_id, set_id, Some(id), 1)
            .await?
            .pop()
            .ok_or(EvaluationError::NotFound)?;
        let snapshot = serde_json::to_value(&run).map_err(|_| snapshot_error())?;
        mutation
            .commit(
                None,
                Some(AuditSnapshot {
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: Some(snapshot),
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok((run, false))
    }

    /// Each model's score from the batch's output and error files.
    async fn score(
        &self,
        organization_id: OrganizationId,
        set: &EvaluationSet,
        batch_id: Uuid,
    ) -> Result<(Vec<model_evaluation::ModelScore>, u32), EvaluationError> {
        let batch = self
            .store
            .run_batch(organization_id, batch_id)
            .await?
            .ok_or(EvaluationError::BatchNotFound)?;
        if batch.status != "completed" {
            return Err(EvaluationError::BatchUnusable("the batch is not completed"));
        }
        if batch.operation != "chat_batch" {
            return Err(EvaluationError::BatchUnusable(
                "the batch is not a chat batch",
            ));
        }
        let objects = self
            .objects
            .as_ref()
            .ok_or(EvaluationError::StoreUnavailable)?;
        let mut lines = Vec::new();
        for (file, key, bytes, purged) in [
            (
                batch.output_file_id,
                &batch.object_key,
                batch.bytes,
                batch.content_purged_at,
            ),
            (
                batch.error_file_id,
                &batch.error_object_key,
                batch.error_bytes,
                batch.error_purged_at,
            ),
        ] {
            if file.is_none() {
                continue;
            }
            let (Some(key), None) = (key, purged) else {
                return Err(EvaluationError::BatchUnusable(
                    "the batch's output was deleted by retention",
                ));
            };
            if bytes.is_some_and(|bytes| bytes > MAX_FILE_BYTES as i64) {
                return Err(EvaluationError::BatchUnusable(
                    "the batch's output is too large",
                ));
            }
            let content = objects
                .get(key)
                .await
                .map_err(|_| EvaluationError::StoreUnavailable)?
                .ok_or(EvaluationError::BatchUnusable(
                    "the batch's output was deleted by retention",
                ))?;
            lines.extend(
                content
                    .split(|byte| *byte == b'\n')
                    .filter(|line| !line.is_empty())
                    .filter_map(|line| serde_json::from_slice::<Value>(line).ok()),
            );
        }
        let models = self.store.line_models(organization_id, batch_id).await?;
        Ok(model_evaluation::score(&set.items, &lines, &models))
    }
}
