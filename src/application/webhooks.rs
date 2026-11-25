// Webhook endpoint admin application service: list, create, read, update,
// delete and rotate the signing secret of an organization's webhook
// endpoints, list an endpoint's deliveries, and redeliver a parked one.
//
// Reads need `webhook_read`; writes need `webhook_write` (owner and admin
// only). Writes run through the admin mutation coordinator, so each is
// idempotent, versioned, and audited in the same transaction as the change.
// The signing secret is generated here, sealed with the credential key, and
// returned only in the response that created it; a replay of that request
// reports `secret_unavailable_on_replay` instead.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditStatus, AuditTargetResource},
    credentials::{CredentialCipher, SealedCredential},
    identity::{OrganizationId, UserAccountId},
    keys::ApiKeyReplayError,
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    version::{IfMatch, VersionConflict},
    webhook::{
        MAX_ENDPOINTS_PER_ORGANIZATION, WebhookDeliveryView, WebhookEndpointInput,
        WebhookEndpointUpdate, WebhookEndpointView, WebhookError, WebhookSecret,
    },
};
use crate::persistence::{
    admin::{AdminMutation, AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    webhooks::WebhookStore,
};

pub const ROUTE_WEBHOOK_ENDPOINTS: &str = "/api/admin/v1/organizations/{org}/webhook-endpoints";
pub const ROUTE_WEBHOOK_ENDPOINT: &str =
    "/api/admin/v1/organizations/{org}/webhook-endpoints/{endpoint_id}";
pub const ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET: &str =
    "/api/admin/v1/organizations/{org}/webhook-endpoints/{endpoint_id}/rotate-secret";
pub const ROUTE_WEBHOOK_DELIVERIES: &str =
    "/api/admin/v1/organizations/{org}/webhook-endpoints/{endpoint_id}/deliveries";
pub const ROUTE_WEBHOOK_DELIVERY_REDELIVER: &str = "/api/admin/v1/organizations/{org}/webhook-endpoints/{endpoint_id}/deliveries/{delivery_id}/redeliver";

const ENDPOINT_COLLECTION: &str = "webhook_endpoints";
const DELIVERY_COLLECTION: &str = "webhook_deliveries";
const ORDER_ID: &str = "id";
const ORDER_CREATED_DESC: &str = "created_at_desc,id_desc";

#[derive(Debug)]
pub enum WebhookAdminError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    /// No credential key is configured, so secrets cannot be sealed.
    EncryptionUnavailable,
    Forbidden,
    NotFound,
    Invalid(WebhookError),
    LimitReached,
    /// Only a parked delivery can be redelivered.
    NotParked,
    Pagination(PaginationError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for WebhookAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

/// The response to create and rotate-secret: the endpoint and, only on the
/// original response, its new signing secret.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct WebhookSecretResponse {
    pub endpoint: WebhookEndpointView,
    pub secret: Option<String>,
    pub replay: bool,
    pub error: Option<ApiKeyReplayError>,
}

impl std::fmt::Debug for WebhookSecretResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSecretResponse")
            .field("endpoint", &self.endpoint)
            .field("secret", &self.secret.as_ref().map(|_| "REDACTED"))
            .field("replay", &self.replay)
            .field("error", &self.error)
            .finish()
    }
}

impl WebhookSecretResponse {
    fn issued(endpoint: WebhookEndpointView, secret: &WebhookSecret) -> Self {
        Self {
            endpoint,
            secret: Some(secret.expose().to_owned()),
            replay: false,
            error: None,
        }
    }

    fn replayed(endpoint: WebhookEndpointView) -> Self {
        Self {
            endpoint,
            secret: None,
            replay: true,
            error: Some(ApiKeyReplayError::SecretUnavailableOnReplay),
        }
    }
}

#[derive(Clone)]
pub struct WebhookAdminService {
    admin: AdminStore,
    store: WebhookStore,
    cipher: Option<CredentialCipher>,
    codec: Option<SignedCursorCodec>,
}

impl WebhookAdminService {
    pub fn new(
        admin: AdminStore,
        store: WebhookStore,
        cipher: Option<CredentialCipher>,
        codec: Option<SignedCursorCodec>,
    ) -> Self {
        Self {
            admin,
            store,
            cipher,
            codec,
        }
    }

    /// Every endpoint of the organization, by id.
    pub async fn list(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<WebhookEndpointView>, WebhookAdminError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(token, actor, scope, ENDPOINT_COLLECTION, "", ORDER_ID)
                    .map_err(WebhookAdminError::Pagination)?;
                // Only a validly signed token gets here, so a bad keyset is
                // a server-side inconsistency.
                Some(Uuid::parse_str(&claims.last_keyset).map_err(|_| {
                    WebhookAdminError::Store("webhook page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list(organization_id, page.page_size, after)
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode_token(
                codec,
                CursorClaims::new(
                    actor,
                    scope,
                    ENDPOINT_COLLECTION,
                    "",
                    ORDER_ID,
                    last.id.to_string(),
                    None,
                ),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn get(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
    ) -> Result<WebhookEndpointView, WebhookAdminError> {
        self.authorize_read(actor, organization_id).await?;
        self.store
            .get(organization_id, endpoint_id)
            .await?
            .ok_or(WebhookAdminError::NotFound)
    }

    /// One endpoint's deliveries, newest first.
    pub async fn list_deliveries(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        page: &PageRequest,
    ) -> Result<Page<WebhookDeliveryView>, WebhookAdminError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        // Tokens are bound to the endpoint they page through.
        let filters = format!("endpoint_id={endpoint_id}");
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        DELIVERY_COLLECTION,
                        &filters,
                        ORDER_CREATED_DESC,
                    )
                    .map_err(WebhookAdminError::Pagination)?;
                Some(decode_delivery_keyset(&claims.last_keyset).ok_or_else(|| {
                    WebhookAdminError::Store("delivery page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list_deliveries(organization_id, endpoint_id, page.page_size, after)
            .await?
            .ok_or(WebhookAdminError::NotFound)?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode_token(
                codec,
                CursorClaims::new(
                    actor,
                    scope,
                    DELIVERY_COLLECTION,
                    filters,
                    ORDER_CREATED_DESC,
                    encode_delivery_keyset(last.created_at, last.id),
                    None,
                ),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Creates an endpoint with a new signing secret, returned only here.
    pub async fn create(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: WebhookEndpointInput,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<WebhookSecretResponse>, WebhookAdminError> {
        let endpoint = input.validate().map_err(WebhookAdminError::Invalid)?;
        let cipher = self.cipher()?;
        let target_id = deterministic_target_id(&[
            "webhook_endpoint.create",
            &organization_id.0.to_string(),
            &actor.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::WebhookWrite,
            route: ROUTE_WEBHOOK_ENDPOINTS.into(),
            method: "POST".into(),
            operation: operation("webhook_endpoint.create"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::WebhookEndpoint,
            audit_action: audit_action("webhook_endpoint.created"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({
                "url": endpoint.url,
                "description": endpoint.description,
                "event_types": endpoint.event_types,
                "enabled": endpoint.enabled,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: WebhookSecretResponse::replayed(replayed(
                    record
                        .response_metadata
                        .and_then(|metadata| metadata.snapshot),
                )?),
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim. The coordinator holds the organization
                // row lock, so concurrent creates count one at a time.
                let tx = mutation.transaction();
                if WebhookStore::count(tx, organization_id).await? >= MAX_ENDPOINTS_PER_ORGANIZATION
                {
                    return Err(WebhookAdminError::LimitReached);
                }
                let secret = WebhookSecret::generate();
                let sealed = seal(cipher, organization_id, target_id, &secret)?;
                WebhookStore::insert(tx, organization_id, target_id, &endpoint, &sealed, actor)
                    .await?;
                let view = self
                    .committed_view(&mut mutation, organization_id, target_id)
                    .await?;
                let next = view.version;
                mutation
                    .commit(
                        None,
                        Some(snapshot(view.enabled, next)),
                        Some(metadata(target_id, next, true, Some(&view))?),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: WebhookSecretResponse::issued(view, &secret),
                    replayed: false,
                })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        input: WebhookEndpointUpdate,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<WebhookEndpointView>, WebhookAdminError> {
        let update = input.validate().map_err(WebhookAdminError::Invalid)?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::WebhookWrite,
            route: ROUTE_WEBHOOK_ENDPOINT.into(),
            method: "PATCH".into(),
            operation: operation("webhook_endpoint.update"),
            idempotency_key,
            target_id: endpoint_id,
            parent_id: None,
            target: AuditTargetResource::WebhookEndpoint,
            audit_action: audit_action("webhook_endpoint.updated"),
            if_match,
            request_id,
            body: serde_json::json!({
                "url": update.url,
                "description": update.description,
                "event_types": update.event_types,
                "enabled": update.enabled,
            }),
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
                let before = WebhookStore::view(tx, organization_id, endpoint_id)
                    .await?
                    .ok_or(WebhookAdminError::NotFound)?;
                WebhookStore::update(tx, organization_id, endpoint_id, &update).await?;
                let view = self
                    .committed_view(&mut mutation, organization_id, endpoint_id)
                    .await?;
                let next = view.version;
                mutation
                    .commit(
                        Some(snapshot(before.enabled, before.version)),
                        Some(snapshot(view.enabled, next)),
                        Some(metadata(endpoint_id, next, false, Some(&view))?),
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

    /// Replaces the signing secret. The old secret stops signing at once,
    /// including for deliveries already pending.
    pub async fn rotate_secret(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<WebhookSecretResponse>, WebhookAdminError> {
        let cipher = self.cipher()?;
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::WebhookWrite,
            route: ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET.into(),
            method: "POST".into(),
            operation: operation("webhook_endpoint.rotate_secret"),
            idempotency_key,
            target_id: endpoint_id,
            parent_id: None,
            target: AuditTargetResource::WebhookEndpoint,
            audit_action: audit_action("webhook_endpoint.secret_rotated"),
            if_match,
            request_id,
            body: serde_json::json!({}),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => Ok(MutationOutcome {
                item: WebhookSecretResponse::replayed(replayed(
                    record
                        .response_metadata
                        .and_then(|metadata| metadata.snapshot),
                )?),
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let before = WebhookStore::view(tx, organization_id, endpoint_id)
                    .await?
                    .ok_or(WebhookAdminError::NotFound)?;
                let secret = WebhookSecret::generate();
                let sealed = seal(cipher, organization_id, endpoint_id, &secret)?;
                WebhookStore::set_secret(tx, organization_id, endpoint_id, &sealed).await?;
                let view = self
                    .committed_view(&mut mutation, organization_id, endpoint_id)
                    .await?;
                let next = view.version;
                mutation
                    .commit(
                        Some(snapshot(before.enabled, before.version)),
                        Some(snapshot(view.enabled, next)),
                        Some(metadata(endpoint_id, next, true, Some(&view))?),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: WebhookSecretResponse::issued(view, &secret),
                    replayed: false,
                })
            }
        }
    }

    /// Deletes the endpoint and its deliveries, pending ones included.
    pub async fn delete(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, WebhookAdminError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::WebhookWrite,
            route: ROUTE_WEBHOOK_ENDPOINT.into(),
            method: "DELETE".into(),
            operation: operation("webhook_endpoint.delete"),
            idempotency_key,
            target_id: endpoint_id,
            parent_id: None,
            target: AuditTargetResource::WebhookEndpoint,
            audit_action: audit_action("webhook_endpoint.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({}),
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
                let before = WebhookStore::view(tx, organization_id, endpoint_id)
                    .await?
                    .ok_or(WebhookAdminError::NotFound)?;
                WebhookStore::delete(tx, organization_id, endpoint_id).await?;
                let next = mutation.next_version();
                mutation
                    .commit(
                        Some(snapshot(before.enabled, before.version)),
                        None,
                        Some(metadata(endpoint_id, next.get(), false, None)?),
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

    /// Returns a parked delivery to the queue for a fresh run of attempts.
    /// It is sent once the endpoint is enabled.
    pub async fn redeliver(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        delivery_id: Uuid,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<WebhookDeliveryView>, WebhookAdminError> {
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::WebhookWrite,
            route: ROUTE_WEBHOOK_DELIVERY_REDELIVER.into(),
            method: "POST".into(),
            operation: operation("webhook_delivery.redeliver"),
            idempotency_key,
            target_id: delivery_id,
            parent_id: Some(endpoint_id),
            target: AuditTargetResource::WebhookDelivery,
            audit_action: audit_action("webhook_delivery.redelivered"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "endpoint_id": endpoint_id }),
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
                // The coordinator locked the delivery, so the worker cannot
                // claim or sweep it until this commits.
                let delivery = WebhookStore::redeliver(
                    mutation.transaction(),
                    organization_id,
                    endpoint_id,
                    delivery_id,
                )
                .await?
                .ok_or(WebhookAdminError::NotParked)?;
                let next = mutation.next_version().get();
                let snapshot = serde_json::to_value(&delivery).map_err(|_| {
                    WebhookAdminError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        // Before its first redelivery a delivery has no version.
                        Some(delivery_snapshot(
                            AuditStatus::Failed,
                            (next > 1).then(|| next - 1),
                        )),
                        Some(delivery_snapshot(AuditStatus::Pending, Some(next))),
                        Some(IdempotencyResultMetadata {
                            resource_id: delivery_id,
                            version: Some(next),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: delivery,
                    replayed: false,
                })
            }
        }
    }

    /// The endpoint as this mutation leaves it, at the version its commit
    /// writes, which is also the ETag.
    async fn committed_view(
        &self,
        mutation: &mut AdminMutation<'_>,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
    ) -> Result<WebhookEndpointView, WebhookAdminError> {
        let stored = WebhookStore::view(mutation.transaction(), organization_id, endpoint_id)
            .await?
            .ok_or(WebhookAdminError::NotFound)?;
        Ok(WebhookEndpointView {
            version: mutation.next_version().get(),
            ..stored
        })
    }

    async fn authorize_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminScope, WebhookAdminError> {
        let scope = AdminScope::Organization(organization_id);
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(AdminPermission::WebhookRead, scope) {
            return Err(WebhookAdminError::Forbidden);
        }
        Ok(scope)
    }

    fn codec(&self) -> Result<&SignedCursorCodec, WebhookAdminError> {
        self.codec
            .as_ref()
            .ok_or(WebhookAdminError::AuthenticationUnavailable)
    }

    fn cipher(&self) -> Result<&CredentialCipher, WebhookAdminError> {
        self.cipher
            .as_ref()
            .ok_or(WebhookAdminError::EncryptionUnavailable)
    }
}

fn seal(
    cipher: &CredentialCipher,
    organization_id: OrganizationId,
    endpoint_id: Uuid,
    secret: &WebhookSecret,
) -> Result<SealedCredential, WebhookAdminError> {
    // Key and cipher failures carry no secret material.
    cipher
        .seal_webhook_secret(organization_id, endpoint_id, &secret.key_bytes())
        .map_err(|error| WebhookAdminError::Store(error.to_string()))
}

fn encode_token(
    codec: &SignedCursorCodec,
    claims: Result<CursorClaims, PaginationError>,
) -> Result<String, WebhookAdminError> {
    claims
        .and_then(|claims| codec.encode(&claims))
        .map_err(WebhookAdminError::Pagination)
}

fn encode_delivery_keyset(created_at: DateTime<Utc>, id: Uuid) -> String {
    format!("{}|{id}", created_at.to_rfc3339())
}

fn decode_delivery_keyset(value: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let (created_at, id) = value.split_once('|')?;
    Some((
        DateTime::parse_from_rfc3339(created_at)
            .ok()?
            .with_timezone(&Utc),
        Uuid::parse_str(id).ok()?,
    ))
}

fn snapshot(enabled: bool, version: u64) -> AuditSnapshot {
    AuditSnapshot {
        enabled: Some(enabled),
        version: Some(version),
        ..Default::default()
    }
}

/// A parked delivery is audited as failed and a queued one as pending.
fn delivery_snapshot(status: AuditStatus, version: Option<u64>) -> AuditSnapshot {
    AuditSnapshot {
        status: Some(status),
        version,
        ..Default::default()
    }
}

fn metadata(
    resource_id: Uuid,
    version: u64,
    secret_issued: bool,
    view: Option<&WebhookEndpointView>,
) -> Result<IdempotencyResultMetadata, WebhookAdminError> {
    let snapshot = view
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| WebhookAdminError::Store("response snapshot is not serializable".into()))?;
    Ok(IdempotencyResultMetadata {
        resource_id,
        version: Some(version),
        secret_unavailable_on_replay: secret_issued,
        snapshot,
    })
}

fn replayed<T: serde::de::DeserializeOwned>(
    snapshot: Option<serde_json::Value>,
) -> Result<T, WebhookAdminError> {
    let snapshot = snapshot.ok_or(WebhookAdminError::NotFound)?;
    serde_json::from_value(snapshot)
        .map_err(|_| WebhookAdminError::Store("recorded response snapshot is invalid".into()))
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> WebhookAdminError {
    match error {
        AdminStoreError::Authorization(_) => WebhookAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => WebhookAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => WebhookAdminError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => WebhookAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => WebhookAdminError::IdempotencyInProgress,
        other => WebhookAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding},
        identity::TeamId,
    };

    #[test]
    fn webhook_writes_are_owner_and_admin_only_and_reads_are_org_wide() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, read, write) in [
            (AdminRole::OrganizationOwner, true, true),
            (AdminRole::OrganizationAdmin, true, true),
            (AdminRole::Operator, true, false),
            (AdminRole::Auditor, true, false),
            (AdminRole::ReadOnlyMember, true, false),
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![AdminRoleBinding::organization(org, user, role)],
            };
            assert_eq!(
                context.allows(AdminPermission::WebhookRead, scope),
                read,
                "{role:?}"
            );
            assert_eq!(
                context.allows(AdminPermission::WebhookWrite, scope),
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
        assert!(!team_admin.allows(AdminPermission::WebhookRead, scope));
        assert!(!team_admin.allows(AdminPermission::WebhookWrite, scope));
    }

    #[test]
    fn delivery_keysets_round_trip() {
        let created_at = Utc::now();
        let id = Uuid::new_v4();
        assert_eq!(
            decode_delivery_keyset(&encode_delivery_keyset(created_at, id)),
            Some((created_at, id))
        );
        assert_eq!(decode_delivery_keyset("not-a-keyset"), None);
    }

    #[test]
    fn secret_responses_redact_the_secret_in_debug() {
        let endpoint = WebhookEndpointView {
            id: Uuid::new_v4(),
            url: "https://hooks.example.com/in".into(),
            description: None,
            event_types: vec![],
            enabled: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            version: 1,
        };
        let secret = WebhookSecret::generate();
        let response = WebhookSecretResponse::issued(endpoint.clone(), &secret);
        assert!(!format!("{response:?}").contains(secret.expose()));
        let replay = WebhookSecretResponse::replayed(endpoint);
        assert!(replay.secret.is_none());
        assert_eq!(
            replay.error,
            Some(ApiKeyReplayError::SecretUnavailableOnReplay)
        );
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-webhooks-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_WEBHOOK_ENDPOINTS,
            ROUTE_WEBHOOK_ENDPOINT,
            ROUTE_WEBHOOK_ENDPOINT_ROTATE_SECRET,
            ROUTE_WEBHOOK_DELIVERIES,
            ROUTE_WEBHOOK_DELIVERY_REDELIVER,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
