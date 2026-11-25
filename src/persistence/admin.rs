use std::{future::Future, pin::Pin};

use sqlx::{FromRow, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::{
    admin::{
        AdminAuthorizationContext, AdminIdempotencyKey, AdminOperation, AdminPermission,
        AdminRoleBinding, AdminScope, IdempotencyResultMetadata, RequestFingerprint, authorize,
        team_membership_version_id,
    },
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    identity::{OrganizationId, TeamId, UserAccountId},
    pagination::{AdminRequestFingerprintInput, canonical_admin_fingerprint},
    version::{IfMatch, ResourceVersion, VersionConflict},
};

#[derive(Debug, Error)]
pub enum AdminStoreError {
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("invalid stored administrative role")]
    InvalidRole,
    #[error("authorization failed: {0}")]
    Authorization(#[from] crate::domain::admin::AuthorizationError),
    #[error("idempotency key has a different request fingerprint")]
    IdempotencyConflict,
    #[error("idempotent request is still in progress")]
    IdempotencyInProgress,
    #[error("resource version row does not exist")]
    ResourceNotFound,
    #[error("resource version conflict")]
    VersionConflict(VersionConflict),
    #[error("audit event or replay metadata could not be serialized safely")]
    SafeSerialization,
    #[error("audit actor is not valid for the organization")]
    InvalidAuditActor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminIdempotencyRecord {
    pub organization_id: OrganizationId,
    pub actor_id: UserAccountId,
    pub operation: String,
    pub key: String,
    pub fingerprint: RequestFingerprint,
    pub status: String,
    pub resource_id: Option<Uuid>,
    pub response_metadata: Option<IdempotencyResultMetadata>,
}

#[derive(Clone)]
pub struct AdminStore {
    pool: PgPool,
}

impl AdminStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn coordinator(&self) -> AdminMutationCoordinator {
        AdminMutationCoordinator {
            pool: self.pool.clone(),
        }
    }

    pub fn audit_store(&self) -> crate::persistence::audit::AuditStore {
        crate::persistence::audit::AuditStore::new(self.pool.clone())
    }

    /// Read-only context loading still revalidates user and membership state;
    /// callers must never construct authorization from a client-selected role.
    pub async fn authorization_context(
        &self,
        actor_id: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminAuthorizationContext, AdminStoreError> {
        let mut transaction = self.pool.begin().await?;
        let context =
            load_authorization_context(&mut transaction, actor_id, organization_id).await?;
        transaction.commit().await?;
        Ok(context)
    }

    pub async fn current_version(
        &self,
        organization_id: OrganizationId,
        resource_type: &str,
        resource_id: Uuid,
    ) -> Result<Option<ResourceVersion>, AdminStoreError> {
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM gateway.admin_resource_version
             WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3",
        )
        .bind(organization_id.0)
        .bind(resource_type)
        .bind(resource_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(version
            .and_then(|v| u64::try_from(v).ok())
            .and_then(ResourceVersion::new))
    }

    /// Record an audit outcome (typically a denied authorization) on its own
    /// transaction. This never shares a mutation transaction, so recording a
    /// denial can never commit resource-mutation state.
    pub async fn record_audit(&self, event: AdminAuditEvent) -> Result<(), AdminStoreError> {
        let mut transaction = self.pool.begin().await?;
        insert_audit(&mut transaction, &event).await?;
        transaction.commit().await?;
        Ok(())
    }
}

/// Request data used by every mutating admin endpoint. The coordinator derives
/// the fingerprint from these fields, rather than trusting a transport value.
#[derive(Clone, Debug)]
pub struct AdminMutationRequest {
    pub actor_id: UserAccountId,
    pub organization_id: OrganizationId,
    pub scope: AdminScope,
    pub permission: AdminPermission,
    pub route: String,
    pub method: String,
    pub operation: AdminOperation,
    pub idempotency_key: AdminIdempotencyKey,
    pub target_id: Uuid,
    /// Parent reference required by create-mode mutations so the missing target
    /// is authorized only after revalidating a persisted parent. For an API key
    /// create/rotate this is the parent service account; for a role-binding or
    /// service-account create it is the user who will hold the membership/SA;
    /// membership creates identify the user via `target_id` and leave this unset.
    pub parent_id: Option<Uuid>,
    pub target: AuditTargetResource,
    pub audit_action: AuditAction,
    pub if_match: IfMatch,
    pub request_id: Uuid,
    pub body: serde_json::Value,
}

pub enum MutationStart<'a> {
    New(AdminMutation<'a>),
    Replay(AdminIdempotencyRecord),
}

/// A key claimed and committed by [`AdminMutationCoordinator::hold`], before
/// a mutation calls a provider: another request with the key meanwhile gets
/// the usual conflict (another body) or in-progress answer (the same body),
/// with no lock held across the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldClaim(Uuid);

pub enum HoldStart {
    Held(HeldClaim),
    Replay(Box<AdminIdempotencyRecord>),
}

/// How [`AdminMutationCoordinator::begin_with`] claims the key.
enum ClaimMode {
    /// Claim it in this transaction.
    Fresh,
    /// Claim it and commit only the claim, held this long.
    Hold(std::time::Duration),
    /// Continue a claim [`AdminMutationCoordinator::hold`] committed.
    Resume(HeldClaim),
}

// Short-lived and internal: the mutation is unwrapped at once.
#[allow(clippy::large_enum_variant)]
enum Started<'a> {
    Mutation(MutationStart<'a>),
    Held(HeldClaim),
}

/// Owns the transaction for one admin mutation. Resource SQL must execute via
/// `transaction()` and completion is the only path that commits it.
#[must_use]
pub struct AdminMutation<'a> {
    transaction: Option<Transaction<'a, Postgres>>,
    request: AdminMutationRequest,
    claim_id: Uuid,
    next_version: ResourceVersion,
}

impl<'a> AdminMutation<'a> {
    pub fn transaction(&mut self) -> &mut Transaction<'a, Postgres> {
        self.transaction
            .as_mut()
            .expect("admin mutation transaction is active")
    }

    pub fn next_version(&self) -> ResourceVersion {
        self.next_version
    }

    pub fn target_id(&self) -> Uuid {
        self.request.target_id
    }

    /// Records the mutation under another action, when what it did depends on
    /// state read inside the transaction (a delete that disables instead).
    pub fn set_audit_action(&mut self, action: AuditAction) {
        self.request.audit_action = action;
    }

    /// Audit insertion and idempotency completion occur before commit. Any
    /// failure drops the transaction and rolls back the resource mutation.
    pub async fn commit(
        mut self,
        before: Option<AuditSnapshot>,
        after: Option<AuditSnapshot>,
        response_metadata: Option<IdempotencyResultMetadata>,
    ) -> Result<(), AdminStoreError> {
        let event = AdminAuditEvent {
            actor: AuditActor::User(self.request.actor_id),
            organization_id: self.request.organization_id,
            team_id: self.request.scope.team_id(),
            target: self.request.target,
            action: self.request.audit_action,
            target_id: self.request.target_id,
            outcome: AuditOutcome::Succeeded,
            request_id: self.request.request_id,
            before,
            after,
        };
        let transaction = self
            .transaction
            .as_mut()
            .expect("admin mutation transaction is active");
        insert_audit(transaction, &event).await?;
        let response = response_metadata
            .map(|value| {
                serde_json::to_value(value).map_err(|_| AdminStoreError::SafeSerialization)
            })
            .transpose()?;
        let changed = sqlx::query(
            "UPDATE gateway.admin_idempotency_record
             SET status = 'succeeded', resource_id = $6, response_metadata = $7,
                 completed_at = CURRENT_TIMESTAMP, claim_expires_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND actor_user_id = $2 AND operation = $3
               AND idempotency_key = $4 AND claim_id = $5 AND status = 'in_progress'",
        )
        .bind(self.request.organization_id.0)
        .bind(self.request.actor_id.0)
        .bind(self.request.operation.as_str())
        .bind(self.request.idempotency_key.as_str())
        .bind(self.claim_id)
        .bind(self.request.target_id)
        .bind(response)
        .execute(&mut **transaction)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(AdminStoreError::IdempotencyInProgress);
        }
        self.transaction
            .take()
            .expect("admin transaction present")
            .commit()
            .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct AdminMutationCoordinator {
    pool: PgPool,
}

impl AdminMutationCoordinator {
    pub async fn begin(
        &self,
        request: AdminMutationRequest,
    ) -> Result<MutationStart<'_>, AdminStoreError> {
        match self.begin_with(request, ClaimMode::Fresh).await? {
            Started::Mutation(start) => Ok(start),
            Started::Held(_) => unreachable!("a fresh claim is not held"),
        }
    }

    /// Authorizes the request and claims its key, committing only the claim
    /// (held for `hold`), for a mutation that calls a provider before it
    /// records anything. Finish with [`Self::resume`] and `commit`, or
    /// [`Self::release`] when nothing was done.
    pub async fn hold(
        &self,
        request: AdminMutationRequest,
        hold: std::time::Duration,
    ) -> Result<HoldStart, AdminStoreError> {
        match self.begin_with(request, ClaimMode::Hold(hold)).await? {
            Started::Held(claim) => Ok(HoldStart::Held(claim)),
            Started::Mutation(MutationStart::Replay(record)) => {
                Ok(HoldStart::Replay(Box::new(record)))
            }
            Started::Mutation(MutationStart::New(_)) => unreachable!("a held claim commits"),
        }
    }

    /// Continues a held claim as a mutation. A claim that expired and was
    /// taken (or finished) by another request is `IdempotencyInProgress`.
    pub async fn resume(
        &self,
        request: AdminMutationRequest,
        claim: HeldClaim,
    ) -> Result<MutationStart<'_>, AdminStoreError> {
        match self.begin_with(request, ClaimMode::Resume(claim)).await? {
            Started::Mutation(start) => Ok(start),
            Started::Held(_) => unreachable!("a resumed claim is not held"),
        }
    }

    /// Lets a held claim go when the request did nothing (it was refused),
    /// so a retry with the key is decided afresh.
    pub async fn release(
        &self,
        request: &AdminMutationRequest,
        claim: HeldClaim,
    ) -> Result<(), AdminStoreError> {
        sqlx::query(
            "DELETE FROM gateway.admin_idempotency_record
             WHERE organization_id = $1 AND actor_user_id = $2 AND operation = $3
               AND idempotency_key = $4 AND claim_id = $5 AND status = 'in_progress'",
        )
        .bind(request.organization_id.0)
        .bind(request.actor_id.0)
        .bind(request.operation.as_str())
        .bind(request.idempotency_key.as_str())
        .bind(claim.0)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn begin_with(
        &self,
        mut request: AdminMutationRequest,
        mode: ClaimMode,
    ) -> Result<Started<'_>, AdminStoreError> {
        let mut transaction = self.pool.begin().await?;
        lock_scope(&mut transaction, request.scope).await?;
        let canonical_target_id = request.target_id;
        reuse_disabled_role_binding(&mut transaction, &mut request).await?;
        let target_presence = validate_target_scope(&mut transaction, &request).await?;
        let context =
            load_authorization_context(&mut transaction, request.actor_id, request.organization_id)
                .await?;
        if let Err(error) = authorize(&context, request.permission, request.scope) {
            transaction.rollback().await?;
            // Denial is audited on a fresh transaction (best effort) so a
            // failed authorization can never commit mutation state, and a
            // missing membership cannot turn a 403 into an audit error.
            let _ = insert_denied_audit(
                &self.pool,
                &AdminAuditEvent {
                    actor: AuditActor::User(request.actor_id),
                    organization_id: request.organization_id,
                    team_id: request.scope.team_id(),
                    target: request.target,
                    action: request.audit_action,
                    target_id: request.target_id,
                    outcome: AuditOutcome::Denied,
                    request_id: request.request_id,
                    before: None,
                    after: None,
                },
            )
            .await;
            return Err(error.into());
        }

        let if_match = request.if_match.to_string();
        let fingerprint = canonical_admin_fingerprint(AdminRequestFingerprintInput {
            method: &request.method,
            route: &request.route,
            operation: request.operation.as_str(),
            target_ids: std::slice::from_ref(&canonical_target_id),
            if_match: Some(&if_match),
            body: &request.body,
        });
        let claim = match mode {
            ClaimMode::Resume(held) => resume_claim(&mut transaction, &request, held).await?,
            _ => claim_idempotency(&mut transaction, &request, fingerprint).await?,
        };
        if let (ClaimMode::Hold(hold), Claim::New(claim_id)) = (&mode, &claim) {
            sqlx::query(
                "UPDATE gateway.admin_idempotency_record
                 SET claim_expires_at = CURRENT_TIMESTAMP + make_interval(secs => $2)
                 WHERE claim_id = $1",
            )
            .bind(claim_id)
            .bind(hold.as_secs_f64())
            .execute(&mut *transaction)
            .await?;
            transaction.commit().await?;
            return Ok(Started::Held(HeldClaim(*claim_id)));
        }
        let start = match claim {
            Claim::Replay(record) => {
                transaction.rollback().await?;
                Ok(MutationStart::Replay(*record))
            }
            Claim::Conflict => {
                transaction.rollback().await?;
                Err(AdminStoreError::IdempotencyConflict)
            }
            Claim::InProgress => {
                transaction.rollback().await?;
                Err(AdminStoreError::IdempotencyInProgress)
            }
            Claim::New(claim_id) => {
                let version_id = version_resource_id(&request);
                let mut current = sqlx::query_scalar::<_, i64>(
                    "SELECT version FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3
                     FOR UPDATE",
                )
                .bind(request.organization_id.0)
                .bind(target_name(request.target))
                .bind(version_id)
                .fetch_optional(&mut *transaction)
                .await?;
                let next = match (current.take(), target_presence) {
                    // Create mode: a missing version row is valid only after the
                    // scope/parent was revalidated above. The first created
                    // version/ETag must be 1, so the row is seeded and never
                    // bumped to 2.
                    (
                        None,
                        TargetPresence::Missing {
                            create_allowed: true,
                        },
                    ) => {
                        if let IfMatch::Exact(expected) = request.if_match {
                            if expected.get() != 1 {
                                transaction.rollback().await?;
                                return Err(AdminStoreError::VersionConflict(VersionConflict {
                                    expected: Some(expected),
                                    current: ResourceVersion::INITIAL,
                                }));
                            }
                        }
                        sqlx::query(
                            "INSERT INTO gateway.admin_resource_version
                                 (organization_id, resource_type, resource_id)
                             VALUES ($1,$2,$3)",
                        )
                        .bind(request.organization_id.0)
                        .bind(target_name(request.target))
                        .bind(version_id)
                        .execute(&mut *transaction)
                        .await?;
                        1_i64
                    }
                    (Some(current), _) => {
                        let current = ResourceVersion::new(current as u64)
                            .ok_or(AdminStoreError::ResourceNotFound)?;
                        if !request.if_match.matches(current) {
                            transaction.rollback().await?;
                            return Err(AdminStoreError::VersionConflict(VersionConflict {
                                expected: match request.if_match {
                                    IfMatch::Any | IfMatch::Absent => None,
                                    IfMatch::Exact(v) => Some(v),
                                },
                                current,
                            }));
                        }
                        sqlx::query_scalar::<_, i64>(
                            "UPDATE gateway.admin_resource_version
                             SET version = version + 1, updated_at = CURRENT_TIMESTAMP
                             WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3
                             RETURNING version",
                        )
                        .bind(request.organization_id.0)
                        .bind(target_name(request.target))
                        .bind(version_id)
                        .fetch_one(&mut *transaction)
                        .await?
                    }
                    (None, _) => return Err(AdminStoreError::ResourceNotFound),
                };
                Ok(MutationStart::New(AdminMutation {
                    transaction: Some(transaction),
                    request,
                    claim_id,
                    next_version: ResourceVersion::new(next as u64)
                        .ok_or(AdminStoreError::ResourceNotFound)?,
                }))
            }
        };
        start.map(Started::Mutation)
    }
}

/// The claim a held request continues: still its own and in progress, else
/// what became of the key.
async fn resume_claim(
    transaction: &mut Transaction<'_, Postgres>,
    request: &AdminMutationRequest,
    held: HeldClaim,
) -> Result<Claim, AdminStoreError> {
    let row = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT claim_id, status FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND actor_user_id = $2
           AND operation = $3 AND idempotency_key = $4
         FOR UPDATE",
    )
    .bind(request.organization_id.0)
    .bind(request.actor_id.0)
    .bind(request.operation.as_str())
    .bind(request.idempotency_key.as_str())
    .fetch_optional(&mut **transaction)
    .await?;
    match row {
        Some((claim_id, status)) if claim_id == held.0 && status == "in_progress" => {
            Ok(Claim::New(claim_id))
        }
        _ => Ok(Claim::InProgress),
    }
}

/// A revoked grant remains the durable binding for its natural key. Reusing it
/// keeps its resource version and historical identity intact instead of trying
/// to insert a second row that the natural uniqueness constraint must reject.
async fn reuse_disabled_role_binding(
    transaction: &mut Transaction<'_, Postgres>,
    request: &mut AdminMutationRequest,
) -> Result<(), AdminStoreError> {
    if request.operation.as_str() != "role_binding.create"
        || request.target != AuditTargetResource::RoleBinding
        || request.scope.team_id().is_some()
    {
        return Ok(());
    }
    let Some(user_id) = request.parent_id else {
        return Ok(());
    };
    let Some(role) = request.body.get("role").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL
           AND role = $3 AND NOT enabled
         FOR UPDATE",
    )
    .bind(request.organization_id.0)
    .bind(user_id)
    .bind(role)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(existing) = existing {
        request.target_id = existing;
    }
    Ok(())
}

enum Claim {
    New(Uuid),
    Replay(Box<AdminIdempotencyRecord>),
    Conflict,
    InProgress,
}

/// Result of create-aware scope validation. `Exists` keeps the update/delete
/// path unchanged; `Missing` is produced only after the authorized parent/org/
/// team scope has been revalidated for a create-capable permission.
enum TargetPresence {
    Exists,
    Missing { create_allowed: bool },
}

/// Record an audit outcome (typically a denial) on its own transaction so a
/// failed authorization is never coupled to a mutation transaction.
async fn insert_denied_audit(
    pool: &PgPool,
    event: &AdminAuditEvent,
) -> Result<(), AdminStoreError> {
    let mut transaction = pool.begin().await?;
    insert_audit(&mut transaction, event).await?;
    transaction.commit().await?;
    Ok(())
}

async fn claim_idempotency(
    transaction: &mut Transaction<'_, Postgres>,
    request: &AdminMutationRequest,
    fingerprint: RequestFingerprint,
) -> Result<Claim, AdminStoreError> {
    let claim_id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO gateway.admin_idempotency_record
             (organization_id, actor_user_id, operation, idempotency_key,
              request_fingerprint, claim_id)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (organization_id, actor_user_id, operation, idempotency_key)
         DO NOTHING",
    )
    .bind(request.organization_id.0)
    .bind(request.actor_id.0)
    .bind(request.operation.as_str())
    .bind(request.idempotency_key.as_str())
    .bind(fingerprint.0.as_slice())
    .bind(claim_id)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Claim::New(claim_id));
    }

    let row = sqlx::query_as::<_, IdempotencyRow>(
        "SELECT request_fingerprint, status, resource_id,
                response_metadata, claim_expires_at
         FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND actor_user_id = $2
           AND operation = $3 AND idempotency_key = $4
         FOR UPDATE",
    )
    .bind(request.organization_id.0)
    .bind(request.actor_id.0)
    .bind(request.operation.as_str())
    .bind(request.idempotency_key.as_str())
    .fetch_one(&mut **transaction)
    .await?;
    if row.request_fingerprint.as_slice() != fingerprint.0.as_slice() {
        return Ok(Claim::Conflict);
    }
    if row.status == "succeeded" {
        return Ok(Claim::Replay(Box::new(to_record(
            request,
            row,
            fingerprint,
        )?)));
    }
    if row.claim_expires_at <= chrono::Utc::now() {
        sqlx::query(
            "UPDATE gateway.admin_idempotency_record
             SET claim_id = $5, claim_expires_at = CURRENT_TIMESTAMP + INTERVAL '60 seconds',
                 status = 'in_progress', completed_at = NULL
             WHERE organization_id = $1 AND actor_user_id = $2 AND operation = $3
               AND idempotency_key = $4",
        )
        .bind(request.organization_id.0)
        .bind(request.actor_id.0)
        .bind(request.operation.as_str())
        .bind(request.idempotency_key.as_str())
        .bind(claim_id)
        .execute(&mut **transaction)
        .await?;
        return Ok(Claim::New(claim_id));
    }
    Ok(Claim::InProgress)
}

fn to_record(
    request: &AdminMutationRequest,
    row: IdempotencyRow,
    fingerprint: RequestFingerprint,
) -> Result<AdminIdempotencyRecord, AdminStoreError> {
    let response_metadata = row
        .response_metadata
        .map(|value| serde_json::from_value(value).map_err(|_| AdminStoreError::SafeSerialization))
        .transpose()?;
    Ok(AdminIdempotencyRecord {
        organization_id: request.organization_id,
        actor_id: request.actor_id,
        operation: request.operation.as_str().to_owned(),
        key: request.idempotency_key.as_str().to_owned(),
        fingerprint,
        status: row.status,
        resource_id: row.resource_id,
        response_metadata,
    })
}

async fn load_authorization_context(
    transaction: &mut Transaction<'_, Postgres>,
    actor_id: UserAccountId,
    organization_id: OrganizationId,
) -> Result<AdminAuthorizationContext, AdminStoreError> {
    let rows = sqlx::query_as::<_, RoleBindingRow>(
        "SELECT b.organization_id, b.user_id, b.team_id, b.role, b.enabled
         FROM gateway.admin_role_binding b
         JOIN gateway.user_account u ON u.id = b.user_id AND u.enabled
         JOIN gateway.organization_membership om
           ON om.organization_id = b.organization_id AND om.user_id = b.user_id AND om.enabled
         WHERE b.organization_id = $1 AND b.user_id = $2 AND b.enabled
            AND (b.team_id IS NULL OR EXISTS (
              SELECT 1 FROM gateway.team_membership tm
              WHERE tm.organization_id = b.organization_id AND tm.team_id = b.team_id
                AND tm.user_id = b.user_id AND tm.enabled
            ))
         FOR UPDATE OF b, u, om",
    )
    .bind(organization_id.0)
    .bind(actor_id.0)
    .fetch_all(&mut **transaction)
    .await?;
    let bindings = rows
        .into_iter()
        .map(AdminRoleBinding::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    for binding in &bindings {
        if let Some(team_id) = binding.team_id {
            let precise_team_membership = sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.team_membership
                 WHERE organization_id = $1 AND team_id = $2 AND user_id = $3 AND enabled
                 FOR UPDATE",
            )
            .bind(organization_id.0)
            .bind(team_id.0)
            .bind(actor_id.0)
            .fetch_optional(&mut **transaction)
            .await?;
            if precise_team_membership.is_none() {
                return Ok(AdminAuthorizationContext {
                    actor_id,
                    organization_id,
                    bindings: Vec::new(),
                });
            }
        }
    }
    Ok(AdminAuthorizationContext {
        actor_id,
        organization_id,
        bindings,
    })
}

async fn lock_scope(
    transaction: &mut Transaction<'_, Postgres>,
    scope: AdminScope,
) -> Result<(), AdminStoreError> {
    // A suspended (or deleted) organization takes no writes. Suspension and
    // deletion lock the same row, so a write waiting here sees their result.
    let exists = match scope {
        AdminScope::Organization(organization_id) => {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization
                 WHERE id = $1 AND suspended_at IS NULL FOR UPDATE",
            )
            .bind(organization_id.0)
            .fetch_optional(&mut **transaction)
            .await?
        }
        AdminScope::Team {
            organization_id,
            team_id,
        } => {
            let live = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization
                 WHERE id = $1 AND suspended_at IS NULL FOR SHARE",
            )
            .bind(organization_id.0)
            .fetch_optional(&mut **transaction)
            .await?;
            if live.is_none() {
                return Err(AdminStoreError::ResourceNotFound);
            }
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.team WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(organization_id.0)
            .bind(team_id.0)
            .fetch_optional(&mut **transaction)
            .await?
        }
    };
    if exists.is_some() {
        Ok(())
    } else {
        Err(AdminStoreError::ResourceNotFound)
    }
}

async fn validate_target_scope(
    transaction: &mut Transaction<'_, Postgres>,
    request: &AdminMutationRequest,
) -> Result<TargetPresence, AdminStoreError> {
    let team_id = request.scope.team_id();
    // `found` is Some only when the target resource itself exists; `parent_ok`
    // is set only by the API-key create arms, whose parent service account was
    // revalidated. A validated parent does not make the missing key "exist".
    let (found, parent_ok) = match (request.target, team_id) {
        (AuditTargetResource::Organization, Some(_)) => {
            return Err(crate::domain::admin::AuthorizationError::TeamScopeRequired.into());
        }
        (AuditTargetResource::Organization, None) => {
            // The organization scope row is already locked by lock_scope.
            (Some(request.target_id), false)
        }
        (AuditTargetResource::Team, Some(team_id)) => {
            // The only valid team-scoped team operation targets the scope team
            // itself (already locked); a team is never created at team scope.
            if team_id.0 == request.target_id {
                return Ok(TargetPresence::Exists);
            }
            return Err(AdminStoreError::ResourceNotFound);
        }
        (AuditTargetResource::Team, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.team WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ServiceAccount, Some(team_id)) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.service_account
                 WHERE organization_id = $1 AND id = $2 AND team_id = $3 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .bind(team_id.0)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ServiceAccount, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.service_account
                 WHERE organization_id = $1 AND id = $2 AND team_id IS NULL FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ApiKey, Some(team_id)) => {
            let existing = sqlx::query_scalar::<_, Uuid>(
                "SELECT k.id FROM gateway.api_key k
                 JOIN gateway.service_account sa ON sa.organization_id = k.organization_id
                  AND sa.id = k.service_account_id AND sa.team_id = $3
                 WHERE k.organization_id = $1 AND k.id = $2 FOR UPDATE OF k, sa",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .bind(team_id.0)
            .fetch_optional(&mut **transaction)
            .await?;
            if existing.is_some() {
                (Some(request.target_id), false)
            } else if matches!(
                request.permission,
                AdminPermission::ApiKeyCreate | AdminPermission::ApiKeyRotate
            ) {
                let parent = request.parent_id.ok_or(AdminStoreError::ResourceNotFound)?;
                let service_account = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM gateway.service_account
                     WHERE organization_id = $1 AND id = $2 AND team_id = $3 FOR UPDATE",
                )
                .bind(request.organization_id.0)
                .bind(parent)
                .bind(team_id.0)
                .fetch_optional(&mut **transaction)
                .await?;
                (None, service_account.is_some())
            } else {
                (None, false)
            }
        }
        (AuditTargetResource::ApiKey, None) => {
            let existing = sqlx::query_scalar::<_, Uuid>(
                "SELECT k.id FROM gateway.api_key k
                 JOIN gateway.service_account sa ON sa.organization_id = k.organization_id
                  AND sa.id = k.service_account_id AND sa.team_id IS NULL
                 WHERE k.organization_id = $1 AND k.id = $2 FOR UPDATE OF k, sa",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if existing.is_some() {
                (Some(request.target_id), false)
            } else if matches!(
                request.permission,
                AdminPermission::ApiKeyCreate | AdminPermission::ApiKeyRotate
            ) {
                let parent = request.parent_id.ok_or(AdminStoreError::ResourceNotFound)?;
                let service_account = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM gateway.service_account
                     WHERE organization_id = $1 AND id = $2 AND team_id IS NULL FOR UPDATE",
                )
                .bind(request.organization_id.0)
                .bind(parent)
                .fetch_optional(&mut **transaction)
                .await?;
                (None, service_account.is_some())
            } else {
                (None, false)
            }
        }
        // Usage reports and traces are read-only, and operators bind cloud
        // principals outside the admin API: neither enters the mutation path.
        (
            AuditTargetResource::UsageReport
            | AuditTargetResource::InferenceTrace
            | AuditTargetResource::DataExport
            | AuditTargetResource::CloudPrincipalBinding
            | AuditTargetResource::OrganizationDomain,
            _,
        ) => {
            return Err(AdminStoreError::ResourceNotFound);
        }
        (AuditTargetResource::Invitation, None) => {
            let invitation = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization_invitation
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if invitation.is_some() {
                if request.permission != AdminPermission::InvitationWrite {
                    return Err(AdminStoreError::ResourceNotFound);
                }
                // An invitation the operator made with the CLI has no version
                // row; its first revoke through the API seeds one.
                let versioned = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (
                         SELECT 1 FROM gateway.admin_resource_version
                         WHERE organization_id = $1 AND resource_type = 'invitation'
                           AND resource_id = $2
                     )",
                )
                .bind(request.organization_id.0)
                .bind(request.target_id)
                .fetch_one(&mut **transaction)
                .await?;
                return Ok(if versioned {
                    TargetPresence::Exists
                } else {
                    TargetPresence::Missing {
                        create_allowed: true,
                    }
                });
            }
            (None, false)
        }
        (AuditTargetResource::ReconciliationItem, Some(_)) => {
            return Err(AdminStoreError::ResourceNotFound);
        }
        (AuditTargetResource::ReconciliationItem, None) => {
            // Items are created by the accounting path, never by the API. The
            // first resolution seeds the item's version; the store then
            // rejects any later one because the item is no longer open. The
            // item is not locked here: the store locks it after its
            // reservation, in the order settlement uses.
            let item = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.accounting_reconciliation
                 WHERE organization_id = $1 AND id = $2",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if item.is_none() || request.permission != AdminPermission::ReconciliationResolve {
                return Err(AdminStoreError::ResourceNotFound);
            }
            let versioned = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = 'reconciliation_item'
                       AND resource_id = $2
                 )",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_one(&mut **transaction)
            .await?;
            return Ok(if versioned {
                TargetPresence::Exists
            } else {
                TargetPresence::Missing {
                    create_allowed: true,
                }
            });
        }
        // Catalog, credential and rate-limit resources are organization-wide
        // only; a team's policy is still managed at organization scope.
        (
            AuditTargetResource::CatalogRelease
            | AuditTargetResource::CatalogActivation
            | AuditTargetResource::ProviderCredential
            | AuditTargetResource::PriceOverride
            | AuditTargetResource::RateLimitPolicy
            | AuditTargetResource::BudgetPolicy
            | AuditTargetResource::WebhookEndpoint
            | AuditTargetResource::WebhookDelivery
            | AuditTargetResource::CatalogSyncJob
            | AuditTargetResource::PriceSourceFetch
            | AuditTargetResource::PriceMapping
            | AuditTargetResource::RetentionPolicy
            | AuditTargetResource::TracePolicy
            | AuditTargetResource::Invitation
            | AuditTargetResource::IdentityProvider
            | AuditTargetResource::ConfigPin
            | AuditTargetResource::UsageExport
            | AuditTargetResource::CatalogAutoOffer
            | AuditTargetResource::CatalogPricePolicy
            | AuditTargetResource::RoutingPolicy
            | AuditTargetResource::ModelCardNote
            | AuditTargetResource::ModelCardImport
            | AuditTargetResource::ModelEvaluationSet
            | AuditTargetResource::ModelEvaluationRun
            | AuditTargetResource::ModelMaster
            | AuditTargetResource::ModelMasterLink
            | AuditTargetResource::ModelAgreement
            | AuditTargetResource::BatchSettings
            | AuditTargetResource::BatchStaging
            | AuditTargetResource::Batch
            | AuditTargetResource::CatalogPriceHold
            | AuditTargetResource::Agent
            | AuditTargetResource::EndUserSetting
            | AuditTargetResource::EndUserPlan
            | AuditTargetResource::EndUser
            | AuditTargetResource::EndUserCreditGrant
            | AuditTargetResource::AgentToken,
            Some(_),
        ) => return Err(AdminStoreError::ResourceNotFound),
        (AuditTargetResource::CatalogRelease, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_release WHERE organization_id = $1 AND id = $2",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::CatalogActivation, None) => {
            // The target is the release being activated; it must exist. The
            // versioned resource is the organization's active pointer, which
            // is seeded on the first API activation.
            let release = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_release WHERE organization_id = $1 AND id = $2",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if release.is_none() || request.permission != AdminPermission::CatalogWrite {
                return Err(AdminStoreError::ResourceNotFound);
            }
            let versioned = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = 'catalog_activation'
                       AND resource_id = $1
                 )",
            )
            .bind(request.organization_id.0)
            .fetch_one(&mut **transaction)
            .await?;
            return Ok(if versioned {
                TargetPresence::Exists
            } else {
                TargetPresence::Missing {
                    create_allowed: true,
                }
            });
        }
        (AuditTargetResource::ConfigPin, None) => {
            // A pin targets the version being pinned, which must be one of
            // the organization's; an unpin targets the organization. The
            // versioned resource is the organization's pin either way.
            if request.permission != AdminPermission::CatalogWrite {
                return Err(AdminStoreError::ResourceNotFound);
            }
            if request.target_id != request.organization_id.0 {
                let version = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM gateway.config_version WHERE organization_id = $1 AND id = $2",
                )
                .bind(request.organization_id.0)
                .bind(request.target_id)
                .fetch_optional(&mut **transaction)
                .await?;
                if version.is_none() {
                    return Err(AdminStoreError::ResourceNotFound);
                }
            }
            let versioned = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = 'config_pin'
                       AND resource_id = $1
                 )",
            )
            .bind(request.organization_id.0)
            .fetch_one(&mut **transaction)
            .await?;
            return Ok(if versioned {
                TargetPresence::Exists
            } else {
                TargetPresence::Missing {
                    create_allowed: true,
                }
            });
        }
        (AuditTargetResource::ProviderCredential, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.provider_credential
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::PriceOverride, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_price_override
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::RateLimitPolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.rate_limit_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::BudgetPolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.budget_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::WebhookEndpoint, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.webhook_endpoint
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        // A fetch request stores nothing, so it is always new.
        (AuditTargetResource::PriceSourceFetch, None) => (None, false),
        (AuditTargetResource::CatalogSyncJob, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_sync_job
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::PriceMapping, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_price_mapping
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::UsageExport, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.usage_export
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::Agent, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.agent
                 WHERE organization_id = $1 AND user_id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::AgentToken, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.agent_token
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::CatalogAutoOffer, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_auto_offer
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::CatalogPricePolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_price_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelCardNote, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_card_note
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelCardImport, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_card_import
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelEvaluationSet, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_evaluation_set
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelMaster, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_master
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        // A provider account's agreements have no row of their own: the
        // first acceptance seeds their version.
        (AuditTargetResource::ModelAgreement, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT resource_id FROM gateway.admin_resource_version
                 WHERE organization_id = $1 AND resource_type = 'model_agreement'
                   AND resource_id = $2",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelMasterLink, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_master_link
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::ModelEvaluationRun, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.model_evaluation_run
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::RoutingPolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.routing_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::BatchSettings, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization_batch_settings
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        // A deleted staging is still found: setting it again revives it,
        // and its version goes on.
        (AuditTargetResource::BatchStaging, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.batch_staging
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::Batch, None) => {
            // Batches are made through the inference API, never the admin
            // API; an administrator's first action on one seeds its version.
            let batch = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.batch WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if batch.is_none() || request.permission != AdminPermission::ServiceAccountMutate {
                return Err(AdminStoreError::ResourceNotFound);
            }
            let versioned = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = 'batch'
                       AND resource_id = $2
                 )",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_one(&mut **transaction)
            .await?;
            return Ok(if versioned {
                TargetPresence::Exists
            } else {
                TargetPresence::Missing {
                    create_allowed: true,
                }
            });
        }
        (AuditTargetResource::CatalogPriceHold, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.catalog_price_hold
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::EndUserSetting, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.end_user_setting
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::EndUserPlan, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.end_user_plan
                 WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::EndUser, None) => (
            sqlx::query_scalar::<_, Uuid>(
                // A user only requests have seen has no version yet: its
                // first admin write creates the version, as for a new user.
                "SELECT u.id FROM gateway.end_user u
                 WHERE u.organization_id = $1 AND u.id = $2
                   AND EXISTS (SELECT 1 FROM gateway.admin_resource_version v
                               WHERE v.organization_id = u.organization_id
                                 AND v.resource_type = 'end_user' AND v.resource_id = u.id)
                 FOR UPDATE OF u",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::EndUserCreditGrant, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.end_user_credit_grant
                 WHERE organization_id = $1 AND id = $2 FOR KEY SHARE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::RetentionPolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization_retention_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::TracePolicy, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization_trace_policy
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::IdentityProvider, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.organization_identity_provider
                 WHERE organization_id = $1 AND id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::WebhookDelivery, None) => {
            // Deliveries are created by outbox fan-out, never by the API; the
            // parent is the endpoint the delivery belongs to. The first
            // redelivery seeds the delivery's version.
            let endpoint = request.parent_id.ok_or(AdminStoreError::ResourceNotFound)?;
            let delivery = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.webhook_delivery
                 WHERE organization_id = $1 AND endpoint_id = $2 AND id = $3 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(endpoint)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if delivery.is_none() || request.permission != AdminPermission::WebhookWrite {
                return Err(AdminStoreError::ResourceNotFound);
            }
            let versioned = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM gateway.admin_resource_version
                     WHERE organization_id = $1 AND resource_type = 'webhook_delivery'
                       AND resource_id = $2
                 )",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_one(&mut **transaction)
            .await?;
            return Ok(if versioned {
                TargetPresence::Exists
            } else {
                TargetPresence::Missing {
                    create_allowed: true,
                }
            });
        }
        (AuditTargetResource::Membership, Some(team_id)) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.team_membership
                 WHERE organization_id = $1 AND team_id = $2 AND user_id = $3 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(team_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::Membership, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.organization_membership
                 WHERE organization_id = $1 AND user_id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::RoleBinding, Some(team_id)) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.admin_role_binding
                 WHERE organization_id = $1 AND id = $2 AND team_id = $3 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .bind(team_id.0)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
        (AuditTargetResource::RoleBinding, None) => (
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.admin_role_binding
                 WHERE organization_id = $1 AND id = $2 AND team_id IS NULL FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?,
            false,
        ),
    };
    if found.is_some() {
        return Ok(TargetPresence::Exists);
    }
    if parent_ok {
        return Ok(TargetPresence::Missing {
            create_allowed: true,
        });
    }
    if permission_can_create(request.permission, request.target, &request.operation) {
        validate_create_parent(transaction, request).await?;
        return Ok(TargetPresence::Missing {
            create_allowed: true,
        });
    }
    // A replayed delete arrives after its target is gone. It may reach the
    // idempotency claim only when this key already recorded a completed
    // result; any other delete of a missing target is not found, even if a
    // stale version row would let the coordinator advance it.
    if request.operation.as_str().ends_with(".delete")
        && has_completed_idempotency_record(transaction, request).await?
    {
        return Ok(TargetPresence::Missing {
            create_allowed: false,
        });
    }
    Err(AdminStoreError::ResourceNotFound)
}

async fn has_completed_idempotency_record(
    transaction: &mut Transaction<'_, Postgres>,
    request: &AdminMutationRequest,
) -> Result<bool, AdminStoreError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM gateway.admin_idempotency_record
             WHERE organization_id = $1 AND actor_user_id = $2
               AND operation = $3 AND idempotency_key = $4
               AND status = 'succeeded'
         )",
    )
    .bind(request.organization_id.0)
    .bind(request.actor_id.0)
    .bind(request.operation.as_str())
    .bind(request.idempotency_key.as_str())
    .fetch_one(&mut **transaction)
    .await?)
}

/// Whether `permission` may create a resource of `target` type. Update and
/// delete requests never reach the create path: the missing target still
/// resolves to `ResourceNotFound` unless this returns true.
fn permission_can_create(
    permission: AdminPermission,
    target: AuditTargetResource,
    operation: &AdminOperation,
) -> bool {
    let operation = operation.as_str();
    match target {
        AuditTargetResource::Organization
        | AuditTargetResource::UsageReport
        | AuditTargetResource::InferenceTrace
        | AuditTargetResource::ReconciliationItem
        | AuditTargetResource::CatalogActivation
        | AuditTargetResource::ConfigPin
        | AuditTargetResource::WebhookDelivery
        | AuditTargetResource::DataExport
        | AuditTargetResource::CloudPrincipalBinding
        | AuditTargetResource::CatalogPriceHold
        | AuditTargetResource::OrganizationDomain => false,
        AuditTargetResource::Invitation => {
            matches!(permission, AdminPermission::InvitationWrite)
                && operation == "invitation.create"
        }
        AuditTargetResource::RetentionPolicy => {
            matches!(permission, AdminPermission::RetentionWrite)
                && operation == "retention_policy.set"
        }
        AuditTargetResource::TracePolicy => {
            matches!(permission, AdminPermission::TracingWrite) && operation == "trace_policy.set"
        }
        AuditTargetResource::UsageExport => {
            matches!(permission, AdminPermission::UsageExportWrite)
                && operation == "usage_export.set"
        }
        AuditTargetResource::CatalogAutoOffer => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "catalog_auto_offer.set"
        }
        AuditTargetResource::CatalogPricePolicy => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "catalog_price_policy.set"
        }
        AuditTargetResource::RoutingPolicy => {
            matches!(permission, AdminPermission::CatalogWrite) && operation == "routing_policy.set"
        }
        AuditTargetResource::ModelCardNote => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_card_note.set"
        }
        AuditTargetResource::ModelCardImport => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_card_import.set"
        }
        AuditTargetResource::ModelEvaluationSet => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_evaluation_set.create"
        }
        AuditTargetResource::ModelEvaluationRun => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_evaluation_run.create"
        }
        AuditTargetResource::ModelMaster => {
            matches!(permission, AdminPermission::CatalogWrite) && operation == "model_master.set"
        }
        AuditTargetResource::ModelAgreement => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_agreement.accept"
        }
        AuditTargetResource::ModelMasterLink => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "model_master_link.set"
        }
        AuditTargetResource::BatchSettings => {
            matches!(permission, AdminPermission::CatalogWrite) && operation == "batch_settings.set"
        }
        AuditTargetResource::BatchStaging => {
            matches!(permission, AdminPermission::CredentialWrite)
                && operation == "batch_staging.set"
        }
        AuditTargetResource::Batch => false,
        AuditTargetResource::EndUserSetting => {
            matches!(permission, AdminPermission::BudgetWrite)
                && operation == "end_user_setting.set"
        }
        AuditTargetResource::EndUserPlan => {
            matches!(permission, AdminPermission::BudgetWrite) && operation == "end_user_plan.set"
        }
        AuditTargetResource::EndUser => {
            matches!(permission, AdminPermission::EndUserWrite) && operation == "end_user.set"
        }
        AuditTargetResource::EndUserCreditGrant => {
            matches!(permission, AdminPermission::EndUserWrite)
                && operation == "end_user_credit_grant.create"
        }
        AuditTargetResource::Agent => {
            matches!(permission, AdminPermission::MemberMutate) && operation == "agent.create"
        }
        AuditTargetResource::AgentToken => {
            matches!(permission, AdminPermission::MemberMutate) && operation == "agent_token.create"
        }
        AuditTargetResource::IdentityProvider => {
            matches!(permission, AdminPermission::IdentityProviderWrite)
                && operation == "identity_provider.set"
        }
        AuditTargetResource::CatalogRelease => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "catalog_release.create"
        }
        AuditTargetResource::ProviderCredential => {
            matches!(permission, AdminPermission::CredentialWrite)
                && operation == "provider_credential.set"
        }
        AuditTargetResource::PriceOverride => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "price_override.create"
        }
        AuditTargetResource::RateLimitPolicy => {
            matches!(permission, AdminPermission::RateLimitWrite)
                && operation == "rate_limit_policy.set"
        }
        AuditTargetResource::BudgetPolicy => {
            matches!(permission, AdminPermission::BudgetWrite) && operation == "budget_policy.set"
        }
        AuditTargetResource::WebhookEndpoint => {
            matches!(permission, AdminPermission::WebhookWrite)
                && operation == "webhook_endpoint.create"
        }
        AuditTargetResource::CatalogSyncJob => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "catalog_sync_job.create"
        }
        AuditTargetResource::PriceMapping => {
            matches!(permission, AdminPermission::CatalogWrite) && operation == "price_mapping.set"
        }
        AuditTargetResource::PriceSourceFetch => {
            matches!(permission, AdminPermission::CatalogWrite)
                && operation == "price_source_fetch.create"
        }
        AuditTargetResource::Team => {
            matches!(permission, AdminPermission::TeamMutate) && operation == "team.create"
        }
        AuditTargetResource::Membership => {
            matches!(permission, AdminPermission::MemberMutate)
                && matches!(operation, "membership.create" | "team_membership.create")
        }
        AuditTargetResource::RoleBinding => {
            matches!(permission, AdminPermission::RoleBindingMutate)
                && operation == "role_binding.create"
        }
        AuditTargetResource::ServiceAccount => {
            matches!(permission, AdminPermission::ServiceAccountMutate)
                && operation == "service_account.create"
        }
        AuditTargetResource::ApiKey => matches!(
            (permission, operation),
            (AdminPermission::ApiKeyCreate, "api_key.create")
                | (AdminPermission::ApiKeyRotate, "api_key.rotate")
        ),
    }
}

/// Create-mode parent revalidation: the missing target is accepted only when
/// the persisted parent/scope the new row will reference is valid.
async fn validate_create_parent(
    transaction: &mut Transaction<'_, Postgres>,
    request: &AdminMutationRequest,
) -> Result<(), AdminStoreError> {
    match (request.target, request.scope.team_id()) {
        // The scope row (org/team) is already locked, and API-key creates
        // validate their parent service account above. Releases,
        // credentials and price overrides hang directly off the locked
        // organization. Rate-limit policies check their team or service
        // account in the mutation itself.
        (AuditTargetResource::Team, _)
        | (AuditTargetResource::ApiKey, _)
        | (AuditTargetResource::CatalogRelease, None)
        | (AuditTargetResource::ProviderCredential, None)
        | (AuditTargetResource::PriceOverride, None)
        | (AuditTargetResource::RateLimitPolicy, None)
        | (AuditTargetResource::BudgetPolicy, None)
        | (AuditTargetResource::WebhookEndpoint, None)
        | (AuditTargetResource::CatalogSyncJob, None)
        | (AuditTargetResource::PriceSourceFetch, None)
        | (AuditTargetResource::PriceMapping, None)
        | (AuditTargetResource::RetentionPolicy, None)
        | (AuditTargetResource::TracePolicy, None)
        | (AuditTargetResource::UsageExport, None)
        | (AuditTargetResource::CatalogAutoOffer, None)
        | (AuditTargetResource::CatalogPricePolicy, None)
        | (AuditTargetResource::RoutingPolicy, None)
        | (AuditTargetResource::ModelCardNote, None)
        | (AuditTargetResource::ModelCardImport, None)
        | (AuditTargetResource::ModelEvaluationSet, None)
        | (AuditTargetResource::ModelEvaluationRun, None)
        | (AuditTargetResource::ModelMaster, None)
        | (AuditTargetResource::ModelMasterLink, None)
        | (AuditTargetResource::ModelAgreement, None)
        | (AuditTargetResource::BatchSettings, None)
        | (AuditTargetResource::BatchStaging, None)
        | (AuditTargetResource::Batch, None)
        | (AuditTargetResource::CatalogPriceHold, None)
        | (AuditTargetResource::EndUserSetting, None)
        | (AuditTargetResource::EndUserPlan, None)
        | (AuditTargetResource::EndUser, None)
        | (AuditTargetResource::EndUserCreditGrant, None)
        | (AuditTargetResource::Agent, None)
        | (AuditTargetResource::AgentToken, None)
        | (AuditTargetResource::Invitation, None)
        | (AuditTargetResource::IdentityProvider, None) => Ok(()),
        (
            AuditTargetResource::CatalogRelease
            | AuditTargetResource::ProviderCredential
            | AuditTargetResource::PriceOverride
            | AuditTargetResource::RateLimitPolicy
            | AuditTargetResource::BudgetPolicy
            | AuditTargetResource::WebhookEndpoint
            | AuditTargetResource::WebhookDelivery
            | AuditTargetResource::CatalogSyncJob
            | AuditTargetResource::PriceSourceFetch
            | AuditTargetResource::PriceMapping
            | AuditTargetResource::RetentionPolicy
            | AuditTargetResource::TracePolicy
            | AuditTargetResource::UsageExport
            | AuditTargetResource::CatalogAutoOffer
            | AuditTargetResource::CatalogPricePolicy
            | AuditTargetResource::RoutingPolicy
            | AuditTargetResource::ModelCardNote
            | AuditTargetResource::ModelCardImport
            | AuditTargetResource::ModelEvaluationSet
            | AuditTargetResource::ModelEvaluationRun
            | AuditTargetResource::ModelMaster
            | AuditTargetResource::ModelMasterLink
            | AuditTargetResource::ModelAgreement
            | AuditTargetResource::BatchSettings
            | AuditTargetResource::BatchStaging
            | AuditTargetResource::Batch
            | AuditTargetResource::CatalogPriceHold
            | AuditTargetResource::Agent
            | AuditTargetResource::EndUserSetting
            | AuditTargetResource::EndUserPlan
            | AuditTargetResource::EndUser
            | AuditTargetResource::EndUserCreditGrant
            | AuditTargetResource::AgentToken
            | AuditTargetResource::CatalogActivation
            | AuditTargetResource::ConfigPin
            | AuditTargetResource::Invitation
            | AuditTargetResource::IdentityProvider,
            _,
        ) => Err(AdminStoreError::ResourceNotFound),
        // Reporting targets are read-only, and operators bind cloud
        // principals outside the admin API: neither enters the mutation path.
        (
            AuditTargetResource::UsageReport
            | AuditTargetResource::InferenceTrace
            | AuditTargetResource::ReconciliationItem
            | AuditTargetResource::DataExport
            | AuditTargetResource::CloudPrincipalBinding
            | AuditTargetResource::OrganizationDomain,
            _,
        ) => Err(AdminStoreError::ResourceNotFound),
        // Membership creates reference the user identified by `target_id`;
        // a team membership only requires an existing org membership, and the
        // scope team itself is already locked.
        (AuditTargetResource::Membership, Some(_)) => {
            let member = sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.organization_membership
                 WHERE organization_id = $1 AND user_id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if member.is_some() {
                Ok(())
            } else {
                Err(AdminStoreError::ResourceNotFound)
            }
        }
        (AuditTargetResource::Membership, None) => {
            let user = sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM gateway.user_account WHERE id = $1 FOR UPDATE",
            )
            .bind(request.target_id)
            .fetch_optional(&mut **transaction)
            .await?;
            if user.is_some() {
                Ok(())
            } else {
                Err(AdminStoreError::ResourceNotFound)
            }
        }
        // Role-binding and service-account creates carry the subject/owner user
        // in `parent_id`; it must already hold the membership the new row
        // references (org membership, or the scope team's membership).
        (AuditTargetResource::RoleBinding | AuditTargetResource::ServiceAccount, Some(team_id)) => {
            let parent = request.parent_id.ok_or(AdminStoreError::ResourceNotFound)?;
            let membership = sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.team_membership
                 WHERE organization_id = $1 AND team_id = $2 AND user_id = $3 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(team_id.0)
            .bind(parent)
            .fetch_optional(&mut **transaction)
            .await?;
            if membership.is_some() {
                Ok(())
            } else {
                Err(AdminStoreError::ResourceNotFound)
            }
        }
        (AuditTargetResource::RoleBinding | AuditTargetResource::ServiceAccount, None) => {
            let parent = request.parent_id.ok_or(AdminStoreError::ResourceNotFound)?;
            let membership = sqlx::query_scalar::<_, Uuid>(
                "SELECT user_id FROM gateway.organization_membership
                 WHERE organization_id = $1 AND user_id = $2 FOR UPDATE",
            )
            .bind(request.organization_id.0)
            .bind(parent)
            .fetch_optional(&mut **transaction)
            .await?;
            if membership.is_some() {
                Ok(())
            } else {
                Err(AdminStoreError::ResourceNotFound)
            }
        }
        (AuditTargetResource::Organization, _) => Err(AdminStoreError::ResourceNotFound),
    }
}

/// The `admin_resource_version.resource_id` a mutation targets. Team memberships
/// use the same `md5(team_id:user_id)` identity as the migration backfill;
/// everything else is addressed by its own id.
fn version_resource_id(request: &AdminMutationRequest) -> Uuid {
    match (request.target, request.scope.team_id()) {
        (AuditTargetResource::Membership, Some(team_id)) => {
            team_membership_version_id(team_id, UserAccountId(request.target_id))
        }
        (AuditTargetResource::CatalogActivation | AuditTargetResource::ConfigPin, _) => {
            request.organization_id.0
        }
        _ => request.target_id,
    }
}

/// Writes `event` on the caller's transaction, so it commits or rolls back
/// with the change it records.
pub(crate) async fn insert_audit(
    transaction: &mut Transaction<'_, Postgres>,
    event: &AdminAuditEvent,
) -> Result<(), AdminStoreError> {
    insert_audit_on(transaction, event).await
}

/// [`insert_audit`] on any connection; the caller owns the transaction.
pub(crate) async fn insert_audit_on(
    connection: &mut sqlx::PgConnection,
    event: &AdminAuditEvent,
) -> Result<(), AdminStoreError> {
    let (actor_type, actor_user_id, actor_service_account_id) = match event.actor {
        AuditActor::User(id) => ("user", Some(id.0), None),
        AuditActor::ServiceAccount(id) => ("service_account", None, Some(id.0)),
        AuditActor::System => ("system", None, None),
    };
    let before = event
        .before
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| AdminStoreError::SafeSerialization)?;
    let after = event
        .after
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| AdminStoreError::SafeSerialization)?;
    sqlx::query(
        "INSERT INTO gateway.audit_event
             (id, organization_id, actor_user_id, actor_type, actor_service_account_id,
              team_id, action, entity_type, entity_id, outcome, request_id,
              metadata, before_metadata, after_metadata)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
    )
    .bind(Uuid::new_v4())
    .bind(event.organization_id.0)
    .bind(actor_user_id)
    .bind(actor_type)
    .bind(actor_service_account_id)
    .bind(event.team_id.map(|id| id.0))
    .bind(event.action.as_str())
    .bind(target_name(event.target))
    .bind(event.target_id)
    .bind(outcome_str(event.outcome))
    .bind(event.request_id)
    .bind(serde_json::json!({ "target_id": event.target_id }))
    .bind(before)
    .bind(after)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

fn outcome_str(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Succeeded => "succeeded",
        AuditOutcome::Failed => "failed",
        AuditOutcome::Denied => "denied",
    }
}

fn target_name(target: AuditTargetResource) -> &'static str {
    match target {
        AuditTargetResource::Organization => "organization",
        AuditTargetResource::Team => "team",
        AuditTargetResource::Membership => "membership",
        AuditTargetResource::RoleBinding => "role_binding",
        AuditTargetResource::ServiceAccount => "service_account",
        AuditTargetResource::ApiKey => "api_key",
        AuditTargetResource::UsageReport => "usage_report",
        AuditTargetResource::InferenceTrace => "inference_trace",
        AuditTargetResource::ReconciliationItem => "reconciliation_item",
        AuditTargetResource::CatalogRelease => "catalog_release",
        AuditTargetResource::CatalogActivation => "catalog_activation",
        AuditTargetResource::ProviderCredential => "provider_credential",
        AuditTargetResource::PriceOverride => "price_override",
        AuditTargetResource::RateLimitPolicy => "rate_limit_policy",
        AuditTargetResource::BudgetPolicy => "budget_policy",
        AuditTargetResource::WebhookEndpoint => "webhook_endpoint",
        AuditTargetResource::WebhookDelivery => "webhook_delivery",
        AuditTargetResource::CatalogSyncJob => "catalog_sync_job",
        AuditTargetResource::PriceMapping => "price_mapping",
        AuditTargetResource::PriceSourceFetch => "price_source_fetch",
        AuditTargetResource::RetentionPolicy => "retention_policy",
        AuditTargetResource::TracePolicy => "trace_policy",
        AuditTargetResource::DataExport => "data_export",
        AuditTargetResource::CloudPrincipalBinding => "cloud_principal_binding",
        AuditTargetResource::OrganizationDomain => "organization_domain",
        AuditTargetResource::Invitation => "invitation",
        AuditTargetResource::IdentityProvider => "identity_provider",
        AuditTargetResource::ConfigPin => "config_pin",
        AuditTargetResource::UsageExport => "usage_export",
        AuditTargetResource::CatalogAutoOffer => "catalog_auto_offer",
        AuditTargetResource::CatalogPricePolicy => "catalog_price_policy",
        AuditTargetResource::RoutingPolicy => "routing_policy",
        AuditTargetResource::ModelCardNote => "model_card_note",
        AuditTargetResource::ModelCardImport => "model_card_import",
        AuditTargetResource::ModelEvaluationSet => "model_evaluation_set",
        AuditTargetResource::ModelEvaluationRun => "model_evaluation_run",
        AuditTargetResource::ModelMaster => "model_master",
        AuditTargetResource::ModelMasterLink => "model_master_link",
        AuditTargetResource::ModelAgreement => "model_agreement",
        AuditTargetResource::BatchSettings => "batch_settings",
        AuditTargetResource::BatchStaging => "batch_staging",
        AuditTargetResource::Batch => "batch",
        AuditTargetResource::CatalogPriceHold => "catalog_price_hold",
        AuditTargetResource::EndUserSetting => "end_user_setting",
        AuditTargetResource::EndUserPlan => "end_user_plan",
        AuditTargetResource::EndUser => "end_user",
        AuditTargetResource::EndUserCreditGrant => "end_user_credit_grant",
        AuditTargetResource::Agent => "agent",
        AuditTargetResource::AgentToken => "agent_token",
    }
}

#[derive(FromRow)]
struct RoleBindingRow {
    organization_id: Uuid,
    user_id: Uuid,
    team_id: Option<Uuid>,
    role: String,
    enabled: bool,
}

impl TryFrom<RoleBindingRow> for AdminRoleBinding {
    type Error = AdminStoreError;
    fn try_from(row: RoleBindingRow) -> Result<Self, Self::Error> {
        Ok(Self {
            organization_id: row.organization_id.into(),
            user_id: row.user_id.into(),
            team_id: row.team_id.map(TeamId),
            role: row.role.parse().map_err(|_| AdminStoreError::InvalidRole)?,
            enabled: row.enabled,
        })
    }
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_fingerprint: Vec<u8>,
    status: String,
    resource_id: Option<Uuid>,
    response_metadata: Option<serde_json::Value>,
    claim_expires_at: chrono::DateTime<chrono::Utc>,
}

// Keep the future import visible in the public contract documentation: lanes
// may wrap `AdminMutation::transaction` in boxed async SQL operations.
#[allow(dead_code)]
type MutationFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdminStoreError>> + Send + 'a>>;
