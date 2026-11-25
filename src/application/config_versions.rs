// Configuration version admin application service: list an organization's
// versions, see which version each API-serving instance serves it, and pin
// the organization to a version (or unpin it) as a rollback brake.
//
// Reads need `catalog_read`; pinning needs `catalog_write` (owner and admin
// only). A pin change runs through the admin mutation coordinator, so it is
// idempotent, versioned on the organization's pin, and audited in the same
// transaction as the feed entry that every instance applies.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::admin_identities::MutationOutcome;
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    config_versions::{ConfigStore, ConfigVersionView, InstanceView, PinError, PinState},
};

pub const ROUTE_CONFIG_VERSIONS: &str = "/api/admin/v1/organizations/{org}/config-versions";
pub const ROUTE_CONFIG_PIN: &str = "/api/admin/v1/organizations/{org}/config-versions/pin";
pub const ROUTE_CONFIG_INSTANCES: &str = "/api/admin/v1/organizations/{org}/config-instances";

const COLLECTION: &str = "config_versions";
const ORDER_SEQUENCE: &str = "-sequence";

#[derive(Debug)]
pub enum ConfigAdminError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    /// Unpinning an organization that is not pinned.
    NotPinned,
    Pagination(PaginationError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for ConfigAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

impl From<PinError> for ConfigAdminError {
    fn from(error: PinError) -> Self {
        match error {
            PinError::VersionNotFound => Self::NotFound,
            PinError::NotPinned => Self::NotPinned,
            PinError::Database(error) => error.into(),
        }
    }
}

/// The organization's pin and what it serves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PinView {
    pub pinned_version_id: Option<Uuid>,
    pub served_version_id: Option<Uuid>,
    pub newest_version_id: Option<Uuid>,
    /// The pin's resource version, which is also the ETag; 0 before the
    /// organization was first pinned.
    pub version: u64,
}

impl PinView {
    fn new(state: PinState, version: u64) -> Self {
        Self {
            pinned_version_id: state.pinned,
            served_version_id: state.served,
            newest_version_id: state.newest,
            version,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinInput {
    pub version_id: Uuid,
}

#[derive(Clone)]
pub struct ConfigAdminService {
    admin: AdminStore,
    store: ConfigStore,
    codec: Option<SignedCursorCodec>,
}

impl ConfigAdminService {
    pub fn new(admin: AdminStore, store: ConfigStore, codec: Option<SignedCursorCodec>) -> Self {
        Self {
            admin,
            store,
            codec,
        }
    }

    async fn authorize_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<(), ConfigAdminError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(
            AdminPermission::CatalogRead,
            AdminScope::Organization(organization_id),
        ) {
            return Err(ConfigAdminError::Forbidden);
        }
        Ok(())
    }

    /// The organization's versions, newest first.
    pub async fn versions(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<ConfigVersionView>, ConfigAdminError> {
        self.authorize_read(actor, organization_id).await?;
        let scope = AdminScope::Organization(organization_id);
        let codec = self
            .codec
            .as_ref()
            .ok_or(ConfigAdminError::AuthenticationUnavailable)?;
        let before = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(token, actor, scope, COLLECTION, "", ORDER_SEQUENCE)
                    .map_err(ConfigAdminError::Pagination)?;
                // Only a validly signed token gets here, so a bad keyset is
                // a server-side inconsistency.
                Some(claims.last_keyset.parse::<i64>().map_err(|_| {
                    ConfigAdminError::Store("config version page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .versions(organization_id, page.page_size, before)
            .await
            .map_err(|error| ConfigAdminError::Store(format!("{error:?}")))?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let claims = CursorClaims::new(
                    actor,
                    scope,
                    COLLECTION,
                    "",
                    ORDER_SEQUENCE,
                    last.sequence.to_string(),
                    None,
                )
                .map_err(ConfigAdminError::Pagination)?;
                Some(
                    codec
                        .encode(&claims)
                        .map_err(ConfigAdminError::Pagination)?,
                )
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Every API-serving instance and the version it serves the
    /// organization.
    pub async fn instances(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<Page<InstanceView>, ConfigAdminError> {
        self.authorize_read(actor, organization_id).await?;
        let items = self
            .store
            .instances(organization_id)
            .await
            .map_err(|error| ConfigAdminError::Store(format!("{error:?}")))?;
        Ok(Page::new(items, None))
    }

    pub async fn pin(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<PinView, ConfigAdminError> {
        self.authorize_read(actor, organization_id).await?;
        // The version first: a pin changed between the reads then fails
        // the caller's `If-Match` rather than passing it with a stale view.
        let version = self.store.pin_version(organization_id).await?;
        let mut conn = self.store.pool().acquire().await?;
        let state = ConfigStore::pin_state(&mut conn, organization_id).await?;
        Ok(PinView::new(state, version))
    }

    /// Pin the organization to `version_id` (`Some`), or unpin it (`None`).
    pub async fn set_pin(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        version_id: Option<Uuid>,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PinView>, ConfigAdminError> {
        let (method, operation_name, action, body) = match version_id {
            Some(version_id) => (
                "PUT",
                "config_pin.set",
                "config_pin.set",
                serde_json::json!({ "version_id": version_id }),
            ),
            None => (
                "DELETE",
                "config_pin.delete",
                "config_pin.deleted",
                serde_json::json!({}),
            ),
        };
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_CONFIG_PIN.into(),
            method: method.into(),
            operation: operation(operation_name),
            idempotency_key,
            // The audit event names the pinned version; an unpin names the
            // organization.
            target_id: version_id.unwrap_or(organization_id.0),
            parent_id: None,
            target: AuditTargetResource::ConfigPin,
            audit_action: audit_action(action),
            if_match,
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
            // The recorded response, even if the pin has since changed.
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: replayed(
                    record
                        .response_metadata
                        .and_then(|metadata| metadata.snapshot),
                )?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                ConfigStore::set_pin(tx, organization_id, version_id).await?;
                let state = ConfigStore::pin_state(tx, organization_id).await?;
                let next = mutation.next_version();
                let pin = PinView::new(state, next.get());
                let snapshot = serde_json::to_value(&pin).map_err(|_| {
                    ConfigAdminError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        previous(next).map(|version| AuditSnapshot {
                            version: Some(version),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            enabled: Some(version_id.is_some()),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: organization_id.0,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: pin,
                    replayed: false,
                })
            }
        }
    }
}

fn replayed<T: serde::de::DeserializeOwned>(
    snapshot: Option<serde_json::Value>,
) -> Result<T, ConfigAdminError> {
    let snapshot = snapshot.ok_or(ConfigAdminError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| ConfigAdminError::Store("recorded response snapshot is invalid".into()))
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

fn map_admin_error(error: AdminStoreError) -> ConfigAdminError {
    match error {
        AdminStoreError::Authorization(_) => ConfigAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => ConfigAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => ConfigAdminError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => ConfigAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => ConfigAdminError::IdempotencyInProgress,
        other => ConfigAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-config-versions-v1.json"))
                .unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_CONFIG_VERSIONS,
            ROUTE_CONFIG_PIN,
            ROUTE_CONFIG_INSTANCES,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }

    #[test]
    fn pin_operations_and_actions_are_valid() {
        for value in ["config_pin.set", "config_pin.delete", "config_pin.deleted"] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }
}
