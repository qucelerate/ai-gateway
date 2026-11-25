// Automatic price adoption application service (D-024): read and set
// whether the organization's catalog prices follow their sources, list the
// prices held for an admin, and dismiss one.
//
// Reading needs `catalog_read`; setting the policy and dismissing a hold
// need `catalog_write`, run through the admin mutation coordinator, and are
// idempotent, versioned and audited (`catalog_price_policy.set`,
// `catalog_price_hold.dismissed`). A held price is adopted by adopting it as
// any published price (a release that bills it); the next run then resolves
// the hold as adopted. The releases the worker creates and activates are
// audited as the system's.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    price_adoption::{HoldStatus, PriceHoldView, PricePolicyInput, PricePolicyView},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    price_adoption::PriceAdoptionStore,
};

pub const ROUTE_PRICE_POLICY: &str = "/api/admin/v1/organizations/{org}/catalog/price-policy";
pub const ROUTE_PRICE_HOLDS: &str = "/api/admin/v1/organizations/{org}/catalog/price-holds";
pub const ROUTE_PRICE_HOLD_DISMISS: &str =
    "/api/admin/v1/organizations/{org}/catalog/price-holds/{hold_id}/dismiss";

#[derive(Debug)]
pub enum PriceAdoptionError {
    Forbidden,
    NotFound,
    /// The hold is no longer open.
    NotHeld,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for PriceAdoptionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct PricePolicyWrite {
    pub policy: PricePolicyView,
    pub created: bool,
}

#[derive(Clone)]
pub struct PriceAdoptionService {
    admin: AdminStore,
    store: PriceAdoptionStore,
}

impl PriceAdoptionService {
    pub fn new(admin: AdminStore, store: PriceAdoptionStore) -> Self {
        Self { admin, store }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), PriceAdoptionError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(PriceAdoptionError::Forbidden)
        }
    }

    /// The organization's policy; one never set adopts.
    pub async fn policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<PricePolicyView, PriceAdoptionError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.store.policy(organization_id).await?)
    }

    /// Turns adoption on or off. On, the next run adopts what differs.
    pub async fn set_policy(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: PricePolicyInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PricePolicyWrite>, PriceAdoptionError> {
        let target_id = policy_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_POLICY.into(),
            method: "PUT".into(),
            operation: operation("catalog_price_policy.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::CatalogPricePolicy,
            audit_action: audit_action("catalog_price_policy.set"),
            if_match,
            request_id,
            body: serde_json::json!({ "auto_adopt": input.auto_adopt }),
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
                    .ok_or(PriceAdoptionError::NotFound)?;
                let policy = serde_json::from_value(snapshot).map_err(|_| {
                    PriceAdoptionError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: PricePolicyWrite {
                        policy,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let before = PriceAdoptionStore::policy_in(tx, organization_id).await?;
                let created = PriceAdoptionStore::upsert_policy(
                    tx,
                    organization_id,
                    target_id,
                    &input,
                    actor,
                )
                .await?;
                let stored = PriceAdoptionStore::policy_in(tx, organization_id)
                    .await?
                    .ok_or(PriceAdoptionError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let policy = PricePolicyView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&policy).map_err(|_| {
                    PriceAdoptionError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        before.map(|before| AuditSnapshot {
                            enabled: Some(before.auto_adopt),
                            version: previous(next),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            enabled: Some(input.auto_adopt),
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
                    item: PricePolicyWrite { policy, created },
                    replayed: false,
                })
            }
        }
    }

    /// The open holds, or every hold with `all`.
    pub async fn holds(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        all: bool,
    ) -> Result<Vec<PriceHoldView>, PriceAdoptionError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        Ok(self.store.holds(organization_id, all).await?)
    }

    /// Dismisses a held price: it is not adopted, and the same proposal is
    /// not held again until its source publishes something else.
    pub async fn dismiss(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        hold_id: Uuid,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PriceHoldView>, PriceAdoptionError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_HOLD_DISMISS.into(),
            method: "POST".into(),
            operation: operation("catalog_price_hold.dismiss"),
            idempotency_key,
            target_id: hold_id,
            parent_id: None,
            target: AuditTargetResource::CatalogPriceHold,
            audit_action: audit_action("catalog_price_hold.dismissed"),
            if_match,
            request_id,
            body: serde_json::json!({ "hold_id": hold_id }),
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
                    .ok_or(PriceAdoptionError::NotFound)?;
                let hold = serde_json::from_value(snapshot).map_err(|_| {
                    PriceAdoptionError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: hold,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                if !PriceAdoptionStore::dismiss(tx, organization_id, hold_id, actor).await? {
                    return Err(PriceAdoptionError::NotHeld);
                }
                let stored = PriceAdoptionStore::hold_in(tx, organization_id, hold_id)
                    .await?
                    .ok_or(PriceAdoptionError::NotFound)?;
                let next = mutation.next_version();
                let hold = PriceHoldView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&hold).map_err(|_| {
                    PriceAdoptionError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        Some(AuditSnapshot {
                            enabled: Some(true),
                            version: previous(next),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            enabled: Some(hold.status == HoldStatus::Held),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: hold_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: hold,
                    replayed: false,
                })
            }
        }
    }
}

/// Stable per organization.
fn policy_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&["catalog_price_policy", &organization_id.0.to_string()])
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

fn map_admin_error(error: AdminStoreError) -> PriceAdoptionError {
    match error {
        AdminStoreError::Authorization(_) => PriceAdoptionError::Forbidden,
        AdminStoreError::ResourceNotFound => PriceAdoptionError::NotFound,
        AdminStoreError::VersionConflict(conflict) => PriceAdoptionError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => PriceAdoptionError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => PriceAdoptionError::IdempotencyInProgress,
        other => PriceAdoptionError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_and_actions_are_valid() {
        for value in [
            "catalog_price_policy.set",
            "catalog_price_hold.dismiss",
            "catalog_price_hold.dismissed",
            "catalog_release.prices_adopted",
        ] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }

    /// The contract's schemas list exactly the fields a response carries.
    #[test]
    fn responses_match_the_contract() {
        use crate::domain::price_adoption::{DimensionChange, HoldReason};
        use crate::domain::pricing::PriceDimension;
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let fields = |value: serde_json::Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort_unstable();
            keys
        };
        let required = |schema: &str| {
            let mut names: Vec<String> = contract["components"]["schemas"][schema]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
            names.sort_unstable();
            names
        };
        let change = DimensionChange {
            dimension: PriceDimension::CachedInput,
            catalog: 9,
            observed: "8.25".into(),
            proposed: 9,
        };
        let hold = PriceHoldView {
            id: Uuid::nil(),
            deployment: "d".into(),
            operation: "chat".into(),
            model: "m".into(),
            currency: "USD".into(),
            source: "s".into(),
            reason: HoldReason::ToZero,
            changes: vec![change.clone()],
            status: HoldStatus::Held,
            release_id: Uuid::nil(),
            created_at: chrono::Utc::now(),
            resolved_at: None,
            version: 1,
        };
        assert_eq!(
            fields(serde_json::to_value(PricePolicyView::unset()).unwrap()),
            required("PricePolicy")
        );
        assert_eq!(
            fields(serde_json::to_value(&hold).unwrap()),
            required("PriceHold")
        );
        assert_eq!(
            fields(serde_json::to_value(&change).unwrap()),
            required("PriceHoldChange")
        );
        assert_eq!(
            serde_json::to_value(&change).unwrap()["dimension"],
            "cached_input"
        );
    }

    #[test]
    fn the_policy_is_one_per_organization() {
        let org = OrganizationId::new();
        assert_eq!(policy_id(org), policy_id(org));
        assert_ne!(policy_id(org), policy_id(OrganizationId::new()));
    }
}
