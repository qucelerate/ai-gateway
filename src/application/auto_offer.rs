// Automatic model offer application service: read, set and delete a
// provider account's opt-in.
//
// Reading needs `catalog_read`; setting and deleting need `catalog_write`
// and run through the admin mutation coordinator, so they are idempotent,
// versioned and audited. Setting an opt-in for an account of the active
// release also queues a sync of it, so its models are offered on the
// worker's next tick rather than at the next scheduled listing. The
// releases the worker then creates and activates are audited as the
// system's.

use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    auto_offer::{AutoOfferInput, AutoOfferView},
    credentials::validate_adapter_instance_key,
    identity::{OrganizationId, UserAccountId},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    auto_offer::AutoOfferStore,
    catalog_sync::CatalogSyncStore,
};

pub const ROUTE_AUTO_OFFER: &str =
    "/api/admin/v1/organizations/{org}/catalog/auto-offers/{adapter_instance_key}";

#[derive(Debug)]
pub enum AutoOfferError {
    Forbidden,
    NotFound,
    InvalidKey,
    InvalidLocation,
    /// The active release declares no provider account with the key.
    UnknownAccount,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for AutoOfferError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone, Debug)]
pub struct AutoOfferWrite {
    pub offer: AutoOfferView,
    pub created: bool,
}

#[derive(Clone)]
pub struct AutoOfferService {
    admin: AdminStore,
    store: AutoOfferStore,
}

impl AutoOfferService {
    pub fn new(admin: AdminStore, store: AutoOfferStore) -> Self {
        Self { admin, store }
    }

    pub async fn get(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<AutoOfferView, AutoOfferError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(
            AdminPermission::CatalogRead,
            AdminScope::Organization(organization_id),
        ) {
            return Err(AutoOfferError::Forbidden);
        }
        self.store
            .view(organization_id, adapter_instance_key)
            .await?
            .ok_or(AutoOfferError::NotFound)
    }

    /// Opts the account in, or changes where its models are offered, and
    /// queues a sync of it.
    #[allow(clippy::too_many_arguments)]
    pub async fn set(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: String,
        input: AutoOfferInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<AutoOfferWrite>, AutoOfferError> {
        validate_adapter_instance_key(&adapter_instance_key)
            .map_err(|_| AutoOfferError::InvalidKey)?;
        let input = input
            .validate()
            .map_err(|_| AutoOfferError::InvalidLocation)?;
        let target_id = offer_id(organization_id, &adapter_instance_key);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_AUTO_OFFER.into(),
            method: "PUT".into(),
            operation: operation("catalog_auto_offer.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::CatalogAutoOffer,
            audit_action: audit_action("catalog_auto_offer.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "adapter_instance_key": adapter_instance_key,
                "location": input.location,
            }),
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
                    .ok_or(AutoOfferError::NotFound)?;
                let offer = serde_json::from_value(snapshot).map_err(|_| {
                    AutoOfferError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: AutoOfferWrite {
                        offer,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let Some(family) = CatalogSyncStore::active_account_family(
                    tx,
                    organization_id,
                    &adapter_instance_key,
                )
                .await?
                else {
                    return Err(AutoOfferError::UnknownAccount);
                };
                let created = AutoOfferStore::upsert(
                    tx,
                    organization_id,
                    target_id,
                    &adapter_instance_key,
                    &input,
                    actor,
                )
                .await?;
                // An open job lists the account soon enough.
                CatalogSyncStore::insert_manual(
                    tx,
                    organization_id,
                    Uuid::new_v4(),
                    &adapter_instance_key,
                    &family,
                    actor,
                )
                .await?;
                let stored = AutoOfferStore::view_in(tx, organization_id, &adapter_instance_key)
                    .await?
                    .ok_or(AutoOfferError::NotFound)?;
                let next = mutation.next_version();
                let offer = AutoOfferView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&offer).map_err(|_| {
                    AutoOfferError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        (!created).then(|| AuditSnapshot {
                            enabled: Some(true),
                            version: previous(next),
                            ..Default::default()
                        }),
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
                    item: AutoOfferWrite { offer, created },
                    replayed: false,
                })
            }
        }
    }

    /// Opts the account out. Deployments already offered stay in the
    /// catalog, no longer kept in step.
    pub async fn delete(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: String,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, AutoOfferError> {
        validate_adapter_instance_key(&adapter_instance_key)
            .map_err(|_| AutoOfferError::InvalidKey)?;
        let target_id = offer_id(organization_id, &adapter_instance_key);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_AUTO_OFFER.into(),
            method: "DELETE".into(),
            operation: operation("catalog_auto_offer.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::CatalogAutoOffer,
            audit_action: audit_action("catalog_auto_offer.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({ "adapter_instance_key": adapter_instance_key }),
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
                if !AutoOfferStore::delete(tx, organization_id, &adapter_instance_key).await? {
                    return Err(AutoOfferError::NotFound);
                }
                let next = mutation.next_version();
                mutation
                    .commit(
                        Some(AuditSnapshot {
                            enabled: Some(true),
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

/// Stable per account, so an opt-in set again keeps its version history.
fn offer_id(organization_id: OrganizationId, adapter_instance_key: &str) -> Uuid {
    deterministic_target_id(&[
        "catalog_auto_offer",
        &organization_id.0.to_string(),
        adapter_instance_key,
    ])
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

fn map_admin_error(error: AdminStoreError) -> AutoOfferError {
    match error {
        AdminStoreError::Authorization(_) => AutoOfferError::Forbidden,
        AdminStoreError::ResourceNotFound => AutoOfferError::NotFound,
        AdminStoreError::VersionConflict(conflict) => AutoOfferError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => AutoOfferError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => AutoOfferError::IdempotencyInProgress,
        other => AutoOfferError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_and_actions_are_valid() {
        for value in [
            "catalog_auto_offer.set",
            "catalog_auto_offer.delete",
            "catalog_auto_offer.deleted",
            "catalog_release.created",
            "catalog_release.activated",
        ] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }

    /// The contract's schema lists exactly the fields a response carries.
    #[test]
    fn responses_match_the_contract() {
        use crate::domain::auto_offer::{OfferStatus, OfferedModelView};
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let fields = |value: &serde_json::Value| {
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
        let now = chrono::Utc::now();
        let model = OfferedModelView {
            model: "m".into(),
            deployment: "k-m".into(),
            status: OfferStatus::NeedsPrice,
            price_source: None,
            price_item: None,
            updated_at: now,
        };
        let offer = AutoOfferView {
            adapter_instance_key: "k".into(),
            location: None,
            created_at: now,
            updated_at: now,
            applied_at: None,
            applied_release_id: None,
            error_code: None,
            version: 1,
            models: vec![model.clone()],
        };
        assert_eq!(
            fields(&serde_json::to_value(&offer).unwrap()),
            required("AutoOffer")
        );
        assert_eq!(
            fields(&serde_json::to_value(&model).unwrap()),
            required("OfferedModel")
        );
    }

    #[test]
    fn opt_ins_are_per_account() {
        let org = OrganizationId::new();
        assert_eq!(offer_id(org, "bedrock"), offer_id(org, "bedrock"));
        assert_ne!(offer_id(org, "bedrock"), offer_id(org, "openai"));
        assert_ne!(
            offer_id(org, "bedrock"),
            offer_id(OrganizationId::new(), "bedrock")
        );
    }
}
