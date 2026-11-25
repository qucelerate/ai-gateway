// Routing policy application service (D-023, Phase 9): read and set the
// provider families and deployment locations an organization's requests
// may be sent to.
//
// Reading needs `catalog_read`; setting it needs `catalog_write`, runs
// through the admin mutation coordinator, and is idempotent, versioned and
// audited (`routing_policy.set`). The write publishes a configuration
// version in the same transaction, so every process routes by it once it
// loads that version.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    routing_policy::{RoutingPolicyError as InvalidPolicy, RoutingPolicyInput, RoutingPolicyView},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    routing_policy::RoutingPolicyStore,
};

pub const ROUTE_ROUTING_POLICY: &str = "/api/admin/v1/organizations/{org}/catalog/routing-policy";

#[derive(Debug)]
pub enum RoutingPolicyError {
    Forbidden,
    NotFound,
    Invalid(InvalidPolicy),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for RoutingPolicyError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct RoutingPolicyWrite {
    pub policy: RoutingPolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct RoutingPolicyService {
    admin: AdminStore,
    store: RoutingPolicyStore,
}

impl RoutingPolicyService {
    pub fn new(admin: AdminStore, store: RoutingPolicyStore) -> Self {
        Self { admin, store }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), RoutingPolicyError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(RoutingPolicyError::Forbidden)
        }
    }

    /// The organization's policy; one never set permits any deployment.
    pub async fn policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<RoutingPolicyView, RoutingPolicyError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.store.policy(organization_id).await?)
    }

    /// Replaces the policy. Requests routed after the configuration version
    /// it publishes is loaded consider only the deployments it permits.
    pub async fn set_policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: RoutingPolicyInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<RoutingPolicyWrite>, RoutingPolicyError> {
        let input = input.normalized().map_err(RoutingPolicyError::Invalid)?;
        let target_id = policy_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_ROUTING_POLICY.into(),
            method: "PUT".into(),
            operation: operation("routing_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::RoutingPolicy,
            audit_action: audit_action("routing_policy.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "providers": input.providers,
                "locations": input.locations,
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
                    .ok_or(RoutingPolicyError::NotFound)?;
                let policy = serde_json::from_value(snapshot).map_err(|_| {
                    RoutingPolicyError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: RoutingPolicyWrite {
                        policy,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let before = RoutingPolicyStore::policy_in(tx, organization_id).await?;
                let created = RoutingPolicyStore::upsert_policy(
                    tx,
                    organization_id,
                    target_id,
                    &input,
                    actor,
                )
                .await?;
                let stored = RoutingPolicyStore::policy_in(tx, organization_id)
                    .await?
                    .ok_or(RoutingPolicyError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = RoutingPolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    RoutingPolicyError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        before.map(|before| AuditSnapshot {
                            enabled: Some(before.providers.is_some() || before.locations.is_some()),
                            version: previous(next),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            // Whether anything is restricted.
                            enabled: Some(policy.providers.is_some() || policy.locations.is_some()),
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
                    item: RoutingPolicyWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }
}

fn policy_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&["routing_policy", &organization_id.0.to_string()])
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

fn map_admin_error(error: AdminStoreError) -> RoutingPolicyError {
    match error {
        AdminStoreError::Authorization(_) => RoutingPolicyError::Forbidden,
        AdminStoreError::ResourceNotFound => RoutingPolicyError::NotFound,
        AdminStoreError::VersionConflict(conflict) => RoutingPolicyError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => RoutingPolicyError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => RoutingPolicyError::IdempotencyInProgress,
        other => RoutingPolicyError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_and_action_are_valid() {
        assert!(AdminOperation::new("routing_policy.set").is_ok());
        assert!(AuditAction::new("routing_policy.set").is_ok());
    }

    /// The contract's schema lists exactly the fields a response carries.
    #[test]
    fn the_response_matches_the_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let mut required: Vec<String> =
            contract["components"]["schemas"]["RoutingPolicy"]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
        required.sort_unstable();
        let mut fields: Vec<String> = serde_json::to_value(RoutingPolicyView::unset())
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        fields.sort_unstable();
        assert_eq!(fields, required);
    }

    #[test]
    fn the_policy_is_one_per_organization() {
        let org = OrganizationId::new();
        assert_eq!(policy_id(org), policy_id(org));
        assert_ne!(policy_id(org), policy_id(OrganizationId::new()));
    }
}
