// Rate-limit admin application service: list, set and delete the request-rate,
// token-rate and concurrency policies of an organization, its teams, its
// service accounts and their API keys (the levels budgets have).
//
// Reads need `rate_limit_read`; writes need `rate_limit_write` (owner and
// admin only) at organization scope, including for a team's, service
// account's or API key's policy. Writes run through the admin mutation coordinator, so
// each is idempotent, versioned, and audited in the same transaction as the
// change. Enforcement is in `persistence::rate_limits::check_admission`
// and, with `GATEWAY_RATE_LIMIT_LEASES` on, `persistence::rate_leases`.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    rate_limit::{RateLimitError, RateLimitInput, RateLimitPolicyView, RateLimitScope},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    rate_limits::RateLimitStore,
};

pub const ROUTE_RATE_LIMITS: &str = "/api/admin/v1/organizations/{org}/rate-limits";
pub const ROUTE_ORGANIZATION_RATE_LIMIT: &str =
    "/api/admin/v1/organizations/{org}/rate-limits/organization";
pub const ROUTE_TEAM_RATE_LIMIT: &str =
    "/api/admin/v1/organizations/{org}/rate-limits/teams/{team_id}";
pub const ROUTE_SERVICE_ACCOUNT_RATE_LIMIT: &str =
    "/api/admin/v1/organizations/{org}/rate-limits/service-accounts/{service_account_id}";
pub const ROUTE_API_KEY_RATE_LIMIT: &str =
    "/api/admin/v1/organizations/{org}/rate-limits/api-keys/{api_key_id}";

const COLLECTION: &str = "rate_limit_policies";
const ORDER_ID: &str = "id";

#[derive(Debug)]
pub enum RateLimitAdminError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Invalid(RateLimitError),
    Pagination(PaginationError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for RateLimitAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct RateLimitWrite {
    pub policy: RateLimitPolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct RateLimitAdminService {
    admin: AdminStore,
    store: RateLimitStore,
    codec: Option<SignedCursorCodec>,
}

impl RateLimitAdminService {
    pub fn new(admin: AdminStore, store: RateLimitStore, codec: Option<SignedCursorCodec>) -> Self {
        Self {
            admin,
            store,
            codec,
        }
    }

    /// Every policy of the organization, by id.
    pub async fn list(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<RateLimitPolicyView>, RateLimitAdminError> {
        let scope = AdminScope::Organization(organization_id);
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(AdminPermission::RateLimitRead, scope) {
            return Err(RateLimitAdminError::Forbidden);
        }
        let codec = self
            .codec
            .as_ref()
            .ok_or(RateLimitAdminError::AuthenticationUnavailable)?;
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(token, actor, scope, COLLECTION, "", ORDER_ID)
                    .map_err(RateLimitAdminError::Pagination)?;
                // Only a validly signed token gets here, so a bad keyset is
                // a server-side inconsistency.
                Some(Uuid::parse_str(&claims.last_keyset).map_err(|_| {
                    RateLimitAdminError::Store("rate limit page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list(organization_id, page.page_size, after)
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let claims = CursorClaims::new(
                    actor,
                    scope,
                    COLLECTION,
                    "",
                    ORDER_ID,
                    last.id.to_string(),
                    None,
                )
                .map_err(RateLimitAdminError::Pagination)?;
                Some(
                    codec
                        .encode(&claims)
                        .map_err(RateLimitAdminError::Pagination)?,
                )
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Create or replace the policy of one scope. Limits left out of the
    /// request are removed.
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        scope: RateLimitScope,
        input: RateLimitInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<RateLimitWrite>, RateLimitAdminError> {
        let input = input.validate().map_err(RateLimitAdminError::Invalid)?;
        let target_id = policy_id(organization_id, scope);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::RateLimitWrite,
            route: route(scope).into(),
            method: "PUT".into(),
            operation: operation("rate_limit_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::RateLimitPolicy,
            audit_action: audit_action("rate_limit_policy.set"),
            if_match,
            request_id,
            body: mutation_body(scope, &input),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the policy has since changed.
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: RateLimitWrite {
                    policy: replayed(
                        record
                            .response_metadata
                            .and_then(|metadata| metadata.snapshot),
                    )?,
                    created: false,
                },
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                if !RateLimitStore::scope_exists(tx, organization_id, scope).await? {
                    return Err(RateLimitAdminError::NotFound);
                }
                let before = RateLimitStore::view(tx, organization_id, target_id).await?;
                let created =
                    RateLimitStore::upsert(tx, organization_id, target_id, scope, input, actor)
                        .await?;
                let stored = RateLimitStore::view(tx, organization_id, target_id)
                    .await?
                    .ok_or(RateLimitAdminError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = RateLimitPolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    RateLimitAdminError::Store("response snapshot is not serializable".into())
                })?;
                let before = before.map(|_| AuditSnapshot {
                    enabled: Some(true),
                    version: previous(next),
                    ..Default::default()
                });
                mutation
                    .commit(
                        before,
                        Some(AuditSnapshot {
                            enabled: Some(true),
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
                    item: RateLimitWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }

    pub async fn delete(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        scope: RateLimitScope,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, RateLimitAdminError> {
        let target_id = policy_id(organization_id, scope);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::RateLimitWrite,
            route: route(scope).into(),
            method: "DELETE".into(),
            operation: operation("rate_limit_policy.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::RateLimitPolicy,
            audit_action: audit_action("rate_limit_policy.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({
                "scope": scope.kind(),
                "subject_id": subject(scope),
            }),
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
                if !RateLimitStore::delete(mutation.transaction(), organization_id, target_id)
                    .await?
                {
                    return Err(RateLimitAdminError::NotFound);
                }
                let next = mutation.next_version();
                mutation
                    .commit(
                        previous(next).map(|version| AuditSnapshot {
                            enabled: Some(true),
                            version: Some(version),
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

/// Stable per (organization, scope), so set and delete address the same
/// versioned resource across re-creation.
pub(crate) fn policy_id(organization_id: OrganizationId, scope: RateLimitScope) -> Uuid {
    let subject = subject(scope).map_or_else(String::new, |id| id.to_string());
    deterministic_target_id(&[
        "rate_limit_policy",
        &organization_id.0.to_string(),
        scope.kind(),
        &subject,
    ])
}

fn subject(scope: RateLimitScope) -> Option<Uuid> {
    match scope {
        RateLimitScope::Organization => None,
        RateLimitScope::Team(team) => Some(team.0),
        RateLimitScope::ServiceAccount(account) => Some(account.0),
        RateLimitScope::ApiKey(key) => Some(key.0),
    }
}

fn route(scope: RateLimitScope) -> &'static str {
    match scope {
        RateLimitScope::Organization => ROUTE_ORGANIZATION_RATE_LIMIT,
        RateLimitScope::Team(_) => ROUTE_TEAM_RATE_LIMIT,
        RateLimitScope::ServiceAccount(_) => ROUTE_SERVICE_ACCOUNT_RATE_LIMIT,
        RateLimitScope::ApiKey(_) => ROUTE_API_KEY_RATE_LIMIT,
    }
}

fn replayed<T: serde::de::DeserializeOwned>(
    snapshot: Option<serde_json::Value>,
) -> Result<T, RateLimitAdminError> {
    let snapshot = snapshot.ok_or(RateLimitAdminError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| RateLimitAdminError::Store("recorded response snapshot is invalid".into()))
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

fn map_admin_error(error: AdminStoreError) -> RateLimitAdminError {
    match error {
        AdminStoreError::Authorization(_) => RateLimitAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => RateLimitAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => {
            RateLimitAdminError::VersionConflict(conflict)
        }
        AdminStoreError::IdempotencyConflict => RateLimitAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => RateLimitAdminError::IdempotencyInProgress,
        other => RateLimitAdminError::Admin(other),
    }
}

/// What a write's idempotency fingerprint covers. `tokens_per_minute` is
/// recorded only when set, so a write retried across the upgrade that added
/// it (with the same key and body) replays rather than conflicts.
fn mutation_body(scope: RateLimitScope, input: &RateLimitInput) -> serde_json::Value {
    let mut body = serde_json::json!({
        "scope": scope.kind(),
        "subject_id": subject(scope),
        "requests_per_minute": input.requests_per_minute,
        "max_concurrent": input.max_concurrent,
    });
    if let Some(tokens_per_minute) = input.tokens_per_minute {
        body["tokens_per_minute"] = tokens_per_minute.into();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding},
        identity::{ApiKeyId, ServiceAccountId, TeamId},
    };

    #[test]
    fn a_write_records_tokens_per_minute_only_when_set() {
        let input = RateLimitInput {
            requests_per_minute: Some(60),
            tokens_per_minute: None,
            max_concurrent: None,
        };
        let body = mutation_body(RateLimitScope::Organization, &input);
        // As a write from before tokens per minute recorded it.
        assert_eq!(
            body,
            serde_json::json!({
                "scope": "organization", "subject_id": null,
                "requests_per_minute": 60, "max_concurrent": null,
            })
        );
        let body = mutation_body(
            RateLimitScope::Organization,
            &RateLimitInput {
                tokens_per_minute: Some(1_000),
                ..input
            },
        );
        assert_eq!(body["tokens_per_minute"], 1_000);
    }

    #[test]
    fn rate_limit_writes_are_owner_and_admin_only_and_reads_are_org_wide() {
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
                context.allows(AdminPermission::RateLimitRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::RateLimitWrite, scope),
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
        assert!(!team_admin.allows(AdminPermission::RateLimitRead, scope));
        assert!(!team_admin.allows(AdminPermission::RateLimitWrite, scope));
    }

    #[test]
    fn policy_ids_are_stable_per_tenant_and_scope() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        assert_eq!(
            policy_id(org, RateLimitScope::Team(team)),
            policy_id(org, RateLimitScope::Team(team))
        );
        // The same uuid as a team and as a service account is two policies.
        assert_ne!(
            policy_id(org, RateLimitScope::Team(team)),
            policy_id(
                org,
                RateLimitScope::ServiceAccount(ServiceAccountId(team.0))
            )
        );
        assert_ne!(
            policy_id(org, RateLimitScope::Team(team)),
            policy_id(org, RateLimitScope::ApiKey(ApiKeyId(team.0)))
        );
        assert_ne!(
            policy_id(org, RateLimitScope::Organization),
            policy_id(OrganizationId::new(), RateLimitScope::Organization)
        );
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-rate-limits-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_RATE_LIMITS,
            ROUTE_ORGANIZATION_RATE_LIMIT,
            ROUTE_TEAM_RATE_LIMIT,
            ROUTE_SERVICE_ACCOUNT_RATE_LIMIT,
            ROUTE_API_KEY_RATE_LIMIT,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
