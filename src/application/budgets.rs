// Budget admin application service: list, set and delete the spending budgets
// of an organization, its teams, its service accounts and their API keys.
//
// Reads need `budget_read`; writes need `budget_write` (owner and admin only)
// at organization scope, including for a team's or service account's budget.
// Writes run through the admin mutation coordinator, so each is idempotent,
// versioned, and audited in the same transaction as the change and as the
// quota buckets that enforce it (`persistence::budgets`).

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    budget::{BudgetError, BudgetInput, BudgetPolicyView, BudgetScope},
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    budgets::BudgetStore,
};

pub const ROUTE_BUDGETS: &str = "/api/admin/v1/organizations/{org}/budgets";
pub const ROUTE_ORGANIZATION_BUDGET: &str =
    "/api/admin/v1/organizations/{org}/budgets/organization";
pub const ROUTE_TEAM_BUDGET: &str = "/api/admin/v1/organizations/{org}/budgets/teams/{team_id}";
pub const ROUTE_SERVICE_ACCOUNT_BUDGET: &str =
    "/api/admin/v1/organizations/{org}/budgets/service-accounts/{service_account_id}";
pub const ROUTE_API_KEY_BUDGET: &str =
    "/api/admin/v1/organizations/{org}/budgets/api-keys/{api_key_id}";

const COLLECTION: &str = "budget_policies";
const ORDER_ID: &str = "id";

#[derive(Debug)]
pub enum BudgetAdminError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Invalid(BudgetError),
    Pagination(PaginationError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for BudgetAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct BudgetWrite {
    pub policy: BudgetPolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct BudgetAdminService {
    admin: AdminStore,
    store: BudgetStore,
    codec: Option<SignedCursorCodec>,
}

impl BudgetAdminService {
    pub fn new(admin: AdminStore, store: BudgetStore, codec: Option<SignedCursorCodec>) -> Self {
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
    ) -> Result<Page<BudgetPolicyView>, BudgetAdminError> {
        let scope = AdminScope::Organization(organization_id);
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(AdminPermission::BudgetRead, scope) {
            return Err(BudgetAdminError::Forbidden);
        }
        let codec = self
            .codec
            .as_ref()
            .ok_or(BudgetAdminError::AuthenticationUnavailable)?;
        let after =
            match page.page_token.as_deref() {
                Some(token) => {
                    let claims = codec
                        .decode_and_validate(token, actor, scope, COLLECTION, "", ORDER_ID)
                        .map_err(BudgetAdminError::Pagination)?;
                    // Only a validly signed token gets here, so a bad keyset is
                    // a server-side inconsistency.
                    Some(Uuid::parse_str(&claims.last_keyset).map_err(|_| {
                        BudgetAdminError::Store("budget page keyset is invalid".into())
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
                .map_err(BudgetAdminError::Pagination)?;
                Some(
                    codec
                        .encode(&claims)
                        .map_err(BudgetAdminError::Pagination)?,
                )
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Create or replace the budget of one scope, and bring its quota
    /// buckets in line in the same transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        scope: BudgetScope,
        input: BudgetInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<BudgetWrite>, BudgetAdminError> {
        let input = input.validate().map_err(BudgetAdminError::Invalid)?;
        let target_id = policy_id(organization_id, scope);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::BudgetWrite,
            route: route(scope).into(),
            method: "PUT".into(),
            operation: operation("budget_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::BudgetPolicy,
            audit_action: audit_action("budget_policy.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "scope": scope.kind(),
                "subject_id": subject(scope),
                "currency": input.currency,
                "limit_minor": input.limit_minor,
                "period": input.period,
                "starts_at": input.starts_at,
                "ends_at": input.ends_at,
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
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: BudgetWrite {
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
                if !BudgetStore::scope_exists(tx, organization_id, scope).await? {
                    return Err(BudgetAdminError::NotFound);
                }
                let now = chrono::Utc::now();
                let before = BudgetStore::view(tx, organization_id, target_id, now).await?;
                let created =
                    BudgetStore::upsert(tx, organization_id, target_id, scope, &input, actor)
                        .await?;
                BudgetStore::apply(tx, organization_id, scope, &input, now).await?;
                let stored = BudgetStore::view(tx, organization_id, target_id, now)
                    .await?
                    .ok_or(BudgetAdminError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = BudgetPolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    BudgetAdminError::Store("response snapshot is not serializable".into())
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
                    item: BudgetWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }

    pub async fn delete(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        scope: BudgetScope,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, BudgetAdminError> {
        let target_id = policy_id(organization_id, scope);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::BudgetWrite,
            route: route(scope).into(),
            method: "DELETE".into(),
            operation: operation("budget_policy.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::BudgetPolicy,
            audit_action: audit_action("budget_policy.deleted"),
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
                let tx = mutation.transaction();
                let now = chrono::Utc::now();
                let Some(stored) = BudgetStore::view(tx, organization_id, target_id, now).await?
                else {
                    return Err(BudgetAdminError::NotFound);
                };
                // Retired first: from this commit no request is charged
                // against, or admitted by, the removed budget.
                BudgetStore::retire_scope(tx, organization_id, scope, &stored, now).await?;
                if !BudgetStore::delete(tx, organization_id, target_id).await? {
                    return Err(BudgetAdminError::NotFound);
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
pub(crate) fn policy_id(organization_id: OrganizationId, scope: BudgetScope) -> Uuid {
    let subject = subject(scope).map_or_else(String::new, |id| id.to_string());
    deterministic_target_id(&[
        "budget_policy",
        &organization_id.0.to_string(),
        scope.kind(),
        &subject,
    ])
}

fn subject(scope: BudgetScope) -> Option<Uuid> {
    match scope {
        BudgetScope::Organization => None,
        BudgetScope::Team(team) => Some(team.0),
        BudgetScope::ServiceAccount(account) => Some(account.0),
        BudgetScope::ApiKey(key) => Some(key.0),
    }
}

fn route(scope: BudgetScope) -> &'static str {
    match scope {
        BudgetScope::Organization => ROUTE_ORGANIZATION_BUDGET,
        BudgetScope::Team(_) => ROUTE_TEAM_BUDGET,
        BudgetScope::ServiceAccount(_) => ROUTE_SERVICE_ACCOUNT_BUDGET,
        BudgetScope::ApiKey(_) => ROUTE_API_KEY_BUDGET,
    }
}

fn replayed<T: serde::de::DeserializeOwned>(
    snapshot: Option<serde_json::Value>,
) -> Result<T, BudgetAdminError> {
    let snapshot = snapshot.ok_or(BudgetAdminError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| BudgetAdminError::Store("recorded response snapshot is invalid".into()))
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

fn map_admin_error(error: AdminStoreError) -> BudgetAdminError {
    match error {
        AdminStoreError::Authorization(_) => BudgetAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => BudgetAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => BudgetAdminError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => BudgetAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => BudgetAdminError::IdempotencyInProgress,
        other => BudgetAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding},
        identity::{ApiKeyId, ServiceAccountId, TeamId},
    };

    #[test]
    fn budget_writes_are_owner_and_admin_only_and_reads_are_org_wide() {
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
                context.allows(AdminPermission::BudgetRead, scope),
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::BudgetWrite, scope),
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
        assert!(!team_admin.allows(AdminPermission::BudgetRead, scope));
        assert!(!team_admin.allows(AdminPermission::BudgetWrite, scope));
    }

    #[test]
    fn policy_ids_are_stable_per_tenant_and_scope() {
        let org = OrganizationId::new();
        let team = TeamId::new();
        assert_eq!(
            policy_id(org, BudgetScope::Team(team)),
            policy_id(org, BudgetScope::Team(team))
        );
        // The same uuid as a team and as a service account is two policies.
        assert_ne!(
            policy_id(org, BudgetScope::Team(team)),
            policy_id(org, BudgetScope::ServiceAccount(ServiceAccountId(team.0)))
        );
        assert_ne!(
            policy_id(org, BudgetScope::ServiceAccount(ServiceAccountId(team.0))),
            policy_id(org, BudgetScope::ApiKey(ApiKeyId(team.0)))
        );
        assert_ne!(
            policy_id(org, BudgetScope::Organization),
            policy_id(OrganizationId::new(), BudgetScope::Organization)
        );
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-budgets-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_BUDGETS,
            ROUTE_ORGANIZATION_BUDGET,
            ROUTE_TEAM_BUDGET,
            ROUTE_SERVICE_ACCOUNT_BUDGET,
            ROUTE_API_KEY_BUDGET,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
