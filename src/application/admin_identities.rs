// Service-identity application service.
//
// Owns the orchestration shared by every handler: human-session identity is
// resolved by transport, then each operation loads the server-side
// authorization context, derives the effective scope *from the server*
// (bindings for creates; the persisted resource's team for existing rows),
// runs the mutation through the foundation coordinator (idempotency, typed
// audit, scoped RBAC, resource versions), and executes resource SQL on the
// coordinator transaction. Scope is never taken from the client body.
//
// Key material is never persisted or reconstructed: create/rotate return the
// plaintext exactly once through `ApiKeyOperationResponse::created`, and an
// idempotent replay returns metadata plus `secret_unavailable_on_replay`.

use uuid::Uuid;

use crate::domain::{
    admin::{
        AdminAuthorizationContext, AdminIdempotencyKey, AdminOperation, AdminPermission,
        AdminScope, IdempotencyResultMetadata, authorize,
    },
    audit::{AuditAction, AuditSnapshot, AuditStatus, AuditTargetResource, PublicKeyPrefix},
    identity::{ApiKeyId, ApiKeyMetadata, OrganizationId, ServiceAccountId, TeamId, UserAccountId},
    keys::{ApiKeyMutationRequest, ApiKeyOperationResponse, KeyError},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::{IfMatch, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    service_identities::{
        ApiKeyRecord, Keyset, ServiceAccountRecord, ServiceAccountRemoval, ServiceIdentityStore,
        ServiceIdentityStoreError,
    },
};

pub const ROUTE_SERVICE_ACCOUNTS: &str = "/api/admin/v1/organizations/{org}/service-accounts";
pub const ROUTE_SERVICE_ACCOUNT: &str = "/api/admin/v1/organizations/{org}/service-accounts/{id}";
pub const ROUTE_SERVICE_ACCOUNT_DEACTIVATE: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{id}/deactivate";
pub const ROUTE_API_KEYS: &str = "/api/admin/v1/organizations/{org}/service-accounts/{id}/keys";
pub const ROUTE_API_KEY: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{id}/keys/{key_id}";
pub const ROUTE_API_KEY_ROTATE: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{id}/keys/{key_id}/rotate";
pub const ROUTE_API_KEY_REVOKE: &str =
    "/api/admin/v1/organizations/{org}/service-accounts/{id}/keys/{key_id}/revoke";

/// Collection names and ordering used by the signed cursor bindings. These are
/// part of the cursor's authenticated claim, so a token can never be replayed
/// across collections, filters, or ordering.
const SA_COLLECTION: &str = "service_accounts";
const KEY_COLLECTION: &str = "api_keys";
const ORDER_CREATED_AT_ID: &str = "created_at,id";

#[derive(Debug)]
pub enum ServiceIdentityError {
    /// The admin session/auth crypto is unavailable; the request cannot be
    /// authenticated.
    AuthenticationUnavailable,
    /// Authorization was denied at the derived scope.
    Forbidden,
    /// The resource does not exist in the actor's authorized scopes.
    NotFound,
    /// A team-scoped create requires exactly one authorized team.
    AmbiguousTeamScope,
    /// Optimistic-concurrency precondition failed.
    VersionConflict(VersionConflict),
    /// Idempotency-Key reused with a different request fingerprint.
    IdempotencyConflict,
    /// Idempotent request is still in progress.
    IdempotencyInProgress,
    /// Request validation failed; the detail is a fixed, non-secret message.
    InvalidRequest(String),
    /// Resource state does not permit the operation (e.g. rotating a revoked
    /// key or revoking an already-revoked key).
    InvalidState,
    /// Cursor pagination failed.
    Pagination(PaginationError),
    /// Persistence failure.
    Store(ServiceIdentityStoreError),
    /// A coordinator failure not otherwise classified above.
    Admin(AdminStoreError),
}

/// Result of a successful mutation, including whether the Idempotency-Key
/// replayed a prior completion (in which case no secret and no new row were
/// produced).
pub struct MutationOutcome<T> {
    pub item: T,
    pub replayed: bool,
}

#[derive(Clone)]
pub struct ServiceIdentityService {
    admin: AdminStore,
    store: ServiceIdentityStore,
    codec: Option<SignedCursorCodec>,
}

impl ServiceIdentityService {
    pub fn new(
        admin: AdminStore,
        store: ServiceIdentityStore,
        codec: Option<SignedCursorCodec>,
    ) -> Self {
        Self {
            admin,
            store,
            codec,
        }
    }

    // ------------------------------------------------------------------
    // Service accounts
    // ------------------------------------------------------------------

    pub async fn list_service_accounts(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        enabled: Option<bool>,
        page: &PageRequest,
    ) -> Result<Page<ServiceAccountRecord>, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let (org_wide, teams) =
            partition_scopes(context.authorized_scopes(AdminPermission::ServiceAccountsList));
        if !org_wide && teams.is_empty() {
            return Err(ServiceIdentityError::Forbidden);
        }
        let mut filters = if org_wide {
            "scope=organization".to_owned()
        } else {
            format!("teams={}", join_team_ids(&teams))
        };
        // A page token continues only the list it was issued for.
        if let Some(enabled) = enabled {
            filters.push_str(&format!(";enabled={enabled}"));
        }
        let scope = AdminScope::Organization(organization_id);
        let codec = self
            .codec
            .as_ref()
            .ok_or(ServiceIdentityError::AuthenticationUnavailable)?;
        let keyset = self.decode_keyset(
            codec,
            page,
            actor,
            scope,
            SA_COLLECTION,
            &filters,
            ORDER_CREATED_AT_ID,
        )?;
        let clamp = if org_wide {
            None
        } else {
            Some(teams.as_slice())
        };
        let (items, has_more) = self
            .store
            .list_service_accounts(organization_id, clamp, enabled, page.page_size, keyset)
            .await
            .map_err(ServiceIdentityError::Store)?;
        let next_page_token = if has_more {
            let last = items.last().ok_or(ServiceIdentityError::NotFound)?;
            Some(self.encode_keyset(
                codec,
                actor,
                scope,
                SA_COLLECTION,
                &filters,
                ORDER_CREATED_AT_ID,
                last.keyset(),
            )?)
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn get_service_account(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
    ) -> Result<ServiceAccountRecord, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        self.lookup_service_account(
            &context,
            organization_id,
            service_account_id,
            AdminPermission::ServiceAccountRead,
        )
        .await
    }

    pub async fn create_service_account(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        name: &str,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<ServiceAccountRecord>, ServiceIdentityError> {
        let name = normalize_name(name)?;
        let context = self.context(actor, organization_id).await?;
        let scope = create_scope(&context, AdminPermission::ServiceAccountMutate)?;
        let target_id = deterministic_target_id(&[
            "service_account.create",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let service_account_id = ServiceAccountId(target_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ServiceAccountMutate,
            route: ROUTE_SERVICE_ACCOUNTS.into(),
            method: "POST".into(),
            operation: operation("service_account.create"),
            idempotency_key,
            target_id,
            parent_id: Some(actor.0),
            target: AuditTargetResource::ServiceAccount,
            audit_action: audit_action("service_account.created"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "name": name }),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(request).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let item = self
                    .store
                    .get_service_account(organization_id, ServiceAccountId(resource_id), None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let team_id = scope.team_id();
                self.store
                    .insert_service_account(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                        team_id,
                        actor,
                        &name,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            enabled: Some(true),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let item = self
                    .store
                    .get_service_account(organization_id, service_account_id, None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: false,
                })
            }
        }
    }

    pub async fn update_service_account(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        name: &str,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<ServiceAccountRecord>, ServiceIdentityError> {
        let name = normalize_name(name)?;
        let context = self.context(actor, organization_id).await?;
        let current = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ServiceAccountMutate,
            )
            .await?;
        let scope = resource_scope(&current);
        authorize(&context, AdminPermission::ServiceAccountMutate, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ServiceAccountMutate,
            route: ROUTE_SERVICE_ACCOUNT.into(),
            method: "PATCH".into(),
            operation: operation("service_account.update"),
            idempotency_key,
            target_id: service_account_id.0,
            parent_id: None,
            target: AuditTargetResource::ServiceAccount,
            audit_action: audit_action("service_account.updated"),
            if_match,
            request_id,
            body: serde_json::json!({ "name": name }),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(request).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let item = self
                    .store
                    .get_service_account(organization_id, ServiceAccountId(resource_id), None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let changed = self
                    .store
                    .update_service_account_name(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                        &name,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                if !changed {
                    return Err(ServiceIdentityError::NotFound);
                }
                let before = Some(AuditSnapshot {
                    enabled: Some(current.enabled),
                    version: Some(current.version),
                    ..Default::default()
                });
                let next = mutation.next_version();
                mutation
                    .commit(
                        before,
                        Some(AuditSnapshot {
                            enabled: Some(current.enabled),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: service_account_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let item = self
                    .store
                    .get_service_account(organization_id, service_account_id, None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: false,
                })
            }
        }
    }

    pub async fn deactivate_service_account(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<ServiceAccountRecord>, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let current = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ServiceAccountMutate,
            )
            .await?;
        let scope = resource_scope(&current);
        authorize(&context, AdminPermission::ServiceAccountMutate, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ServiceAccountMutate,
            route: ROUTE_SERVICE_ACCOUNT_DEACTIVATE.into(),
            method: "POST".into(),
            operation: operation("service_account.deactivate"),
            idempotency_key,
            target_id: service_account_id.0,
            parent_id: None,
            target: AuditTargetResource::ServiceAccount,
            audit_action: audit_action("service_account.deactivated"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(request).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let item = self
                    .store
                    .get_service_account(organization_id, ServiceAccountId(resource_id), None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let changed = self
                    .store
                    .set_service_account_enabled(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                        false,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                if !changed {
                    return Err(ServiceIdentityError::NotFound);
                }
                let before = Some(AuditSnapshot {
                    enabled: Some(current.enabled),
                    version: Some(current.version),
                    ..Default::default()
                });
                let next = mutation.next_version();
                mutation
                    .commit(
                        before,
                        Some(AuditSnapshot {
                            enabled: Some(false),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: service_account_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let item = self
                    .store
                    .get_service_account(organization_id, service_account_id, None)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: false,
                })
            }
        }
    }

    /// Deletes the account, or disables it when it has made requests (its
    /// usage is kept and keeps referring to it). The outcome carries the
    /// disabled account, or `None` once deleted.
    pub async fn delete_service_account(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<Option<ServiceAccountRecord>>, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        // The resource may already be gone on an idempotent replay, so the
        // scope is derived from the live row when present and otherwise falls
        // back to the actor's own binding scope. The coordinator reaches the
        // idempotency claim before any version work, so a replay of a delete is
        // still recognized even though the row is gone.
        let current = match self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ServiceAccountMutate,
            )
            .await
        {
            Ok(record) => Some(record),
            Err(ServiceIdentityError::NotFound) => None,
            Err(error) => return Err(error),
        };
        let scope = match current.as_ref() {
            Some(record) => resource_scope(record),
            None => fallback_scope(&context, AdminPermission::ServiceAccountMutate)
                .ok_or(ServiceIdentityError::NotFound)?,
        };
        if let Some(record) = current.as_ref() {
            authorize(
                &context,
                AdminPermission::ServiceAccountMutate,
                resource_scope(record),
            )
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        }
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ServiceAccountMutate,
            route: ROUTE_SERVICE_ACCOUNT.into(),
            method: "DELETE".into(),
            operation: operation("service_account.delete"),
            idempotency_key,
            target_id: service_account_id.0,
            // On an idempotent replay the row is already gone, so the
            // coordinator's create-mode parent check needs the owning actor to
            // revalidate before reaching the idempotency claim.
            parent_id: Some(actor.0),
            target: AuditTargetResource::ServiceAccount,
            audit_action: audit_action("service_account.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(request).await.map_err(map_admin_error)?;
        match start {
            // A replay answers what the first request did, from its saved
            // result: the account as it was disabled, or nothing once deleted.
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| ServiceIdentityError::InvalidState)?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let before = current.as_ref().map(|record| AuditSnapshot {
                    enabled: Some(record.enabled),
                    version: Some(record.version),
                    ..Default::default()
                });
                let removal = self
                    .store
                    .delete_service_account(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                let next = mutation.next_version();
                // The account as the disable leaves it, answered now and
                // saved for replays.
                let disabled = match removal {
                    ServiceAccountRemoval::Deleted => None,
                    ServiceAccountRemoval::Disabled(record) => {
                        mutation.set_audit_action(audit_action("service_account.deactivated"));
                        // Read under the mutation's lock; the version is the
                        // one this mutation commits.
                        Some(ServiceAccountRecord {
                            version: next.get(),
                            ..record
                        })
                    }
                };
                let after = disabled.as_ref().map(|record| AuditSnapshot {
                    enabled: Some(record.enabled),
                    version: Some(record.version),
                    ..Default::default()
                });
                let snapshot = disabled
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|_| ServiceIdentityError::InvalidState)?;
                mutation
                    .commit(
                        before,
                        after,
                        Some(IdempotencyResultMetadata {
                            resource_id: service_account_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: disabled,
                    replayed: false,
                })
            }
        }
    }

    // ------------------------------------------------------------------
    // API keys
    // ------------------------------------------------------------------

    pub async fn list_api_keys(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        usable_only: bool,
        page: &PageRequest,
    ) -> Result<Page<ApiKeyRecord>, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let parent = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ApiKeysList,
            )
            .await?;
        let scope = resource_scope(&parent);
        authorize(&context, AdminPermission::ApiKeysList, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let mut filters = format!("service_account={}", service_account_id.0);
        if usable_only {
            filters.push_str(";usable=true");
        }
        let codec = self
            .codec
            .as_ref()
            .ok_or(ServiceIdentityError::AuthenticationUnavailable)?;
        let keyset = self.decode_keyset(
            codec,
            page,
            actor,
            scope,
            KEY_COLLECTION,
            &filters,
            ORDER_CREATED_AT_ID,
        )?;
        let (items, has_more) = self
            .store
            .list_api_keys(
                organization_id,
                service_account_id,
                usable_only,
                page.page_size,
                keyset,
            )
            .await
            .map_err(ServiceIdentityError::Store)?;
        let next_page_token = if has_more {
            let last = items.last().ok_or(ServiceIdentityError::NotFound)?;
            Some(self.encode_keyset(
                codec,
                actor,
                scope,
                KEY_COLLECTION,
                &filters,
                ORDER_CREATED_AT_ID,
                last.keyset(),
            )?)
        } else {
            None
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn get_api_key(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        key_id: ApiKeyId,
    ) -> Result<ApiKeyRecord, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let parent = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ApiKeyRead,
            )
            .await?;
        let scope = resource_scope(&parent);
        authorize(&context, AdminPermission::ApiKeyRead, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let key = self
            .store
            .get_api_key(organization_id, service_account_id, key_id)
            .await
            .map_err(ServiceIdentityError::Store)?
            .ok_or(ServiceIdentityError::NotFound)?;
        Ok(key)
    }

    pub async fn create_api_key(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        request: &ApiKeyMutationRequest,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<ApiKeyOperationResponse, ServiceIdentityError> {
        request
            .validate()
            .map_err(|error| ServiceIdentityError::InvalidRequest(key_error_detail(error)))?;
        let context = self.context(actor, organization_id).await?;
        let parent = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ApiKeyCreate,
            )
            .await?;
        let scope = resource_scope(&parent);
        authorize(&context, AdminPermission::ApiKeyCreate, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        // The target id is derived from the idempotency key AND the parent
        // service account so a retry replays the same key, while reusing the
        // key for a different service account is a fingerprint conflict.
        let target_id = deterministic_target_id(&[
            "api_key.create",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
            &service_account_id.0.to_string(),
        ]);
        let key_id = ApiKeyId(target_id);
        let mutation = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ApiKeyCreate,
            route: ROUTE_API_KEYS.into(),
            method: "POST".into(),
            operation: operation("api_key.create"),
            idempotency_key,
            target_id,
            parent_id: Some(service_account_id.0),
            target: AuditTargetResource::ApiKey,
            audit_action: audit_action("api_key.created"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::to_value(request)
                .map_err(|_| ServiceIdentityError::InvalidRequest("request body invalid".into()))?,
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(mutation).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let metadata = self
                    .fetch_key_metadata(organization_id, service_account_id, resource_id)
                    .await?;
                Ok(ApiKeyOperationResponse::replayed(metadata))
            }
            MutationStart::New(mut mutation) => {
                let created = self
                    .store
                    .create_api_key_on_transaction(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                        &request.scopes,
                        request.expires_at,
                        None,
                        key_id,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                let next = mutation.next_version();
                let prefix = PublicKeyPrefix::new(&created.public_prefix).map_err(|_| {
                    ServiceIdentityError::InvalidRequest("generated prefix invalid".into())
                })?;
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Active),
                            scopes: Some(created.scopes.clone()),
                            expires_at: created.expires_at,
                            public_prefix: Some(prefix),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: key_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let metadata = ApiKeyMetadata {
                    id: key_id,
                    organization_id,
                    service_account_id,
                    successor_of: None,
                    public_prefix: created.public_prefix.clone(),
                    scopes: created.scopes.clone(),
                    expires_at: created.expires_at,
                    revoked_at: None,
                    disabled_at: None,
                    status: "active".into(),
                };
                Ok(ApiKeyOperationResponse::created(metadata, created.bearer))
            }
        }
    }

    /// Non-revoking rotation: a successor key is created for the same service
    /// account with the predecessor's scopes/expiry, and the predecessor is
    /// left active. Revocation remains an explicit, separate operation so an
    /// in-flight consumer is never interrupted.
    pub async fn rotate_api_key(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        key_id: ApiKeyId,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<ApiKeyOperationResponse, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let parent = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ApiKeyRotate,
            )
            .await?;
        let scope = resource_scope(&parent);
        authorize(&context, AdminPermission::ApiKeyRotate, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let target_id = deterministic_target_id(&[
            "api_key.rotate",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
            &service_account_id.0.to_string(),
            &key_id.0.to_string(),
        ]);
        let successor_id = ApiKeyId(target_id);
        let mutation = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ApiKeyRotate,
            route: ROUTE_API_KEY_ROTATE.into(),
            method: "POST".into(),
            operation: operation("api_key.rotate"),
            idempotency_key,
            target_id,
            parent_id: Some(service_account_id.0),
            target: AuditTargetResource::ApiKey,
            audit_action: audit_action("api_key.rotated"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "predecessor": key_id }),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(mutation).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let metadata = self
                    .fetch_key_metadata(organization_id, service_account_id, resource_id)
                    .await?;
                Ok(ApiKeyOperationResponse::replayed(metadata))
            }
            MutationStart::New(mut mutation) => {
                if !self
                    .store
                    .is_api_key_active(&mut mutation.transaction(), organization_id, key_id)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                {
                    return Err(ServiceIdentityError::InvalidState);
                }
                let predecessor = self
                    .store
                    .get_api_key(organization_id, service_account_id, key_id)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                // An expired key cannot be rotated: chaining a successor onto it
                // would inherit an already-past expiry. This is a client-state
                // conflict (409 invalid_state), not a server failure.
                if predecessor
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= chrono::Utc::now())
                {
                    return Err(ServiceIdentityError::InvalidState);
                }
                let created = self
                    .store
                    .create_api_key_on_transaction(
                        &mut mutation.transaction(),
                        organization_id,
                        service_account_id,
                        &predecessor.scopes,
                        predecessor.expires_at,
                        Some(key_id),
                        successor_id,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                let next = mutation.next_version();
                let prefix = PublicKeyPrefix::new(&created.public_prefix).map_err(|_| {
                    ServiceIdentityError::InvalidRequest("generated prefix invalid".into())
                })?;
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Active),
                            scopes: Some(created.scopes.clone()),
                            expires_at: created.expires_at,
                            public_prefix: Some(prefix),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: successor_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let metadata = ApiKeyMetadata {
                    id: successor_id,
                    organization_id,
                    service_account_id,
                    successor_of: Some(key_id),
                    public_prefix: created.public_prefix.clone(),
                    scopes: created.scopes.clone(),
                    expires_at: created.expires_at,
                    revoked_at: None,
                    disabled_at: None,
                    status: "active".into(),
                };
                Ok(ApiKeyOperationResponse::created(metadata, created.bearer))
            }
        }
    }

    pub async fn revoke_api_key(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        key_id: ApiKeyId,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<ApiKeyRecord>, ServiceIdentityError> {
        let context = self.context(actor, organization_id).await?;
        let parent = self
            .lookup_service_account(
                &context,
                organization_id,
                service_account_id,
                AdminPermission::ApiKeyRevoke,
            )
            .await?;
        let scope = resource_scope(&parent);
        authorize(&context, AdminPermission::ApiKeyRevoke, scope)
            .map_err(|_| ServiceIdentityError::Forbidden)?;
        let current = self
            .store
            .get_api_key(organization_id, service_account_id, key_id)
            .await
            .map_err(ServiceIdentityError::Store)?
            .ok_or(ServiceIdentityError::NotFound)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope,
            permission: AdminPermission::ApiKeyRevoke,
            route: ROUTE_API_KEY_REVOKE.into(),
            method: "POST".into(),
            operation: operation("api_key.revoke"),
            idempotency_key,
            target_id: key_id.0,
            parent_id: None,
            target: AuditTargetResource::ApiKey,
            audit_action: audit_action("api_key.revoked"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        let coordinator = self.admin.coordinator();
        let start = coordinator.begin(request).await.map_err(map_admin_error)?;
        match start {
            MutationStart::Replay(record) => {
                let resource_id = record.resource_id.ok_or(ServiceIdentityError::NotFound)?;
                let item = self
                    .store
                    .get_api_key(organization_id, service_account_id, ApiKeyId(resource_id))
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let changed = self
                    .store
                    .revoke_api_key_on_transaction(
                        &mut mutation.transaction(),
                        organization_id,
                        key_id,
                    )
                    .await
                    .map_err(ServiceIdentityError::Store)?;
                if !changed {
                    return Err(ServiceIdentityError::InvalidState);
                }
                let before = Some(AuditSnapshot {
                    status: Some(AuditStatus::Active),
                    version: Some(current.version),
                    ..Default::default()
                });
                let next = mutation.next_version();
                mutation
                    .commit(
                        before,
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Revoked),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: key_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                let item = self
                    .store
                    .get_api_key(organization_id, service_account_id, key_id)
                    .await
                    .map_err(ServiceIdentityError::Store)?
                    .ok_or(ServiceIdentityError::NotFound)?;
                Ok(MutationOutcome {
                    item,
                    replayed: false,
                })
            }
        }
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    async fn context(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminAuthorizationContext, ServiceIdentityError> {
        self.admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)
    }

    /// Locate a service account within the actor's authorized scopes for
    /// `permission`. An actor with no grant at all is rejected with
    /// `Forbidden`; a row outside the granted scope resolves to `NotFound` so
    /// existence is never leaked across tenant/team boundaries.
    async fn lookup_service_account(
        &self,
        context: &AdminAuthorizationContext,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        permission: AdminPermission,
    ) -> Result<ServiceAccountRecord, ServiceIdentityError> {
        let (org_wide, teams) = partition_scopes(context.authorized_scopes(permission));
        if !org_wide && teams.is_empty() {
            return Err(ServiceIdentityError::Forbidden);
        }
        let clamp = if org_wide {
            None
        } else {
            Some(teams.as_slice())
        };
        let record = self
            .store
            .get_service_account(organization_id, service_account_id, clamp)
            .await
            .map_err(ServiceIdentityError::Store)?;
        record.ok_or(ServiceIdentityError::NotFound)
    }

    async fn fetch_key_metadata(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        key_id: Uuid,
    ) -> Result<ApiKeyMetadata, ServiceIdentityError> {
        let record = self
            .store
            .get_api_key(organization_id, service_account_id, ApiKeyId(key_id))
            .await
            .map_err(ServiceIdentityError::Store)?
            .ok_or(ServiceIdentityError::NotFound)?;
        Ok(ApiKeyMetadata {
            id: record.id,
            organization_id: record.organization_id,
            service_account_id: record.service_account_id,
            successor_of: record.successor_of,
            public_prefix: record.public_prefix,
            scopes: record.scopes,
            expires_at: record.expires_at,
            revoked_at: record.revoked_at,
            disabled_at: record.disabled_at,
            status: record.status,
        })
    }

    fn decode_keyset(
        &self,
        codec: &SignedCursorCodec,
        page: &PageRequest,
        actor: UserAccountId,
        scope: AdminScope,
        collection: &str,
        filters: &str,
        order: &str,
    ) -> Result<Option<Keyset>, ServiceIdentityError> {
        let Some(token) = page.page_token.as_deref() else {
            return Ok(None);
        };
        let claims = codec
            .decode_and_validate(token, actor, scope, collection, filters, order)
            .map_err(ServiceIdentityError::Pagination)?;
        if claims.last_keyset.is_empty() {
            return Ok(None);
        }
        Keyset::parse(&claims.last_keyset)
            .map(Some)
            .map_err(ServiceIdentityError::Store)
    }

    fn encode_keyset(
        &self,
        codec: &SignedCursorCodec,
        actor: UserAccountId,
        scope: AdminScope,
        collection: &str,
        filters: &str,
        order: &str,
        keyset: Keyset,
    ) -> Result<String, ServiceIdentityError> {
        let claims = CursorClaims::new(
            actor,
            scope,
            collection,
            filters,
            order,
            keyset.to_claim(),
            None,
        )
        .map_err(ServiceIdentityError::Pagination)?;
        codec
            .encode(&claims)
            .map_err(ServiceIdentityError::Pagination)
    }
}

/// Partition authorized scopes into org-wide vs. exact team set. Org-wide wins:
/// a binding at the organization scope covers every team, so no team filter is
/// applied and the collection exposes org-level rows too.
pub(crate) fn partition_scopes(scopes: Vec<AdminScope>) -> (bool, Vec<TeamId>) {
    if scopes
        .iter()
        .any(|scope| matches!(scope, AdminScope::Organization(_)))
    {
        return (true, Vec::new());
    }
    let mut teams: Vec<TeamId> = scopes.iter().filter_map(|scope| scope.team_id()).collect();
    teams.sort_unstable_by_key(|team| team.0);
    teams.dedup();
    (false, teams)
}

pub(crate) fn join_team_ids(teams: &[TeamId]) -> String {
    teams
        .iter()
        .map(|team| team.0.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Scope for a resource-targeting operation: the persisted team decides the
/// scope (a team row is managed at its team scope, an org-level row at the
/// organization scope). An org-wide binding inherits into the derived scope.
fn resource_scope(record: &ServiceAccountRecord) -> AdminScope {
    match record.team_id {
        Some(team_id) => AdminScope::Team {
            organization_id: record.organization_id,
            team_id,
        },
        None => AdminScope::Organization(record.organization_id),
    }
}

/// Best-effort scope when the target row no longer exists (an idempotent
/// delete replay). Only the actor's binding scope is known at that point; the
/// coordinator recognizes the replay before any version work, so the scope only
/// needs to pass authorization and reach the idempotency claim.
fn fallback_scope(
    context: &AdminAuthorizationContext,
    permission: AdminPermission,
) -> Option<AdminScope> {
    match context.authorized_scopes(permission).as_slice() {
        [AdminScope::Organization(organization_id)] => {
            Some(AdminScope::Organization(*organization_id))
        }
        [
            AdminScope::Team {
                organization_id,
                team_id,
            },
        ] => Some(AdminScope::Team {
            organization_id: *organization_id,
            team_id: *team_id,
        }),
        _ => None,
    }
}

/// Scope for create-mode operations is server-derived from the actor's
/// bindings. An org-wide grant creates an org-level row; a team grant creates
/// inside that team. Multiple team grants are ambiguous and denied rather than
/// guessing a team from client input.
fn create_scope(
    context: &AdminAuthorizationContext,
    permission: AdminPermission,
) -> Result<AdminScope, ServiceIdentityError> {
    match context.authorized_scopes(permission).as_slice() {
        [AdminScope::Organization(organization_id)] => {
            Ok(AdminScope::Organization(*organization_id))
        }
        [
            AdminScope::Team {
                organization_id,
                team_id,
            },
        ] => Ok(AdminScope::Team {
            organization_id: *organization_id,
            team_id: *team_id,
        }),
        [] => Err(ServiceIdentityError::Forbidden),
        _ => Err(ServiceIdentityError::AmbiguousTeamScope),
    }
}

/// Stable, server-derived target id for create-mode resources. The id is a
/// function of the operation, tenant, idempotency key, and (for keys) the
/// parent service account / predecessor, so a retry with the same
/// Idempotency-Key deterministically targets the same row and therefore
/// produces an identical canonical fingerprint (replay) while reusing the key
/// for a different parent or body produces a fingerprint conflict. This mirrors
/// the foundation's existing `md5(...)::uuid` stable-id pattern.
pub(crate) fn deterministic_target_id(parts: &[&str]) -> Uuid {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b":");
    }
    let digest = hasher.finalize();
    Uuid::from_bytes(digest.into())
}

fn normalize_name(name: &str) -> Result<String, ServiceIdentityError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 128 || name.len() > 256 {
        return Err(ServiceIdentityError::InvalidRequest(
            "service account name must be between 1 and 128 characters".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn key_error_detail(error: KeyError) -> String {
    match error {
        KeyError::EmptyScopes => "at least one scope is required".into(),
        KeyError::InvalidExpiry => "expiry must be in the future".into(),
        _ => "invalid key request".into(),
    }
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> ServiceIdentityError {
    match error {
        AdminStoreError::Authorization(_) => ServiceIdentityError::Forbidden,
        AdminStoreError::ResourceNotFound => ServiceIdentityError::NotFound,
        AdminStoreError::VersionConflict(conflict) => {
            ServiceIdentityError::VersionConflict(conflict)
        }
        AdminStoreError::IdempotencyConflict => ServiceIdentityError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => ServiceIdentityError::IdempotencyInProgress,
        other => ServiceIdentityError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::{ApiKeyScope, ServiceAccountId};

    fn admin_context(org: OrganizationId, actor: UserAccountId) -> AdminAuthorizationContext {
        AdminAuthorizationContext {
            actor_id: actor,
            organization_id: org,
            bindings: Vec::new(),
        }
    }

    #[test]
    fn deterministic_target_id_is_stable_and_parent_scoped() {
        let org = OrganizationId::new();
        let key = AdminIdempotencyKey::new("rotate-1").unwrap();
        let sa_a = ServiceAccountId::new();
        let sa_b = ServiceAccountId::new();
        let first = deterministic_target_id(&[
            "api_key.create",
            &org.0.to_string(),
            key.as_str(),
            &sa_a.0.to_string(),
        ]);
        let second = deterministic_target_id(&[
            "api_key.create",
            &org.0.to_string(),
            key.as_str(),
            &sa_a.0.to_string(),
        ]);
        assert_eq!(first, second, "retry must produce the same target id");
        assert_ne!(
            first,
            deterministic_target_id(&[
                "api_key.create",
                &org.0.to_string(),
                key.as_str(),
                &sa_b.0.to_string(),
            ]),
            "reusing the key for another service account must be a conflict"
        );
        assert_ne!(
            first,
            deterministic_target_id(&[
                "api_key.rotate",
                &org.0.to_string(),
                key.as_str(),
                &sa_a.0.to_string(),
            ]),
            "different operations must never share a target id"
        );
    }

    #[test]
    fn create_scope_derives_from_bindings_and_never_guesses() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        let actor = UserAccountId::new();
        let org_admin = AdminAuthorizationContext {
            bindings: vec![crate::domain::admin::AdminRoleBinding::organization(
                org,
                actor,
                crate::domain::admin::AdminRole::OrganizationAdmin,
            )],
            ..admin_context(org, actor)
        };
        assert_eq!(
            create_scope(&org_admin, AdminPermission::ServiceAccountMutate).unwrap(),
            AdminScope::Organization(org)
        );
        let team_admin = AdminAuthorizationContext {
            bindings: vec![crate::domain::admin::AdminRoleBinding::team(
                org,
                team,
                actor,
                crate::domain::admin::AdminRole::TeamAdmin,
            )],
            ..admin_context(org, actor)
        };
        assert_eq!(
            create_scope(&team_admin, AdminPermission::ServiceAccountMutate).unwrap(),
            AdminScope::Team {
                organization_id: org,
                team_id: team
            }
        );
        assert!(matches!(
            create_scope(
                &admin_context(org, actor),
                AdminPermission::ServiceAccountMutate
            ),
            Err(ServiceIdentityError::Forbidden)
        ));
    }

    #[test]
    fn partition_scopes_clamps_teams_and_prefers_org_wide() {
        let org = OrganizationId::new();
        let a = TeamId::new();
        let b = TeamId::new();
        let (org_wide, teams) = partition_scopes(vec![
            AdminScope::Team {
                organization_id: org,
                team_id: a,
            },
            AdminScope::Team {
                organization_id: org,
                team_id: b,
            },
        ]);
        assert!(!org_wide);
        assert_eq!(teams.len(), 2);
        assert!(teams.contains(&a) && teams.contains(&b));
        let (org_wide, teams) = partition_scopes(vec![AdminScope::Organization(org)]);
        assert!(org_wide);
        assert!(teams.is_empty());
    }

    #[test]
    fn normalize_name_trims_and_bounds() {
        assert_eq!(normalize_name("  gateway bot  ").unwrap(), "gateway bot");
        assert!(matches!(
            normalize_name("   "),
            Err(ServiceIdentityError::InvalidRequest(_))
        ));
        assert!(matches!(
            normalize_name(&"x".repeat(129)),
            Err(ServiceIdentityError::InvalidRequest(_))
        ));
    }

    #[test]
    fn key_error_details_are_fixed_and_non_secret() {
        assert_eq!(
            key_error_detail(KeyError::EmptyScopes),
            "at least one scope is required"
        );
        assert!(key_error_detail(KeyError::InvalidBearer).contains("invalid"));
        assert!(!key_error_detail(KeyError::InvalidBearer).contains("bearer"));
    }

    #[test]
    fn key_mutation_request_rejects_unknown_fields() {
        let parsed = serde_json::from_str::<ApiKeyMutationRequest>(
            r#"{"scopes":["Inference"],"expires_at":null}"#,
        )
        .unwrap();
        assert_eq!(parsed.scopes, vec![ApiKeyScope::Inference]);
        assert!(
            serde_json::from_str::<ApiKeyMutationRequest>(
                r#"{"scopes":["Inference"],"secret":"leak"}"#
            )
            .is_err()
        );
    }
}
