use crate::domain::{
    identity::{ApiKeyScope, VerifiedIdentity},
    inference::{
        ChatRequest, ChatResponse, ChatStreamEvent, CorrelationId, DeploymentModelReference,
        EmbeddingRequest, EmbeddingResponse, InferenceContext, ProviderAttemptIdempotencyKey,
    },
};
use futures::Stream;
use sha2::{Digest, Sha256};
use std::pin::Pin;
use thiserror::Error;

use super::config_cache::ConfigCache;
use super::{
    AccountedChatRequest, AccountingPolicy, AccountingService, AccountingServiceError,
    accounting::Fallbacks,
    circuit_breaker::{CircuitBreakers, CircuitGuardedAdapter, CircuitPermit},
    provider_resolver::ProviderResolver,
};
use crate::domain::accounting::CatalogProvenance;
#[cfg(test)]
use crate::domain::catalog::ChatInputCountMode;
use crate::persistence::catalog::{CatalogStoreError, ResolvedCatalogRoute, route_from_row};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicOperation {
    Chat,
    ChatStream,
    Embeddings,
    /// A line of a chat batch (D-021).
    ChatBatch,
    /// A line of an embeddings batch.
    EmbeddingsBatch,
}

impl PublicOperation {
    fn capability(self) -> ApiKeyScope {
        match self {
            Self::Chat | Self::ChatStream | Self::ChatBatch => ApiKeyScope::Inference,
            Self::Embeddings | Self::EmbeddingsBatch => ApiKeyScope::Embeddings,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::ChatStream => "chat_stream",
            Self::Embeddings => "embeddings",
            Self::ChatBatch => "chat_batch",
            Self::EmbeddingsBatch => "embeddings_batch",
        }
    }
}

#[derive(Clone)]
pub struct PublicModelAlias {
    pub name: String,
    pub operation: PublicOperation,
    pub capability: ApiKeyScope,
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: Option<String>,
    pub provider_family: Option<String>,
    /// Whether every deployment the route may reach takes tools (D-032).
    pub tools: bool,
    /// Whether the deployments the route may reach make tool call ids of
    /// different dialects (D-032), so no conversation with calls can be
    /// replayed to every one of them.
    pub mixed_tool_call_ids: bool,
    /// The tool options a deployment the route may reach refuses (D-032).
    pub tool_option_refusals: crate::domain::catalog::ToolOptionRefusals,
    policy: AccountingPolicy,
}

#[derive(Clone)]
pub struct PublicModelCatalog {
    aliases: Vec<PublicModelAlias>,
}

impl PublicModelCatalog {
    #[cfg(test)]
    pub(crate) fn for_tests(policy: AccountingPolicy) -> Self {
        let chat_policy = policy.clone().with_route("deployment", "model");
        let embedding_policy = policy.with_route("embedding-deployment", "embedding-model");
        Self {
            aliases: vec![
                PublicModelAlias {
                    name: "chat-default".into(),
                    operation: PublicOperation::Chat,
                    capability: ApiKeyScope::Inference,
                    deployment: "deployment".into(),
                    model: "model".into(),
                    adapter_instance_key: None,
                    provider_family: None,
                    tools: true,
                    mixed_tool_call_ids: false,
                    tool_option_refusals: Default::default(),
                    policy: chat_policy,
                },
                PublicModelAlias {
                    name: "chat-default".into(),
                    operation: PublicOperation::ChatStream,
                    capability: ApiKeyScope::Inference,
                    deployment: "deployment".into(),
                    model: "model".into(),
                    adapter_instance_key: None,
                    provider_family: None,
                    tools: true,
                    mixed_tool_call_ids: false,
                    tool_option_refusals: Default::default(),
                    policy: AccountingPolicy::for_tests().with_route("deployment", "model"),
                },
                PublicModelAlias {
                    name: "embedding-default".into(),
                    operation: PublicOperation::Embeddings,
                    capability: ApiKeyScope::Embeddings,
                    deployment: "embedding-deployment".into(),
                    model: "embedding-model".into(),
                    adapter_instance_key: None,
                    provider_family: None,
                    tools: true,
                    mixed_tool_call_ids: false,
                    tool_option_refusals: Default::default(),
                    policy: embedding_policy.clone(),
                },
                PublicModelAlias {
                    name: "chat-default".into(),
                    operation: PublicOperation::ChatBatch,
                    capability: ApiKeyScope::Inference,
                    deployment: "deployment".into(),
                    model: "model".into(),
                    adapter_instance_key: Some("openai-primary".into()),
                    provider_family: Some("openai".into()),
                    tools: true,
                    mixed_tool_call_ids: false,
                    tool_option_refusals: Default::default(),
                    policy: AccountingPolicy::for_tests()
                        .with_route("deployment", "model")
                        .with_chat_input_count_mode(ChatInputCountMode::Unsupported),
                },
                PublicModelAlias {
                    name: "embedding-default".into(),
                    operation: PublicOperation::EmbeddingsBatch,
                    capability: ApiKeyScope::Embeddings,
                    deployment: "embedding-deployment".into(),
                    model: "embedding-model".into(),
                    adapter_instance_key: Some("openai-primary".into()),
                    provider_family: Some("openai".into()),
                    tools: true,
                    mixed_tool_call_ids: false,
                    tool_option_refusals: Default::default(),
                    policy: embedding_policy,
                },
            ],
        }
    }

    pub(crate) fn resolve(
        &self,
        name: &str,
        operation: PublicOperation,
    ) -> Option<&PublicModelAlias> {
        self.aliases.iter().find(|alias| {
            alias.name == name
                && alias.operation == operation
                && alias.capability == operation.capability()
        })
    }

    fn from_route(
        route: ResolvedCatalogRoute,
        base: &AccountingPolicy,
    ) -> Result<PublicModelAlias, InferenceFacadeError> {
        let operation = match route.operation.as_str() {
            "chat" => PublicOperation::Chat,
            "chat_stream" => PublicOperation::ChatStream,
            "embeddings" => PublicOperation::Embeddings,
            "chat_batch" => PublicOperation::ChatBatch,
            "embeddings_batch" => PublicOperation::EmbeddingsBatch,
            _ => PublicOperation::Chat,
        };
        let capability = operation.capability();
        let count_mode = route.chat_input_count_mode;
        let policy = base.clone().with_catalog(
            route.deployment.clone(),
            route.model.clone(),
            route.price.clone(),
            route.currency.clone(),
            CatalogProvenance {
                release_id: route.release_id,
                route_id: route.route_id,
                deployment_id: route.deployment_id,
                price_id: route.price_id,
                digest: route.digest.0,
                operation: route.operation.clone(),
                family: route.family.clone(),
                price_source: route.source.clone(),
                price_override_id: route.price_override_id,
                chat_input_count_mode: count_mode.as_str().to_owned(),
            },
        );
        let policy = policy
            .with_capability_policy(
                usize::try_from(route.max_input_bytes)
                    .map_err(|_| InferenceFacadeError::UnknownModel)?,
                std::num::NonZeroU32::new(route.max_output_tokens)
                    .ok_or(InferenceFacadeError::UnknownModel)?,
                std::num::NonZeroU32::new(route.max_tokens_per_byte)
                    .ok_or(InferenceFacadeError::UnknownModel)?,
                &route.tokenizer,
                std::time::Duration::from_secs(route.reservation_ttl_seconds),
            )
            .with_reasoning_allowance(route.reasoning_allowance_tokens);
        Ok(PublicModelAlias {
            name: String::new(),
            operation,
            capability,
            deployment: route.deployment,
            model: route.model,
            adapter_instance_key: Some(route.adapter_instance_key),
            provider_family: Some(route.family),
            tools: route.tools,
            mixed_tool_call_ids: route.mixed_tool_call_ids,
            tool_option_refusals: route.tool_option_refusals,
            policy,
        })
    }
}

#[derive(Debug, Error)]
pub enum InferenceFacadeError {
    #[error("unknown or incompatible public model alias")]
    UnknownModel,
    #[error("identity lacks the capability bound to the public alias")]
    InsufficientScope,
    #[error(transparent)]
    Catalog(#[from] CatalogStoreError),
    #[error(transparent)]
    Accounting(#[from] AccountingServiceError),
    #[error("the deployment's circuit is open")]
    CircuitOpen { retry_after_seconds: u32 },
    /// The alias routes only to deployments the organization's routing
    /// policy does not permit (D-023).
    #[error("the organization's routing policy permits none of the alias's deployments")]
    RoutingPolicyDenied,
    /// The request uses tools and the alias's route does not take them, or
    /// its provider cannot honour a tool option the request sets (D-032).
    #[error("{0}")]
    ToolsUnsupported(&'static str),
}

/// Refuses a request that uses tools where `alias` cannot serve them as
/// asked (D-032), before anything is reserved.
pub(crate) fn check_tools(
    alias: &PublicModelAlias,
    request: &ChatRequest,
) -> Result<(), InferenceFacadeError> {
    if !request.uses_tools() {
        return Ok(());
    }
    if !alias.tools {
        return Err(InferenceFacadeError::ToolsUnsupported(
            "this model alias does not take tools: not every deployment it routes to is set to take them",
        ));
    }
    if alias.mixed_tool_call_ids {
        return Err(InferenceFacadeError::ToolsUnsupported(
            "this model alias routes to deployments whose tool call ids differ (Gemini's and other providers'), so a conversation's calls cannot be sent back to any of them: for tool calling, an alias's deployments must share a provider family",
        ));
    }
    // The options every deployment the route may reach honours, by the
    // wire each is called through (a partner model on an Agent Platform
    // account goes over its publisher's API, not Gemini's), so a failover
    // never meets a refusal after the request was reserved. The primary's
    // own wire is checked too, for a route resolved without the catalog's.
    let own = alias
        .provider_family
        .as_deref()
        .map(|family| crate::domain::catalog::ToolOptionRefusals::of(family, &alias.model))
        .unwrap_or_default();
    if let Some(refusal) = alias.tool_option_refusals.or(own).refusal(request) {
        return Err(InferenceFacadeError::ToolsUnsupported(refusal));
    }
    Ok(())
}

pub type AccountedEventStream = Pin<
    Box<dyn Stream<Item = Result<ChatStreamEvent, crate::domain::inference::ProviderError>> + Send>,
>;

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn frame_digest(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn provider_attempt_idempotency_key(
    identity: &VerifiedIdentity,
    alias: &PublicModelAlias,
    idempotency_key: &str,
) -> Result<ProviderAttemptIdempotencyKey, InferenceFacadeError> {
    let mut digest = Sha256::new();
    frame_digest(
        &mut digest,
        b"qucelerate-gateway/provider-attempt-idempotency/v1",
    );
    frame_digest(&mut digest, identity.organization_id.0.as_bytes());
    frame_digest(&mut digest, identity.service_account_id.0.as_bytes());
    frame_digest(
        &mut digest,
        alias
            .adapter_instance_key
            .as_deref()
            .unwrap_or("-")
            .as_bytes(),
    );
    frame_digest(&mut digest, idempotency_key.as_bytes());
    ProviderAttemptIdempotencyKey::new(format!(
        "gw-provider-attempt-{}",
        hex_encode(&digest.finalize())
    ))
    .map_err(|_| InferenceFacadeError::UnknownModel)
}

/// A request's route: the deployment it goes to first, those it may fail
/// over to, and the key of the attempt a retry replays.
struct Resolved {
    alias: PublicModelAlias,
    fallbacks: Vec<PublicModelAlias>,
    attempt_key: Option<String>,
}

/// A request admitted to its first deployment.
struct Admitted {
    accounting: AccountingService,
    context: InferenceContext,
    fallbacks: Option<Box<dyn Fallbacks>>,
    accounted: AccountedChatRequest,
}

/// The rest of a request's route, each admitted (its circuit closed) only
/// when the request fails over to it.
struct RouteFallbacks {
    facade: InferenceFacade,
    identity: VerifiedIdentity,
    correlation_id: CorrelationId,
    idempotency_key: String,
    rest: std::collections::VecDeque<PublicModelAlias>,
}

#[async_trait::async_trait]
impl Fallbacks for RouteFallbacks {
    async fn next(&mut self) -> Option<(AccountingService, InferenceContext)> {
        while let Some(alias) = self.rest.pop_front() {
            let Ok(context) = InferenceFacade::context(
                &self.identity,
                &alias,
                self.correlation_id,
                &self.idempotency_key,
            ) else {
                continue;
            };
            if !self.identity.scopes.contains(&alias.capability) {
                continue;
            }
            if let Ok(accounting) = self
                .facade
                .accounting_for(&self.identity, &alias, &context)
                .await
            {
                return Some((accounting, context));
            }
        }
        None
    }
}

#[derive(Clone)]
pub struct InferenceFacade {
    accounting: AccountingService,
    catalog: PublicModelCatalog,
    config: Option<ConfigCache>,
    providers: Option<ProviderResolver>,
    breakers: CircuitBreakers,
    evidence: super::model_evidence::EvidenceRecorder,
    route_evidence: super::routing::RouteEvidence,
}

impl InferenceFacade {
    #[cfg(test)]
    pub(crate) fn new(accounting: AccountingService) -> Self {
        let catalog = PublicModelCatalog::for_tests(accounting.policy());
        Self {
            accounting,
            catalog,
            config: None,
            providers: None,
            breakers: CircuitBreakers::default(),
            evidence: super::model_evidence::EvidenceRecorder::disabled(),
            route_evidence: super::routing::RouteEvidence::default(),
        }
    }

    /// Serves no models: every alias is unknown. For states that never
    /// run inference.
    pub(crate) fn without_models(accounting: AccountingService) -> Self {
        Self {
            accounting,
            catalog: PublicModelCatalog {
                aliases: Vec::new(),
            },
            config: None,
            providers: None,
            breakers: CircuitBreakers::default(),
            evidence: super::model_evidence::EvidenceRecorder::disabled(),
            route_evidence: super::routing::RouteEvidence::default(),
        }
    }

    /// Routes from `config`, the configuration versions this process has
    /// loaded.
    pub(crate) fn from_config(
        accounting: AccountingService,
        config: ConfigCache,
        providers: ProviderResolver,
    ) -> Self {
        Self {
            accounting,
            catalog: PublicModelCatalog {
                aliases: Vec::new(),
            },
            config: Some(config),
            providers: Some(providers),
            breakers: CircuitBreakers::default(),
            evidence: super::model_evidence::EvidenceRecorder::disabled(),
            route_evidence: super::routing::RouteEvidence::default(),
        }
    }

    /// Accounting bound to the alias policy and to the adapter that serves
    /// the request: the organization's stored credential for the routed key
    /// when it has one, otherwise the process adapter. Attempts that did
    /// nothing are retried (`provider_retry`); the adapter reports its final
    /// outcome to the deployment's circuit, which must have admitted the
    /// request first, so a request refused by an open circuit fails before
    /// credentials are loaded or anything is reserved.
    /// Records what this facade's traffic shows about model availability.
    pub(crate) fn with_evidence(
        mut self,
        evidence: super::model_evidence::EvidenceRecorder,
    ) -> Self {
        self.evidence = evidence;
        self
    }

    /// Skips deployments whose stored availability evidence says the
    /// organization cannot call them, where a route has another (D-023).
    pub(crate) fn with_route_evidence(mut self, evidence: super::routing::RouteEvidence) -> Self {
        self.route_evidence = evidence;
        self
    }

    async fn accounting_for(
        &self,
        identity: &VerifiedIdentity,
        alias: &PublicModelAlias,
        context: &InferenceContext,
    ) -> Result<AccountingService, InferenceFacadeError> {
        let permit: CircuitPermit = self
            .breakers
            .admit(identity.organization_id, context)
            .map_err(|open| InferenceFacadeError::CircuitOpen {
                retry_after_seconds: open.retry_after_seconds,
            })?;
        let accounting = self.accounting.with_policy(alias.policy.clone());
        let adapter = match &self.providers {
            Some(providers) => providers
                .resolve(identity.organization_id, context)
                .await
                .map_err(AccountingServiceError::from)?,
            None => None,
        }
        .unwrap_or_else(|| accounting.provider());
        // Evidence sees each call's final outcome, after retries.
        let reporting = super::model_evidence::EvidenceReportingAdapter::new(
            std::sync::Arc::new(super::provider_retry::RetryingAdapter::new(adapter)),
            self.evidence.clone(),
            identity.organization_id,
        );
        Ok(
            accounting.with_provider(std::sync::Arc::new(CircuitGuardedAdapter::new(
                std::sync::Arc::new(reporting),
                permit,
            ))),
        )
    }

    /// The route `name` and `operation` take for this request, and the
    /// deployments it may fail over to. Where the alias routes to several
    /// deployments, a retry under an `idempotency_key` already reserved
    /// replays the request's latest attempt (its own, or the failover that
    /// replaced it): it goes to the deployment that attempt named, whatever
    /// its health is now, under that attempt's key, so it replays rather
    /// than conflicting. Every attempt of a route that may fail over is held
    /// at the highest price of the deployments it may reach (D-023).
    async fn resolve(
        &self,
        identity: &VerifiedIdentity,
        name: &str,
        operation: PublicOperation,
        idempotency_key: Option<&str>,
    ) -> Result<Resolved, InferenceFacadeError> {
        let Some(config) = &self.config else {
            return Ok(Resolved {
                alias: self
                    .catalog
                    .resolve(name, operation)
                    .cloned()
                    .ok_or(InferenceFacadeError::UnknownModel)?,
                fallbacks: Vec::new(),
                attempt_key: idempotency_key.map(str::to_owned),
            });
        };
        let configured = config
            .routes_or_refresh(identity.organization_id, name, operation.as_str())
            .await;
        // Whether the alias may fail over is the configured route's, not
        // what the routing policy leaves of it: a request that failed over
        // before the policy changed still replays its latest attempt.
        let several = configured.len() > 1;
        let forbidden: Vec<uuid::Uuid> = configured
            .iter()
            .filter(|candidate| !candidate.permitted)
            .map(|candidate| candidate.row.route_id)
            .collect();
        let candidates = super::routing::permitted(configured)
            .ok_or(InferenceFacadeError::RoutingPolicyDenied)?;
        let latest = match idempotency_key {
            Some(key) if several => self
                .accounting
                .store()
                .latest_attempt_by_service_account_idempotency(
                    identity.organization_id,
                    identity.service_account_id,
                    key,
                )
                .await
                .map_err(AccountingServiceError::from)?,
            _ => None,
        };
        // A retry whose latest attempt went to a deployment the policy now
        // forbids is refused: replaying it elsewhere under that attempt's
        // key would be a different request.
        if latest
            .as_ref()
            .and_then(|reservation| reservation.catalog_provenance.as_ref())
            .is_some_and(|provenance| forbidden.contains(&provenance.route_id))
        {
            return Err(InferenceFacadeError::RoutingPolicyDenied);
        }
        let selection = super::routing::select(
            identity.organization_id,
            candidates,
            latest
                .as_ref()
                .and_then(|reservation| reservation.catalog_provenance.as_ref())
                .map(|provenance| provenance.route_id),
            super::routing::roll(
                identity.organization_id,
                identity.service_account_id.0,
                idempotency_key,
            ),
            &self.breakers,
            &self.route_evidence,
        )
        .await
        .ok_or(InferenceFacadeError::UnknownModel)?;
        let first = route_from_row(selection.row)?;
        let fallbacks = selection
            .fallbacks
            .into_iter()
            .map(route_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let hold_price = if fallbacks.is_empty() {
            None
        } else {
            crate::domain::accounting::PriceSnapshot::ceiling(
                std::iter::once(&first)
                    .chain(&fallbacks)
                    .map(|route| &route.price),
            )
        };
        let base = self.accounting.policy();
        let alias = |route: ResolvedCatalogRoute| {
            let mut alias = PublicModelCatalog::from_route(route, &base)?;
            alias.policy = alias.policy.with_hold_price(hold_price.clone());
            Ok::<_, InferenceFacadeError>(alias)
        };
        Ok(Resolved {
            alias: alias(first)?,
            fallbacks: fallbacks.into_iter().map(alias).collect::<Result<_, _>>()?,
            attempt_key: latest
                .map(|reservation| reservation.idempotency_key)
                .or_else(|| idempotency_key.map(str::to_owned)),
        })
    }

    /// Accounting for the first of `resolved`'s deployments whose circuit
    /// admits the request, the rest left to fail over to.
    async fn admitted(
        &self,
        identity: &VerifiedIdentity,
        resolved: Resolved,
        correlation_id: CorrelationId,
        idempotency_key: &str,
    ) -> Result<Admitted, InferenceFacadeError> {
        if !identity.scopes.contains(&resolved.alias.capability) {
            return Err(InferenceFacadeError::InsufficientScope);
        }
        let mut rest = std::collections::VecDeque::from(resolved.fallbacks);
        let mut alias = resolved.alias;
        loop {
            let context = Self::context(identity, &alias, correlation_id, idempotency_key)?;
            match self.accounting_for(identity, &alias, &context).await {
                Ok(accounting) => {
                    let fallbacks: Option<Box<dyn Fallbacks>> = (!rest.is_empty()).then(|| {
                        Box::new(RouteFallbacks {
                            facade: self.clone(),
                            identity: identity.clone(),
                            correlation_id,
                            idempotency_key: idempotency_key.to_owned(),
                            rest,
                        }) as Box<dyn Fallbacks>
                    });
                    return Ok(Admitted {
                        accounting,
                        context,
                        fallbacks,
                        accounted: AccountedChatRequest::new(
                            resolved
                                .attempt_key
                                .unwrap_or_else(|| idempotency_key.to_owned()),
                        ),
                    });
                }
                Err(error @ InferenceFacadeError::CircuitOpen { .. }) => {
                    let Some(next) = rest.pop_front() else {
                        return Err(error);
                    };
                    crate::metrics::global().inc(
                        &crate::metrics::ROUTE_FAILOVERS,
                        &[
                            crate::metrics::provider_family_label(alias.provider_family.as_deref()),
                            "circuit_open",
                        ],
                    );
                    alias = next;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn context(
        identity: &VerifiedIdentity,
        alias: &PublicModelAlias,
        correlation_id: CorrelationId,
        idempotency_key: &str,
    ) -> Result<InferenceContext, InferenceFacadeError> {
        let attempt = provider_attempt_idempotency_key(identity, alias, idempotency_key)?;
        let context = InferenceContext::new(
            correlation_id,
            attempt,
            DeploymentModelReference::new(alias.deployment.clone(), alias.model.clone())
                .map_err(|_| InferenceFacadeError::UnknownModel)?,
        );
        Ok(
            match (&alias.adapter_instance_key, &alias.provider_family) {
                (Some(key), Some(family)) => context
                    .with_adapter_instance_key(key.clone())
                    .with_provider_family(family.clone()),
                (Some(key), None) => context.with_adapter_instance_key(key.clone()),
                _ => context,
            },
        )
    }

    /// The route and accounting policy of a batch line's alias, checked
    /// against the identity's capabilities as a single request would be.
    pub(crate) async fn resolve_batch(
        &self,
        identity: &VerifiedIdentity,
        alias: &str,
        operation: PublicOperation,
    ) -> Result<PublicModelAlias, InferenceFacadeError> {
        let alias = self.resolve(identity, alias, operation, None).await?.alias;
        if !identity.scopes.contains(&alias.capability) {
            return Err(InferenceFacadeError::InsufficientScope);
        }
        Ok(alias)
    }

    /// The adapter a batch line run as a single request (emulated) goes
    /// to through `alias`, to check each line as a single request is
    /// checked before it is reserved.
    pub(crate) async fn batch_line_check(
        &self,
        identity: &VerifiedIdentity,
        alias: &PublicModelAlias,
    ) -> Result<BatchLineCheck, InferenceFacadeError> {
        let context = Self::context(identity, alias, CorrelationId::new(), "batch-line-check")?;
        let adapter = match &self.providers {
            Some(providers) => providers
                .resolve(identity.organization_id, &context)
                .await
                .map_err(AccountingServiceError::from)?,
            None => None,
        }
        .unwrap_or_else(|| self.accounting.provider());
        Ok(BatchLineCheck { context, adapter })
    }

    /// The reservation of one batch line on `alias` (from
    /// [`Self::resolve_batch`]), held for `ttl`.
    pub(crate) fn plan_batch_line(
        &self,
        identity: &VerifiedIdentity,
        alias: &PublicModelAlias,
        request: &crate::domain::batch::LineRequest,
        idempotency_key: &str,
        ttl: std::time::Duration,
    ) -> Result<crate::persistence::accounting::ReservationRequest, InferenceFacadeError> {
        let context = Self::context(identity, alias, CorrelationId::new(), idempotency_key)?;
        Ok(self
            .accounting
            .with_policy(alias.policy.clone())
            .plan_batch_line(identity, &context, request, idempotency_key, ttl)?)
    }

    pub async fn chat(
        &self,
        identity: &VerifiedIdentity,
        alias: &str,
        request: ChatRequest,
        idempotency_key: &str,
        correlation_id: CorrelationId,
    ) -> Result<ChatResponse, InferenceFacadeError> {
        let resolved = self
            .resolve(
                identity,
                alias,
                PublicOperation::Chat,
                Some(idempotency_key),
            )
            .await?;
        check_tools(&resolved.alias, &request)?;
        for fallback in &resolved.fallbacks {
            check_tools(fallback, &request)?;
        }
        let admitted = self
            .admitted(identity, resolved, correlation_id, idempotency_key)
            .await?;
        admitted
            .accounting
            .preflight(&admitted.context, Some(&request))?;
        admitted
            .accounting
            .chat_routed(
                identity,
                admitted.context,
                request,
                admitted.accounted,
                admitted.fallbacks,
            )
            .await
            .map_err(Into::into)
    }

    pub async fn chat_stream(
        &self,
        identity: &VerifiedIdentity,
        alias: &str,
        request: ChatRequest,
        idempotency_key: &str,
        correlation_id: CorrelationId,
    ) -> Result<AccountedEventStream, InferenceFacadeError> {
        let resolved = self
            .resolve(
                identity,
                alias,
                PublicOperation::ChatStream,
                Some(idempotency_key),
            )
            .await?;
        check_tools(&resolved.alias, &request)?;
        for fallback in &resolved.fallbacks {
            check_tools(fallback, &request)?;
        }
        let admitted = self
            .admitted(identity, resolved, correlation_id, idempotency_key)
            .await?;
        admitted
            .accounting
            .preflight(&admitted.context, Some(&request))?;
        let stream = admitted
            .accounting
            .chat_stream_routed(
                identity,
                admitted.context,
                request,
                admitted.accounted,
                admitted.fallbacks,
            )
            .await?;
        Ok(Box::pin(stream))
    }

    pub async fn embeddings(
        &self,
        identity: &VerifiedIdentity,
        alias: &str,
        request: EmbeddingRequest,
        idempotency_key: &str,
        correlation_id: CorrelationId,
    ) -> Result<EmbeddingResponse, InferenceFacadeError> {
        let resolved = self
            .resolve(
                identity,
                alias,
                PublicOperation::Embeddings,
                Some(idempotency_key),
            )
            .await?;
        let admitted = self
            .admitted(identity, resolved, correlation_id, idempotency_key)
            .await?;
        admitted.accounting.preflight(&admitted.context, None)?;
        admitted
            .accounting
            .embeddings_routed(
                identity,
                admitted.context,
                request,
                admitted.accounted,
                admitted.fallbacks,
            )
            .await
            .map_err(Into::into)
    }
}

/// Checks batch lines against the adapter their route reaches, from
/// [`InferenceFacade::batch_line_check`].
pub(crate) struct BatchLineCheck {
    context: InferenceContext,
    adapter: std::sync::Arc<dyn crate::provider::InferenceProviderAdapter>,
}

impl BatchLineCheck {
    /// Whether the adapter takes `request`, as a single request: it
    /// refuses locally what it would refuse at send time.
    pub(crate) fn check(
        &self,
        request: &crate::domain::batch::LineRequest,
    ) -> Result<(), crate::domain::inference::ProviderError> {
        match request {
            crate::domain::batch::LineRequest::Chat(request) => {
                self.adapter.validate_chat_request(&self.context, request)
            }
            // Embeddings have no local check beyond the request's own.
            crate::domain::batch::LineRequest::Embeddings(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::{OrganizationId, ServiceAccountId};

    #[test]
    fn test_catalog_binds_distinct_operation_routes_and_capabilities() {
        let catalog = PublicModelCatalog::for_tests(AccountingPolicy::for_tests());
        let chat = catalog
            .resolve("chat-default", PublicOperation::Chat)
            .unwrap();
        let embeddings = catalog
            .resolve("embedding-default", PublicOperation::Embeddings)
            .unwrap();
        assert_eq!(chat.capability, ApiKeyScope::Inference);
        assert_eq!(embeddings.capability, ApiKeyScope::Embeddings);
        assert_ne!(chat.deployment, embeddings.deployment);
        assert_ne!(chat.model, embeddings.model);
        assert!(
            catalog
                .resolve("chat-default", PublicOperation::Embeddings)
                .is_none()
        );
    }

    // `family`/`chat_input_count_mode` are parameterized because catalog
    // validation only ever produces `ProviderApiExact` for families with a
    // real provider-side counting endpoint (e.g. Anthropic), and only ever
    // produces `Unsupported` for families without one (e.g. OpenAI) — see
    // `CatalogManifest`'s validation in `src/domain/catalog.rs`. Fixtures
    // here must respect that pairing to stay representative of what the
    // catalog can actually produce.
    fn catalog_route(
        family: &str,
        operation: &str,
        chat_input_count_mode: ChatInputCountMode,
    ) -> ResolvedCatalogRoute {
        ResolvedCatalogRoute {
            release_id: uuid::Uuid::new_v4(),
            route_id: uuid::Uuid::new_v4(),
            digest: crate::domain::catalog::CatalogDigest([0; 32]),
            deployment_id: uuid::Uuid::new_v4(),
            deployment: "test-deployment".into(),
            model: "test-model".into(),
            adapter_instance_key: "test-primary".into(),
            family: family.into(),
            operation: operation.into(),
            price_id: uuid::Uuid::new_v4(),
            source: "test".into(),
            price_override_id: None,
            max_input_bytes: 4096,
            max_output_tokens: 32,
            max_tokens_per_byte: 1,
            tokenizer: "configured".into(),
            reservation_ttl_seconds: 17,
            chat_input_count_mode,
            reasoning_allowance_tokens: 0,
            tools: false,
            mixed_tool_call_ids: false,
            tool_option_refusals: Default::default(),
            price: crate::domain::accounting::PriceSnapshot::new(1, 1, 0).unwrap(),
            currency: crate::domain::accounting::Currency::new("USD").unwrap(),
        }
    }

    #[test]
    fn exact_count_mode_catalog_chat_route_reaches_configured_adapter() {
        let alias = PublicModelCatalog::from_route(
            catalog_route("anthropic", "chat", ChatInputCountMode::ProviderApiExact),
            &AccountingPolicy::for_tests(),
        )
        .expect("exact-count-mode chat route should resolve");
        let context = InferenceFacade::context(
            &VerifiedIdentity {
                organization_id: OrganizationId::new(),
                service_account_id: ServiceAccountId::new(),
                team_id: None,
                api_key_id: None,
                scopes: vec![ApiKeyScope::Inference],
                end_user: None,
            },
            &alias,
            CorrelationId::new(),
            "chat-key",
        )
        .expect("exact-count-mode chat route should build a provider context");

        assert_eq!(alias.operation, PublicOperation::Chat);
        assert_eq!(
            context.adapter_instance_key.as_deref(),
            Some("test-primary")
        );
        assert_eq!(context.provider_family.as_deref(), Some("anthropic"));
    }

    #[test]
    fn unsupported_count_mode_catalog_chat_route_reaches_configured_adapter() {
        // Regression test for the routing gap Greptile flagged: catalog
        // validation rejects `ProviderApiExact` for families with no
        // provider-side counting endpoint (OpenAI among them), so
        // `from_route` must accept `Unsupported`-mode chat/chat_stream
        // routes rather than requiring `ProviderApiExact` unconditionally.
        let alias = PublicModelCatalog::from_route(
            catalog_route("openai", "chat", ChatInputCountMode::Unsupported),
            &AccountingPolicy::for_tests(),
        )
        .expect("unsupported-count-mode chat route should resolve");
        let context = InferenceFacade::context(
            &VerifiedIdentity {
                organization_id: OrganizationId::new(),
                service_account_id: ServiceAccountId::new(),
                team_id: None,
                api_key_id: None,
                scopes: vec![ApiKeyScope::Inference],
                end_user: None,
            },
            &alias,
            CorrelationId::new(),
            "chat-key",
        )
        .expect("unsupported-count-mode chat route should build a provider context");

        assert_eq!(alias.operation, PublicOperation::Chat);
        assert_eq!(context.provider_family.as_deref(), Some("openai"));
    }

    #[test]
    fn exact_count_mode_catalog_stream_route_reaches_configured_adapter() {
        let alias = PublicModelCatalog::from_route(
            catalog_route(
                "anthropic",
                "chat_stream",
                ChatInputCountMode::ProviderApiExact,
            ),
            &AccountingPolicy::for_tests(),
        )
        .expect("exact-count-mode stream route should resolve");
        let context = InferenceFacade::context(
            &VerifiedIdentity {
                organization_id: OrganizationId::new(),
                service_account_id: ServiceAccountId::new(),
                team_id: None,
                api_key_id: None,
                scopes: vec![ApiKeyScope::Inference],
                end_user: None,
            },
            &alias,
            CorrelationId::new(),
            "stream-key",
        )
        .expect("exact-count-mode stream route should build a provider context");

        assert_eq!(alias.operation, PublicOperation::ChatStream);
        assert_eq!(
            context.adapter_instance_key.as_deref(),
            Some("test-primary")
        );
        assert_eq!(context.provider_family.as_deref(), Some("anthropic"));
    }

    #[test]
    fn provider_attempt_idempotency_key_is_scoped_by_identity_and_adapter() {
        let base_identity = VerifiedIdentity {
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            team_id: None,
            api_key_id: None,
            scopes: vec![ApiKeyScope::Inference],
            end_user: None,
        };
        let same_identity = base_identity.clone();
        let other_org_identity = VerifiedIdentity {
            organization_id: OrganizationId::new(),
            ..base_identity.clone()
        };
        let other_service_account_identity = VerifiedIdentity {
            service_account_id: ServiceAccountId::new(),
            ..base_identity.clone()
        };
        let alias = PublicModelAlias {
            name: "chat".into(),
            operation: PublicOperation::Chat,
            capability: ApiKeyScope::Inference,
            deployment: "deployment".into(),
            model: "model".into(),
            adapter_instance_key: Some("openai-primary".into()),
            provider_family: Some("openai".into()),
            tools: false,
            mixed_tool_call_ids: false,
            tool_option_refusals: Default::default(),
            policy: AccountingPolicy::for_tests(),
        };
        let other_alias = PublicModelAlias {
            adapter_instance_key: Some("openai-secondary".into()),
            ..alias.clone()
        };

        let base = provider_attempt_idempotency_key(&base_identity, &alias, "client-key")
            .expect("provider attempt key");
        let same = provider_attempt_idempotency_key(&same_identity, &alias, "client-key")
            .expect("provider attempt key");
        let other_org = provider_attempt_idempotency_key(&other_org_identity, &alias, "client-key")
            .expect("provider attempt key");
        let other_service_account =
            provider_attempt_idempotency_key(&other_service_account_identity, &alias, "client-key")
                .expect("provider attempt key");
        let other_adapter =
            provider_attempt_idempotency_key(&base_identity, &other_alias, "client-key")
                .expect("provider attempt key");

        assert_eq!(base, same);
        assert_ne!(base, other_org);
        assert_ne!(base, other_service_account);
        assert_ne!(base, other_adapter);
    }

    #[test]
    fn tool_refusals_follow_the_wire_and_the_routes_call_id_dialects() {
        use crate::domain::inference::{ToolChoice, ToolDefinition};
        let alias = |family: &str, model: &str| PublicModelAlias {
            name: "chat".into(),
            operation: PublicOperation::Chat,
            capability: ApiKeyScope::Inference,
            deployment: "deployment".into(),
            model: model.into(),
            adapter_instance_key: Some("key".into()),
            provider_family: Some(family.into()),
            tools: true,
            mixed_tool_call_ids: false,
            tool_option_refusals: Default::default(),
            policy: AccountingPolicy::for_tests(),
        };
        let serial = ChatRequest {
            tools: vec![ToolDefinition {
                name: "get_weather".into(),
                description: None,
                parameters: serde_json::json!({"type": "object"}),
            }],
            parallel_tool_calls: Some(false),
            ..Default::default()
        };
        let refused = |alias: &PublicModelAlias, request: &ChatRequest| {
            matches!(
                check_tools(alias, request),
                Err(InferenceFacadeError::ToolsUnsupported(_))
            )
        };
        // Claude on an Agent Platform account goes over Anthropic's API.
        assert!(!refused(
            &alias("gemini", "anthropic/claude-haiku-4-5@20251001"),
            &serial
        ));
        assert!(refused(&alias("gemini", "gemini-2.5-flash"), &serial));
        assert!(refused(
            &alias("bedrock", "anthropic.claude-haiku-4-5-20251001-v1:0"),
            &ChatRequest {
                parallel_tool_calls: None,
                tool_choice: Some(ToolChoice::None),
                ..serial.clone()
            }
        ));
        // Claude first, Bedrock behind it: the route refuses what Converse
        // cannot do, though the preferred deployment could.
        let claude_then_bedrock = PublicModelAlias {
            tool_option_refusals: crate::domain::catalog::ToolOptionRefusals::of(
                "bedrock",
                "anthropic.claude-haiku-4-5-20251001-v1:0",
            ),
            ..alias("anthropic", "claude-haiku-4-5")
        };
        assert!(!refused(&alias("anthropic", "claude-haiku-4-5"), &serial));
        assert!(refused(&claude_then_bedrock, &serial));
        let none = ChatRequest {
            parallel_tool_calls: None,
            tool_choice: Some(ToolChoice::None),
            ..serial.clone()
        };
        match check_tools(&claude_then_bedrock, &none) {
            Err(InferenceFacadeError::ToolsUnsupported(detail)) => {
                assert!(detail.contains("tool_choice none"), "{detail}");
            }
            _ => unreachable!(),
        }
        // A route mixing Gemini's call ids with others' takes no tools.
        let mixed = PublicModelAlias {
            mixed_tool_call_ids: true,
            tool_option_refusals: Default::default(),
            ..alias("openai", "gpt-4o")
        };
        let plain = ChatRequest {
            parallel_tool_calls: None,
            ..serial.clone()
        };
        assert!(refused(&mixed, &plain));
        match check_tools(&mixed, &plain) {
            Err(InferenceFacadeError::ToolsUnsupported(detail)) => {
                assert!(detail.contains("must share a provider family"));
            }
            _ => unreachable!(),
        }
        // Without tools, the route serves as before.
        assert!(check_tools(&mixed, &ChatRequest::default()).is_ok());
    }

    #[tokio::test]
    async fn an_open_circuit_refuses_before_anything_is_reserved() {
        // Any database use fails fast: the pool never connects.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://gateway_app:opaque-test-value@127.0.0.1:1/gateway")
            .unwrap();
        let facade = InferenceFacade::new(AccountingService::new(
            crate::persistence::AccountingStore::new(pool),
            std::sync::Arc::new(crate::provider::UnavailableProviderAdapter),
            AccountingPolicy::for_tests(),
        ));
        let identity = |organization_id| VerifiedIdentity {
            organization_id,
            service_account_id: ServiceAccountId::new(),
            team_id: None,
            api_key_id: None,
            scopes: vec![ApiKeyScope::Inference],
            end_user: None,
        };
        let tripped = identity(OrganizationId::new());
        let alias = facade
            .resolve(&tripped, "chat-default", PublicOperation::Chat, None)
            .await
            .unwrap()
            .alias;
        let context =
            InferenceFacade::context(&tripped, &alias, CorrelationId::new(), "key").unwrap();
        facade.breakers.trip(tripped.organization_id, &context);
        let request = || ChatRequest {
            messages: vec![],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        };

        let refused = facade
            .chat(
                &tripped,
                "chat-default",
                request(),
                "key",
                CorrelationId::new(),
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(InferenceFacadeError::CircuitOpen { retry_after_seconds }) if retry_after_seconds > 0
            ),
            "{refused:?}"
        );
        // Another organization's circuit is closed; its request reaches the
        // store.
        let other = facade
            .chat(
                &identity(OrganizationId::new()),
                "chat-default",
                request(),
                "key",
                CorrelationId::new(),
            )
            .await;
        assert!(
            !matches!(other, Err(InferenceFacadeError::CircuitOpen { .. })),
            "{other:?}"
        );
    }
}
