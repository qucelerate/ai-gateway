// Identity-provider application service: an organization's own OIDC
// provider, read, saved as a pending configuration, activated by a test
// sign-in, and disabled.
//
// Reading needs `identity_provider_read` (owner, admin, auditor); every
// change needs `identity_provider_write` (owner), organization-wide only.
// Writes run through the admin mutation coordinator, so each is idempotent,
// versioned and audited (`identity_provider.created`, `.updated`,
// `.activated`, `.disabled`) in the same transaction as the change.
//
// The client secret is sealed with the credential cipher before it is
// stored. It never reaches a response, an audit event or an idempotency
// record: the request fingerprint uses a keyed digest of it, and a replay
// returns the recorded view, which has no secret to begin with.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::application::oidc::HttpOidcBackend;
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditStatus, AuditTargetResource},
    auth::OidcBackend,
    credentials::CredentialCipher,
    identity::{OrganizationId, UserAccountId},
    identity_provider::{
        ConfigurationView, IdentityProviderError, IdentityProviderInput, IdentityProviderView,
        IssuerRule, require_secret,
    },
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::jobs::egress::DeliveryPolicy;
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    identity_providers::{IdentityProviderStore, ProviderState, StoredConfiguration},
};

pub const ROUTE_IDENTITY_PROVIDER: &str = "/api/admin/v1/organizations/{org}/identity-provider";
pub const ROUTE_IDENTITY_PROVIDER_TEST: &str =
    "/api/admin/v1/organizations/{org}/identity-provider/test";

const RESOURCE_TYPE: &str = "identity_provider";

#[derive(Debug)]
pub enum IdentityProviderAdminError {
    Forbidden,
    NotFound,
    Invalid(IdentityProviderError),
    /// No credential-encryption key: secrets can be neither sealed nor
    /// opened.
    EncryptionUnavailable,
    /// There is no pending configuration to test.
    NothingToTest,
    /// The pending configuration was saved by another owner, who must test
    /// it themselves.
    TestedByAnotherOwner,
    /// The configuration changed after the test started.
    Changed,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for IdentityProviderAdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

/// A save's result: the provider as it now stands, and whether this save
/// created it.
#[derive(Clone, Debug)]
pub struct IdentityProviderWrite {
    pub provider: IdentityProviderView,
    pub created: bool,
}

/// What a test sign-in starts from: the pending configuration and the
/// resource version its activation must still find.
#[derive(Debug)]
pub struct TestStart {
    pub configuration: StoredConfiguration,
    pub version: u64,
}

/// The test sign-in that just came back, to activate what it tested.
#[derive(Clone, Copy, Debug)]
pub struct CompletedTest {
    pub actor: UserAccountId,
    pub organization_id: OrganizationId,
    pub configuration_id: Uuid,
    pub expected_version: u64,
    pub login_transaction_id: Uuid,
    pub claim_id: Uuid,
    pub request_id: Uuid,
}

#[derive(Clone)]
pub struct IdentityProviderService {
    admin: AdminStore,
    store: IdentityProviderStore,
    cipher: Option<CredentialCipher>,
    /// The gateway's own `OIDC_ISSUER`, which an organization may not reuse.
    process_issuer: Option<String>,
    /// `OIDC_REDIRECT_URI`: the one callback every provider returns to.
    redirect_uri: Option<String>,
    /// Follows the outbound policy of organizations' providers: loopback
    /// issuers only where that policy admits loopback (tests).
    issuer_rule: IssuerRule,
}

/// The provider's admin resource id: one per organization.
pub fn resource_id(organization_id: OrganizationId) -> Uuid {
    deterministic_target_id(&[RESOURCE_TYPE, &organization_id.0.to_string()])
}

impl IdentityProviderService {
    pub fn new(
        admin: AdminStore,
        store: IdentityProviderStore,
        cipher: Option<CredentialCipher>,
        process_issuer: Option<String>,
        redirect_uri: Option<String>,
    ) -> Self {
        Self {
            admin,
            store,
            cipher,
            process_issuer,
            redirect_uri,
            issuer_rule: IssuerRule::PublicHttps,
        }
    }

    /// Also accepts a loopback test provider's issuer. Only for a gateway
    /// whose organization providers are reached under the tests' outbound
    /// policy.
    pub fn with_issuer_rule(mut self, rule: IssuerRule) -> Self {
        self.issuer_rule = rule;
        self
    }

    /// Whether `actor` holds `permission` in the organization. Someone with
    /// no role there at all is told it is not found, so the answer does not
    /// reveal whether the organization exists or has a provider; a member
    /// without the permission is forbidden.
    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), IdentityProviderAdminError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else if context.bindings.is_empty() {
            Err(IdentityProviderAdminError::NotFound)
        } else {
            Err(IdentityProviderAdminError::Forbidden)
        }
    }

    /// The organization's provider, without its secret.
    pub async fn get(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<IdentityProviderView, IdentityProviderAdminError> {
        self.authorize(
            actor,
            organization_id,
            AdminPermission::IdentityProviderRead,
        )
        .await?;
        // One snapshot: a save in between must not pair the old state with
        // the new ETag.
        let (state, version) = self
            .store
            .state_with_version(organization_id, resource_id(organization_id))
            .await?;
        Ok(self.view(state.as_ref(), version))
    }

    fn view(&self, state: Option<&ProviderState>, version: u64) -> IdentityProviderView {
        let configuration = |stored: &StoredConfiguration| ConfigurationView {
            configuration_version: u64::try_from(stored.version).unwrap_or_default(),
            issuer: stored.issuer.clone(),
            client_id: stored.client_id.clone(),
            client_secret_set: !stored.secret_ciphertext.is_empty(),
            extra_scopes: stored.extra_scopes.clone(),
            created_at: stored.created_at,
            created_by: stored.created_by,
            tested_at: stored.tested_at,
        };
        let active = state.and_then(|state| state.active.as_ref());
        let pending = state.and_then(|state| state.pending.as_ref());
        let enabled = state.is_some_and(|state| state.pointers.enabled);
        IdentityProviderView {
            status: IdentityProviderView::status_of(state.is_some(), enabled, pending.is_some())
                .to_owned(),
            enabled,
            active: active.map(configuration),
            pending: pending.map(configuration),
            last_tested_at: active.and_then(|active| active.tested_at),
            last_tested_by: active.and_then(|active| active.tested_by),
            redirect_uri: self.redirect_uri.clone(),
            version,
        }
    }

    /// Saves a new pending configuration. Sign-in keeps using the active one
    /// (or the process-wide provider) until the pending one passes its test.
    pub async fn put(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: IdentityProviderInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<IdentityProviderWrite>, IdentityProviderAdminError> {
        // Checked again, under lock, by the mutation coordinator.
        self.authorize(
            actor,
            organization_id,
            AdminPermission::IdentityProviderWrite,
        )
        .await?;
        let input = input
            .validate_with(self.process_issuer.as_deref(), self.issuer_rule)
            .map_err(IdentityProviderAdminError::Invalid)?;
        let cipher = self
            .cipher
            .as_ref()
            .ok_or(IdentityProviderAdminError::EncryptionUnavailable)?;
        // A keyed digest: a retry with another secret conflicts, and the
        // idempotency record cannot confirm a guessed secret.
        let secret_digest = cipher
            .identity_provider_fingerprint(
                organization_id,
                input.client_secret.as_ref().map(|secret| secret.as_str()),
            )
            .map_err(|_| IdentityProviderAdminError::EncryptionUnavailable)?;
        let target_id = resource_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::IdentityProviderWrite,
            route: ROUTE_IDENTITY_PROVIDER.into(),
            method: "PUT".into(),
            operation: operation("identity_provider.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::IdentityProvider,
            audit_action: audit_action("identity_provider.updated"),
            if_match,
            request_id,
            body: serde_json::json!({
                "issuer": input.issuer,
                "client_id": input.client_id,
                "client_secret_digest": secret_digest,
                "extra_scopes": input.extra_scopes,
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
                item: IdentityProviderWrite {
                    provider: replayed(record.response_metadata)?,
                    created: false,
                },
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim and rolls everything back.
                let next = mutation.next_version();
                let tx = mutation.transaction();
                let before = IdentityProviderStore::state_for_update(tx, organization_id).await?;
                // `*` only creates: an existing provider is changed only by
                // someone who saw its current version.
                if before.is_some() && !matches!(if_match, IfMatch::Exact(_)) {
                    return Err(IdentityProviderAdminError::VersionConflict(
                        VersionConflict {
                            expected: None,
                            current: ResourceVersion::new(next.get().saturating_sub(1))
                                .unwrap_or(ResourceVersion::INITIAL),
                        },
                    ));
                }
                // The configuration a missing secret would come from: the
                // latest saved one.
                let latest = before
                    .as_ref()
                    .and_then(|state| state.pending.as_ref().or(state.active.as_ref()));
                require_secret(
                    &input,
                    latest.map(|stored| (stored.issuer.as_str(), stored.client_id.as_str())),
                )
                .map_err(IdentityProviderAdminError::Invalid)?;
                let configuration_id = Uuid::new_v4();
                let secret = match (&input.client_secret, latest) {
                    (Some(secret), _) => Zeroizing::new(secret.as_str().to_owned()),
                    // Same issuer and client (checked above): reseal the
                    // stored secret for the new configuration.
                    (None, Some(stored)) => cipher
                        .open_identity_provider_secret(
                            organization_id,
                            stored.id,
                            &stored.secret_key_id,
                            &stored.secret_ciphertext,
                        )
                        .map_err(|_| IdentityProviderAdminError::EncryptionUnavailable)?,
                    (None, None) => {
                        return Err(IdentityProviderAdminError::Invalid(
                            IdentityProviderError::ClientSecretRequired,
                        ));
                    }
                };
                let sealed = cipher
                    .seal_identity_provider_secret(organization_id, configuration_id, &secret)
                    .map_err(|_| IdentityProviderAdminError::EncryptionUnavailable)?;
                drop(secret);
                IdentityProviderStore::insert_configuration(
                    tx,
                    organization_id,
                    configuration_id,
                    &input.issuer,
                    &input.client_id,
                    &sealed,
                    &input.extra_scopes,
                    actor,
                )
                .await?;
                let created = IdentityProviderStore::set_pending(
                    tx,
                    organization_id,
                    target_id,
                    configuration_id,
                )
                .await?;
                let after = IdentityProviderStore::state_for_update(tx, organization_id)
                    .await?
                    .ok_or(IdentityProviderAdminError::NotFound)?;
                if created {
                    mutation.set_audit_action(audit_action("identity_provider.created"));
                }
                let provider = self.view(Some(&after), next.get());
                let response = metadata(target_id, next.get(), &provider)?;
                mutation
                    .commit(
                        before.as_ref().map(|state| snapshot(state, next.get() - 1)),
                        Some(snapshot(&after, next.get())),
                        Some(response),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: IdentityProviderWrite { provider, created },
                    replayed: false,
                })
            }
        }
    }

    /// Disables the provider. Sign-in to the organization falls back to the
    /// process-wide provider, the provider's sessions end, and a pending
    /// configuration is dropped. Nothing else is deleted.
    pub async fn disable(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<IdentityProviderView>, IdentityProviderAdminError> {
        // Checked again, under lock, by the mutation coordinator.
        self.authorize(
            actor,
            organization_id,
            AdminPermission::IdentityProviderWrite,
        )
        .await?;
        let target_id = resource_id(organization_id);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::IdentityProviderWrite,
            route: ROUTE_IDENTITY_PROVIDER.into(),
            method: "DELETE".into(),
            operation: operation("identity_provider.disable"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::IdentityProvider,
            audit_action: audit_action("identity_provider.disabled"),
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
                item: replayed(record.response_metadata)?,
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                let next = mutation.next_version();
                let tx = mutation.transaction();
                let before = IdentityProviderStore::state_for_update(tx, organization_id)
                    .await?
                    .ok_or(IdentityProviderAdminError::NotFound)?;
                IdentityProviderStore::disable(tx, organization_id).await?;
                let after = IdentityProviderStore::state_for_update(tx, organization_id)
                    .await?
                    .ok_or(IdentityProviderAdminError::NotFound)?;
                let provider = self.view(Some(&after), next.get());
                let response = metadata(target_id, next.get(), &provider)?;
                mutation
                    .commit(
                        Some(snapshot(&before, next.get() - 1)),
                        Some(snapshot(&after, next.get())),
                        Some(response),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: provider,
                    replayed: false,
                })
            }
        }
    }

    /// Checks that `actor` may test the pending configuration now: they hold
    /// `identity_provider_write` and saved it themselves.
    pub async fn begin_test(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<TestStart, IdentityProviderAdminError> {
        self.authorize(
            actor,
            organization_id,
            AdminPermission::IdentityProviderWrite,
        )
        .await?;
        if self.cipher.is_none() {
            return Err(IdentityProviderAdminError::EncryptionUnavailable);
        }
        // The pending configuration and the version it was saved at, from
        // one snapshot: activation must find both unchanged.
        let (state, version) = self
            .store
            .state_with_version(organization_id, resource_id(organization_id))
            .await?;
        let configuration = state
            .and_then(|state| state.pending)
            .ok_or(IdentityProviderAdminError::NothingToTest)?;
        if configuration.created_by != Some(actor.0) {
            return Err(IdentityProviderAdminError::TestedByAnotherOwner);
        }
        if version == 0 {
            return Err(IdentityProviderAdminError::NotFound);
        }
        Ok(TestStart {
            configuration,
            version,
        })
    }

    /// Activates the configuration a test sign-in just proved, as the owner
    /// who started it: still authorized, at the version they saved, with the
    /// tested configuration still pending. The test's login transaction is
    /// completed in the same transaction.
    pub async fn activate(
        &self,
        test: CompletedTest,
    ) -> Result<IdentityProviderView, IdentityProviderAdminError> {
        let organization_id = test.organization_id;
        let target_id = resource_id(organization_id);
        let expected = ResourceVersion::new(test.expected_version)
            .ok_or(IdentityProviderAdminError::Changed)?;
        let idempotency_key =
            AdminIdempotencyKey::new(format!("provider-test:{}", test.login_transaction_id))
                .map_err(|_| IdentityProviderAdminError::Changed)?;
        let request = AdminMutationRequest {
            actor_id: test.actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::IdentityProviderWrite,
            route: ROUTE_IDENTITY_PROVIDER_TEST.into(),
            method: "GET".into(),
            operation: operation("identity_provider.activate"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::IdentityProvider,
            audit_action: audit_action("identity_provider.activated"),
            if_match: IfMatch::Exact(expected),
            request_id: test.request_id,
            body: serde_json::json!({ "configuration_id": test.configuration_id }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // Each test has its own key, so a replay means it already ran.
            MutationStart::Replay(_) => Err(IdentityProviderAdminError::Changed),
            MutationStart::New(mut mutation) => {
                let next = mutation.next_version();
                let tx = mutation.transaction();
                let before = IdentityProviderStore::state_for_update(tx, organization_id)
                    .await?
                    .ok_or(IdentityProviderAdminError::Changed)?;
                if !IdentityProviderStore::complete_test_login(
                    tx,
                    test.login_transaction_id,
                    test.claim_id,
                )
                .await?
                    || !IdentityProviderStore::activate(
                        tx,
                        organization_id,
                        test.configuration_id,
                        test.actor,
                    )
                    .await?
                {
                    return Err(IdentityProviderAdminError::Changed);
                }
                let after = IdentityProviderStore::state_for_update(tx, organization_id)
                    .await?
                    .ok_or(IdentityProviderAdminError::Changed)?;
                let provider = self.view(Some(&after), next.get());
                let response = metadata(target_id, next.get(), &provider)?;
                mutation
                    .commit(
                        Some(snapshot(&before, next.get() - 1)),
                        Some(snapshot(&after, next.get())),
                        Some(response),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(provider)
            }
        }
    }
}

/// OIDC clients for organizations' providers, one per configuration (its
/// discovery document and keys are cached with it). A configuration never
/// changes, so a cached client is never stale; at most [`MAX_CLIENTS`] are
/// kept.
pub struct OrganizationOidcClients {
    policy: DeliveryPolicy,
    clients: Mutex<HashMap<Uuid, Arc<HttpOidcBackend>>>,
}

const MAX_CLIENTS: usize = 256;

impl OrganizationOidcClients {
    /// Public https providers only. The only policy for real requests.
    pub fn new() -> Self {
        Self::with_policy(DeliveryPolicy::public_only())
    }

    /// `DeliveryPolicy::allow_private_targets_for_tests()` lets tests reach
    /// a provider on loopback. Never use it outside tests.
    pub fn with_policy(policy: DeliveryPolicy) -> Self {
        Self {
            policy,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The issuers these clients may be pointed at.
    pub fn issuer_rule(&self) -> IssuerRule {
        if self.policy.allows_private_targets() {
            IssuerRule::AlsoLoopbackForTests
        } else {
            IssuerRule::PublicHttps
        }
    }

    /// The client for `configuration`, opening its secret only to create
    /// one.
    pub async fn client(
        &self,
        configuration: &StoredConfiguration,
        cipher: &CredentialCipher,
    ) -> Result<Arc<dyn OidcBackend>, IdentityProviderAdminError> {
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(&configuration.id) {
            return Ok(client.clone());
        }
        let secret = cipher
            .open_identity_provider_secret(
                OrganizationId(configuration.organization_id),
                configuration.id,
                &configuration.secret_key_id,
                &configuration.secret_ciphertext,
            )
            .map_err(|_| IdentityProviderAdminError::EncryptionUnavailable)?;
        let client = Arc::new(
            HttpOidcBackend::for_organization(secret, self.policy)
                .map_err(|_| IdentityProviderAdminError::EncryptionUnavailable)?,
        );
        if clients.len() >= MAX_CLIENTS {
            clients.clear();
        }
        clients.insert(configuration.id, client.clone());
        Ok(client)
    }
}

impl Default for OrganizationOidcClients {
    fn default() -> Self {
        Self::new()
    }
}

fn snapshot(state: &ProviderState, version: u64) -> AuditSnapshot {
    let status = match (state.pointers.enabled, state.pointers.active.is_some()) {
        (true, _) => AuditStatus::Active,
        (false, true) => AuditStatus::Disabled,
        (false, false) => AuditStatus::Pending,
    };
    AuditSnapshot {
        enabled: Some(state.pointers.enabled),
        status: Some(status),
        version: (version > 0).then_some(version),
        ..Default::default()
    }
}

fn metadata(
    resource_id: Uuid,
    version: u64,
    provider: &IdentityProviderView,
) -> Result<IdempotencyResultMetadata, IdentityProviderAdminError> {
    Ok(IdempotencyResultMetadata {
        resource_id,
        version: Some(version),
        secret_unavailable_on_replay: false,
        snapshot: Some(serde_json::to_value(provider).map_err(|_| {
            IdentityProviderAdminError::Store("response snapshot is not serializable".into())
        })?),
    })
}

fn replayed(
    metadata: Option<IdempotencyResultMetadata>,
) -> Result<IdentityProviderView, IdentityProviderAdminError> {
    let snapshot = metadata
        .and_then(|metadata| metadata.snapshot)
        .ok_or(IdentityProviderAdminError::NotFound)?;
    serde_json::from_value(snapshot).map_err(|_| {
        IdentityProviderAdminError::Store("recorded response snapshot is invalid".into())
    })
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("static identity provider operation is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("static identity provider audit action is valid")
}

fn map_admin_error(error: AdminStoreError) -> IdentityProviderAdminError {
    match error {
        AdminStoreError::Authorization(_) => IdentityProviderAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => IdentityProviderAdminError::NotFound,
        AdminStoreError::VersionConflict(conflict) => {
            IdentityProviderAdminError::VersionConflict(conflict)
        }
        AdminStoreError::IdempotencyConflict => IdentityProviderAdminError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => IdentityProviderAdminError::IdempotencyInProgress,
        other => IdentityProviderAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn owners_write_admins_and_auditors_read_members_see_nothing() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (role, read, write) in [
            (AdminRole::OrganizationOwner, true, true),
            (AdminRole::OrganizationAdmin, true, false),
            (AdminRole::Auditor, true, false),
            (AdminRole::Operator, false, false),
            (AdminRole::ReadOnlyMember, false, false),
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![AdminRoleBinding::organization(org, user, role)],
            };
            assert_eq!(
                context.allows(AdminPermission::IdentityProviderRead, scope),
                read,
                "{role:?} read"
            );
            assert_eq!(
                context.allows(AdminPermission::IdentityProviderWrite, scope),
                write,
                "{role:?} write"
            );
        }
        // Never through a team binding.
        let team = crate::domain::identity::TeamId::new();
        let team_admin = AdminAuthorizationContext {
            actor_id: user,
            organization_id: org,
            bindings: vec![AdminRoleBinding::team(
                org,
                team,
                user,
                AdminRole::TeamAdmin,
            )],
        };
        for permission in [
            AdminPermission::IdentityProviderRead,
            AdminPermission::IdentityProviderWrite,
        ] {
            assert!(!team_admin.allows(
                permission,
                AdminScope::Team {
                    organization_id: org,
                    team_id: team
                }
            ));
        }
    }

    #[tokio::test]
    async fn openapi_contract_documents_every_route_and_field() {
        let contract: serde_json::Value = serde_json::from_str(include_str!(
            "../../openapi/admin-identity-provider-v1.json"
        ))
        .unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![ROUTE_IDENTITY_PROVIDER, ROUTE_IDENTITY_PROVIDER_TEST];
        routes.sort_unstable();
        assert_eq!(documented, routes);

        // The documented view is exactly what is serialized, and has no
        // secret in it.
        let service = IdentityProviderService::new(
            AdminStore::new(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://unused@localhost/unused")
                    .unwrap(),
            ),
            IdentityProviderStore::new(
                sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://unused@localhost/unused")
                    .unwrap(),
            ),
            None,
            None,
            Some("https://gateway.example/api/admin/v1/oidc/callback".into()),
        );
        let view = serde_json::to_value(service.view(None, 0)).unwrap();
        let mut fields: Vec<&str> = view
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        let schema = &contract["components"]["schemas"]["IdentityProvider"];
        let mut documented: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        assert_eq!(fields, documented);
        let configuration = &contract["components"]["schemas"]["Configuration"]["properties"];
        assert!(configuration.get("client_secret").is_none());
        assert_eq!(view["status"], "not_configured");
    }

    #[test]
    fn the_resource_id_is_stable_per_organization() {
        let org = OrganizationId::new();
        assert_eq!(resource_id(org), resource_id(org));
        assert_ne!(resource_id(org), resource_id(OrganizationId::new()));
    }
}
