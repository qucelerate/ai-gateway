// Catalog write application service: create releases, activate (or roll back
// to) a release, set or remove encrypted provider credentials, and create or
// revoke organization price overrides.
//
// Every write runs through the admin mutation coordinator, so each one is
// idempotent, authorized server-side (`catalog_write` / `credential_write`,
// owner and admin only), versioned, and audited in the same transaction as
// the change. Release creation never activates; activation is its own audited
// step. Credentials are sealed before they reach the store, and nothing on
// this path returns, logs, or audits the plaintext: the idempotency body
// carries only a keyed fingerprint of the secret.
//
// The inference path reads stored credentials through
// `provider_resolver::ProviderResolver`, and effective price overrides through
// `CatalogStore::resolve_active`.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditStatus,
        AuditTargetResource,
    },
    catalog::{CatalogError, CatalogManifest, ProviderFamily},
    catalog_admin::CatalogReleaseSummary,
    credentials::{
        CredentialBinding, CredentialCipher, CredentialError, ProviderCredentialMetadata,
        ProviderSecret, ProviderSecretInput, validate_adapter_instance_key,
    },
    identity::{OrganizationId, UserAccountId},
    price_override::{
        PriceOverrideError, PriceOverrideInput, PriceOverrideRevocation, PriceOverrideView,
    },
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    catalog::{CatalogStoreError, activate_release, insert_release, lock_catalog},
    catalog_admin::{CatalogAdminStore, CatalogAdminStoreError},
    price_overrides::PriceOverrideStore,
    provider_credentials::ProviderCredentialStore,
};

use super::catalog_admin::{ROUTE_PRICE_OVERRIDES, ROUTE_RELEASES};

pub const ROUTE_RELEASE_ACTIVATE: &str =
    "/api/admin/v1/organizations/{org}/catalog/releases/{release_id}/activate";
pub const ROUTE_CREDENTIALS: &str = "/api/admin/v1/organizations/{org}/catalog/credentials";
pub const ROUTE_CREDENTIAL: &str =
    "/api/admin/v1/organizations/{org}/catalog/credentials/{adapter_instance_key}";
pub const ROUTE_PRICE_OVERRIDE_REVOKE: &str =
    "/api/admin/v1/organizations/{org}/catalog/price-overrides/{override_id}/revoke";

#[derive(Debug)]
pub enum CatalogWriteError {
    Forbidden,
    NotFound,
    /// No release of the organization declares the adapter instance key.
    ProviderAccountNotFound,
    /// The manifest is unparseable or its graph is invalid.
    InvalidManifest(String),
    /// A release with the same canonical digest already exists.
    ReleaseExists(Uuid),
    InvalidCredential(&'static str),
    InvalidPriceOverride(PriceOverrideError),
    /// No release of the organization prices the operation on a deployment
    /// of that name.
    DeploymentNotDeclared,
    /// A declaring release's provider family never reports a usage dimension
    /// the override charges for.
    UnreportedUsagePrice(String),
    AlreadyRevoked,
    /// The release's provider family cannot bill a price this unrevoked,
    /// unexpired override sets; revoke the override first.
    IncompatiblePriceOverride(Uuid),
    /// `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` is not configured.
    CredentialEncryptionUnavailable,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for CatalogWriteError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

impl From<CatalogAdminStoreError> for CatalogWriteError {
    fn from(error: CatalogAdminStoreError) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

impl From<CatalogStoreError> for CatalogWriteError {
    fn from(error: CatalogStoreError) -> Self {
        match error {
            CatalogStoreError::IncompatiblePriceOverride(id) => Self::IncompatiblePriceOverride(id),
            error => Self::Store(format!("{error:?}")),
        }
    }
}

/// An activation result: the now-active release plus the version of the
/// organization's activation pointer, which is the ETag for the next
/// conditional activation.
#[derive(Clone, Debug)]
pub struct Activation {
    pub release: CatalogReleaseSummary,
    pub version: u64,
}

#[derive(Clone, Debug)]
pub struct CredentialWrite {
    pub metadata: ProviderCredentialMetadata,
    pub created: bool,
}

#[derive(Clone)]
pub struct CatalogWriteService {
    admin: AdminStore,
    catalog: CatalogAdminStore,
    cipher: Option<CredentialCipher>,
}

impl CatalogWriteService {
    pub fn new(
        admin: AdminStore,
        catalog: CatalogAdminStore,
        cipher: Option<CredentialCipher>,
    ) -> Self {
        Self {
            admin,
            catalog,
            cipher,
        }
    }

    /// Store a sealed, inactive release. An identical manifest (same
    /// canonical digest) is rejected with the existing release id.
    pub async fn create_release(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        manifest: &[u8],
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<CatalogReleaseSummary>, CatalogWriteError> {
        let (manifest, digest) = CatalogManifest::parse(manifest).map_err(manifest_error)?;
        let digest_hex = digest.to_string();
        let target_id = deterministic_target_id(&[
            "catalog_release",
            &organization_id.0.to_string(),
            &digest_hex,
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_RELEASES.into(),
            method: "POST".into(),
            operation: operation("catalog_release.create"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::CatalogRelease,
            audit_action: audit_action("catalog_release.created"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "digest": digest_hex }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let release_id = record.resource_id.ok_or(CatalogWriteError::NotFound)?;
                Ok(MutationOutcome {
                    item: self.release(organization_id, release_id).await?,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                lock_catalog(tx, organization_id).await?;
                // Dropping the mutation rolls back its claim and version row.
                if let Some(existing) =
                    CatalogAdminStore::release_by_digest(tx, organization_id, &digest.0).await?
                {
                    return Err(CatalogWriteError::ReleaseExists(existing));
                }
                insert_release(tx, organization_id, target_id, &manifest, digest).await?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
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
                Ok(MutationOutcome {
                    item: self.release(organization_id, target_id).await?,
                    replayed: false,
                })
            }
        }
    }

    /// Point the organization at `release_id`. Activating an older release is
    /// how a rollback is performed; releases themselves never change.
    pub async fn activate_release(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Uuid,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<Activation>, CatalogWriteError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_RELEASE_ACTIVATE.into(),
            method: "POST".into(),
            operation: operation("catalog_release.activate"),
            idempotency_key,
            target_id: release_id,
            parent_id: None,
            target: AuditTargetResource::CatalogActivation,
            audit_action: audit_action("catalog_release.activated"),
            if_match,
            request_id,
            body: serde_json::json!({ "release_id": release_id }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let metadata = record
                    .response_metadata
                    .ok_or(CatalogWriteError::NotFound)?;
                let version = metadata.version.ok_or(CatalogWriteError::NotFound)?;
                Ok(MutationOutcome {
                    item: Activation {
                        release: replayed(metadata.snapshot)?,
                        version,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                lock_catalog(tx, organization_id).await?;
                activate_release(tx, organization_id, release_id).await?;
                let release = CatalogAdminStore::release_in(tx, organization_id, release_id)
                    .await?
                    .ok_or(CatalogWriteError::NotFound)?;
                let snapshot = snapshot(&release)?;
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
                            resource_id: release_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: Activation {
                        release,
                        version: next.get(),
                    },
                    replayed: false,
                })
            }
        }
    }

    /// Create or rotate the credential for an adapter instance declared by
    /// one of the organization's releases.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_credential(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        input: ProviderSecretInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<CredentialWrite>, CatalogWriteError> {
        validate_adapter_instance_key(adapter_instance_key).map_err(credential_error)?;
        let target_id = credential_id(organization_id, adapter_instance_key);
        self.authorize_credential_write(
            actor,
            organization_id,
            target_id,
            "provider_credential.set",
            request_id,
        )
        .await?;
        let cipher = self
            .cipher
            .as_ref()
            .ok_or(CatalogWriteError::CredentialEncryptionUnavailable)?;
        // The fingerprint covers the request, not the family the key
        // resolves to, so a retry still replays after an activation changes
        // that family.
        let fingerprint = cipher
            .fingerprint(organization_id, adapter_instance_key, &input)
            .map_err(credential_error)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CredentialWrite,
            route: ROUTE_CREDENTIAL.into(),
            method: "PUT".into(),
            operation: operation("provider_credential.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ProviderCredential,
            audit_action: audit_action("provider_credential.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "adapter_instance_key": adapter_instance_key,
                "secret_fingerprint": fingerprint,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the credential has since been
            // rotated or deleted.
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: CredentialWrite {
                    metadata: replayed(
                        record
                            .response_metadata
                            .and_then(|metadata| metadata.snapshot),
                    )?,
                    created: false,
                },
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                // The org row lock taken by the coordinator holds the catalog
                // still until commit. Errors from here on drop the mutation,
                // which releases the idempotency claim.
                let tx = mutation.transaction();
                let family_name = ProviderCredentialStore::declared_family(
                    tx,
                    organization_id,
                    adapter_instance_key,
                )
                .await?
                .ok_or(CatalogWriteError::ProviderAccountNotFound)?;
                let family = ProviderFamily::from_str(&family_name).map_err(|_| {
                    CatalogWriteError::Store("unknown stored provider family".into())
                })?;
                let secret = ProviderSecret::for_family(family, input).map_err(credential_error)?;
                let binding = CredentialBinding {
                    organization_id,
                    adapter_instance_key,
                    family,
                };
                let sealed = cipher.seal(binding, &secret).map_err(credential_error)?;
                // The principal is recorded only by digest; the inference
                // path refuses it until an operator binds it.
                let principal_digest = secret.cloud_principal().map(|principal| principal.digest());
                let created = ProviderCredentialStore::upsert(
                    tx,
                    organization_id,
                    target_id,
                    adapter_instance_key,
                    &family_name,
                    &sealed,
                    principal_digest.as_deref(),
                    actor,
                )
                .await?;
                let metadata = ProviderCredentialStore::metadata(tx, organization_id, target_id)
                    .await?
                    .ok_or(CatalogWriteError::NotFound)?;
                let snapshot = snapshot(&metadata)?;
                let next = mutation.next_version();
                let before =
                    (!created)
                        .then(|| previous(next))
                        .flatten()
                        .map(|version| AuditSnapshot {
                            enabled: Some(true),
                            version: Some(version),
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
                    item: CredentialWrite { metadata, created },
                    replayed: false,
                })
            }
        }
    }

    pub async fn delete_credential(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, CatalogWriteError> {
        validate_adapter_instance_key(adapter_instance_key).map_err(credential_error)?;
        let target_id = credential_id(organization_id, adapter_instance_key);
        self.authorize_credential_write(
            actor,
            organization_id,
            target_id,
            "provider_credential.deleted",
            request_id,
        )
        .await?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CredentialWrite,
            route: ROUTE_CREDENTIAL.into(),
            method: "DELETE".into(),
            operation: operation("provider_credential.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ProviderCredential,
            audit_action: audit_action("provider_credential.deleted"),
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
                if !ProviderCredentialStore::delete(
                    mutation.transaction(),
                    organization_id,
                    target_id,
                )
                .await?
                {
                    return Err(CatalogWriteError::NotFound);
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

    /// Create an override for a deployment name and operation that at least
    /// one of the organization's releases prices. Every declaring release's
    /// provider family must be able to bill each priced dimension, because
    /// the override applies whichever of those releases is active.
    pub async fn create_price_override(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: PriceOverrideInput,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PriceOverrideView>, CatalogWriteError> {
        // The fingerprint covers the request as sent, so a retry without an
        // explicit start replays instead of conflicting on a later "now".
        let body = snapshot(&input)?;
        let target_id = deterministic_target_id(&[
            "price_override.create",
            &organization_id.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_OVERRIDES.into(),
            method: "POST".into(),
            operation: operation("price_override.create"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::PriceOverride,
            audit_action: audit_action("price_override.created"),
            if_match: IfMatch::Any,
            request_id,
            body,
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: replayed(
                    record
                        .response_metadata
                        .and_then(|metadata| metadata.snapshot),
                )?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                // Hold releases still while the declaring families are checked.
                lock_catalog(tx, organization_id).await?;
                // Validated only for a new write, so a retry after the window
                // has passed still replays, and against database time, the
                // clock route resolution compares the window with. Read after
                // the lock wait: `now()` would be the transaction's start.
                let now = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT clock_timestamp()")
                    .fetch_one(&mut **tx)
                    .await?;
                let new = input
                    .validate(now)
                    .map_err(CatalogWriteError::InvalidPriceOverride)?;
                let families = PriceOverrideStore::declaring_families(
                    tx,
                    organization_id,
                    &new.deployment,
                    new.operation.as_str(),
                )
                .await?;
                if families.is_empty() {
                    return Err(CatalogWriteError::DeploymentNotDeclared);
                }
                for name in families {
                    let family = ProviderFamily::from_str(&name).map_err(|_| {
                        CatalogWriteError::Store("unknown stored provider family".into())
                    })?;
                    if family.charges_unreported_usage(
                        new.cached_input_minor_per_token,
                        new.reasoning_minor_per_token,
                        new.other_minor_per_token,
                    ) {
                        return Err(CatalogWriteError::UnreportedUsagePrice(name));
                    }
                }
                PriceOverrideStore::insert(tx, organization_id, target_id, &new, actor).await?;
                let view = PriceOverrideStore::get(tx, organization_id, target_id)
                    .await?
                    .ok_or(CatalogWriteError::NotFound)?;
                let snapshot = snapshot(&view)?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        None,
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
                    item: view,
                    replayed: false,
                })
            }
        }
    }

    /// Stop an override from applying to new requests. Reservations already
    /// priced by it keep that price.
    #[allow(clippy::too_many_arguments)]
    pub async fn revoke_price_override(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        override_id: Uuid,
        revocation: PriceOverrideRevocation,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PriceOverrideView>, CatalogWriteError> {
        let reason = revocation
            .validate()
            .map_err(CatalogWriteError::InvalidPriceOverride)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_OVERRIDE_REVOKE.into(),
            method: "POST".into(),
            operation: operation("price_override.revoke"),
            idempotency_key,
            target_id: override_id,
            parent_id: None,
            target: AuditTargetResource::PriceOverride,
            audit_action: audit_action("price_override.revoked"),
            if_match,
            request_id,
            body: serde_json::json!({ "override_id": override_id, "reason": reason }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: replayed(
                    record
                        .response_metadata
                        .and_then(|metadata| metadata.snapshot),
                )?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                if !PriceOverrideStore::revoke(tx, organization_id, override_id, &reason, actor)
                    .await?
                {
                    return Err(CatalogWriteError::AlreadyRevoked);
                }
                let view = PriceOverrideStore::get(tx, organization_id, override_id)
                    .await?
                    .ok_or(CatalogWriteError::NotFound)?;
                let snapshot = snapshot(&view)?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        previous(next).map(|version| AuditSnapshot {
                            version: Some(version),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Revoked),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: override_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: view,
                    replayed: false,
                })
            }
        }
    }

    async fn release(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<CatalogReleaseSummary, CatalogWriteError> {
        self.catalog
            .release(organization_id, release_id)
            .await?
            .ok_or(CatalogWriteError::NotFound)
    }

    /// Check `credential_write` before anything that depends on the adapter
    /// instance key. Otherwise an unauthorized caller could tell declared keys
    /// (403 after the lookup) from undeclared ones (404 before it). A denial
    /// is audited as the mutation coordinator audits its own.
    async fn authorize_credential_write(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        target_id: Uuid,
        action: &str,
        request_id: Uuid,
    ) -> Result<(), CatalogWriteError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(
            AdminPermission::CredentialWrite,
            AdminScope::Organization(organization_id),
        ) {
            return Ok(());
        }
        // Best effort, like the coordinator: a failed audit write must not
        // turn a 403 into a 503.
        let _ = self
            .admin
            .record_audit(AdminAuditEvent {
                actor: AuditActor::User(actor),
                organization_id,
                team_id: None,
                target: AuditTargetResource::ProviderCredential,
                action: audit_action(action),
                target_id,
                outcome: AuditOutcome::Denied,
                request_id,
                before: None,
                after: None,
            })
            .await;
        Err(CatalogWriteError::Forbidden)
    }
}

/// Stable per (organization, adapter instance), so set and delete address the
/// same versioned resource across rotations and re-creation.
fn credential_id(organization_id: OrganizationId, adapter_instance_key: &str) -> Uuid {
    deterministic_target_id(&[
        "provider_credential",
        &organization_id.0.to_string(),
        adapter_instance_key,
    ])
}

/// A non-secret response recorded with the idempotency result, so a replay
/// returns what the original request returned.
fn snapshot<T: serde::Serialize>(response: &T) -> Result<serde_json::Value, CatalogWriteError> {
    serde_json::to_value(response)
        .map_err(|_| CatalogWriteError::Store("response snapshot is not serializable".into()))
}

fn replayed<T: serde::de::DeserializeOwned>(
    snapshot: Option<serde_json::Value>,
) -> Result<T, CatalogWriteError> {
    let snapshot = snapshot.ok_or(CatalogWriteError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| CatalogWriteError::Store("recorded response snapshot is invalid".into()))
}

fn previous(next: ResourceVersion) -> Option<u64> {
    next.get().checked_sub(1).filter(|version| *version > 0)
}

fn manifest_error(error: CatalogError) -> CatalogWriteError {
    CatalogWriteError::InvalidManifest(error.to_string())
}

fn credential_error(error: CredentialError) -> CatalogWriteError {
    match error {
        CredentialError::Invalid(detail) => CatalogWriteError::InvalidCredential(detail),
        // Key and cipher failures carry no secret material.
        other => CatalogWriteError::Store(other.to_string()),
    }
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> CatalogWriteError {
    match error {
        AdminStoreError::Authorization(_) => CatalogWriteError::Forbidden,
        AdminStoreError::ResourceNotFound => CatalogWriteError::NotFound,
        AdminStoreError::VersionConflict(conflict) => CatalogWriteError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => CatalogWriteError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => CatalogWriteError::IdempotencyInProgress,
        other => CatalogWriteError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn catalog_and_credential_writes_are_owner_and_admin_only() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, allowed) in [
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
            for permission in [
                AdminPermission::CatalogWrite,
                AdminPermission::CredentialWrite,
            ] {
                assert_eq!(context.allows(permission, scope), allowed, "{role:?}");
            }
        }
        let team_admin = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::team(
                org,
                crate::domain::identity::TeamId::new(),
                user,
                AdminRole::TeamAdmin,
            )],
        };
        assert!(!team_admin.allows(AdminPermission::CatalogWrite, scope));
        assert!(!team_admin.allows(AdminPermission::CredentialWrite, scope));
    }

    #[test]
    fn credential_ids_are_stable_per_tenant_and_adapter() {
        let org = OrganizationId::new();
        assert_eq!(
            credential_id(org, "openai-main"),
            credential_id(org, "openai-main")
        );
        assert_ne!(
            credential_id(org, "openai-main"),
            credential_id(org, "openai-alt")
        );
        assert_ne!(
            credential_id(org, "openai-main"),
            credential_id(OrganizationId::new(), "openai-main")
        );
    }
}
