//! Per-organization provider credentials on the inference path.
//!
//! A credential stored through the admin catalog API overrides the process
//! credential for its adapter instance key. Everything except the secret
//! (endpoint, API version, timeouts, response limit) still comes from process
//! configuration for that key. A key with no process configuration can use a
//! stored credential only when its family has a fixed public endpoint.
//!
//! A stored credential that cannot be used fails the request. Falling back to
//! the process credential would bill the gateway operator's account for a
//! request the organization meant to pay for.
//!
//! Adapters are cached per organization and key. Every write seals with a
//! fresh nonce, so an unchanged ciphertext means an unchanged credential, and
//! a rotation or delete takes effect on the next request. A cached adapter
//! holds a decrypted secret, so the cache is bounded in size and drops entries
//! that go unused, and a lookup that finds the credential gone or unusable
//! drops its entry at once.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    config::{
        AgentPlatform, AnthropicConfig, AzureAiFoundryConfig, AzureAuth, AzureOpenAiConfig,
        BedrockConfig, GEMINI_API_URL, GeminiAuth, GeminiConfig, OpenAiCompatibleConfig,
        OpenAiConfig,
    },
    domain::{
        catalog::ProviderFamily,
        credentials::{CredentialBinding, CredentialCipher, ProviderSecret},
        identity::OrganizationId,
        inference::{InferenceContext, ProviderError, ProviderErrorCategory},
    },
    gcp_credentials::CLOUD_PLATFORM_SCOPE,
    jobs::egress::{DeliveryPolicy, EgressError, pinned_client},
    persistence::provider_credentials::ProviderCredentialStore,
    provider::{
        InferenceProviderAdapter, anthropic::AnthropicAdapter,
        azure_ai_foundry::AzureAiFoundryAdapter, azure_openai::AzureOpenAiAdapter,
        bedrock::BedrockAdapter, gemini::GeminiAdapter, openai::OpenAiAdapter,
        openai_compatible::OpenAiCompatibleAdapter,
    },
};

/// Process configuration for one adapter instance key.
#[derive(Clone)]
pub(crate) enum ProviderTemplate {
    OpenAi(OpenAiConfig),
    Anthropic(AnthropicConfig),
    Gemini(GeminiConfig),
    Bedrock(BedrockConfig),
    AzureOpenAi(AzureOpenAiConfig),
    AzureAiFoundry(AzureAiFoundryConfig),
    OpenAiCompatible(OpenAiCompatibleConfig),
}

impl ProviderTemplate {
    pub(crate) fn family(&self) -> ProviderFamily {
        match self {
            Self::OpenAi(_) => ProviderFamily::OpenAi,
            Self::Anthropic(_) => ProviderFamily::Anthropic,
            Self::Gemini(_) => ProviderFamily::Gemini,
            Self::Bedrock(_) => ProviderFamily::Bedrock,
            Self::AzureOpenAi(_) => ProviderFamily::AzureOpenAi,
            Self::AzureAiFoundry(_) => ProviderFamily::AzureAiFoundry,
            Self::OpenAiCompatible(_) => ProviderFamily::OpenAiCompatible,
        }
    }

    pub(crate) fn adapter(&self) -> Result<Arc<dyn InferenceProviderAdapter>, ProviderError> {
        Ok(match self {
            Self::OpenAi(config) => Arc::new(OpenAiAdapter::new(config)?),
            Self::Anthropic(config) => Arc::new(AnthropicAdapter::new(config)?),
            Self::Gemini(config) => Arc::new(GeminiAdapter::new(config)?),
            Self::Bedrock(config) => Arc::new(BedrockAdapter::new(config)?),
            Self::AzureOpenAi(config) => Arc::new(AzureOpenAiAdapter::new(config)?),
            Self::AzureAiFoundry(config) => Arc::new(AzureAiFoundryAdapter::new(config)?),
            Self::OpenAiCompatible(config) => Arc::new(OpenAiCompatibleAdapter::new(config)?),
        })
    }

    /// The same configuration authenticated with `secret`, or `None` when the
    /// secret is the wrong kind for the family. A Bedrock role is assumed by
    /// the process's own AWS identity, in a session named for `organization`;
    /// an Entra directory is signed in to by its Entra identity; a Google
    /// service account is impersonated by its Google identity.
    pub(crate) fn with_secret(
        &self,
        secret: &ProviderSecret,
        organization: OrganizationId,
    ) -> Option<Self> {
        let mut template = self.clone();
        match (&mut template, secret) {
            (Self::OpenAi(config), ProviderSecret::ApiKey { api_key }) => {
                config.api_key = api_key.to_string();
            }
            (Self::Anthropic(config), ProviderSecret::ApiKey { api_key }) => {
                config.api_key = api_key.to_string();
            }
            // Keys are for the Gemini API: a process on Agent Platform hands
            // a tenant's key to the Gemini API's endpoint.
            (Self::Gemini(config), ProviderSecret::ApiKey { api_key }) => {
                if config.agent_platform.take().is_some() {
                    config.base_url = GEMINI_API_URL.to_owned();
                }
                config.auth = GeminiAuth::ApiKey(api_key.to_string());
            }
            // On Agent Platform: the Gemini API admits no new service
            // accounts. In the credential's project, else the account's own,
            // and its location, else the process's.
            (
                Self::Gemini(config),
                ProviderSecret::GoogleServiceAccount {
                    service_account,
                    project_id,
                    location,
                },
            ) => {
                let token = config
                    .identity
                    .as_ref()?
                    .impersonate(service_account, CLOUD_PLATFORM_SCOPE)?;
                let project = project_id
                    .as_deref()
                    .or_else(|| crate::gcp_credentials::service_account_project(service_account))?;
                let location = location
                    .as_deref()
                    .or(config.agent_platform_location.as_deref())
                    .unwrap_or("global");
                let platform = AgentPlatform::new(project, location)?;
                config.base_url = platform.base_url();
                config.agent_platform = Some(platform);
                config.auth = GeminiAuth::Google {
                    token,
                    quota_project: None,
                };
            }
            // A tenant's credential never lists the process's resource:
            // a key names none, so discovery is unsupported.
            (Self::AzureOpenAi(config), ProviderSecret::ApiKey { api_key }) => {
                config.auth = AzureAuth::ApiKey(api_key.to_string());
                config.management = None;
            }
            (Self::AzureAiFoundry(config), ProviderSecret::ApiKey { api_key }) => {
                config.auth = AzureAuth::ApiKey(api_key.to_string());
                config.management = None;
            }
            (
                Self::AzureOpenAi(AzureOpenAiConfig {
                    base_url,
                    auth,
                    identity,
                    management,
                    ..
                })
                | Self::AzureAiFoundry(AzureAiFoundryConfig {
                    base_url,
                    auth,
                    identity,
                    management,
                    ..
                }),
                ProviderSecret::AzureEntra {
                    tenant_id,
                    client_id,
                    endpoint,
                    resource_id,
                },
            ) => {
                let identity = identity.as_ref()?;
                *auth = AzureAuth::Entra(identity.in_tenant(
                    tenant_id,
                    client_id.as_deref(),
                    identity.cognitive_services_scope(),
                )?);
                // The tenant's own resource in Resource Manager, read in its
                // directory; never the process's.
                *management = match resource_id {
                    Some(resource_id) => Some(crate::config::AzureManagement {
                        endpoint: identity.management_endpoint().to_owned(),
                        resource_id: resource_id.clone(),
                        token: identity.in_tenant(
                            tenant_id,
                            client_id.as_deref(),
                            identity.management_scope(),
                        )?,
                    }),
                    None => None,
                };
                if let Some(endpoint) = endpoint {
                    // A token from another cloud is for another audience.
                    if !identity.serves(endpoint) {
                        return None;
                    }
                    base_url.clone_from(endpoint);
                }
            }
            (Self::OpenAiCompatible(config), ProviderSecret::ApiKey { api_key }) => {
                config.api_key = Some(api_key.to_string());
            }
            (
                Self::Bedrock(config),
                ProviderSecret::AwsSigV4 {
                    access_key_id,
                    secret_access_key,
                    session_token,
                },
            ) => {
                config.credentials = crate::aws_credentials::AwsCredentialSource::fixed(
                    crate::aws::AwsCredentials {
                        access_key_id: access_key_id.to_string(),
                        secret_access_key: secret_access_key.clone(),
                        session_token: session_token.clone(),
                    },
                );
            }
            (Self::Bedrock(config), ProviderSecret::AwsAssumeRole { role_arn }) => {
                config.credentials = config.credentials.assume_role(
                    role_arn.clone(),
                    // The organization's, never the credential's: a role
                    // whose trust policy requires it trusts only this
                    // organization.
                    Some(crate::domain::credentials::aws_external_id(organization)),
                    // CloudTrail in the tenant's account shows which
                    // organization the gateway acted for.
                    crate::domain::credentials::aws_role_session_name(organization),
                    &config.region,
                    config.sts_endpoint.as_deref(),
                )?;
            }
            _ => return None,
        }
        Some(template)
    }

    /// Default configuration for a credential with no process configuration.
    /// Bedrock needs a region and the Azure and OpenAI-compatible families a
    /// per-resource endpoint, which a stored credential does not carry; only
    /// an Entra directory names its own Azure resource. A tenant's role is
    /// assumed with the gateway's Bedrock settings, whatever its account is
    /// called.
    fn with_default_endpoint(
        family: ProviderFamily,
        secret: &ProviderSecret,
        organization: OrganizationId,
        for_tenants: &[ProviderTemplate],
    ) -> Option<Self> {
        if matches!(
            secret,
            ProviderSecret::AzureEntra {
                endpoint: Some(_),
                ..
            } | ProviderSecret::GoogleServiceAccount { .. }
                | ProviderSecret::AwsAssumeRole { .. }
        ) {
            return for_tenants
                .iter()
                .find(|template| template.family() == family)?
                .with_secret(secret, organization);
        }
        let ProviderSecret::ApiKey { api_key } = secret else {
            return None;
        };
        match family {
            ProviderFamily::OpenAi => OpenAiConfig::with_default_endpoint(api_key)
                .ok()
                .map(Self::OpenAi),
            ProviderFamily::Anthropic => AnthropicConfig::with_default_endpoint(api_key)
                .ok()
                .map(Self::Anthropic),
            ProviderFamily::Gemini => GeminiConfig::with_default_endpoint(api_key)
                .ok()
                .map(Self::Gemini),
            ProviderFamily::Bedrock
            | ProviderFamily::AzureOpenAi
            | ProviderFamily::AzureAiFoundry
            | ProviderFamily::OpenAiCompatible => None,
        }
    }
}

const CACHE_CAPACITY: usize = 1024;
const CACHE_IDLE: Duration = Duration::from_secs(15 * 60);
/// Longest a tenant endpoint's adapter is kept, however busy: its client is
/// pinned to the addresses checked when it was built, so a server that moves
/// is found, and checked again, at the next rebuild.
const PINNED_MAX_AGE: Duration = Duration::from_secs(5 * 60);

type CacheKey = (OrganizationId, String);

struct CachedAdapter {
    key_id: String,
    ciphertext: Vec<u8>,
    adapter: Arc<dyn InferenceProviderAdapter>,
    used_at: Instant,
    built_at: Instant,
    /// Set for adapters whose client is pinned to resolved addresses.
    max_age: Option<Duration>,
}

struct AdapterCache {
    entries: HashMap<CacheKey, CachedAdapter>,
    capacity: usize,
    idle: Duration,
    swept_at: Instant,
}

impl AdapterCache {
    fn new(capacity: usize, idle: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
            idle,
            swept_at: Instant::now(),
        }
    }

    /// The cached adapter if it was built from exactly this ciphertext.
    fn get(
        &mut self,
        key: &CacheKey,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Option<Arc<dyn InferenceProviderAdapter>> {
        self.sweep();
        let cached = self.entries.get_mut(key).filter(|cached| {
            cached.key_id == key_id
                && cached.ciphertext == ciphertext
                && cached.used_at.elapsed() < self.idle
                && cached
                    .max_age
                    .is_none_or(|max_age| cached.built_at.elapsed() < max_age)
        })?;
        cached.used_at = Instant::now();
        Some(cached.adapter.clone())
    }

    fn insert(
        &mut self,
        key: CacheKey,
        key_id: String,
        ciphertext: Vec<u8>,
        adapter: Arc<dyn InferenceProviderAdapter>,
        max_age: Option<Duration>,
    ) {
        if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
            let least_recent = self
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.used_at)
                .map(|(key, _)| key.clone());
            if let Some(least_recent) = least_recent {
                self.entries.remove(&least_recent);
            }
        }
        self.entries.insert(
            key,
            CachedAdapter {
                key_id,
                ciphertext,
                adapter,
                used_at: Instant::now(),
                built_at: Instant::now(),
                max_age,
            },
        );
    }

    fn remove(&mut self, key: &CacheKey) {
        self.entries.remove(key);
    }

    /// Drops idle entries, at most once per idle period so a lookup stays
    /// cheap.
    fn sweep(&mut self) {
        if self.swept_at.elapsed() < self.idle {
            return;
        }
        let idle = self.idle;
        self.entries
            .retain(|_, cached| cached.used_at.elapsed() < idle);
        self.swept_at = Instant::now();
    }
}

#[derive(Clone)]
pub(crate) struct ProviderResolver {
    store: ProviderCredentialStore,
    cipher: Option<CredentialCipher>,
    templates: Arc<HashMap<(String, ProviderFamily), ProviderTemplate>>,
    /// Serve tenants' credentials that need no process configuration (an
    /// Entra directory that names its own resource, a Google service
    /// account) for families without a template.
    for_tenants: Arc<Vec<ProviderTemplate>>,
    /// Origins of tenants' OpenAI-compatible endpoints the operator allows
    /// over plain HTTP or at private addresses.
    private_origins: Arc<Vec<String>>,
    /// [`PINNED_MAX_AGE`]; shorter in tests.
    pinned_max_age: Duration,
    cache: Arc<Mutex<AdapterCache>>,
}

impl ProviderResolver {
    pub(crate) fn new(
        store: ProviderCredentialStore,
        cipher: Option<CredentialCipher>,
        templates: impl IntoIterator<Item = (String, ProviderTemplate)>,
    ) -> Self {
        Self::with_cache_limits(store, cipher, templates, CACHE_CAPACITY, CACHE_IDLE)
    }

    fn with_cache_limits(
        store: ProviderCredentialStore,
        cipher: Option<CredentialCipher>,
        templates: impl IntoIterator<Item = (String, ProviderTemplate)>,
        capacity: usize,
        idle: Duration,
    ) -> Self {
        Self {
            store,
            cipher,
            templates: Arc::new(
                templates
                    .into_iter()
                    .map(|(key, template)| ((key, template.family()), template))
                    .collect(),
            ),
            for_tenants: Arc::new(Vec::new()),
            private_origins: Arc::new(Vec::new()),
            pinned_max_age: PINNED_MAX_AGE,
            cache: Arc::new(Mutex::new(AdapterCache::new(capacity, idle))),
        }
    }

    /// With the origins of tenants' OpenAI-compatible endpoints that may use
    /// plain HTTP or private addresses
    /// (`GATEWAY_OPENAI_COMPATIBLE_PRIVATE_ENDPOINTS`).
    pub(crate) fn with_openai_compatible_private_origins(
        mut self,
        origins: impl IntoIterator<Item = String>,
    ) -> Self {
        self.private_origins = Arc::new(origins.into_iter().collect());
        self
    }

    /// A tenant's own OpenAI-compatible endpoint as an adapter configuration
    /// whose client reaches only the addresses checked here: HTTPS on public
    /// addresses, or any address at an origin the operator allows. Resolved
    /// when the adapter is built, so the cache keeps the pinned addresses
    /// for as long as it keeps the adapter.
    async fn tenant_endpoint(
        &self,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<ProviderTemplate, EgressError> {
        let url = url::Url::parse(base_url).map_err(|_| EgressError::InvalidUrl)?;
        let policy = if self
            .private_origins
            .contains(&url.origin().ascii_serialization())
        {
            DeliveryPolicy::operator_allowed()
        } else {
            DeliveryPolicy::public_only()
        };
        let mut config =
            OpenAiCompatibleConfig::for_tenant_endpoint(base_url, api_key.map(str::to_owned));
        config.pinned_client = Some(pinned_client(&url, policy, config.client_builder()).await?);
        Ok(ProviderTemplate::OpenAiCompatible(config))
    }

    /// With templates for tenants' credentials that need no process
    /// configuration, for the families that have none.
    pub(crate) fn with_tenant_templates(
        mut self,
        templates: impl IntoIterator<Item = ProviderTemplate>,
    ) -> Self {
        self.for_tenants = Arc::new(templates.into_iter().collect());
        self
    }

    /// The adapter serving `context` for `organization_id` from a stored
    /// credential, or `None` when the organization has no credential for the
    /// routed key and the process adapter applies.
    pub(crate) async fn resolve(
        &self,
        organization_id: OrganizationId,
        context: &InferenceContext,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        let (Some(key), Some(family)) = (
            context.adapter_instance_key.as_deref(),
            context.provider_family.as_deref(),
        ) else {
            return Ok(None);
        };
        let cache_key = (organization_id, key.to_owned());
        let result = self.build(organization_id, key, family, &cache_key).await;
        if !matches!(result, Ok(Some(_))) {
            self.cache().remove(&cache_key);
        }
        result
    }

    async fn build(
        &self,
        organization_id: OrganizationId,
        key: &str,
        family: &str,
        cache_key: &CacheKey,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        let stored = self
            .store
            .sealed(organization_id, key)
            .await
            .map_err(|_| unusable(organization_id, key, "credential lookup failed"))?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        // A credential sealed for another family predates an activation that
        // redeclared the key; it has to be set again for the new family.
        if stored.family != family {
            return Err(unusable(organization_id, key, "family mismatch"));
        }
        // Checked on every request, so revoking a binding takes effect at
        // once, cached adapter or not.
        if stored.principal_digest.is_some() && !stored.principal_approved {
            return Err(unusable(
                organization_id,
                key,
                "cloud principal not approved",
            ));
        }
        if let Some(adapter) = self
            .cache()
            .get(cache_key, &stored.key_id, &stored.ciphertext)
        {
            return Ok(Some(adapter));
        }
        let family = ProviderFamily::from_str(family)
            .map_err(|_| unusable(organization_id, key, "unknown family"))?;
        let cipher = self
            .cipher
            .as_ref()
            .ok_or_else(|| unusable(organization_id, key, "encryption key not configured"))?;
        let secret = cipher
            .open(
                CredentialBinding {
                    organization_id,
                    adapter_instance_key: key,
                    family,
                },
                &stored.key_id,
                &stored.ciphertext,
            )
            .map_err(|_| unusable(organization_id, key, "decryption failed"))?;
        // The approval above was for the digest the row records; it must be
        // this secret's. A row written before principals were recorded has
        // none, and is refused until it is set again.
        if secret.cloud_principal().map(|principal| principal.digest()) != stored.principal_digest {
            return Err(unusable(
                organization_id,
                key,
                "cloud principal not approved",
            ));
        }
        let pinned = matches!(secret, ProviderSecret::OpenAiCompatibleEndpoint { .. });
        let template =
            if let ProviderSecret::OpenAiCompatibleEndpoint { base_url, api_key } = &secret {
                // The tenant's endpoint, never the operator's for the same key.
                self.tenant_endpoint(base_url, api_key.as_deref().map(String::as_str))
                    .await
                    .map_err(|error| {
                        unusable(
                            organization_id,
                            key,
                            match error {
                                EgressError::DestinationNotAllowed | EgressError::InvalidUrl => {
                                    "endpoint not allowed"
                                }
                                _ => "endpoint unresolved",
                            },
                        )
                    })?
            } else {
                match self.templates.get(&(key.to_owned(), family)) {
                    Some(template) => template.with_secret(&secret, organization_id),
                    None => ProviderTemplate::with_default_endpoint(
                        family,
                        &secret,
                        organization_id,
                        &self.for_tenants,
                    ),
                }
                .ok_or_else(|| unusable(organization_id, key, "no endpoint configured"))?
            };
        let adapter = template
            .adapter()
            .map_err(|_| unusable(organization_id, key, "adapter construction failed"))?;
        self.cache().insert(
            cache_key.clone(),
            stored.key_id,
            stored.ciphertext,
            adapter.clone(),
            pinned.then_some(self.pinned_max_age),
        );
        Ok(Some(adapter))
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, AdapterCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Longest a probe may take, sign-in and listing included.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Checks a credential the way inference would use it for the organization:
/// the same configuration, sign-in and role assumption, with a fresh adapter
/// that is neither cached nor kept. It only lists models.
#[async_trait::async_trait]
impl crate::domain::onboarding::CredentialProbe for ProviderResolver {
    async fn probe(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: ProviderFamily,
        secret: &ProviderSecret,
    ) -> Result<crate::domain::onboarding::Probe, ProviderError> {
        use crate::domain::onboarding::Probe;
        // A tenant's endpoint, checked as inference checks it.
        if let ProviderSecret::OpenAiCompatibleEndpoint { base_url, api_key } = secret {
            let template = match self
                .tenant_endpoint(base_url, api_key.as_deref().map(String::as_str))
                .await
            {
                Ok(template) => template,
                Err(EgressError::DestinationNotAllowed | EgressError::InvalidUrl) => {
                    return Ok(Probe::EndpointNotAllowed);
                }
                Err(_) => return Ok(Probe::EndpointUnresolved),
            };
            let adapter = template.adapter()?;
            let models = tokio::time::timeout(PROBE_TIMEOUT, adapter.check_access())
                .await
                .map_err(|_| {
                    ProviderError::new(crate::domain::inference::ProviderErrorCategory::Timeout)
                })??;
            return Ok(Probe::Listed {
                models,
                region: None,
            });
        }
        // The key's configuration, else the default for tenants: the choice
        // `build` makes for inference.
        let template = match self
            .templates
            .get(&(adapter_instance_key.to_owned(), family))
        {
            Some(template) => template.with_secret(secret, organization_id),
            None => ProviderTemplate::with_default_endpoint(
                family,
                secret,
                organization_id,
                &self.for_tenants,
            ),
        };
        let Some(template) = template else {
            return Ok(Probe::NotConfigured);
        };
        let region = match &template {
            ProviderTemplate::Bedrock(config) => Some(config.region.clone()),
            _ => None,
        };
        let adapter = template.adapter()?;
        let models = tokio::time::timeout(PROBE_TIMEOUT, adapter.check_access())
            .await
            .map_err(|_| {
                ProviderError::new(crate::domain::inference::ProviderErrorCategory::Timeout)
            })??;
        Ok(Probe::Listed { models, region })
    }
}

/// Discovery lists an account with the adapter inference would use: the
/// organization's stored credential, else the process credential.
#[async_trait::async_trait]
impl crate::jobs::discovery::ProviderAccounts for ProviderResolver {
    async fn adapter(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        let cache_key = (organization_id, adapter_instance_key.to_owned());
        match self
            .build(
                organization_id,
                adapter_instance_key,
                family.as_str(),
                &cache_key,
            )
            .await
        {
            Ok(Some(adapter)) => Ok(Some(adapter)),
            Ok(None) => {
                self.cache().remove(&cache_key);
                self.templates
                    .get(&(adapter_instance_key.to_owned(), family))
                    .map(ProviderTemplate::adapter)
                    .transpose()
            }
            Err(error) => {
                self.cache().remove(&cache_key);
                Err(error)
            }
        }
    }

    async fn tenant_adapter(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        let cache_key = (organization_id, adapter_instance_key.to_owned());
        let built = self
            .build(
                organization_id,
                adapter_instance_key,
                family.as_str(),
                &cache_key,
            )
            .await;
        if built.is_err() {
            self.cache().remove(&cache_key);
        }
        built
    }
}

/// The reason is logged for operators; the caller sees only that the provider
/// is unavailable.
fn unusable(organization_id: OrganizationId, key: &str, reason: &'static str) -> ProviderError {
    tracing::warn!(
        organization_id = %organization_id.0,
        adapter_instance_key = key,
        reason,
        "stored provider credential is unusable"
    );
    ProviderError::new(ProviderErrorCategory::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        credentials::ProviderSecretInput,
        inference::{
            ChatMessage, ChatRequest, ChatRole, CorrelationId, DeploymentModelReference,
            ProviderAttemptIdempotencyKey,
        },
    };
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Duration,
    };
    use uuid::Uuid;
    use zeroize::Zeroizing;

    const KEY: &str = "openai-main";

    fn openai(base_url: String, api_key: &str) -> OpenAiConfig {
        OpenAiConfig {
            base_url,
            api_key: api_key.to_owned(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        }
    }

    fn api_key(value: &str) -> ProviderSecret {
        ProviderSecret::for_family(
            ProviderFamily::OpenAi,
            ProviderSecretInput {
                api_key: Some(Zeroizing::new(value.to_owned())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap()
    }

    fn context(key: &str, family: &str) -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt-resolver").unwrap(),
            DeploymentModelReference::new("deployment", "gpt-test").unwrap(),
        )
        .with_adapter_instance_key(key)
        .with_provider_family(family)
    }

    #[tokio::test]
    async fn a_probe_signs_in_as_inference_would_and_keeps_nothing() {
        use crate::domain::onboarding::{CredentialProbe, Probe};
        // Answers a model listing and reports the bearer token it carried.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, tokens) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                let token = request
                    .lines()
                    .find_map(|line| line.strip_prefix("authorization: bearer "))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let body = r#"{"object":"list","data":[{"id":"gpt-b","object":"model"},{"id":"gpt-a","object":"model"}]}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                if sender.send(token).is_err() {
                    break;
                }
            }
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .unwrap();
        let resolver = ProviderResolver::new(
            ProviderCredentialStore::new(pool),
            None,
            [(
                KEY.to_owned(),
                ProviderTemplate::OpenAi(openai(base_url, "process-key")),
            )],
        );
        let organization = OrganizationId::new();
        match resolver
            .probe(
                organization,
                KEY,
                ProviderFamily::OpenAi,
                &api_key("tenant-key"),
            )
            .await
            .unwrap()
        {
            Probe::Listed { models, region } => {
                let mut names: Vec<_> = models.into_iter().map(|model| model.model).collect();
                names.sort();
                assert_eq!(names, ["gpt-a", "gpt-b"]);
                assert_eq!(region, None);
            }
            other => panic!("openai is configured: {other:?}"),
        }
        // The tenant's key, not the process's.
        assert_eq!(tokens.recv().unwrap(), "tenant-key");
        // Nothing is cached for inference to reuse.
        assert_eq!(resolver.cache().entries.len(), 0);
        // No configuration can use a Bedrock role here.
        let role = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            ProviderSecretInput {
                role_arn: Some("arn:aws:iam::123456789012:role/QucelerateGateway".to_owned()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        assert!(matches!(
            resolver
                .probe(
                    organization,
                    "bedrock-tenant",
                    ProviderFamily::Bedrock,
                    &role
                )
                .await
                .unwrap(),
            Probe::NotConfigured
        ));
    }

    /// Answers model listings and reports the bearer token each carried.
    fn models_server() -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                let token = request
                    .lines()
                    .find_map(|line| line.strip_prefix("authorization: bearer "))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let body = r#"{"object":"list","data":[{"id":"qwen3.8-27b","object":"model"}]}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                if sender.send(token).is_err() {
                    break;
                }
            }
        });
        (base_url, receiver)
    }

    fn endpoint(base_url: &str, key: Option<&str>) -> ProviderSecret {
        ProviderSecret::for_family(
            ProviderFamily::OpenAiCompatible,
            ProviderSecretInput {
                endpoint: Some(base_url.to_owned()),
                api_key: key.map(|key| Zeroizing::new(key.to_owned())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_tenant_endpoint_is_reached_only_where_the_egress_policy_allows() {
        use crate::domain::onboarding::{CredentialProbe, Probe};
        let (base_url, tokens) = models_server();
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .unwrap();
        let organization = OrganizationId::new();
        let key = "openai-compatible-tenant";
        // Plain HTTP to a loopback address: refused, and nothing is sent.
        let public_only = ProviderResolver::new(
            ProviderCredentialStore::new(pool.clone()),
            None,
            Vec::<(String, ProviderTemplate)>::new(),
        );
        assert!(matches!(
            public_only
                .probe(
                    organization,
                    key,
                    ProviderFamily::OpenAiCompatible,
                    &endpoint(&base_url, Some("tenant-key")),
                )
                .await
                .unwrap(),
            Probe::EndpointNotAllowed
        ));
        assert!(tokens.recv_timeout(Duration::from_millis(200)).is_err());
        // An origin the operator allows (the path does not matter).
        let origin = url::Url::parse(&base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        let allowed = ProviderResolver::new(
            ProviderCredentialStore::new(pool),
            None,
            Vec::<(String, ProviderTemplate)>::new(),
        )
        .with_openai_compatible_private_origins([origin]);
        match allowed
            .probe(
                organization,
                key,
                ProviderFamily::OpenAiCompatible,
                &endpoint(&base_url, Some("tenant-key")),
            )
            .await
            .unwrap()
        {
            Probe::Listed { models, region } => {
                assert_eq!(
                    models
                        .into_iter()
                        .map(|model| model.model)
                        .collect::<Vec<_>>(),
                    ["qwen3.8-27b"]
                );
                assert_eq!(region, None);
            }
            other => panic!("the origin is allowed: {other:?}"),
        }
        assert_eq!(tokens.recv().unwrap(), "tenant-key");
        // Without a key, no Authorization header is sent.
        assert!(matches!(
            allowed
                .probe(
                    organization,
                    key,
                    ProviderFamily::OpenAiCompatible,
                    &endpoint(&base_url, None),
                )
                .await
                .unwrap(),
            Probe::Listed { .. }
        ));
        assert_eq!(tokens.recv().unwrap(), "");
        // A host name that does not resolve.
        assert!(matches!(
            allowed
                .probe(
                    organization,
                    key,
                    ProviderFamily::OpenAiCompatible,
                    &endpoint("https://models.invalid/v1", None),
                )
                .await
                .unwrap(),
            Probe::EndpointUnresolved
        ));
    }

    /// Answers every chat request and reports the bearer token it carried.
    fn provider_server() -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                let token = request
                    .lines()
                    .find_map(|line| line.strip_prefix("authorization: bearer "))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let body = r#"{"id":"chat-1","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                if sender.send(token).is_err() {
                    break;
                }
            }
        });
        (base_url, receiver)
    }

    async fn bearer(
        adapter: &Arc<dyn InferenceProviderAdapter>,
        requests: &mpsc::Receiver<String>,
    ) -> String {
        adapter
            .chat(
                context(KEY, "openai"),
                ChatRequest {
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "hello".to_owned(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    }],
                    temperature: None,
                    max_tokens: Some(4),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        requests.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    async fn store(
        pool: &PgPool,
        cipher: &CredentialCipher,
        organization_id: OrganizationId,
        key: &str,
        family: ProviderFamily,
        secret: &str,
    ) {
        let sealed = cipher
            .seal(
                CredentialBinding {
                    organization_id,
                    adapter_instance_key: key,
                    family,
                },
                &api_key(secret),
            )
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.provider_credential
                 (id, organization_id, adapter_instance_key, family, key_id, ciphertext,
                  principal_recorded)
             VALUES ($1, $2, $3, $4, $5, $6, true)
             ON CONFLICT (organization_id, adapter_instance_key) DO UPDATE
             SET family = EXCLUDED.family, key_id = EXCLUDED.key_id,
                 ciphertext = EXCLUDED.ciphertext",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(key)
        .bind(family.as_str())
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn secrets_only_fill_a_template_of_their_own_kind() {
        let template = ProviderTemplate::OpenAi(openai("https://api.openai.com/v1".into(), "env"));
        let Some(ProviderTemplate::OpenAi(config)) =
            template.with_secret(&api_key("stored"), OrganizationId::new())
        else {
            panic!("an API key fills an OpenAI template");
        };
        assert_eq!(config.api_key, "stored");
        let aws = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            ProviderSecretInput {
                access_key_id: Some(Zeroizing::new("AKIAEXAMPLE".into())),
                secret_access_key: Some(Zeroizing::new("secret".into())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        assert!(template.with_secret(&aws, OrganizationId::new()).is_none());
    }

    #[tokio::test]
    async fn a_stored_role_is_assumed_with_the_gateway_identity() {
        let fake = crate::aws_credentials::tests::start().await;
        let template = ProviderTemplate::Bedrock(BedrockConfig {
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            control_base_url: "https://bedrock.us-east-1.amazonaws.com".into(),
            region: "us-east-1".into(),
            credentials: crate::aws_credentials::AwsCredentialSource::fixed(
                crate::aws::AwsCredentials {
                    access_key_id: "AKIDGATEWAY".into(),
                    secret_access_key: Zeroizing::new("gateway-secret".into()),
                    session_token: None,
                },
            ),
            sts_endpoint: Some(format!("{}/sts/", fake.base)),
            s3_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        });
        let role = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            ProviderSecretInput {
                role_arn: Some("arn:aws:iam::123456789012:role/tenant".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let organization = OrganizationId::new();
        let Some(ProviderTemplate::Bedrock(config)) = template.with_secret(&role, organization)
        else {
            panic!("a role fills a Bedrock template");
        };
        let assumed = config.credentials.get().await.unwrap();
        assert!(assumed.access_key_id.starts_with("ASIA"));
        assert!(assumed.session_token.is_some());
        let (headers, body) = fake.last.lock().unwrap().clone().unwrap();
        assert!(
            headers["authorization"]
                .to_str()
                .unwrap()
                .contains("Credential=AKIDGATEWAY/")
        );
        assert!(body.contains(&format!(
            "RoleSessionName=qucelerate-{}",
            organization.0.simple()
        )));
        // The organization's external ID, set by the gateway.
        assert!(body.contains(&format!("ExternalId=qucelerate-{}", organization.0)));
        // Another organization storing the same role presents its own, which
        // the role's trust policy does not accept.
        let other = OrganizationId::new();
        let Some(ProviderTemplate::Bedrock(config)) = template.with_secret(&role, other) else {
            panic!("a role fills a Bedrock template");
        };
        config.credentials.get().await.unwrap();
        let (_, body) = fake.last.lock().unwrap().clone().unwrap();
        assert!(body.contains(&format!("ExternalId=qucelerate-{}", other.0)));

        // A role never fills a template of another family.
        let template = ProviderTemplate::OpenAi(openai("https://api.openai.com/v1".into(), "env"));
        assert!(template.with_secret(&role, organization).is_none());
    }

    #[test]
    fn a_role_under_any_account_name_uses_the_gateways_bedrock_settings() {
        let bedrock = ProviderTemplate::Bedrock(BedrockConfig {
            base_url: "https://bedrock-runtime.us-east-2.amazonaws.com".into(),
            control_base_url: "https://bedrock.us-east-2.amazonaws.com".into(),
            region: "us-east-2".into(),
            credentials: crate::aws_credentials::AwsCredentialSource::fixed(
                crate::aws::AwsCredentials {
                    access_key_id: "AKIDGATEWAY".into(),
                    secret_access_key: Zeroizing::new("gateway-secret".into()),
                    session_token: None,
                },
            ),
            sts_endpoint: None,
            s3_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        });
        let role = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            ProviderSecretInput {
                role_arn: Some("arn:aws:iam::123456789012:role/QucelerateGateway".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let Some(ProviderTemplate::Bedrock(config)) = ProviderTemplate::with_default_endpoint(
            ProviderFamily::Bedrock,
            &role,
            OrganizationId::new(),
            std::slice::from_ref(&bedrock),
        ) else {
            panic!("a role is served with the gateway's Bedrock settings");
        };
        assert_eq!(config.region, "us-east-2");
        // Not without Bedrock configured...
        assert!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::Bedrock,
                &role,
                OrganizationId::new(),
                &[],
            )
            .is_none()
        );
        // ...and access keys still need their own adapter.
        let keys = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            ProviderSecretInput {
                access_key_id: Some(Zeroizing::new("AKIAEXAMPLE".into())),
                secret_access_key: Some(Zeroizing::new("secret".into())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        assert!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::Bedrock,
                &keys,
                OrganizationId::new(),
                &[bedrock],
            )
            .is_none()
        );
    }

    /// Discovery reads Resource Manager for the tenant's own resource, in
    /// its directory, and never for the process's (D-018).
    #[tokio::test]
    async fn a_tenant_credential_lists_only_its_own_resource() {
        use crate::azure_credentials::tests as entra;
        let fake = entra::start().await;
        let identity = entra::secret_identity(&fake);
        let operator = "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/ops/providers/Microsoft.CognitiveServices/accounts/operator";
        let tenant_resource = "/subscriptions/00000000-0000-4000-8000-000000000002/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/tenant";
        let template = ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
            base_url: "https://operator.services.ai.azure.com".into(),
            auth: AzureAuth::ApiKey("operator-key".into()),
            identity: Some(identity.clone()),
            management: Some(crate::config::AzureManagement {
                endpoint: identity.management_endpoint().into(),
                resource_id: operator.into(),
                token: identity.token(identity.management_scope()),
            }),
            api_version: "2024-05-01-preview".into(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        });
        let management = |secret: ProviderSecretInput| {
            let secret =
                ProviderSecret::for_family(ProviderFamily::AzureAiFoundry, secret).unwrap();
            let Some(ProviderTemplate::AzureAiFoundry(config)) =
                template.with_secret(&secret, OrganizationId::new())
            else {
                panic!("an Azure credential fills an Azure template");
            };
            config.management
        };
        let tenant = "8f2c1a4e-0000-4000-8000-00000000000a";
        // A key, or a directory without a resource ID, lists nothing.
        assert!(
            management(ProviderSecretInput {
                api_key: Some(Zeroizing::new("tenant-key".into())),
                ..ProviderSecretInput::default()
            })
            .is_none()
        );
        assert!(
            management(ProviderSecretInput {
                tenant_id: Some(tenant.into()),
                endpoint: Some("https://tenant.services.ai.azure.com".into()),
                ..ProviderSecretInput::default()
            })
            .is_none()
        );
        // A resource ID is read in the tenant's directory, for Resource
        // Manager.
        let listed = management(ProviderSecretInput {
            tenant_id: Some(tenant.into()),
            endpoint: Some("https://tenant.services.ai.azure.com".into()),
            resource_id: Some(tenant_resource.into()),
            ..ProviderSecretInput::default()
        })
        .expect("the tenant's resource");
        assert_eq!(listed.resource_id, tenant_resource);
        assert_eq!(listed.endpoint, "https://management.azure.com");
        listed.token.get().await.unwrap();
        let (directory, form) = fake.requests.lock().unwrap().last().unwrap().clone();
        assert_eq!(directory, tenant);
        assert_eq!(form["scope"], "https://management.azure.com/.default");
    }

    #[tokio::test]
    async fn an_entra_directory_is_signed_in_to_with_the_gateway_identity() {
        use crate::azure_credentials::tests as entra;
        let fake = entra::start().await;
        let template = ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
            base_url: "https://operator.services.ai.azure.com".into(),
            auth: AzureAuth::ApiKey("operator-key".into()),
            identity: Some(entra::secret_identity(&fake)),
            management: None,
            api_version: "2024-05-01-preview".into(),
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        });
        let tenant = "8f2c1a4e-0000-4000-8000-00000000000a";
        let secret = ProviderSecret::for_family(
            ProviderFamily::AzureAiFoundry,
            ProviderSecretInput {
                tenant_id: Some(tenant.into()),
                endpoint: Some("https://tenant-res.services.ai.azure.com/".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let Some(ProviderTemplate::AzureAiFoundry(config)) =
            template.with_secret(&secret, OrganizationId::new())
        else {
            panic!("an Entra directory fills an Azure template");
        };
        assert_eq!(config.base_url, "https://tenant-res.services.ai.azure.com");
        let AzureAuth::Entra(token) = &config.auth else {
            panic!("Entra auth replaces the operator's key");
        };
        assert_eq!(token.get().await.unwrap().as_str(), "token-1");
        let (directory, form) = fake.requests.lock().unwrap()[0].clone();
        assert_eq!(directory, tenant);
        assert_eq!(form["client_id"], "gateway-app");
        assert_eq!(
            form["scope"],
            crate::azure_credentials::COGNITIVE_SERVICES_SCOPE
        );

        // A managed identity cannot sign in to another directory as itself.
        let managed = ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
            identity: Some(entra::unused_identity()),
            ..config.clone()
        });
        assert!(
            managed
                .with_secret(&secret, OrganizationId::new())
                .is_none()
        );
        // With no process adapter, a directory that names its resource is
        // served with the family's settings for tenants...
        let for_tenants = [ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
            base_url: "https://tenant.services.ai.azure.com".into(),
            auth: AzureAuth::Entra(
                entra::secret_identity(&fake)
                    .token(crate::azure_credentials::COGNITIVE_SERVICES_SCOPE),
            ),
            identity: Some(entra::secret_identity(&fake)),
            management: None,
            api_version: "2025-configured".into(),
            request_timeout: Duration::from_secs(7),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        })];
        let Some(ProviderTemplate::AzureAiFoundry(config)) =
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::AzureAiFoundry,
                &secret,
                OrganizationId::new(),
                &for_tenants,
            )
        else {
            panic!("a directory with an endpoint needs no process adapter");
        };
        assert_eq!(config.base_url, "https://tenant-res.services.ai.azure.com");
        assert_eq!(config.api_version, "2025-configured");
        assert!(matches!(config.auth, AzureAuth::Entra(_)));
        // ...but not without them, nor without an endpoint...
        assert!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::AzureAiFoundry,
                &secret,
                OrganizationId::new(),
                &[],
            )
            .is_none()
        );
        let no_endpoint = ProviderSecret::for_family(
            ProviderFamily::AzureAiFoundry,
            ProviderSecretInput {
                tenant_id: Some(tenant.into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        assert!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::AzureAiFoundry,
                &no_endpoint,
                OrganizationId::new(),
                &for_tenants,
            )
            .is_none()
        );
        // ...nor in another cloud than the identity's: its tokens would be
        // for another audience.
        let government = ProviderSecret::for_family(
            ProviderFamily::AzureAiFoundry,
            ProviderSecretInput {
                tenant_id: Some(tenant.into()),
                endpoint: Some("https://gov-res.services.ai.azure.us".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        assert!(
            for_tenants[0]
                .with_secret(&government, OrganizationId::new())
                .is_none()
        );

        // Nor does an Entra directory fill another family.
        let template = ProviderTemplate::OpenAi(openai("https://api.openai.com/v1".into(), "env"));
        assert!(
            template
                .with_secret(&secret, OrganizationId::new())
                .is_none()
        );
    }

    #[test]
    fn tenant_gemini_credentials_pick_their_api() {
        let process_on_agent_platform = GeminiConfig {
            base_url: AgentPlatform::new("gateway-project", "global")
                .unwrap()
                .base_url(),
            auth: GeminiAuth::ApiKey("unused".into()),
            identity: None,
            agent_platform: AgentPlatform::new("gateway-project", "global"),
            agent_platform_location: Some("europe-west4".into()),
            storage_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        // A tenant's key goes to the Gemini API, not the process's Agent
        // Platform project.
        let Some(ProviderTemplate::Gemini(keyed)) =
            ProviderTemplate::Gemini(process_on_agent_platform.clone())
                .with_secret(&api_key("tenant-key"), OrganizationId::new())
        else {
            panic!("a key fills a Gemini template");
        };
        assert_eq!(keyed.base_url, GEMINI_API_URL);
        assert!(keyed.agent_platform.is_none());
        // A process on the Gemini API keeps its own URL for keys.
        let process_on_api = GeminiConfig {
            base_url: "http://127.0.0.1:9/v1beta".into(),
            agent_platform: None,
            ..process_on_agent_platform
        };
        let Some(ProviderTemplate::Gemini(keyed)) = ProviderTemplate::Gemini(process_on_api)
            .with_secret(&api_key("tenant-key"), OrganizationId::new())
        else {
            panic!("a key fills a Gemini template");
        };
        assert_eq!(keyed.base_url, "http://127.0.0.1:9/v1beta");
    }

    #[tokio::test]
    async fn a_tenant_service_account_is_impersonated_by_the_gateway_identity() {
        use crate::gcp_credentials::tests as google;
        let fake = google::start().await;
        let dir = google::TempDir::new();
        let identity = google::identity(
            &fake,
            &dir.write("adc.json", &google::user_credentials(&fake)),
        );
        let gemini = |identity: Option<crate::gcp_credentials::GoogleIdentity>| GeminiConfig {
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            auth: GeminiAuth::ApiKey("operator-key".into()),
            identity,
            agent_platform: None,
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        };
        let secret = ProviderSecret::for_family(
            ProviderFamily::Gemini,
            ProviderSecretInput {
                service_account: Some("gemini@tenant-project.iam.gserviceaccount.com".into()),
                project_id: Some("tenant-project".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let template = ProviderTemplate::Gemini(gemini(Some(identity.clone())));
        let Some(ProviderTemplate::Gemini(config)) =
            template.with_secret(&secret, OrganizationId::new())
        else {
            panic!("a service account fills a Gemini template");
        };
        let GeminiAuth::Google {
            token,
            quota_project,
        } = &config.auth
        else {
            panic!("the service account replaces the operator's key");
        };
        // On Agent Platform, which bills the project in its URL.
        assert_eq!(quota_project.as_deref(), None);
        assert_eq!(
            config.base_url,
            "https://aiplatform.googleapis.com/v1/projects/tenant-project/locations/global/publishers/google"
        );
        assert_eq!(
            config.agent_platform,
            Some(AgentPlatform {
                project: "tenant-project".into(),
                location: "global".into()
            })
        );
        // With no location of its own, the configured one, though the
        // process's Gemini uses a key.
        let configured = ProviderTemplate::Gemini(GeminiConfig {
            agent_platform_location: Some("us-east5".into()),
            storage_base_url: None,
            ..gemini(Some(identity.clone()))
        });
        let Some(ProviderTemplate::Gemini(in_region)) =
            configured.with_secret(&secret, OrganizationId::new())
        else {
            panic!("a service account fills a Gemini template");
        };
        assert!(
            in_region
                .base_url
                .starts_with("https://us-east5-aiplatform.googleapis.com/"),
            "{}",
            in_region.base_url
        );
        // Without a project, the account's own; in a region, its endpoint.
        let regional = ProviderSecret::for_family(
            ProviderFamily::Gemini,
            ProviderSecretInput {
                service_account: Some("gemini@other-project.iam.gserviceaccount.com".into()),
                location: Some("europe-west4".into()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let Some(ProviderTemplate::Gemini(regional)) =
            template.with_secret(&regional, OrganizationId::new())
        else {
            panic!("a service account fills a Gemini template");
        };
        assert_eq!(
            regional.base_url,
            "https://europe-west4-aiplatform.googleapis.com/v1/projects/other-project/locations/europe-west4/publishers/google"
        );
        // The gateway's token, then the tenant service account's.
        assert_eq!(token.get().await.unwrap().as_str(), "sa-2");
        let (path, authorization, _) = fake.requests.lock().unwrap()[1].clone();
        assert!(path.contains("gemini@tenant-project.iam.gserviceaccount.com:generateAccessToken"));
        assert_eq!(authorization, "Bearer token-1");

        // With no process adapter, Gemini's settings for tenants serve it...
        let for_tenants = [ProviderTemplate::Gemini(gemini(Some(identity)))];
        assert!(matches!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::Gemini,
                &secret,
                OrganizationId::new(),
                &for_tenants,
            ),
            Some(ProviderTemplate::Gemini(GeminiConfig {
                auth: GeminiAuth::Google { .. },
                ..
            }))
        ));
        // ...but nothing without a Google identity, nor for another family.
        assert!(
            ProviderTemplate::Gemini(gemini(None))
                .with_secret(&secret, OrganizationId::new())
                .is_none()
        );
        assert!(
            ProviderTemplate::with_default_endpoint(
                ProviderFamily::Gemini,
                &secret,
                OrganizationId::new(),
                &[],
            )
            .is_none()
        );
        let template = ProviderTemplate::OpenAi(openai("https://api.openai.com/v1".into(), "env"));
        assert!(
            template
                .with_secret(&secret, OrganizationId::new())
                .is_none()
        );
    }

    #[test]
    fn only_fixed_endpoint_families_work_without_process_configuration() {
        let secret = api_key("stored");
        for family in [
            ProviderFamily::OpenAi,
            ProviderFamily::Anthropic,
            ProviderFamily::Gemini,
        ] {
            let template = ProviderTemplate::with_default_endpoint(
                family,
                &secret,
                OrganizationId::new(),
                &[],
            )
            .unwrap_or_else(|| panic!("{family:?} has a default endpoint"));
            assert_eq!(template.family(), family);
        }
        for family in [
            ProviderFamily::Bedrock,
            ProviderFamily::AzureOpenAi,
            ProviderFamily::AzureAiFoundry,
            ProviderFamily::OpenAiCompatible,
        ] {
            assert!(
                ProviderTemplate::with_default_endpoint(
                    family,
                    &secret,
                    OrganizationId::new(),
                    &[]
                )
                .is_none()
            );
        }
    }

    #[tokio::test]
    async fn a_cloud_principal_is_used_only_while_bound_to_the_organization() {
        use crate::{
            domain::credentials::CloudPrincipal, gcp_credentials::tests as google,
            persistence::cloud_principals::CloudPrincipalStore,
        };
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let identity = crate::persistence::IdentityStore::new(
            pool.clone(),
            crate::domain::keys::ApiKeyPepper::from_bytes(b"resolver-test-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("principal-{suffix}"), "Principal")
            .await
            .unwrap()
            .id;
        let other = identity
            .create_organization(&format!("principal-other-{suffix}"), "Other")
            .await
            .unwrap()
            .id;
        let fake = google::start().await;
        let dir = google::TempDir::new();
        let google_identity = google::identity(
            &fake,
            &dir.write("adc.json", &google::user_credentials(&fake)),
        );
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let resolver = ProviderResolver::new(
            ProviderCredentialStore::new(pool.clone()),
            Some(cipher.clone()),
            [(
                "gemini-main".to_owned(),
                ProviderTemplate::Gemini(GeminiConfig {
                    base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                    auth: GeminiAuth::ApiKey("operator-key".into()),
                    identity: Some(google_identity),
                    agent_platform: None,
                    agent_platform_location: None,
                    storage_base_url: None,
                    request_timeout: Duration::from_secs(5),
                    connect_timeout: Duration::from_secs(1),
                    max_response_bytes: 1024 * 1024,
                }),
            )],
        );
        let account = format!(
            "tenant-{}@tenant-project.iam.gserviceaccount.com",
            &suffix[..8]
        );
        let secret = ProviderSecret::for_family(
            ProviderFamily::Gemini,
            ProviderSecretInput {
                service_account: Some(account.clone()),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let principal = secret.cloud_principal().unwrap();
        // Two organizations store the same service account, as the gateway's
        // write path records it.
        for organization_id in [org, other] {
            let sealed = cipher
                .seal(
                    CredentialBinding {
                        organization_id,
                        adapter_instance_key: "gemini-main",
                        family: ProviderFamily::Gemini,
                    },
                    &secret,
                )
                .unwrap();
            sqlx::query(
                "INSERT INTO gateway.provider_credential
                     (id, organization_id, adapter_instance_key, family, key_id, ciphertext,
                      principal_digest, principal_recorded)
                 VALUES ($1, $2, 'gemini-main', 'gemini', $3, $4, $5, true)",
            )
            .bind(Uuid::new_v4())
            .bind(organization_id.0)
            .bind(&sealed.key_id)
            .bind(&sealed.ciphertext)
            .bind(principal.digest())
            .execute(&pool)
            .await
            .unwrap();
        }
        let gemini = context("gemini-main", "gemini");

        // Unbound: refused, with nothing asked of Google.
        let error = resolver.resolve(org, &gemini).await.err().unwrap();
        assert_eq!(error.category(), ProviderErrorCategory::Unavailable);
        assert_eq!(fake.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Bound to `org` only.
        let bindings = CloudPrincipalStore::new(pool.clone());
        assert!(bindings.allow(org, &principal).await.unwrap());
        assert!(!bindings.allow(org, &principal).await.unwrap());
        let adapter = resolver.resolve(org, &gemini).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &adapter,
            &resolver.resolve(org, &gemini).await.unwrap().unwrap()
        ));
        assert!(resolver.resolve(other, &gemini).await.is_err());
        let listed = bindings.list(org).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].principal, account);

        // Revoked: refused at the next request, cached adapter or not.
        assert!(bindings.revoke(org, &principal).await.unwrap());
        assert!(!bindings.revoke(org, &principal).await.unwrap());
        assert!(resolver.resolve(org, &gemini).await.is_err());

        // Both changes were audited, without the principal.
        let audited: Vec<(String, String)> = sqlx::query_as(
            "SELECT action, metadata::text FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = 'cloud_principal_binding'
             ORDER BY occurred_at",
        )
        .bind(org.0)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            audited
                .iter()
                .map(|(action, _)| action.as_str())
                .collect::<Vec<_>>(),
            [
                "cloud_principal_binding.created",
                "cloud_principal_binding.revoked"
            ]
        );
        assert!(
            audited
                .iter()
                .all(|(_, metadata)| !metadata.contains(&account))
        );

        // A row stored before principals were recorded is refused even when
        // its principal is bound...
        assert!(bindings.allow(org, &principal).await.unwrap());
        sqlx::query(
            "UPDATE gateway.provider_credential
             SET principal_digest = NULL, principal_recorded = false
             WHERE organization_id = $1",
        )
        .bind(org.0)
        .execute(&pool)
        .await
        .unwrap();
        assert!(resolver.resolve(org, &gemini).await.is_err());
        // ...until the API records it at startup, after which it is served.
        let store = ProviderCredentialStore::new(pool.clone());
        // Counts span the shared test database; this organization's row is
        // what must be recorded.
        assert!(store.record_principals(&cipher).await.unwrap() >= 1);
        let recorded: bool = sqlx::query_scalar(
            "SELECT principal_recorded FROM gateway.provider_credential
             WHERE organization_id = $1",
        )
        .bind(org.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(recorded);
        assert!(resolver.resolve(org, &gemini).await.unwrap().is_some());
        let (items, _) = store.list(org, 10, None).await.unwrap();
        assert_eq!(
            items[0].cloud_principal,
            Some(crate::domain::credentials::CloudPrincipalStatus::Approved)
        );
        // A principal kind of the wrong value never matches.
        assert!(CloudPrincipal::parse("google_service_account", "a@b").is_err());
    }

    #[tokio::test]
    async fn inference_reaches_a_stored_tenant_endpoint_only_where_allowed() {
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let identity = crate::persistence::IdentityStore::new(
            pool.clone(),
            crate::domain::keys::ApiKeyPepper::from_bytes(b"resolver-test-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("resolver-endpoint-{suffix}"), "Endpoint")
            .await
            .unwrap()
            .id;
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let (base_url, requests) = provider_server();
        let key = "openai-compatible-qwen";
        let family = ProviderFamily::OpenAiCompatible;
        let sealed = cipher
            .seal(
                CredentialBinding {
                    organization_id: org,
                    adapter_instance_key: key,
                    family,
                },
                &endpoint(&base_url, Some("tenant-key")),
            )
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.provider_credential
                 (id, organization_id, adapter_instance_key, family, key_id, ciphertext,
                  principal_recorded)
             VALUES ($1, $2, $3, $4, $5, $6, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(key)
        .bind(family.as_str())
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .execute(&pool)
        .await
        .unwrap();
        let request = context(key, family.as_str());
        // An operator endpoint for the same key is never used instead.
        let operator =
            OpenAiCompatibleConfig::for_tenant_endpoint("https://operator.invalid/v1", None);
        let public_only = ProviderResolver::new(
            ProviderCredentialStore::new(pool.clone()),
            Some(cipher.clone()),
            [(key.to_owned(), ProviderTemplate::OpenAiCompatible(operator))],
        );
        let Err(refused) = public_only.resolve(org, &request).await else {
            panic!("plain HTTP to a loopback address is refused");
        };
        assert_eq!(refused.category(), ProviderErrorCategory::Unavailable);
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());
        let origin = url::Url::parse(&base_url)
            .unwrap()
            .origin()
            .ascii_serialization();
        let allowed = ProviderResolver::new(
            ProviderCredentialStore::new(pool.clone()),
            Some(cipher),
            Vec::<(String, ProviderTemplate)>::new(),
        )
        .with_openai_compatible_private_origins([origin]);
        let adapter = allowed.resolve(org, &request).await.unwrap().unwrap();
        assert_eq!(bearer(&adapter, &requests).await, "tenant-key");
    }

    #[tokio::test]
    async fn stored_credentials_override_rotate_and_fail_closed() {
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let identity = crate::persistence::IdentityStore::new(
            pool.clone(),
            crate::domain::keys::ApiKeyPepper::from_bytes(b"resolver-test-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("resolver-{suffix}"), "Resolver")
            .await
            .unwrap()
            .id;
        let other = identity
            .create_organization(&format!("resolver-other-{suffix}"), "Other")
            .await
            .unwrap()
            .id;
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let (base_url, requests) = provider_server();
        let resolver_with = |cipher: Option<CredentialCipher>| {
            ProviderResolver::new(
                ProviderCredentialStore::new(pool.clone()),
                cipher,
                [(
                    KEY.to_owned(),
                    ProviderTemplate::OpenAi(openai(base_url.clone(), "process-key")),
                )],
            )
        };
        let resolver = resolver_with(Some(cipher.clone()));
        let openai_context = context(KEY, "openai");

        // Without a stored credential the process adapter applies.
        assert!(
            resolver
                .resolve(org, &openai_context)
                .await
                .unwrap()
                .is_none()
        );

        // A stored credential replaces only the secret: the request goes to
        // the process-configured endpoint with the organization's key.
        store(
            &pool,
            &cipher,
            org,
            KEY,
            ProviderFamily::OpenAi,
            "org-key-1",
        )
        .await;
        let first = resolver
            .resolve(org, &openai_context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bearer(&first, &requests).await, "org-key-1");
        let cached = resolver
            .resolve(org, &openai_context)
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&first, &cached));
        // Another organization never sees it.
        assert!(
            resolver
                .resolve(other, &openai_context)
                .await
                .unwrap()
                .is_none()
        );

        // Rotation takes effect on the next request, without a restart.
        store(
            &pool,
            &cipher,
            org,
            KEY,
            ProviderFamily::OpenAi,
            "org-key-2",
        )
        .await;
        let rotated = resolver
            .resolve(org, &openai_context)
            .await
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &rotated));
        assert_eq!(bearer(&rotated, &requests).await, "org-key-2");

        // A credential that cannot be used fails instead of falling back to
        // the process key.
        let unavailable = |result: Result<Option<_>, ProviderError>| {
            assert_eq!(
                result.err().map(|error| error.category()),
                Some(ProviderErrorCategory::Unavailable)
            );
        };
        unavailable(resolver.resolve(org, &context(KEY, "anthropic")).await);
        unavailable(resolver_with(None).resolve(org, &openai_context).await);
        unavailable(
            resolver_with(Some(CredentialCipher::new(&[8_u8; 32]).unwrap()))
                .resolve(org, &openai_context)
                .await,
        );

        // A key without process configuration works only for a family with a
        // fixed public endpoint.
        store(
            &pool,
            &cipher,
            org,
            "openai-byok",
            ProviderFamily::OpenAi,
            "byok",
        )
        .await;
        assert!(
            resolver
                .resolve(org, &context("openai-byok", "openai"))
                .await
                .unwrap()
                .is_some()
        );
        store(
            &pool,
            &cipher,
            org,
            "azure-byok",
            ProviderFamily::AzureOpenAi,
            "byok",
        )
        .await;
        unavailable(
            resolver
                .resolve(org, &context("azure-byok", "azure-openai"))
                .await,
        );

        // Deleting the credential returns the key to the process adapter.
        sqlx::query("DELETE FROM gateway.provider_credential WHERE organization_id = $1")
            .bind(org.0)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            resolver
                .resolve(org, &openai_context)
                .await
                .unwrap()
                .is_none()
        );
        pool.close().await;
    }

    fn cache_key(name: &str) -> CacheKey {
        (OrganizationId(Uuid::nil()), name.to_owned())
    }

    fn cached(cache: &mut AdapterCache, name: &str) -> bool {
        cache.get(&cache_key(name), "key", b"sealed").is_some()
    }

    #[test]
    fn a_pinned_adapter_is_rebuilt_after_its_maximum_age_however_busy() {
        let mut cache = AdapterCache::new(4, Duration::from_secs(60));
        let adapter: Arc<dyn InferenceProviderAdapter> =
            Arc::new(crate::provider::UnavailableProviderAdapter);
        cache.insert(
            cache_key("pinned"),
            "key".into(),
            b"sealed".to_vec(),
            adapter.clone(),
            Some(Duration::from_millis(50)),
        );
        cache.insert(
            cache_key("plain"),
            "key".into(),
            b"sealed".to_vec(),
            adapter,
            None,
        );
        assert!(cached(&mut cache, "pinned"));
        std::thread::sleep(Duration::from_millis(30));
        // Used again before the idle limit: still too old once its age passes.
        assert!(cached(&mut cache, "pinned"));
        std::thread::sleep(Duration::from_millis(30));
        assert!(!cached(&mut cache, "pinned"));
        assert!(cached(&mut cache, "plain"));
    }

    #[test]
    fn cache_evicts_the_least_recently_used_entry_at_capacity() {
        let mut cache = AdapterCache::new(2, Duration::from_secs(60));
        let adapter: Arc<dyn InferenceProviderAdapter> =
            Arc::new(crate::provider::UnavailableProviderAdapter);
        for name in ["a", "b"] {
            cache.insert(
                cache_key(name),
                "key".into(),
                b"sealed".to_vec(),
                adapter.clone(),
                None,
            );
        }
        assert!(cached(&mut cache, "a"));
        cache.insert(
            cache_key("c"),
            "key".into(),
            b"sealed".to_vec(),
            adapter,
            None,
        );
        assert_eq!(cache.entries.len(), 2);
        assert!(cached(&mut cache, "a"));
        assert!(!cached(&mut cache, "b"));
        assert!(cached(&mut cache, "c"));
        // A different ciphertext is a different credential.
        assert!(cache.get(&cache_key("a"), "key", b"rotated").is_none());
    }

    #[test]
    fn cache_drops_idle_entries() {
        let mut cache = AdapterCache::new(8, Duration::ZERO);
        cache.insert(
            cache_key("a"),
            "key".into(),
            b"sealed".to_vec(),
            Arc::new(crate::provider::UnavailableProviderAdapter),
            None,
        );
        assert!(!cached(&mut cache, "a"));
        assert!(cache.entries.is_empty());
    }

    /// Requests through the inference facade, as the HTTP layer sends them,
    /// reach the provider with the organization's stored credential.
    #[tokio::test]
    async fn inference_requests_use_the_organization_credential() {
        use crate::{
            application::{AccountingPolicy, AccountingService, inference::InferenceFacade},
            domain::{
                accounting::{BudgetAmount, Currency, QuotaScope},
                catalog::CatalogManifest,
                identity::{ApiKeyScope, MembershipRole, VerifiedIdentity},
            },
            persistence::{AccountingStore, catalog::CatalogStore},
            provider::ProviderRegistry,
        };

        let Some(database_url) = std::env::var("GATEWAY_TEST_DATABASE_URL").ok() else {
            assert!(
                std::env::var_os("CI").is_none() && std::env::var_os("GITHUB_ACTIONS").is_none(),
                "GATEWAY_TEST_DATABASE_URL must be set in CI"
            );
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let identity_store = crate::persistence::IdentityStore::new(
            pool.clone(),
            crate::domain::keys::ApiKeyPepper::from_bytes(b"resolver-test-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity_store
            .create_organization(&format!("resolver-facade-{suffix}"), "Resolver")
            .await
            .unwrap()
            .id;
        let user = identity_store
            .create_user(org, None, None, None, Some("owner"))
            .await
            .unwrap();
        identity_store
            .add_organization_membership(org, user.id, MembershipRole::Owner)
            .await
            .unwrap();
        let service_account = identity_store
            .create_service_account(org, user.id, None, "resolver")
            .await
            .unwrap();
        let identity = VerifiedIdentity {
            organization_id: org,
            service_account_id: service_account.id,
            team_id: None,
            api_key_id: None,
            scopes: vec![ApiKeyScope::Inference],
            end_user: None,
        };
        let accounting_store = AccountingStore::new(pool.clone());
        let now = chrono::Utc::now();
        accounting_store
            .create_quota_bucket(
                &QuotaScope {
                    organization_id: org,
                    team_id: None,
                    service_account_id: Some(service_account.id),
                    api_key_id: None,
                },
                BudgetAmount::new(1_000_000).unwrap(),
                &Currency::new("USD").unwrap(),
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        let manifest: CatalogManifest = serde_json::from_value(serde_json::json!({
            "release": {"version": "resolver"},
            "providers": [{"adapter_instance_key": KEY, "family": "openai"}],
            "deployments": [{
                "name": "deployment", "model": "gpt-test", "location": null,
                "adapter_instance_key": KEY,
                "capabilities": [{"operation": "chat", "max_input_bytes": 4096,
                    "max_output_tokens": 32, "max_tokens_per_byte": 1,
                    "tokenizer": "configured", "reservation_ttl_seconds": 60,
                    "chat_input_count_mode": "unsupported"}]
            }],
            "prices": [{
                "deployment": "deployment", "operation": "chat",
                "input_minor_per_token": 1, "output_minor_per_token": 1,
                "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
                "other_minor_per_token": 0, "currency": "USD", "source": "test",
                "source_version": "resolver", "confidence": "known"
            }],
            "routes": [{"alias": "chat", "operation": "chat", "deployment": "deployment"}]
        }))
        .unwrap();
        let catalog = CatalogStore::new(pool.clone());
        catalog
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();

        // The same wiring as `application::connect`: a process registry for
        // the key, and a resolver over the same process configuration.
        let (base_url, requests) = provider_server();
        let template = ProviderTemplate::OpenAi(openai(base_url, "process-key"));
        let mut registry = ProviderRegistry::new();
        registry.insert(KEY, "openai", template.adapter().unwrap());
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let config = crate::application::config_cache::ConfigCache::load(
            crate::persistence::config_versions::ConfigStore::new(pool.clone()),
            crate::config::InstanceConfig::default(),
            "api",
        )
        .await
        .unwrap();
        let facade = InferenceFacade::from_config(
            AccountingService::new(
                accounting_store,
                Arc::new(registry),
                AccountingPolicy::for_runtime(b"resolver-test-pepper"),
            ),
            config,
            ProviderResolver::new(
                ProviderCredentialStore::new(pool.clone()),
                Some(cipher.clone()),
                [(KEY.to_owned(), template)],
            ),
        );
        let chat = |idempotency_key: &'static str| {
            facade.chat(
                &identity,
                "chat",
                ChatRequest {
                    messages: vec![ChatMessage {
                        role: ChatRole::User,
                        content: "hello".to_owned(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    }],
                    temperature: None,
                    max_tokens: Some(4),
                    ..Default::default()
                },
                idempotency_key,
                CorrelationId::new(),
            )
        };
        let sent = || requests.recv_timeout(Duration::from_secs(5)).unwrap();

        chat("resolver-1").await.unwrap();
        assert_eq!(sent(), "process-key");

        store(&pool, &cipher, org, KEY, ProviderFamily::OpenAi, "org-key").await;
        chat("resolver-2").await.unwrap();
        assert_eq!(sent(), "org-key");

        // Unusable: the request fails before anything reaches the provider.
        store(
            &pool,
            &cipher,
            org,
            KEY,
            ProviderFamily::Anthropic,
            "org-key",
        )
        .await;
        assert!(chat("resolver-3").await.is_err());
        assert!(requests.recv_timeout(Duration::from_millis(200)).is_err());

        sqlx::query("DELETE FROM gateway.provider_credential WHERE organization_id = $1")
            .bind(org.0)
            .execute(&pool)
            .await
            .unwrap();
        chat("resolver-4").await.unwrap();
        assert_eq!(sent(), "process-key");
        pool.close().await;
    }

    /// Model discovery for a provider account on Agent Platform, through the
    /// resolver the worker uses and the real Gemini adapter, against a
    /// stand-in for Google's publisher model listing.
    #[tokio::test]
    async fn discovery_lists_an_agent_platform_account_and_records_drift() {
        use crate::{
            domain::catalog::CatalogManifest,
            domain::discovery::{DiscoveredModelQuery, ModelAvailability},
            gcp_credentials::{CLOUD_PLATFORM_SCOPE, tests as google},
            jobs::discovery::DiscoveryRunner,
            persistence::{catalog::CatalogStore, catalog_sync::CatalogSyncStore},
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let Some(database_url) = std::env::var("GATEWAY_TEST_DATABASE_URL").ok() else {
            assert!(
                std::env::var_os("CI").is_none() && std::env::var_os("GITHUB_ACTIONS").is_none(),
                "GATEWAY_TEST_DATABASE_URL must be set in CI"
            );
            return;
        };
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let identity = crate::persistence::IdentityStore::new(
            pool.clone(),
            crate::domain::keys::ApiKeyPepper::from_bytes(b"resolver-test-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("platform-discovery-{suffix}"), "Platform")
            .await
            .unwrap()
            .id;
        // A key no other test's organization uses, so the worker leaves
        // their accounts to fail as not configured.
        let key = format!("gemini-platform-{}", &suffix[..12]);
        let capability = |operation: &str| {
            serde_json::json!({"operation": operation, "max_input_bytes": 4096,
                "max_output_tokens": 32, "max_tokens_per_byte": 1,
                "tokenizer": "configured", "reservation_ttl_seconds": 60,
                "chat_input_count_mode": "unsupported"})
        };
        let price = |deployment: &str, operation: &str| {
            serde_json::json!({"deployment": deployment, "operation": operation,
                "input_minor_per_token": 1,
                "output_minor_per_token": if operation == "embeddings" { 0 } else { 2 },
                "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
                "other_minor_per_token": 0, "currency": "USD", "source": "test",
                "source_version": "v1", "confidence": "known"})
        };
        let manifest: CatalogManifest = serde_json::from_value(serde_json::json!({
            "release": {"version": "agent-platform"},
            "providers": [{"adapter_instance_key": key, "family": "gemini"}],
            "deployments": [
                {"name": "flash", "model": "gemini-2.5-flash", "location": "us-central1",
                 "adapter_instance_key": key,
                 "capabilities": [capability("chat"), capability("chat_stream")]},
                {"name": "embed", "model": "text-embedding-005", "location": "us-central1",
                 "adapter_instance_key": key, "capabilities": [capability("embeddings")]},
                {"name": "old", "model": "gemini-1.0-pro-002", "location": "us-central1",
                 "adapter_instance_key": key, "capabilities": [capability("chat")]}
            ],
            "prices": [price("flash", "chat"), price("flash", "chat_stream"),
                       price("embed", "embeddings"), price("old", "chat")],
            "routes": [
                {"alias": "fast", "operation": "chat", "deployment": "flash"},
                {"alias": "embed", "operation": "embeddings", "deployment": "embed"}
            ]
        }))
        .unwrap();
        let catalog = CatalogStore::new(pool.clone());
        catalog
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();

        // Google's listing: two pages, then (on the second sync) without
        // `gemini-1.0-pro-002`.
        let first = vec![
            serde_json::json!({"publisherModels": [
                {"name": "publishers/google/models/gemini-1.0-pro-002"},
                {"name": "publishers/google/models/gemini-2.5-flash"},
                {"name": "publishers/google/models/imagen-4.0-generate-001"}
            ], "nextPageToken": "page-2"}),
            serde_json::json!({"publisherModels": [
                {"name": "publishers/google/models/gemini-2.5-flash-image"},
                {"name": "publishers/google/models/text-embedding-005"}
            ]}),
        ];
        let second = vec![serde_json::json!({"publisherModels": [
            {"name": "publishers/google/models/gemini-2.5-flash"},
            {"name": "publishers/google/models/gemini-2.5-flash-image"},
            {"name": "publishers/google/models/text-embedding-005"}
        ]})];
        let listing = Arc::new(Mutex::new(first));
        let paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        {
            let (listing, paths) = (listing.clone(), paths.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let mut head = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(count) => head.extend_from_slice(&buffer[..count]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
                    paths.lock().unwrap().push(path.clone());
                    let pages = listing.lock().unwrap().clone();
                    let page = if path.contains("pageToken=page-2") {
                        1
                    } else {
                        0
                    };
                    // No partner publisher lists a model; the project has one
                    // endpoint serving a model.
                    let body = if path.contains("/publishers/google/") {
                        pages.get(page).cloned().unwrap_or_default().to_string()
                    } else if path.contains("/policies/vertexai.allowedModels") {
                        // The project's policy forbids one model.
                        serde_json::json!({"spec": {"rules": [{"values": {"deniedValues": [
                            "publishers/google/models/gemini-2.5-flash:predict"
                        ]}}]}})
                        .to_string()
                    } else if path.contains("/endpoints") {
                        serde_json::json!({"endpoints": [{
                            "name": "projects/1/locations/us-central1/endpoints/mg-endpoint-1",
                            "displayName": "gemma",
                            "deployedModels": [{"id": "1", "displayName": "gemma-3-1b-it"}]
                        }]})
                        .to_string()
                    } else {
                        "{}".to_owned()
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }

        let fake = google::start().await;
        let dir = google::TempDir::new();
        let google_identity = google::identity(
            &fake,
            &dir.write("adc.json", &google::user_credentials(&fake)),
        );
        let platform = AgentPlatform::new("tenant-project", "us-central1").unwrap();
        let resolver = ProviderResolver::new(
            ProviderCredentialStore::new(pool.clone()),
            None,
            [(
                key.clone(),
                ProviderTemplate::Gemini(GeminiConfig {
                    base_url: format!(
                        "{server}/v1/projects/tenant-project/locations/us-central1/publishers/google"
                    ),
                    auth: GeminiAuth::Google {
                        token: google_identity.token(CLOUD_PLATFORM_SCOPE),
                        quota_project: None,
                    },
                    identity: Some(google_identity),
                    agent_platform: Some(platform),
                    agent_platform_location: None,
                    storage_base_url: None,
                    request_timeout: Duration::from_secs(5),
                    connect_timeout: Duration::from_secs(1),
                    max_response_bytes: 1024 * 1024,
                }),
            )],
        );
        let store = CatalogSyncStore::new(pool.clone());
        let runner = DiscoveryRunner::new(store.clone(), Arc::new(resolver)).with_evidence(
            crate::persistence::model_evidence::ModelEvidenceStore::new(pool.clone()),
        );
        // Other tests' accounts share the database and may be scheduled
        // first, so wait until this organization has a job and none open.
        let drain = || async {
            for _ in 0..500 {
                runner.run_once().await.unwrap();
                let (open, jobs): (i64, i64) = sqlx::query_as(
                    "SELECT count(*) FILTER (WHERE status IN ('queued', 'running')), count(*)
                     FROM gateway.catalog_sync_job WHERE organization_id = $1",
                )
                .bind(org.0)
                .fetch_one(&pool)
                .await
                .unwrap();
                if open == 0 && jobs > 0 {
                    return;
                }
            }
            panic!("the discovery job never finished");
        };
        let job_status = || async {
            sqlx::query_as::<_, (String, Option<String>, Option<i32>, Option<i32>)>(
                "SELECT status, error_code, models_listed, models_missing
                 FROM gateway.catalog_sync_job WHERE organization_id = $1
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(org.0)
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        drain().await;
        assert_eq!(
            job_status().await,
            ("succeeded".to_owned(), None, Some(5), Some(0))
        );
        assert_eq!(
            paths.lock().unwrap()[..7],
            [
                "/v1beta1/publishers/google/models?pageSize=300",
                "/v1beta1/publishers/google/models?pageSize=300&pageToken=page-2",
                // Partner publishers, with none listed.
                "/v1beta1/publishers/anthropic/models?pageSize=300",
                "/v1beta1/publishers/mistralai/models?pageSize=300",
                "/v1beta1/publishers/xai/models?pageSize=300",
                "/v1/projects/tenant-project/locations/us-central1/endpoints?pageSize=100",
                "/v2/projects/tenant-project/policies/vertexai.allowedModels:getEffectivePolicy",
            ]
        );
        // Then the deployed models are probed, concurrently, except the one
        // the policy forbids.
        let probed: std::collections::BTreeSet<String> =
            paths.lock().unwrap()[7..].iter().cloned().collect();
        let count = |model: &str| {
            format!(
                "/v1/projects/tenant-project/locations/us-central1/publishers/google/models/{model}:countTokens"
            )
        };
        assert_eq!(
            probed,
            [count("gemini-1.0-pro-002"), count("text-embedding-005")]
                .into_iter()
                .collect()
        );
        // The listing was signed in with the gateway's Google identity.
        assert!(fake.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);

        let (models, _) = store
            .list_models(org, &DiscoveredModelQuery::default(), 50, None)
            .await
            .unwrap();
        let recorded: Vec<(&str, Option<Vec<String>>, bool)> = models
            .iter()
            .map(|model| {
                (
                    model.model.as_str(),
                    model.operations.clone(),
                    model.deployed,
                )
            })
            .collect();
        let ops = |operations: &[&str]| Some(operations.iter().map(|op| op.to_string()).collect());
        assert_eq!(
            recorded,
            [
                // The project's own endpoint: listed, never offered.
                ("endpoints/mg-endpoint-1", None, false),
                ("gemini-1.0-pro-002", ops(&["chat"]), true),
                ("gemini-2.5-flash", ops(&["chat"]), true),
                // Operations unknown: routing never infers them.
                ("gemini-2.5-flash-image", None, false),
                ("text-embedding-005", ops(&["embeddings"]), true),
            ]
        );
        assert_eq!(models[0].availability, ModelAvailability::EndpointOnly);
        // What the policy forbids needs the tenant's action; what it allows
        // is entitled; the endpoint is not the policy's to judge.
        let evidence = |model: &str| {
            models
                .iter()
                .find(|listed| listed.model == model)
                .and_then(|listed| listed.evidence.as_ref())
                .map(|evidence| {
                    (
                        evidence.state.as_str(),
                        evidence.reason.map(|reason| reason.as_str()),
                    )
                })
        };
        assert_eq!(
            evidence("gemini-2.5-flash"),
            Some(("requires_action", Some("policy_denied")))
        );
        assert_eq!(
            evidence("gemini-1.0-pro-002").map(|(state, _)| state),
            Some("entitled")
        );
        assert_eq!(evidence("endpoints/mg-endpoint-1"), None);
        assert_eq!(
            models[0]
                .deployment
                .as_ref()
                .and_then(|details| details.model.as_deref()),
            Some("gemma-3-1b-it")
        );
        assert!(
            models[1..]
                .iter()
                .all(|model| model.availability == ModelAvailability::Unknown)
        );
        let drift = store.drift(org).await.unwrap().unwrap();
        let account = drift
            .accounts
            .iter()
            .find(|account| account.adapter_instance_key == key)
            .unwrap();
        // The image model and the endpoint.
        assert_eq!(account.undeployed_models, Some(2));
        assert_eq!(account.unlisted_deployments.as_deref(), Some(&[][..]));

        // Google retires a model: the deployment that names it is unlisted.
        *listing.lock().unwrap() = second;
        sqlx::query(
            "INSERT INTO gateway.catalog_sync_job
                 (id, organization_id, adapter_instance_key, family, trigger)
             VALUES ($1, $2, $3, 'gemini', 'manual')",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(&key)
        .execute(&pool)
        .await
        .unwrap();
        drain().await;
        assert_eq!(
            job_status().await,
            ("succeeded".to_owned(), None, Some(4), Some(1))
        );
        let drift = store.drift(org).await.unwrap().unwrap();
        let account = drift
            .accounts
            .iter()
            .find(|account| account.adapter_instance_key == key)
            .unwrap();
        let unlisted = account.unlisted_deployments.as_ref().unwrap();
        assert_eq!(unlisted.len(), 1);
        assert_eq!(unlisted[0].deployment, "old");
        assert!(unlisted[0].missing_since.is_some());
        pool.close().await;
    }
}
