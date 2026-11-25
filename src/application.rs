use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{path::Path, sync::Arc};
use thiserror::Error;

use crate::{
    config::{AppConfig, OidcConfig},
    domain::auth::{AuthCrypto, OidcBackend, UnavailableOidcBackend},
    persistence::{
        AccessStore, AccountingStore, AdminStore, AuthStore, IdentityStore, ServiceIdentityStore,
        catalog::CatalogStore, migrations,
    },
    provider::{InferenceProviderAdapter, ProviderRegistry, UnavailableProviderAdapter},
};
use provider_resolver::ProviderTemplate;

pub mod accounting;
pub mod admin_identities;
pub mod agents;
pub mod audit;
pub mod auto_offer;
pub mod batch_admin;
pub mod batches;
pub mod budgets;
pub mod catalog;
pub mod catalog_admin;
pub mod catalog_sync;
pub mod catalog_write;
pub(crate) mod circuit_breaker;
pub mod config_cache;
pub mod config_versions;
pub mod credential_keys;
pub mod end_users;
pub mod identity_provider;
pub mod inference;
pub mod invitations;
pub mod model_agreements;
pub mod model_cards;
pub mod model_evaluations;
pub(crate) mod model_evidence;
pub mod model_masters;
pub mod notifications;
pub mod object_storage;
pub mod oidc;
pub mod price_adoption;
pub(crate) mod provider_resolver;
pub(crate) mod provider_retry;
pub mod rate_limits;
pub mod reporting;
pub mod retention;
pub(crate) mod routing;
pub mod routing_policy;
pub(crate) mod settlement_retry;
pub mod trace_capture;
pub mod trace_content;
pub mod trace_objects;
pub mod trace_policy;
pub mod usage_export;
pub mod webhooks;
pub use accounting::{
    AccountedChatRequest, AccountingPolicy, AccountingService, AccountingServiceError,
};

/// Composition state shared by application layers. Transport receives this
/// state but does not know about or access the concrete database pool.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    db: PgPool,
    identity: IdentityStore,
    auth: AuthStore,
    admin: AdminStore,
    access: AccessStore,
    service_identities: ServiceIdentityStore,
    reporting: crate::persistence::reporting::ReportingStore,
    catalog_admin: crate::persistence::catalog_admin::CatalogAdminStore,
    provider_credentials: crate::persistence::provider_credentials::ProviderCredentialStore,
    rate_limits: crate::persistence::rate_limits::RateLimitStore,
    config_versions: crate::persistence::config_versions::ConfigStore,
    budgets: crate::persistence::budgets::BudgetStore,
    end_users: crate::persistence::end_users::EndUserStore,
    retention: crate::persistence::retention::RetentionStore,
    trace_policy: crate::persistence::trace_policy::TracePolicyStore,
    /// Curated model card files: the shipped ones and the operator's.
    model_cards: Arc<crate::domain::model_card_files::CardLibrary>,
    model_card_hub: Option<crate::application::model_cards::HubSource>,
    price_adoption: crate::persistence::price_adoption::PriceAdoptionStore,
    routing_policy: crate::persistence::routing_policy::RoutingPolicyStore,
    notifications: crate::persistence::notifications::NotificationStore,
    trace_content: crate::persistence::trace_content::TraceContentStore,
    /// The trace store, when configured; content reads open objects from it.
    trace_objects: Option<Arc<dyn trace_objects::TraceObjects>>,
    usage_export: crate::persistence::usage_export::UsageExportStore,
    auto_offer: crate::persistence::auto_offer::AutoOfferStore,
    agents: crate::persistence::agents::AgentStore,
    /// Writes to organizations' storage; outside tests, through their
    /// connections with the gateway's cloud identities.
    export_writer: Arc<dyn crate::domain::usage_export::ObjectWriter>,
    webhooks: crate::persistence::webhooks::WebhookStore,
    catalog_sync: crate::persistence::catalog_sync::CatalogSyncStore,
    price_sync: crate::persistence::price_sync::PriceSyncStore,
    identity_providers: crate::persistence::identity_providers::IdentityProviderStore,
    organization_oidc: Arc<identity_provider::OrganizationOidcClients>,
    credential_cipher: Option<crate::domain::credentials::CredentialCipher>,
    onboarding: crate::domain::onboarding::OnboardingIdentities,
    verifier: Option<Arc<dyn crate::domain::onboarding::CredentialProbe>>,
    /// The organizations' provider accounts, as inference reaches them: for
    /// reading and accepting model agreements (D-035). Absent in tests.
    provider_accounts: Option<Arc<dyn crate::jobs::discovery::ProviderAccounts>>,
    auth_crypto: Option<AuthCrypto>,
    oidc: OidcConfig,
    local_accounts: crate::config::LocalAccountsConfig,
    /// The largest inference request body accepted (chat, streams and
    /// embeddings); other routes keep their own, smaller limits.
    inference_max_body_bytes: usize,
    /// This process's budget leases (`GATEWAY_BUDGET_LEASES`), returned
    /// when it stops.
    budget_leases: Option<Arc<crate::persistence::leases::BudgetLeases>>,
    /// This process's rate-limit leases (`GATEWAY_RATE_LIMIT_LEASES`),
    /// returned when it stops.
    rate_limit_leases: Option<Arc<crate::persistence::rate_leases::RateLimitLeases>>,
    sign_in_limiter: Arc<crate::transport::local_accounts::AttemptLimiter>,
    email_domains: crate::persistence::email_domains::EmailDomainStore,
    deployment: crate::persistence::deployment::DeploymentStore,
    /// Looks up domains' verification records.
    txt_lookup: Arc<dyn crate::domain::email_domains::TxtLookup>,
    session_ttl_seconds: u64,
    oidc_backend: Arc<dyn OidcBackend>,
    accounting: accounting::AccountingService,
    inference: inference::InferenceFacade,
    /// The configuration versions inference routes from; `None` in states
    /// built for tests.
    config: Option<config_cache::ConfigCache>,
}

impl AppState {
    #[cfg(test)]
    pub(crate) fn new(
        db: PgPool,
        identity: IdentityStore,
        auth: AuthStore,
        auth_crypto: Option<AuthCrypto>,
        oidc: OidcConfig,
        session_ttl_seconds: u64,
        oidc_backend: Arc<dyn OidcBackend>,
        provider: Arc<dyn InferenceProviderAdapter>,
        accounting_policy: accounting::AccountingPolicy,
    ) -> Self {
        let accounting = accounting::AccountingService::new(
            crate::persistence::AccountingStore::new(db.clone()),
            provider.clone(),
            accounting_policy,
        );
        let inference = inference::InferenceFacade::new(accounting.clone());
        Self::new_with_inference(
            db,
            identity,
            auth,
            auth_crypto,
            oidc,
            session_ttl_seconds,
            oidc_backend,
            accounting,
            inference,
        )
    }

    fn new_with_inference(
        db: PgPool,
        identity: IdentityStore,
        auth: AuthStore,
        auth_crypto: Option<AuthCrypto>,
        oidc: OidcConfig,
        session_ttl_seconds: u64,
        oidc_backend: Arc<dyn OidcBackend>,
        accounting: accounting::AccountingService,
        inference: inference::InferenceFacade,
    ) -> Self {
        Self {
            db: db.clone(),
            access: AccessStore::new(db.clone(), auth_crypto.clone()),
            service_identities: ServiceIdentityStore::new(db.clone(), identity.clone()),
            reporting: crate::persistence::reporting::ReportingStore::new(db.clone()),
            catalog_admin: crate::persistence::catalog_admin::CatalogAdminStore::new(db.clone()),
            provider_credentials:
                crate::persistence::provider_credentials::ProviderCredentialStore::new(db.clone()),
            rate_limits: crate::persistence::rate_limits::RateLimitStore::new(db.clone()),
            config_versions: crate::persistence::config_versions::ConfigStore::new(db.clone()),
            budgets: crate::persistence::budgets::BudgetStore::new(db.clone()),
            end_users: crate::persistence::end_users::EndUserStore::new(db.clone()),
            retention: crate::persistence::retention::RetentionStore::new(db.clone()),
            trace_policy: crate::persistence::trace_policy::TracePolicyStore::new(db.clone()),
            model_cards: Arc::new(
                crate::domain::model_card_files::CardLibrary::builtin()
                    .expect("shipped model card files are valid"),
            ),
            model_card_hub: None,
            price_adoption: crate::persistence::price_adoption::PriceAdoptionStore::new(db.clone()),
            routing_policy: crate::persistence::routing_policy::RoutingPolicyStore::new(db.clone()),
            notifications: crate::persistence::notifications::NotificationStore::new(db.clone()),
            trace_content: crate::persistence::trace_content::TraceContentStore::new(db.clone()),
            trace_objects: None,
            usage_export: crate::persistence::usage_export::UsageExportStore::new(db.clone()),
            auto_offer: crate::persistence::auto_offer::AutoOfferStore::new(db.clone()),
            agents: crate::persistence::agents::AgentStore::new(db.clone()),
            export_writer: Arc::new(object_storage::CloudObjectWriter::new(
                crate::persistence::provider_credentials::ProviderCredentialStore::new(db.clone()),
                None,
                object_storage::ExportIdentities::default(),
            )),
            webhooks: crate::persistence::webhooks::WebhookStore::new(db.clone()),
            catalog_sync: crate::persistence::catalog_sync::CatalogSyncStore::new(db.clone()),
            price_sync: crate::persistence::price_sync::PriceSyncStore::new(db.clone()),
            identity_providers: crate::persistence::identity_providers::IdentityProviderStore::new(
                db.clone(),
            ),
            organization_oidc: Arc::new(identity_provider::OrganizationOidcClients::new()),
            credential_cipher: None,
            onboarding: crate::domain::onboarding::OnboardingIdentities::default(),
            verifier: None,
            provider_accounts: None,
            identity,
            auth,
            admin: AdminStore::new(db.clone()),
            auth_crypto,
            oidc,
            local_accounts: crate::config::LocalAccountsConfig::default(),
            inference_max_body_bytes: crate::config::DEFAULT_INFERENCE_MAX_BODY_BYTES,
            budget_leases: None,
            rate_limit_leases: None,
            sign_in_limiter: Arc::default(),
            email_domains: crate::persistence::email_domains::EmailDomainStore::new(db.clone()),
            deployment: crate::persistence::deployment::DeploymentStore::new(db.clone()),
            txt_lookup: Arc::new(crate::domain::email_domains::SystemResolver::new()),
            session_ttl_seconds,
            oidc_backend,
            accounting,
            inference,
            config: None,
        }
    }

    /// Local accounts as configured; password sessions authenticate only
    /// while they are enabled.
    #[doc(hidden)]
    pub fn with_local_accounts(mut self, local: crate::config::LocalAccountsConfig) -> Self {
        self.local_accounts = local;
        self.auth = self.auth.clone().with_local_accounts(local.enabled);
        self
    }

    /// The largest inference request body accepted; see
    /// `GATEWAY_INFERENCE_MAX_BODY_BYTES`.
    #[doc(hidden)]
    pub fn with_inference_max_body_bytes(mut self, bytes: usize) -> Self {
        self.inference_max_body_bytes = bytes;
        self
    }

    pub fn inference_max_body_bytes(&self) -> usize {
        self.inference_max_body_bytes
    }

    fn with_budget_leases(
        mut self,
        leases: Option<Arc<crate::persistence::leases::BudgetLeases>>,
    ) -> Self {
        self.budget_leases = leases;
        self
    }

    /// This process's budget leases, when `GATEWAY_BUDGET_LEASES` is on.
    pub fn budget_leases(&self) -> Option<Arc<crate::persistence::leases::BudgetLeases>> {
        self.budget_leases.clone()
    }

    fn with_rate_limit_leases(
        mut self,
        leases: Option<Arc<crate::persistence::rate_leases::RateLimitLeases>>,
    ) -> Self {
        self.rate_limit_leases = leases;
        self
    }

    /// This process's rate-limit leases, when `GATEWAY_RATE_LIMIT_LEASES` is
    /// on.
    pub fn rate_limit_leases(
        &self,
    ) -> Option<Arc<crate::persistence::rate_leases::RateLimitLeases>> {
        self.rate_limit_leases.clone()
    }

    pub fn local_accounts_config(&self) -> crate::config::LocalAccountsConfig {
        self.local_accounts
    }

    pub fn sign_in_limiter(&self) -> &crate::transport::local_accounts::AttemptLimiter {
        &self.sign_in_limiter
    }

    pub fn deployment(&self) -> &crate::persistence::deployment::DeploymentStore {
        &self.deployment
    }

    pub fn email_domains(&self) -> &crate::persistence::email_domains::EmailDomainStore {
        &self.email_domains
    }

    pub fn txt_lookup(&self) -> &dyn crate::domain::email_domains::TxtLookup {
        self.txt_lookup.as_ref()
    }

    /// Answers domain verification lookups from `lookup` instead of DNS.
    #[doc(hidden)]
    pub fn with_txt_lookup(
        mut self,
        lookup: Arc<dyn crate::domain::email_domains::TxtLookup>,
    ) -> Self {
        self.txt_lookup = lookup;
        self
    }

    fn with_config(mut self, config: config_cache::ConfigCache) -> Self {
        self.config = Some(config);
        self
    }

    pub fn config(&self) -> Option<&config_cache::ConfigCache> {
        self.config.as_ref()
    }

    fn with_onboarding(
        mut self,
        onboarding: crate::domain::onboarding::OnboardingIdentities,
    ) -> Self {
        self.onboarding = onboarding;
        self
    }

    pub fn onboarding(&self) -> &crate::domain::onboarding::OnboardingIdentities {
        &self.onboarding
    }

    fn with_verifier(
        mut self,
        verifier: Arc<dyn crate::domain::onboarding::CredentialProbe>,
    ) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Checks tenants' credentials before they are stored; absent in tests.
    pub fn verifier(&self) -> Option<&Arc<dyn crate::domain::onboarding::CredentialProbe>> {
        self.verifier.as_ref()
    }

    /// Provider accounts for model agreements (D-035); integration tests
    /// set a fake.
    pub fn with_provider_accounts(
        mut self,
        accounts: Arc<dyn crate::jobs::discovery::ProviderAccounts>,
    ) -> Self {
        self.provider_accounts = Some(accounts);
        self
    }

    pub fn provider_accounts(&self) -> Option<&Arc<dyn crate::jobs::discovery::ProviderAccounts>> {
        self.provider_accounts.as_ref()
    }

    fn with_credential_cipher(
        mut self,
        cipher: Option<crate::domain::credentials::CredentialCipher>,
    ) -> Self {
        self.credential_cipher = cipher;
        self
    }

    pub fn identity(&self) -> &IdentityStore {
        &self.identity
    }
    pub fn invitations(&self) -> crate::persistence::invitations::InvitationStore {
        crate::persistence::invitations::InvitationStore::new(self.db.clone())
    }
    pub fn auth(&self) -> &AuthStore {
        &self.auth
    }
    pub fn admin(&self) -> &AdminStore {
        &self.admin
    }
    pub fn access(&self) -> &AccessStore {
        &self.access
    }
    pub fn service_identities(&self) -> &ServiceIdentityStore {
        &self.service_identities
    }
    pub fn reporting(&self) -> &crate::persistence::reporting::ReportingStore {
        &self.reporting
    }
    pub fn catalog_admin(&self) -> &crate::persistence::catalog_admin::CatalogAdminStore {
        &self.catalog_admin
    }
    pub fn provider_credentials(
        &self,
    ) -> &crate::persistence::provider_credentials::ProviderCredentialStore {
        &self.provider_credentials
    }
    pub fn retention(&self) -> &crate::persistence::retention::RetentionStore {
        &self.retention
    }

    pub fn model_cards(&self) -> &Arc<crate::domain::model_card_files::CardLibrary> {
        &self.model_cards
    }

    /// Model cards from `library` instead of the shipped files alone.
    #[doc(hidden)]
    pub fn model_card_hub(&self) -> Option<&crate::application::model_cards::HubSource> {
        self.model_card_hub.as_ref()
    }

    /// Where Hugging Face model card imports read; `None` turns them off.
    pub fn with_model_card_hub(
        mut self,
        hub: Option<crate::application::model_cards::HubSource>,
    ) -> Self {
        self.model_card_hub = hub;
        self
    }

    pub fn with_model_cards(
        mut self,
        library: Arc<crate::domain::model_card_files::CardLibrary>,
    ) -> Self {
        self.model_cards = library;
        self
    }

    pub fn trace_policy(&self) -> &crate::persistence::trace_policy::TracePolicyStore {
        &self.trace_policy
    }

    pub fn price_adoption(&self) -> &crate::persistence::price_adoption::PriceAdoptionStore {
        &self.price_adoption
    }

    pub fn routing_policy(&self) -> &crate::persistence::routing_policy::RoutingPolicyStore {
        &self.routing_policy
    }

    pub fn notifications(&self) -> &crate::persistence::notifications::NotificationStore {
        &self.notifications
    }

    pub fn trace_content(&self) -> &crate::persistence::trace_content::TraceContentStore {
        &self.trace_content
    }

    pub fn trace_objects(&self) -> Option<&Arc<dyn trace_objects::TraceObjects>> {
        self.trace_objects.as_ref()
    }

    /// The trace store content is read from; for the API and tests.
    #[doc(hidden)]
    pub fn with_trace_objects(
        mut self,
        objects: Option<Arc<dyn trace_objects::TraceObjects>>,
    ) -> Self {
        self.trace_objects = objects;
        self
    }

    pub fn usage_export(&self) -> &crate::persistence::usage_export::UsageExportStore {
        &self.usage_export
    }

    pub fn agents(&self) -> &crate::persistence::agents::AgentStore {
        &self.agents
    }

    pub fn auto_offer(&self) -> &crate::persistence::auto_offer::AutoOfferStore {
        &self.auto_offer
    }

    pub fn export_writer(&self) -> &Arc<dyn crate::domain::usage_export::ObjectWriter> {
        &self.export_writer
    }

    fn with_clickhouse(mut self, clickhouse: Option<crate::clickhouse::ClickHouse>) -> Self {
        if let Some(clickhouse) = clickhouse {
            self.reporting = self.reporting.with_clickhouse(clickhouse);
        }
        self
    }

    fn with_export_writer(
        mut self,
        writer: Arc<dyn crate::domain::usage_export::ObjectWriter>,
    ) -> Self {
        self.export_writer = writer;
        self
    }

    pub fn config_versions(&self) -> &crate::persistence::config_versions::ConfigStore {
        &self.config_versions
    }

    pub fn rate_limits(&self) -> &crate::persistence::rate_limits::RateLimitStore {
        &self.rate_limits
    }

    pub fn budgets(&self) -> &crate::persistence::budgets::BudgetStore {
        &self.budgets
    }
    pub fn end_users(&self) -> &crate::persistence::end_users::EndUserStore {
        &self.end_users
    }
    pub fn webhooks(&self) -> &crate::persistence::webhooks::WebhookStore {
        &self.webhooks
    }
    pub fn catalog_sync(&self) -> &crate::persistence::catalog_sync::CatalogSyncStore {
        &self.catalog_sync
    }
    pub fn price_sync(&self) -> &crate::persistence::price_sync::PriceSyncStore {
        &self.price_sync
    }
    pub fn identity_providers(
        &self,
    ) -> &crate::persistence::identity_providers::IdentityProviderStore {
        &self.identity_providers
    }
    pub fn organization_oidc(&self) -> &identity_provider::OrganizationOidcClients {
        &self.organization_oidc
    }
    pub fn credential_cipher(&self) -> Option<&crate::domain::credentials::CredentialCipher> {
        self.credential_cipher.as_ref()
    }
    pub fn auth_crypto(&self) -> Option<&AuthCrypto> {
        self.auth_crypto.as_ref()
    }
    pub fn oidc(&self) -> &OidcConfig {
        &self.oidc
    }
    pub fn session_ttl_seconds(&self) -> u64 {
        self.session_ttl_seconds
    }
    pub(crate) fn oidc_backend(&self) -> &dyn OidcBackend {
        self.oidc_backend.as_ref()
    }
    pub fn accounting(&self) -> &accounting::AccountingService {
        &self.accounting
    }
    pub fn inference(&self) -> &inference::InferenceFacade {
        &self.inference
    }

    /// Files and batches, routed as inference is and stored in the trace
    /// store's bucket.
    pub fn batches(&self) -> batches::BatchService {
        batches::BatchService::new(
            crate::persistence::batches::BatchStore::new(self.db.clone()),
            self.inference.clone(),
            self.trace_objects.clone(),
        )
    }

    /// A state for integration tests that drive sign-in through the router:
    /// the stores on `db`, the process-wide OIDC settings and `oidc_backend`,
    /// `cipher` for stored secrets, and organizations' providers reached
    /// under `organization_policy`. Inference is unavailable. Never use
    /// outside tests: `DeliveryPolicy::allow_private_targets_for_tests()`
    /// lets an organization's provider be on loopback.
    #[doc(hidden)]
    pub fn for_sign_in_tests(
        db: PgPool,
        pepper: crate::domain::keys::ApiKeyPepper,
        auth_crypto: AuthCrypto,
        oidc: OidcConfig,
        oidc_backend: Arc<dyn OidcBackend>,
        cipher: Option<crate::domain::credentials::CredentialCipher>,
        organization_policy: crate::jobs::egress::DeliveryPolicy,
    ) -> Self {
        let accounting = accounting::AccountingService::new(
            crate::persistence::AccountingStore::new(db.clone()),
            Arc::new(UnavailableProviderAdapter),
            accounting::AccountingPolicy::for_tests(),
        );
        let inference = inference::InferenceFacade::without_models(accounting.clone());
        let mut state = Self::new_with_inference(
            db.clone(),
            IdentityStore::new(db.clone(), pepper),
            AuthStore::new(db),
            Some(auth_crypto),
            oidc,
            3600,
            oidc_backend,
            accounting,
            inference,
        )
        .with_credential_cipher(cipher);
        state.organization_oidc = Arc::new(
            identity_provider::OrganizationOidcClients::with_policy(organization_policy),
        );
        state
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let db = PgPoolOptions::new()
            .connect_lazy("postgres://gateway_app:opaque-test-value@localhost/gateway")
            .expect("valid lazy pool URL");
        let pepper = crate::domain::keys::ApiKeyPepper::from_bytes(b"opaque-test-pepper")
            .expect("non-empty test pepper");
        let crypto = AuthCrypto::new(b"opaque-test-session-hmac-key-32-bytes", &[7_u8; 32]).ok();
        let claims = crate::domain::auth::OidcClaims {
            issuer: "https://test.invalid".into(),
            subject: "test-subject".into(),
            audience: vec!["test".into()],
            nonce: String::new(),
            expires_at: u64::MAX,
            profile: Default::default(),
        };
        Self::new(
            db.clone(),
            IdentityStore::new(db.clone(), pepper),
            AuthStore::new(db),
            crypto,
            OidcConfig {
                enabled: true,
                origin: Some("http://localhost".into()),
                issuer: Some("https://test.invalid".into()),
                audience: Some("test".into()),
                client_id: Some("test".into()),
                redirect_uri: Some("http://localhost/callback".into()),
                microsoft_graph_photo: false,
            },
            3600,
            Arc::new(crate::domain::auth::MockOidcBackend::new(claims)),
            Arc::new(UnavailableProviderAdapter),
            accounting::AccountingPolicy::for_tests(),
        )
    }
}

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("database connection failed: unable to connect using DATABASE_URL")]
    DatabaseConnection,
    #[error("database migration failed: embedded migration execution did not complete")]
    Migration,
    #[error("API_KEY_PEPPER is required for API application state")]
    PepperRequired,
    #[error("OIDC/session secrets required for API application state")]
    AuthSecret,
    #[error("provider configuration is invalid")]
    ProviderConfig,
    #[error("the OIDC client could not be created")]
    OidcBackendUnavailable,
    #[error("catalog operation failed")]
    Catalog,
    /// The message names what failed, never a database detail.
    #[error("configuration versions could not be loaded: {0}")]
    Configuration(&'static str),
    /// A shipped or operator model card file is invalid; the log names it.
    #[error("model card files are invalid: {0}")]
    ModelCards(String),
    /// The message names what failed, never a database detail.
    #[error("configuration pin operation failed: {0}")]
    ConfigPin(&'static str),
    /// The message names what failed, never a database detail.
    #[error("cloud principal operation failed: {0}")]
    CloudPrincipal(&'static str),
    /// The message names what failed, never a database detail.
    #[error("owner bootstrap failed: {0}")]
    Bootstrap(&'static str),
    /// The message names what failed, never a database detail.
    #[error("invitation failed: {0}")]
    Invitation(&'static str),
    /// The message names what failed, never a database detail or a token.
    #[error("agent creation failed: {0}")]
    Agent(&'static str),
    /// The message names what failed, never a database detail or a token.
    #[error("{0}")]
    LocalAccounts(&'static str),
    /// The message names the failing step, never a key or secret.
    #[error("credential keys unavailable: {0}")]
    CredentialKeys(String),
    /// The ClickHouse client could not be built (its TLS settings).
    #[error("the ClickHouse client could not be created")]
    ClickHouse,
}

/// The cipher for stored secrets: the `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`
/// keyring, or with AWS KMS configured a keyring of KMS-wrapped data keys
/// that refreshes itself every minute.
async fn credential_keys(
    config: &AppConfig,
    pool: &PgPool,
) -> Result<
    (
        Option<crate::domain::credentials::CredentialCipher>,
        Option<credential_keys::CredentialKeys>,
    ),
    ApplicationError,
> {
    let Some(kms) = &config.credential_kms else {
        return Ok((config.credential_cipher.clone(), None));
    };
    let keys = credential_keys::CredentialKeys::start(
        crate::persistence::credential_keys::CredentialKeyStore::new(pool.clone()),
        kms,
        config.credential_cipher.clone(),
    )
    .await
    .map_err(|error| ApplicationError::CredentialKeys(error.to_string()))?;
    keys.spawn_refresh();
    Ok((Some(keys.cipher()), Some(keys)))
}

/// Startup checks beyond configuration parsing. With OIDC enabled the API
/// talks to the provider through [`oidc::HttpOidcBackend`]; the provider is
/// contacted on the first sign-in, not at startup, so an unreachable
/// provider never keeps the API down.
pub fn validate_startup(_config: &AppConfig) -> Result<(), ApplicationError> {
    Ok(())
}

fn oidc_backend(config: &AppConfig) -> Result<Arc<dyn OidcBackend>, ApplicationError> {
    if !config.oidc.enabled {
        return Ok(Arc::new(UnavailableOidcBackend));
    }
    oidc::HttpOidcBackend::new(config.oidc_client_secret.clone())
        .map(|backend| {
            if config.oidc.microsoft_graph_photo {
                backend.with_microsoft_graph_photo()
            } else {
                backend
            }
        })
        .map(|backend| Arc::new(backend) as Arc<dyn OidcBackend>)
        .map_err(|_| ApplicationError::OidcBackendUnavailable)
}

pub async fn apply_catalog(
    config: &AppConfig,
    organization: &str,
    file: &Path,
) -> Result<(), ApplicationError> {
    let organization_id = uuid::Uuid::parse_str(organization)
        .map(crate::domain::identity::OrganizationId)
        .map_err(|_| ApplicationError::Catalog)?;
    let service = catalog::CatalogApplication::new(CatalogStore::new(connect_pool(config).await?));
    service
        .apply_file(organization_id, file)
        .await
        .map_err(|_| ApplicationError::Catalog)?;
    Ok(())
}

/// `gateway cloud-principal`: binds, unbinds or lists the cloud principals
/// an organization's credentials may act as. Returns the lines to print.
pub async fn cloud_principal(
    config: &AppConfig,
    action: crate::config::CloudPrincipalAction,
    organization: &str,
    principal: Option<(&str, &str)>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::{
        config::CloudPrincipalAction, domain::credentials::CloudPrincipal,
        persistence::cloud_principals::CloudPrincipalStore,
    };
    let organization_id = uuid::Uuid::parse_str(organization)
        .map(crate::domain::identity::OrganizationId)
        .map_err(|_| ApplicationError::CloudPrincipal("the organization is not a UUID"))?;
    let principal = principal
        .map(|(kind, value)| {
            CloudPrincipal::parse(kind, value)
                .map_err(|_| ApplicationError::CloudPrincipal("the principal is not valid"))
        })
        .transpose()?;
    let store = CloudPrincipalStore::new(connect_pool(config).await?);
    let failed = |_| ApplicationError::CloudPrincipal("the database operation failed");
    Ok(match (action, principal) {
        (CloudPrincipalAction::Allow, Some(principal)) => {
            let created = store
                .allow(organization_id, &principal)
                .await
                .map_err(failed)?;
            vec![if created { "bound" } else { "already bound" }.to_owned()]
        }
        (CloudPrincipalAction::Revoke, Some(principal)) => {
            let removed = store
                .revoke(organization_id, &principal)
                .await
                .map_err(failed)?;
            vec![if removed { "revoked" } else { "not bound" }.to_owned()]
        }
        (CloudPrincipalAction::List, None) => store
            .list(organization_id)
            .await
            .map_err(|_| ApplicationError::CloudPrincipal("the database operation failed"))?
            .into_iter()
            .map(|binding| {
                format!(
                    "{}\t{}\t{}",
                    binding.kind,
                    binding.principal,
                    binding.created_at.to_rfc3339()
                )
            })
            .collect(),
        _ => return Err(ApplicationError::CloudPrincipal("invalid arguments")),
    })
}

/// `gateway config`: pin an organization to a configuration version, unpin
/// it, or list its versions. A pin change is a new version of the pin, so an
/// admin client holding an older `ETag` conflicts. Returns the lines to print.
pub async fn config_pin(
    config: &AppConfig,
    action: crate::config::ConfigAction,
    organization: &str,
    version: Option<&str>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::{
        config::ConfigAction,
        persistence::config_versions::{ConfigStore, PinError},
    };
    let organization_id = uuid::Uuid::parse_str(organization)
        .map(crate::domain::identity::OrganizationId)
        .map_err(|_| ApplicationError::ConfigPin("the organization is not a UUID"))?;
    let version = version
        .map(|version| {
            uuid::Uuid::parse_str(version)
                .map_err(|_| ApplicationError::ConfigPin("the version is not a UUID"))
        })
        .transpose()?;
    let store = ConfigStore::new(connect_pool(config).await?);
    let failed = |_| ApplicationError::ConfigPin("the database operation failed");
    match action {
        ConfigAction::Pin | ConfigAction::Unpin => {
            let mut tx = store.pool().begin().await.map_err(failed)?;
            // The organization, the pin's version row, then the feed lock in
            // `set_pin`: the order an admin API pin takes them in. Without
            // the organization first, the feed entry's key-share lock on it
            // would wait on an API pin that waits on this transaction.
            ConfigStore::lock_organization(&mut tx, organization_id)
                .await
                .map_err(failed)?;
            ConfigStore::bump_pin_version(&mut tx, organization_id)
                .await
                .map_err(failed)?;
            ConfigStore::set_pin(&mut tx, organization_id, version)
                .await
                .map_err(|error| match error {
                    PinError::VersionNotFound => ApplicationError::ConfigPin(
                        "the version does not exist in the organization",
                    ),
                    PinError::NotPinned => {
                        ApplicationError::ConfigPin("the organization is not pinned")
                    }
                    PinError::Database(_) => {
                        ApplicationError::ConfigPin("the database operation failed")
                    }
                })?;
            tx.commit().await.map_err(failed)?;
            Ok(vec![
                match version {
                    Some(_) => "pinned",
                    None => "unpinned",
                }
                .to_owned(),
            ])
        }
        ConfigAction::List => {
            let mut lines = Vec::new();
            let mut before = None;
            loop {
                let (versions, more) = store
                    .versions(organization_id, 100, before)
                    .await
                    .map_err(|_| ApplicationError::ConfigPin("the database operation failed"))?;
                before = versions.last().map(|version| version.sequence);
                lines.extend(versions.into_iter().map(|version| {
                    let mut marks = Vec::new();
                    for (set, mark) in [
                        (version.served, "served"),
                        (version.pinned, "pinned"),
                        (version.newest, "newest"),
                    ] {
                        if set {
                            marks.push(mark);
                        }
                    }
                    format!(
                        "{}\t{}\t{}\t{}",
                        version.id,
                        version.created_at.to_rfc3339(),
                        &version.digest[..12],
                        marks.join(",")
                    )
                }));
                if !more {
                    break;
                }
            }
            Ok(lines)
        }
    }
}

/// `gateway agent create`: an agent in an organization (created with it
/// when missing) and a new token for it, so automation can drive the admin
/// API without a browser. Returns the lines to print; the token is shown
/// once, here.
pub async fn agent_create(
    config: &AppConfig,
    organization: &str,
    organization_name: Option<&str>,
    name: &str,
    role: Option<&str>,
    token_name: Option<&str>,
    expires_in_days: Option<u32>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::{
        domain::{
            agents::{AgentTokenInput, valid_text},
            identity::MembershipRole,
        },
        persistence::agents::{AgentStore, AgentStoreError},
    };
    if !organization_slug_ok(organization) {
        return Err(ApplicationError::Agent(
            "the organization must be a slug: lowercase letters, digits and hyphens",
        ));
    }
    if !valid_text(name, 100) {
        return Err(ApplicationError::Agent(
            "the name must be 1 to 100 bytes without surrounding spaces or control characters",
        ));
    }
    let role = role
        .unwrap_or("owner")
        .parse::<MembershipRole>()
        .map_err(|_| {
            ApplicationError::Agent("the role must be owner, admin, member, auditor or read-only")
        })?;
    let token = AgentTokenInput {
        name: token_name.unwrap_or("cli").to_owned(),
        expires_in_days,
        service_account_id: None,
    };
    token.validate().map_err(ApplicationError::Agent)?;
    let created = AgentStore::new(connect_pool(config).await?)
        .bootstrap(organization, organization_name, name, role, &token)
        .await
        .map_err(|error| match error {
            AgentStoreError::AgentUnavailable => ApplicationError::Agent("the agent is disabled"),
            AgentStoreError::TooManyAgents => {
                ApplicationError::Agent("the organization has the most agents it may have")
            }
            AgentStoreError::TooManyTokens => {
                ApplicationError::Agent("the agent has the most live tokens it may have")
            }
            _ => ApplicationError::Agent("the database operation failed"),
        })?;
    Ok(vec![
        format!(
            "agent {} ({}) in organization {}",
            created.agent_id,
            if created.created_agent {
                "created"
            } else {
                "existing"
            },
            created.organization_id.0
        ),
        format!(
            "token {} {}",
            token.name,
            token
                .expires_in_days
                .map_or("never expires".to_owned(), |days| format!(
                    "expires in {days} days"
                ))
        ),
        created.token.as_str().to_owned(),
    ])
}

/// `gateway invitation`: invitations to an organization, for whoever signs
/// in with the link. Returns the lines to print; a created invitation's
/// link is shown once, here.
pub async fn invitation(
    config: &AppConfig,
    action: crate::config::InvitationAction,
    organization: &str,
    organization_name: Option<&str>,
    role: Option<&str>,
    valid_hours: Option<u32>,
    id: Option<&str>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::{
        config::InvitationAction, domain::identity::MembershipRole,
        persistence::invitations::InvitationStore,
    };
    let failed = |_| ApplicationError::Invitation("the database operation failed");
    let store =
        || async { Ok::<_, ApplicationError>(InvitationStore::new(connect_pool(config).await?)) };
    match action {
        InvitationAction::Create => {
            let links = invitations::InvitationLinks::from_config_or_local(
                &config.oidc,
                config.local_accounts.enabled,
            )
            .map_err(ApplicationError::Invitation)?;
            let role = role
                .unwrap_or("owner")
                .parse::<MembershipRole>()
                .map_err(|_| {
                    ApplicationError::Invitation(
                        "the role must be owner, admin, member, auditor or read-only",
                    )
                })?;
            if !organization_slug_ok(organization) {
                return Err(ApplicationError::Invitation(
                    "the organization must be a slug: lowercase letters, digits and hyphens",
                ));
            }
            let hours = valid_hours.unwrap_or(invitations::DEFAULT_VALID_HOURS);
            let (id, organization_id, token) = store()
                .await?
                .create(
                    organization,
                    organization_name,
                    role,
                    links.issuer(),
                    chrono::Duration::hours(i64::from(hours)),
                    None,
                )
                .await
                .map_err(|_| ApplicationError::Invitation("the database operation failed"))?;
            Ok(vec![
                format!("invitation {id} for organization {}", organization_id.0),
                format!(
                    "role {}, valid for {hours} hours, single use",
                    role.as_str()
                ),
                links.link(&token),
            ])
        }
        InvitationAction::List => Ok(store()
            .await?
            .list(organization)
            .await
            .map_err(|_| ApplicationError::Invitation("the database operation failed"))?
            .into_iter()
            .map(|invitation| {
                format!(
                    "{}\t{}\t{}\tcreated {}\texpires {}",
                    invitation.id,
                    invitation.role,
                    invitation.status(),
                    invitation.created_at.to_rfc3339(),
                    invitation.expires_at.to_rfc3339()
                )
            })
            .collect()),
        InvitationAction::Revoke => {
            let id = id
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .ok_or(ApplicationError::Invitation("the id is not a UUID"))?;
            let revoked = store()
                .await?
                .revoke(organization, id, None)
                .await
                .map_err(failed)?;
            Ok(vec![
                if revoked {
                    "revoked"
                } else {
                    "no pending invitation with that id"
                }
                .to_owned(),
            ])
        }
    }
}

pub(crate) fn organization_slug_ok(slug: &str) -> bool {
    (1..=64).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
}

/// `gateway bootstrap-owner`: an organization's first owner. The issuer must
/// be the configured OIDC issuer's exact URL and the subject the `sub` the
/// provider gives that person for this gateway's client. Returns the lines
/// to print.
pub async fn bootstrap_owner(
    config: &AppConfig,
    organization: &str,
    organization_name: Option<&str>,
    issuer: &str,
    subject: &str,
    display_name: Option<&str>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::persistence::bootstrap::{BootstrapError, BootstrapStore, OwnerBootstrap};
    let slug_ok = (1..=64).contains(&organization.len())
        && organization
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !organization.starts_with('-');
    if !slug_ok {
        return Err(ApplicationError::Bootstrap(
            "the organization must be a slug: lowercase letters, digits and hyphens",
        ));
    }
    let issuer_ok = url::Url::parse(issuer).is_ok_and(|url| {
        url.scheme() == "https" || (url.scheme() == "http" && crate::net::is_loopback_url(&url))
    });
    if !issuer_ok {
        return Err(ApplicationError::Bootstrap(
            "the issuer must be an https URL",
        ));
    }
    if subject.len() > 255 || subject.chars().any(char::is_control) {
        return Err(ApplicationError::Bootstrap("the subject is not valid"));
    }
    // A mismatch would provision someone who can never sign in.
    match config.oidc.issuer.as_deref() {
        Some(configured) if configured == issuer => {}
        Some(_) => {
            return Err(ApplicationError::Bootstrap(
                "the issuer differs from the configured OIDC_ISSUER",
            ));
        }
        None => {
            return Err(ApplicationError::Bootstrap(
                "OIDC_ISSUER must be set to the issuer users sign in with",
            ));
        }
    }
    let store = BootstrapStore::new(connect_pool(config).await?);
    let done = store
        .bootstrap_owner(&OwnerBootstrap {
            organization_slug: organization,
            organization_name,
            oidc_issuer: issuer,
            oidc_subject: subject,
            display_name,
        })
        .await
        .map_err(|error| match error {
            BootstrapError::UserDisabled => {
                ApplicationError::Bootstrap("the user account exists but is disabled")
            }
            BootstrapError::ForeignBinding => ApplicationError::Bootstrap(
                "the user holds an owner binding the admin API did not create",
            ),
            BootstrapError::Store(_) => {
                ApplicationError::Bootstrap("the database operation failed")
            }
        })?;
    let state = |created: bool| if created { "created" } else { "existing" };
    Ok(vec![
        format!(
            "organization {} ({})",
            done.organization_id.0,
            state(done.created_organization)
        ),
        format!("user {} ({})", done.user_id.0, state(done.created_user)),
        if done.granted_owner {
            "organization_owner granted".to_owned()
        } else {
            "organization_owner already held".to_owned()
        },
    ])
}

/// Connect to PostgreSQL and compose application state without exposing the
/// credential-bearing URL to the binary or transport layer.
pub async fn connect(config: &AppConfig) -> Result<AppState, ApplicationError> {
    validate_startup(config)?;
    let pool = connect_pool(config).await?;
    crate::metrics::global().register_pool("api", pool.clone());
    let (cipher, _keys) = credential_keys(config, &pool).await?;
    // Credentials stored before their cloud principals were recorded are
    // refused until recorded; work them out once, here. A data plane writes
    // nothing it does not need to serve; the control plane does this.
    if let Some(cipher) = cipher
        .as_ref()
        .filter(|_| config.profile != crate::config::GatewayProfile::DataPlane)
    {
        match crate::persistence::provider_credentials::ProviderCredentialStore::new(pool.clone())
            .record_principals(cipher)
            .await
        {
            Ok(0) => {}
            Ok(recorded) => {
                tracing::info!(recorded, "recorded stored credentials' cloud principals")
            }
            Err(error) => tracing::warn!(error = %error, "recording cloud principals failed"),
        }
    }
    let pepper = config
        .api_key_pepper()
        .ok_or(ApplicationError::PepperRequired)?
        .clone();
    // Route, model, and price selection is exclusively database catalog data.
    // This policy supplies only bounded request limits and is replaced with
    // the catalog price/provenance before every reservation.
    let accounting_policy = accounting::AccountingPolicy::for_runtime(pepper.as_bytes());
    let identity = IdentityStore::new(pool.clone(), pepper.clone());
    let auth_crypto = match (
        config.sessions.hmac_secret(),
        config.sessions.encryption_key(),
    ) {
        (Some(hmac), Some(encryption)) => {
            Some(AuthCrypto::new(hmac, encryption).map_err(|_| ApplicationError::AuthSecret)?)
        }
        _ => None,
    };
    let templates = provider_templates(config);
    let mut registry = ProviderRegistry::new();
    for (key, template) in &templates {
        registry.insert(
            key.clone(),
            template.family().as_str(),
            template
                .adapter()
                .map_err(|_| ApplicationError::ProviderConfig)?,
        );
    }
    let provider: Arc<dyn InferenceProviderAdapter> = if registry.is_empty() {
        Arc::new(UnavailableProviderAdapter)
    } else {
        Arc::new(registry)
    };
    let objects = trace_objects(config);
    let budget_leases = config.budget_leases.then(|| {
        let leases = crate::persistence::leases::BudgetLeases::new(pool.clone());
        // Returns idle and drained leases; stops with the last reference.
        let ticking = Arc::downgrade(&leases);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                let Some(leases) = ticking.upgrade() else {
                    break;
                };
                leases.tick().await;
            }
        });
        tracing::info!(holder = %leases.holder(), "budget leases on");
        leases
    });
    let rate_limit_leases = config.rate_limit_leases.then(|| {
        let leases = crate::persistence::rate_leases::RateLimitLeases::new(pool.clone());
        // Returns ended leases; stops with the last reference.
        let ticking = Arc::downgrade(&leases);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(crate::persistence::rate_leases::TICK);
            loop {
                interval.tick().await;
                let Some(leases) = ticking.upgrade() else {
                    break;
                };
                leases.tick().await;
            }
        });
        tracing::info!(holder = %leases.holder(), "rate-limit leases on");
        leases
    });
    let accounting_store = match &budget_leases {
        Some(leases) => AccountingStore::new(pool.clone()).with_leases(leases.clone()),
        None => AccountingStore::new(pool.clone()),
    };
    let accounting_store = match &rate_limit_leases {
        Some(leases) => accounting_store.with_rate_leases(leases.clone()),
        None => accounting_store,
    };
    let accounting =
        accounting::AccountingService::new(accounting_store, provider, accounting_policy)
            .with_capture(trace_capture(
                config,
                &pool,
                cipher.clone(),
                objects.clone(),
            ));
    let resolver = provider_resolver::ProviderResolver::new(
        crate::persistence::provider_credentials::ProviderCredentialStore::new(pool.clone()),
        cipher.clone(),
        templates,
    )
    .with_tenant_templates(tenant_templates(config))
    .with_openai_compatible_private_origins(config.openai_compatible_private_origins.clone());
    // Checks a tenant's credential before it is stored, as inference uses it.
    let verifier: Arc<dyn crate::domain::onboarding::CredentialProbe> = Arc::new(resolver.clone());
    let provider_accounts: Arc<dyn crate::jobs::discovery::ProviderAccounts> =
        Arc::new(resolver.clone());
    let config_versions = config_cache::ConfigCache::load(
        crate::persistence::config_versions::ConfigStore::new(pool.clone()),
        config.instance.clone(),
        config.profile.as_str(),
    )
    .await
    .map_err(|error| {
        ApplicationError::Configuration(match error {
            crate::persistence::config_versions::ConfigStoreError::Database(_) => {
                "the configuration feed could not be read"
            }
            crate::persistence::config_versions::ConfigStoreError::Invalid(_) => {
                "a configuration version failed its digest or schema check"
            }
        })
    })?;
    config_versions.spawn_polling();
    let inference = inference::InferenceFacade::from_config(
        accounting.clone(),
        config_versions.clone(),
        resolver,
    )
    .with_evidence(model_evidence::EvidenceRecorder::new(
        crate::persistence::model_evidence::ModelEvidenceStore::new(pool.clone()),
    ))
    .with_route_evidence(routing::RouteEvidence::new(
        crate::persistence::model_evidence::ModelEvidenceStore::new(pool.clone()),
    ));
    let writer = Arc::new(export_writer(config, &pool, cipher.clone()));
    let mut model_cards =
        crate::domain::model_card_files::CardLibrary::load(config.model_cards_dir.as_deref())
            .map_err(|error| ApplicationError::ModelCards(error.to_string()))?;
    // Usage is stamped with the master its deployment was linked to when
    // served, by the database from these matches (D-033, migration 0141).
    // The API owns them: a data plane writes nothing it does not need to
    // serve, and has no say in which cards there are. Written once before
    // serving, then kept written in the background, so a failed write (the
    // migration not applied yet, say) heals on its own.
    if config.profile != crate::config::GatewayProfile::DataPlane {
        let store = crate::persistence::model_masters::ModelMasterStore::new(pool.clone());
        let matches = crate::persistence::model_masters::CardMatches::of(&model_cards);
        let failures = model_masters::sync_card_matches_once(&store, &matches, 0).await;
        model_masters::spawn_card_match_sync(
            store,
            matches,
            model_masters::CardMatchSchedule::default(),
            failures,
        );
    }
    // Which Gemini service the process's own adapter calls, so a card
    // shows the terms that apply to it.
    if let Some(gemini) = &config.gemini {
        model_cards.set_process_service(
            config.adapter_instance_key("GEMINI_ADAPTER_INSTANCE_KEY", "gemini-main"),
            if gemini.agent_platform.is_some() {
                crate::domain::model_card_files::SERVICE_AGENT_PLATFORM
            } else {
                crate::domain::model_card_files::SERVICE_GEMINI_API
            },
        );
    }
    Ok(AppState::new_with_inference(
        pool.clone(),
        identity,
        AuthStore::new(pool),
        auth_crypto,
        config.oidc.clone(),
        config.sessions.ttl_seconds,
        oidc_backend(config)?,
        accounting,
        inference,
    )
    .with_clickhouse(clickhouse(config)?)
    .with_export_writer(writer)
    .with_trace_objects(objects)
    .with_credential_cipher(cipher)
    .with_onboarding(config.onboarding.clone())
    .with_verifier(verifier)
    .with_provider_accounts(provider_accounts)
    .with_config(config_versions)
    .with_local_accounts(config.local_accounts)
    .with_inference_max_body_bytes(config.inference_max_body_bytes)
    .with_budget_leases(budget_leases)
    .with_rate_limit_leases(rate_limit_leases)
    .with_model_cards(Arc::new(model_cards))
    .with_model_card_hub(
        config
            .model_card_hub_url
            .clone()
            .map(crate::application::model_cards::HubSource::new),
    ))
}

/// `gateway setup-link`: a new first-run setup link, while no organization
/// exists. Returns the lines to print.
pub async fn setup_link(config: &AppConfig) -> Result<Vec<String>, ApplicationError> {
    let origin = local_origin(config)?;
    let store =
        crate::persistence::local_accounts::LocalAccountStore::new(connect_pool(config).await?);
    let hours = crate::transport::local_accounts::LINK_VALID_HOURS;
    match store
        .create_setup_link(chrono::Duration::hours(hours))
        .await
        .map_err(|_| ApplicationError::LocalAccounts("the database operation failed"))?
    {
        Some(token) => Ok(vec![
            format!(
                "Open this link within {hours} hours to create the first organization and its owner:"
            ),
            crate::transport::local_accounts::setup_link(&origin, token.as_str()),
        ]),
        None => Err(ApplicationError::LocalAccounts(
            "setup is done: an organization exists. Sign in, or use `gateway user reset-password`",
        )),
    }
}

/// `gateway user reset-password --email <email>`: a single-use link that
/// lets the local account choose a new password. Returns the lines to print.
pub async fn reset_password_link(
    config: &AppConfig,
    email: &str,
) -> Result<Vec<String>, ApplicationError> {
    let origin = local_origin(config)?;
    let email = crate::domain::password::normalize_email(email).ok_or(
        ApplicationError::LocalAccounts("the email address is not valid"),
    )?;
    let store =
        crate::persistence::local_accounts::LocalAccountStore::new(connect_pool(config).await?);
    let failed = |_| ApplicationError::LocalAccounts("the database operation failed");
    let credential =
        store
            .credential(&email)
            .await
            .map_err(failed)?
            .ok_or(ApplicationError::LocalAccounts(
                "no local account has that email address",
            ))?;
    let hours = crate::transport::local_accounts::LINK_VALID_HOURS;
    let token = store
        .create_reset_link(credential.user_id, chrono::Duration::hours(hours), None)
        .await
        .map_err(failed)?;
    Ok(vec![
        format!("Send this single-use link to {email}; it works for {hours} hours:"),
        crate::transport::local_accounts::reset_link(&origin, token.as_str()),
    ])
}

/// `gateway sign-in-policy allow-passwords`: the organization stops
/// requiring its own identity provider, audited as the system's.
pub async fn allow_passwords(
    config: &AppConfig,
    organization: &str,
) -> Result<Vec<String>, ApplicationError> {
    let store =
        crate::persistence::local_accounts::LocalAccountStore::new(connect_pool(config).await?);
    let failed = |_| ApplicationError::LocalAccounts("the database operation failed");
    let organization_id = store
        .organization_by_slug(organization)
        .await
        .map_err(failed)?
        .ok_or(ApplicationError::LocalAccounts(
            "no organization has that slug",
        ))?;
    store
        .set_require_sso(
            organization_id,
            crate::domain::audit::AuditActor::System,
            false,
        )
        .await
        .map_err(failed)?;
    Ok(vec![format!(
        "{organization} no longer requires its identity provider: its members can sign in with passwords."
    )])
}

/// `gateway admin grant`: makes an existing account a deployment
/// administrator. Returns the lines to print.
pub async fn admin_grant(
    config: &AppConfig,
    email: Option<&str>,
    issuer: Option<&str>,
    subject: Option<&str>,
) -> Result<Vec<String>, ApplicationError> {
    use crate::persistence::local_accounts::AdminAccount;
    let normalized = email.map(crate::domain::password::normalize_email);
    let account = match (normalized.as_ref(), issuer, subject) {
        (Some(Some(email)), None, None) => AdminAccount::Email(email),
        (Some(None), _, _) => {
            return Err(ApplicationError::LocalAccounts(
                "the email address is not valid",
            ));
        }
        (None, Some(issuer), Some(subject)) => AdminAccount::Oidc { issuer, subject },
        _ => {
            return Err(ApplicationError::LocalAccounts(
                "name the account with --email, or with --issuer and --subject",
            ));
        }
    };
    let store =
        crate::persistence::local_accounts::LocalAccountStore::new(connect_pool(config).await?);
    match store
        .grant_deployment_admin(account)
        .await
        .map_err(|_| ApplicationError::LocalAccounts("the database operation failed"))?
    {
        Some((user_id, true)) => Ok(vec![format!(
            "User {} now administers the deployment.",
            user_id.0
        )]),
        Some((user_id, false)) => Ok(vec![format!(
            "User {} already administers the deployment.",
            user_id.0
        )]),
        None => Err(ApplicationError::LocalAccounts(
            "no such account; it must have signed in or accepted an invitation first",
        )),
    }
}

/// `GATEWAY_ORIGIN`, which setup and reset links are built on, with local
/// accounts on.
fn local_origin(config: &AppConfig) -> Result<String, ApplicationError> {
    if !config.local_accounts.enabled {
        return Err(ApplicationError::LocalAccounts(
            "local accounts are turned off (GATEWAY_LOCAL_ACCOUNTS=false)",
        ));
    }
    let origin = config
        .oidc
        .origin
        .as_deref()
        .ok_or(ApplicationError::LocalAccounts(
            "GATEWAY_ORIGIN must be set",
        ))?;
    crate::config::validate_origin(origin).map_err(|_| {
        ApplicationError::LocalAccounts("GATEWAY_ORIGIN must be an origin: https://host[:port]")
    })?;
    Ok(origin.trim_end_matches('/').to_owned())
}

/// First-run setup: while no organization exists and local accounts are
/// on, logs a single-use link that creates the first organization and its
/// owner. Each start logs a new link; any unused one works until setup is
/// done. Failing to make one is logged and does not stop the gateway.
pub async fn announce_setup(state: &AppState) {
    if !state.local_accounts_config().enabled || state.auth_crypto().is_none() {
        return;
    }
    let Some(origin) = state.oidc().origin.as_deref() else {
        return;
    };
    let store = state.auth().local_accounts();
    match store
        .create_setup_link(chrono::Duration::hours(
            crate::transport::local_accounts::LINK_VALID_HOURS,
        ))
        .await
    {
        Ok(Some(token)) => tracing::warn!(
            link = %crate::transport::local_accounts::setup_link(origin, token.as_str()),
            "first-run setup: open this link to create the first organization and its owner \
             (valid 24 hours; `gateway setup-link` makes another)"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(error = %error, "the first-run setup link could not be made"),
    }
}

/// Process-configured providers keyed by adapter instance key. Each key's
/// configuration also serves the organization credentials stored for it.
/// The operator price sheets and public price lists the worker fetches.
fn price_sources(config: &AppConfig) -> Vec<Arc<dyn crate::jobs::pricing::PriceSource>> {
    use crate::jobs::{pricing, webhooks::DeliveryPolicy};
    let policy = DeliveryPolicy::public_only();
    let mut sources: Vec<Arc<dyn pricing::PriceSource>> = config
        .price_sheets
        .iter()
        .map(|(family, url)| {
            Arc::new(pricing::PriceSheetSource::new(*family, url.clone(), policy))
                as Arc<dyn pricing::PriceSource>
        })
        .collect();
    if let Some(currencies) = &config.public_prices.azure_retail {
        sources.push(Arc::new(pricing::AzureRetailSource::new(
            url::Url::parse(pricing::AZURE_RETAIL_URL).expect("valid URL"),
            currencies.clone(),
            policy,
        )));
    }
    if config.public_prices.aws_price_list {
        sources.push(Arc::new(pricing::AwsPriceListSource::new(
            url::Url::parse(pricing::AWS_PRICE_LIST_URL).expect("valid URL"),
            policy,
        )));
    }
    sources
}

/// Writes organizations' usage export objects through their connections,
/// with the identities the gateway uses for their providers.
/// What records traced requests: content goes to the configured trace
/// store, sealed under `cipher`; without either, traces keep timing only.
fn trace_capture(
    config: &AppConfig,
    pool: &PgPool,
    cipher: Option<crate::domain::credentials::CredentialCipher>,
    objects: Option<Arc<dyn trace_objects::TraceObjects>>,
) -> trace_capture::TraceCapture {
    trace_capture::TraceCapture::new(
        crate::persistence::trace_policy::TracePolicyStore::new(pool.clone()),
        crate::persistence::trace_content::TraceContentStore::new(pool.clone()),
        objects,
        cipher,
        config.trace_content_max_bytes,
    )
}

/// The configured trace store, if any.
pub(crate) fn trace_objects(config: &AppConfig) -> Option<Arc<dyn trace_objects::TraceObjects>> {
    let store = config.trace_store.as_ref()?;
    let runtime = || {
        crate::aws_credentials::AwsCredentialSource::runtime(
            &|name: &str| std::env::var(name).ok(),
            &store.region,
        )
    };
    let runtime = if store.credentials.is_some() {
        None
    } else {
        runtime()
    };
    match trace_objects::S3TraceObjects::new(store, runtime) {
        Some(objects) => Some(Arc::new(objects)),
        None => {
            tracing::warn!("trace store has no credentials; traced content is not kept");
            None
        }
    }
}

fn export_writer(
    config: &AppConfig,
    pool: &PgPool,
    cipher: Option<crate::domain::credentials::CredentialCipher>,
) -> object_storage::CloudObjectWriter {
    let azure = [&config.azure_openai_for_tenants, &config.azure_openai]
        .into_iter()
        .flatten()
        .find_map(|azure| azure.identity.clone())
        .or_else(|| {
            [
                &config.azure_ai_foundry_for_tenants,
                &config.azure_ai_foundry,
            ]
            .into_iter()
            .flatten()
            .find_map(|foundry| foundry.identity.clone())
        });
    let google = [&config.gemini_for_tenants, &config.gemini]
        .into_iter()
        .flatten()
        .find_map(|gemini| gemini.identity.clone());
    let aws = config
        .bedrock
        .as_ref()
        .map(|bedrock| object_storage::AwsIdentity {
            credentials: bedrock.credentials.clone(),
            sts_region: bedrock.region.clone(),
            sts_endpoint: bedrock.sts_endpoint.clone(),
        });
    object_storage::CloudObjectWriter::new(
        crate::persistence::provider_credentials::ProviderCredentialStore::new(pool.clone()),
        cipher,
        object_storage::ExportIdentities { aws, azure, google },
    )
}

/// Templates for tenants' credentials that need no process configuration
/// of their own.
fn tenant_templates(config: &AppConfig) -> Vec<ProviderTemplate> {
    config
        .azure_openai_for_tenants
        .clone()
        .map(ProviderTemplate::AzureOpenAi)
        .into_iter()
        .chain(
            config
                .azure_ai_foundry_for_tenants
                .clone()
                .map(ProviderTemplate::AzureAiFoundry),
        )
        .chain(
            config
                .gemini_for_tenants
                .clone()
                .map(ProviderTemplate::Gemini),
        )
        // Tenants' roles, assumed with the gateway's own AWS identity.
        .chain(config.bedrock.clone().map(ProviderTemplate::Bedrock))
        .collect()
}

fn provider_templates(config: &AppConfig) -> Vec<(String, ProviderTemplate)> {
    let key = |name: &str, default: &str| config.adapter_instance_key(name, default);
    let mut templates = Vec::new();
    if let Some(openai) = config.openai.clone() {
        templates.push((
            key("OPENAI_ADAPTER_INSTANCE_KEY", "openai-main"),
            ProviderTemplate::OpenAi(openai),
        ));
    }
    if let Some(anthropic) = config.anthropic.clone() {
        templates.push((
            key("ANTHROPIC_ADAPTER_INSTANCE_KEY", "anthropic-main"),
            ProviderTemplate::Anthropic(anthropic),
        ));
    }
    if let Some(gemini) = config.gemini.clone() {
        templates.push((
            key("GEMINI_ADAPTER_INSTANCE_KEY", "gemini-main"),
            ProviderTemplate::Gemini(gemini),
        ));
    }
    if let Some(bedrock) = config.bedrock.clone() {
        templates.push((
            key("BEDROCK_ADAPTER_INSTANCE_KEY", "bedrock-main"),
            ProviderTemplate::Bedrock(bedrock),
        ));
    }
    if let Some(azure_openai) = config.azure_openai.clone() {
        templates.push((
            key("AZURE_OPENAI_ADAPTER_INSTANCE_KEY", "azure-openai-main"),
            ProviderTemplate::AzureOpenAi(azure_openai),
        ));
    }
    if let Some(azure_ai_foundry) = config.azure_ai_foundry.clone() {
        templates.push((
            key(
                "AZURE_AI_FOUNDRY_ADAPTER_INSTANCE_KEY",
                "azure-ai-foundry-main",
            ),
            ProviderTemplate::AzureAiFoundry(azure_ai_foundry),
        ));
    }
    if let Some(openai_compatible) = config.openai_compatible.clone() {
        templates.push((
            key(
                "OPENAI_COMPATIBLE_ADAPTER_INSTANCE_KEY",
                "openai-compatible-main",
            ),
            ProviderTemplate::OpenAiCompatible(openai_compatible),
        ));
    }
    templates
}

/// Run embedded migrations and return without starting an API or worker role.
pub async fn migrate(config: &AppConfig) -> Result<(), ApplicationError> {
    let pool = connect_pool(config).await?;
    migrations::run(&pool)
        .await
        .map_err(|_| ApplicationError::Migration)
}

/// The ClickHouse client, when the analytics sink is configured.
fn clickhouse(
    config: &AppConfig,
) -> Result<Option<crate::clickhouse::ClickHouse>, ApplicationError> {
    config
        .clickhouse
        .as_ref()
        .map(|settings| {
            crate::clickhouse::ClickHouse::new(settings).map_err(|_| ApplicationError::ClickHouse)
        })
        .transpose()
}

/// Connects everything the background worker drives.
pub async fn connect_worker(config: &AppConfig) -> Result<crate::jobs::Worker, ApplicationError> {
    let pool = connect_pool(config).await?;
    crate::metrics::global().register_pool("worker", pool.clone());
    let (cipher, keys) = credential_keys(config, &pool).await?;
    let webhooks = crate::persistence::webhooks::WebhookStore::new(pool.clone());
    let accounts = provider_resolver::ProviderResolver::new(
        crate::persistence::provider_credentials::ProviderCredentialStore::new(pool.clone()),
        cipher.clone(),
        provider_templates(config),
    )
    .with_tenant_templates(tenant_templates(config))
    .with_openai_compatible_private_origins(config.openai_compatible_private_origins.clone());
    let accounts = Arc::new(accounts);
    let objects = trace_objects(config);
    let analytics_sink = crate::jobs::analytics_sink::AnalyticsSinkRunner::new(
        crate::persistence::analytics_sink::AnalyticsSinkStore::new(pool.clone()),
        clickhouse(config)?,
    );
    Ok(crate::jobs::Worker {
        analytics_sink,
        batch_files: crate::persistence::batches::BatchStore::new(pool.clone()),
        credential_keys: keys,
        batches: crate::jobs::batches::BatchRunner::new(
            crate::persistence::batches::BatchStore::new(pool.clone()),
            AccountingStore::new(pool.clone()),
            objects.clone(),
            accounts.clone(),
        ),
        backlog: crate::persistence::backlog::BacklogStore::new(pool.clone()),
        retention: crate::persistence::retention::RetentionStore::new(pool.clone()),
        accounting: AccountingStore::new(pool.clone()),
        outbox: crate::persistence::OutboxStore::new(pool.clone()),
        config: crate::persistence::config_versions::ConfigStore::new(pool.clone()),
        trace_content: crate::persistence::trace_content::TraceContentStore::new(pool.clone()),
        trace_objects: objects,
        agreements: crate::jobs::agreements::AgreementRunner::new(pool.clone(), accounts.clone()),
        discovery: crate::jobs::discovery::DiscoveryRunner::new(
            crate::persistence::catalog_sync::CatalogSyncStore::new(pool.clone()),
            accounts,
        )
        .with_auto_offers(crate::persistence::auto_offer::AutoOfferStore::new(
            pool.clone(),
        ))
        .with_evidence(crate::persistence::model_evidence::ModelEvidenceStore::new(
            pool.clone(),
        ))
        .with_notifications(),
        pricing: crate::jobs::pricing::PricingRunner::new(
            crate::persistence::price_sync::PriceSyncStore::new(pool.clone()),
            price_sources(config),
        )
        .with_notifications(),
        adoption: crate::persistence::price_adoption::PriceAdoptionStore::new(pool.clone()),
        notifications: crate::persistence::notifications::NotificationStore::new(pool.clone()),
        conditions: crate::persistence::notification_conditions::NotificationConditions::new(
            pool.clone(),
        ),
        usage_export: crate::jobs::usage_export::UsageExportRunner::new(
            crate::persistence::usage_export::UsageExportStore::new(pool.clone()),
            Arc::new(export_writer(config, &pool, cipher.clone())),
        ),
        deliverer: crate::jobs::webhooks::WebhookDeliverer::new(
            webhooks.clone(),
            cipher,
            crate::jobs::webhooks::DeliveryPolicy::public_only(),
        ),
        webhooks,
    })
}

async fn connect_pool(config: &AppConfig) -> Result<PgPool, ApplicationError> {
    PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(config.database_url())
        .await
        .map_err(|_| ApplicationError::DatabaseConnection)
}
