//! Model Garden partner models and the project's own endpoints on Agent
//! Platform (`docs/custom-deployments-design.md`).
//!
//! A deployment of `<publisher>/<model>` is called through its publisher's
//! wire API ([`publisher_wire_api`]): Claude through Anthropic's Messages API
//! (`rawPredict`, with the embedded [`AnthropicAdapter`]), Mistral through
//! its chat API (`rawPredict`), and the others through Agent Platform's
//! OpenAI-compatible `endpoints/openapi/chat/completions`, each signed with
//! the account's Google token. A deployment of `endpoints/<id>` is called
//! through what the endpoint serves, which the adapter reads from the
//! endpoint and its deployed models and caches.

use std::time::Instant;

use super::*;
use crate::{
    config::OpenAiCompatibleConfig,
    domain::agent_platform::{AgentPlatformModel, WireApi, publisher_wire_api},
    domain::inference::DeploymentModelReference,
    provider::{anthropic::AnthropicAdapter, openai_compatible::OpenAiCompatibleAdapter},
};

/// How long a read endpoint is trusted before it is read again, beside the
/// re-read after a 404 or 400 (its deployed models may have changed).
const SERVED_ENDPOINT_TTL: Duration = Duration::from_secs(300);
/// Endpoints remembered at once; past it, the cache starts afresh.
const MAX_SERVED_ENDPOINTS: usize = 256;
/// Deployed models read per endpoint (each one `models.get`).
const MAX_DEPLOYED_MODEL_READS: usize = 8;
/// Deployed models one listing reads, across every endpoint.
pub(super) const MAX_LISTING_MODEL_READS: usize = 64;
/// The domain a dedicated endpoint's host must be under: the gateway sends
/// the account's Google token there.
const DEDICATED_ENDPOINT_DOMAIN: &str = ".prediction.vertexai.goog";

/// What a deployment's model names, as the adapter calls it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Target<'a> {
    Google,
    Partner {
        publisher: &'a str,
        model: &'a str,
        wire_api: WireApi,
    },
    Endpoint(&'a str),
}

/// An endpoint as the adapter calls it: what it serves, and the host of a
/// dedicated endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ServedEndpoint {
    pub(super) wire_api: WireApi,
    pub(super) host: Option<String>,
}

pub(super) type ServedEndpoints = std::sync::Mutex<HashMap<String, (Instant, ServedEndpoint)>>;

fn unsupported() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::UnsupportedFeature)
}

/// `path` under the project and location (`/v1/projects/<p>/locations/<l>/`)
/// on the host inference uses, or on `host` (a dedicated endpoint's).
pub(super) fn agent_platform_url(
    base: &Url,
    platform: &AgentPlatform,
    host: Option<&str>,
    path: &str,
) -> Result<Url, ProviderError> {
    let mut url = base.clone();
    if let Some(host) = host {
        url.set_scheme("https").map_err(|_| internal_error())?;
        url.set_host(Some(host)).map_err(|_| internal_error())?;
        url.set_port(None).map_err(|_| internal_error())?;
    }
    url.set_path(&format!(
        "/v1/projects/{}/locations/{}/{path}",
        platform.project, platform.location
    ));
    url.set_query(None);
    Ok(url)
}

/// Claude's Messages API under the account's project and location, signed
/// with its token; `None` off Agent Platform.
pub(super) fn claude_adapter(
    base: &Url,
    config: &GeminiConfig,
) -> Result<Option<AnthropicAdapter>, ProviderError> {
    let Some(platform) = &config.agent_platform else {
        return Ok(None);
    };
    let url = agent_platform_url(base, platform, None, "publishers/anthropic")?;
    AnthropicAdapter::for_agent_platform(
        &crate::config::AnthropicConfig {
            base_url: url.to_string(),
            api_key: String::new(),
            request_timeout: config.request_timeout,
            connect_timeout: config.connect_timeout,
            max_response_bytes: config.max_response_bytes,
        },
        config.auth.clone(),
    )
    .map(Some)
}

/// `context` naming `model` to the codec it is passed to.
fn with_model(context: &InferenceContext, model: &str) -> Result<InferenceContext, ProviderError> {
    let mut context = context.clone();
    context.deployment = DeploymentModelReference::new(context.deployment.deployment(), model)
        .map_err(|_| internal_error())?;
    Ok(context)
}

/// Mistral's body names the model without its `@` version.
fn unversioned(model: &str) -> &str {
    model.split_once('@').map_or(model, |(model, _)| model)
}

impl GeminiAdapter {
    /// What `model` names. A partner model or an endpoint needs Agent
    /// Platform (an API key cannot call them) and a publisher the gateway
    /// calls; anything else is refused before a call. Identifiers without a
    /// `/` are Google's, as before.
    pub(super) fn target<'a>(&self, model: &'a str) -> Result<Target<'a>, ProviderError> {
        match AgentPlatformModel::parse(model) {
            None if !model.contains('/') => Ok(Target::Google),
            Some(AgentPlatformModel::Google(_)) => Ok(Target::Google),
            Some(AgentPlatformModel::Partner {
                publisher,
                model,
                wire_api: Some(wire_api),
            }) if self.agent_platform.is_some() && publisher != "google" => Ok(Target::Partner {
                publisher,
                model,
                wire_api,
            }),
            Some(AgentPlatformModel::Endpoint(id)) if self.agent_platform.is_some() => {
                Ok(Target::Endpoint(id))
            }
            _ => Err(unsupported()),
        }
    }

    fn platform_url(&self, host: Option<&str>, path: &str) -> Result<Url, ProviderError> {
        let platform = self.agent_platform.as_ref().ok_or_else(unsupported)?;
        agent_platform_url(&self.base_url, platform, host, path)
    }

    /// A partner model's own URL: `publishers/<p>/models/<m>:<method>`.
    fn partner_url(
        &self,
        publisher: &str,
        model: &str,
        method: &str,
    ) -> Result<Url, ProviderError> {
        self.platform_url(
            None,
            &format!("publishers/{publisher}/models/{model}:{method}"),
        )
    }

    /// An endpoint's URL: `endpoints/<id><suffix>`, on its dedicated host
    /// when it has one.
    fn endpoint_url(
        &self,
        id: &str,
        served: &ServedEndpoint,
        suffix: &str,
    ) -> Result<Url, ProviderError> {
        self.platform_url(served.host.as_deref(), &format!("endpoints/{id}{suffix}"))
    }

    /// The OpenAI-compatible codec, signed with the account's token, on the
    /// adapter's own client, posting to `chat` (and `stream` when streamed).
    async fn openai_codec(
        &self,
        chat: Url,
        stream: Url,
        stream_usage: bool,
    ) -> Result<OpenAiCompatibleAdapter, ProviderError> {
        let GeminiAuth::Google {
            token,
            quota_project,
        } = &self.auth
        else {
            return Err(unsupported());
        };
        let token = token
            .get_within(self.request_timeout)
            .await
            .map_err(crate::provider::credentials_error)?;
        let config = OpenAiCompatibleConfig {
            base_url: chat.to_string(),
            api_key: Some(token.as_str().to_owned()),
            extra_headers: quota_project
                .iter()
                .map(|project| ("x-goog-user-project".to_owned(), project.clone()))
                .collect(),
            request_stream_usage: stream_usage,
            request_timeout: self.request_timeout,
            connect_timeout: self.request_timeout,
            max_response_bytes: self.max_response_bytes,
            pinned_client: Some(self.client.clone()),
        };
        Ok(OpenAiCompatibleAdapter::new(&config)?
            .with_chat_urls(chat, stream)
            .with_google_errors())
    }

    /// The codec and the context it is called with, for a partner model or
    /// an endpoint that speaks OpenAI's or Mistral's chat API.
    async fn chat_codec(
        &self,
        target: Target<'_>,
        context: &InferenceContext,
    ) -> Result<(OpenAiCompatibleAdapter, InferenceContext), ProviderError> {
        match target {
            Target::Partner {
                publisher,
                model,
                wire_api: WireApi::MistralChat,
            } => {
                let codec = self
                    .openai_codec(
                        self.partner_url(publisher, model, "rawPredict")?,
                        self.partner_url(publisher, model, "streamRawPredict")?,
                        false,
                    )
                    .await?;
                Ok((codec, with_model(context, unversioned(model))?))
            }
            Target::Partner {
                publisher,
                model,
                wire_api: WireApi::OpenAiChat,
            } => {
                let url = self.platform_url(None, "endpoints/openapi/chat/completions")?;
                let codec = self.openai_codec(url.clone(), url, false).await?;
                Ok((codec, with_model(context, &format!("{publisher}/{model}"))?))
            }
            Target::Endpoint(id) => {
                let served = self.served_endpoint(id).await?;
                if served.wire_api != WireApi::OpenAiChat {
                    return Err(internal_error());
                }
                let url = self.endpoint_url(id, &served, "/chat/completions")?;
                // The endpoint serves one model: the body names none, and a
                // vLLM server asks for usage at the end of a stream.
                let codec = self.openai_codec(url.clone(), url, true).await?;
                Ok((codec.omitting_model(), context.clone()))
            }
            _ => Err(internal_error()),
        }
    }

    /// The Gemini URL a request to `target` is posted to, or `None` when
    /// its wire API is another.
    async fn gemini_url(
        &self,
        target: Target<'_>,
        model: &str,
        method: &str,
        stream: bool,
    ) -> Result<Option<Url>, ProviderError> {
        match target {
            Target::Google => self.endpoint(model, method, stream).map(Some),
            Target::Endpoint(id) => {
                let served = self.served_endpoint(id).await?;
                if served.wire_api != WireApi::Gemini {
                    return Ok(None);
                }
                let mut url = self.endpoint_url(id, &served, &format!(":{method}"))?;
                if stream {
                    url.set_query(Some("alt=sse"));
                }
                Ok(Some(url))
            }
            Target::Partner { .. } => Ok(None),
        }
    }

    /// A chat to a partner model or an endpoint; `None` for a Google model,
    /// which the caller sends itself.
    pub(super) async fn routed_chat(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Option<Result<ChatResponse, ProviderError>> {
        let model = context.deployment.model();
        let target = match self.target(model) {
            Ok(Target::Google) => return None,
            Ok(target) => target,
            Err(error) => return Some(Err(error)),
        };
        let result = async {
            if let Target::Partner {
                model,
                wire_api: WireApi::AnthropicMessages,
                ..
            } = target
            {
                let claude = self.claude.as_ref().ok_or_else(unsupported)?;
                return claude
                    .chat(with_model(context, model)?, request.clone())
                    .await;
            }
            if let Some(url) = self
                .gemini_url(target, model, "generateContent", false)
                .await?
            {
                return self.gemini_chat_at(url, model, request).await;
            }
            let (codec, context) = self.chat_codec(target, context).await?;
            codec.chat(context, request.clone()).await
        }
        .await;
        self.forget_if_stale(target, &result);
        Some(result)
    }

    /// [`Self::routed_chat`], streamed.
    pub(super) async fn routed_chat_stream(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Option<Result<ChatEventStream, ProviderError>> {
        let model = context.deployment.model();
        let target = match self.target(model) {
            Ok(Target::Google) => return None,
            Ok(target) => target,
            Err(error) => return Some(Err(error)),
        };
        let result = async {
            if let Target::Partner {
                model,
                wire_api: WireApi::AnthropicMessages,
                ..
            } = target
            {
                let claude = self.claude.as_ref().ok_or_else(unsupported)?;
                return claude
                    .chat_stream(with_model(context, model)?, request.clone())
                    .await;
            }
            if let Some(url) = self
                .gemini_url(target, model, "streamGenerateContent", true)
                .await?
            {
                return self.gemini_stream_at(url, model, request).await;
            }
            let (codec, context) = self.chat_codec(target, context).await?;
            codec.chat_stream(context, request.clone()).await
        }
        .await;
        self.forget_if_stale(target, &result);
        Some(result)
    }

    /// The exact input count of a chat to a partner model or an endpoint:
    /// Claude's free count; the others have none the catalog may rely on.
    /// `None` for a Google model.
    pub(super) async fn routed_count(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Option<Result<u64, ProviderError>> {
        match self.target(context.deployment.model()) {
            Ok(Target::Google) => None,
            Ok(Target::Partner {
                model,
                wire_api: WireApi::AnthropicMessages,
                ..
            }) => Some(match (&self.claude, with_model(context, model)) {
                (Some(claude), Ok(context)) => {
                    claude.count_chat_input_tokens(&context, request).await
                }
                (None, _) => Err(unsupported()),
                (_, Err(error)) => Err(error),
            }),
            Ok(_) => Some(Err(unsupported())),
            Err(error) => Some(Err(error)),
        }
    }

    /// Forgets an endpoint whose call says it may have changed: gone (404),
    /// or refusing what its cached wire API sends (400).
    fn forget_if_stale<T>(&self, target: Target<'_>, result: &Result<T, ProviderError>) {
        if let (Target::Endpoint(id), Err(error)) = (target, result)
            && matches!(error.http_status(), Some(400 | 404))
            && let Ok(mut served) = self.served_endpoints.lock()
        {
            served.remove(id);
        }
    }

    /// How the endpoint `id` is called: read from the endpoint and its
    /// deployed models (`endpoints.get`, `models.get`, in
    /// `roles/aiplatform.user`), then cached. An endpoint whose wire API is
    /// unknown is refused unsent.
    pub(super) async fn served_endpoint(&self, id: &str) -> Result<ServedEndpoint, ProviderError> {
        if let Ok(served) = self.served_endpoints.lock()
            && let Some((read_at, endpoint)) = served.get(id)
            && read_at.elapsed() < SERVED_ENDPOINT_TTL
        {
            return Ok(endpoint.clone());
        }
        let endpoint = self
            .get_json(self.platform_url(None, &format!("endpoints/{id}"))?)
            .await?;
        let mut reads = MAX_DEPLOYED_MODEL_READS;
        let wire_api = self
            .endpoint_wire_api(&endpoint, &mut reads)
            .await
            .ok_or_else(unsupported)?;
        let host = dedicated_host(&endpoint).map_err(|()| unsupported())?;
        let served = ServedEndpoint { wire_api, host };
        if let Ok(mut cache) = self.served_endpoints.lock() {
            if cache.len() >= MAX_SERVED_ENDPOINTS {
                cache.clear();
            }
            cache.insert(id.to_owned(), (Instant::now(), served.clone()));
        }
        Ok(served)
    }

    /// The wire API of everything an endpoint serves: Google splits calls
    /// across its deployed models by `trafficSplit`, and a call cannot pick
    /// one, so every model with a share must speak the same API. `None` when
    /// any is unknown, they differ, the endpoint serves nothing, or reading
    /// a model would exceed `reads`.
    pub(super) async fn endpoint_wire_api(
        &self,
        endpoint: &Value,
        reads: &mut usize,
    ) -> Option<WireApi> {
        let served = served_models(endpoint);
        let mut wire_api = None;
        for deployed in served {
            let api = match deployed_wire_api(deployed) {
                Some(api) => api,
                None => {
                    let name = deployed
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|name| model_resource_name(name))?;
                    *reads = reads.checked_sub(1)?;
                    let url = self.base_url.join(&format!("/v1/{name}")).ok()?;
                    match self.get_json(url).await {
                        Ok(model) => model_wire_api(&model)?,
                        Err(error) => {
                            tracing::debug!(
                                category = ?error.category(),
                                status = ?error.http_status(),
                                "reading an endpoint's deployed model failed; its API is unknown"
                            );
                            return None;
                        }
                    }
                }
            };
            match wire_api {
                None => wire_api = Some(api),
                Some(known) if known == api => {}
                Some(_) => return None,
            }
        }
        wire_api
    }

    /// Whether the probe of a listed model can call it: a Google model, a
    /// partner model of a publisher the gateway calls, or an endpoint
    /// (discovery probes only those whose wire API it found).
    pub(super) fn probes(&self, model: &str) -> bool {
        self.target(model).is_ok()
    }

    /// Whether the probe costs the tenant: a partner model without a free
    /// count (a one-token call Google bills), or an endpoint (a one-token
    /// call; a tuned Gemini endpoint bills tokens).
    pub(super) fn probe_is_billed(&self, model: &str) -> bool {
        match self.target(model) {
            Ok(Target::Partner { wire_api, .. }) => !wire_api.counts_input_exactly(),
            Ok(Target::Endpoint(_)) => true,
            _ => false,
        }
    }

    /// Probes a partner model or an endpoint: Claude with its free count,
    /// the others with a one-token call. `None` for a Google model.
    pub(super) async fn routed_probe(&self, model: &str) -> Option<Result<(), ProviderError>> {
        let target = match self.target(model) {
            Ok(Target::Google) => return None,
            Ok(target) => target,
            Err(error) => return Some(Err(error)),
        };
        let hi = json!([{"role": "user", "content": "hi"}]);
        let result = async {
            let (url, body) = match target {
                Target::Partner {
                    publisher,
                    model,
                    wire_api: WireApi::AnthropicMessages,
                } => (
                    self.partner_url(publisher, "count-tokens", "rawPredict")?,
                    json!({"model": model, "messages": hi}),
                ),
                Target::Partner {
                    publisher,
                    model,
                    wire_api: WireApi::MistralChat,
                } => (
                    self.partner_url(publisher, model, "rawPredict")?,
                    json!({"model": unversioned(model), "messages": hi,
                           "max_tokens": 1, "stream": false}),
                ),
                Target::Partner {
                    publisher, model, ..
                } => (
                    self.platform_url(None, "endpoints/openapi/chat/completions")?,
                    json!({"model": format!("{publisher}/{model}"), "messages": hi,
                           "max_tokens": 1, "stream": false}),
                ),
                Target::Endpoint(id) => {
                    let served = self.served_endpoint(id).await?;
                    match served.wire_api {
                        WireApi::Gemini => (
                            self.endpoint_url(id, &served, ":generateContent")?,
                            json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}],
                                   "generationConfig": {"maxOutputTokens": 1}}),
                        ),
                        _ => (
                            self.endpoint_url(id, &served, "/chat/completions")?,
                            json!({"messages": hi, "max_tokens": 1, "stream": false}),
                        ),
                    }
                }
                Target::Google => return Err(internal_error()),
            };
            self.post_json(url, &body).await.map(drop)
        }
        .await;
        self.forget_if_stale(target, &result);
        Some(result)
    }
}

/// The deployed models an endpoint's calls reach: those with a share of
/// its `trafficSplit`, or every one when it has none.
fn served_models(endpoint: &Value) -> Vec<&Value> {
    let Some(deployed) = endpoint.get("deployedModels").and_then(Value::as_array) else {
        return Vec::new();
    };
    let split = endpoint.get("trafficSplit").and_then(Value::as_object);
    deployed
        .iter()
        .filter(|model| match split {
            None => true,
            Some(split) => model
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| split.get(id))
                .and_then(Value::as_i64)
                .is_some_and(|share| share > 0),
        })
        .collect()
}

/// A deployed model whose wire API its own entry shows: a Google model
/// deployed by its publisher name.
fn deployed_wire_api(deployed: &Value) -> Option<WireApi> {
    let model = deployed.get("model").and_then(Value::as_str)?;
    let publisher_model = model
        .strip_prefix("publishers/")
        .or_else(|| model.split_once("/publishers/").map(|(_, rest)| rest))?;
    let (publisher, rest) = publisher_model.split_once("/models/")?;
    (publisher == "google" && rest.starts_with("gemini-")).then_some(WireApi::Gemini)
}

/// A Model Registry resource name the gateway may read
/// (`projects/<p>/locations/<l>/models/<id>[@<version>]`).
fn model_resource_name(name: &str) -> bool {
    name.starts_with("projects/")
        && name.contains("/models/")
        && name.len() <= 512
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.@".contains(&b))
}

/// What a deployed model (`models.get`) serves, as far as the gateway
/// recognizes it: a Gemini model tuned from a Google base (`genieSource`)
/// speaks Gemini; a Model Garden serving container that implements OpenAI
/// chat completions (vLLM, Hugging Face TGI, SGLang) speaks OpenAI chat.
/// Anything else is unknown, and its endpoint stays `endpoint_only`. Both
/// shapes are *to verify* on a live endpoint (design, milestone 3).
fn model_wire_api(model: &Value) -> Option<WireApi> {
    let base = model
        .pointer("/baseModelSource/genieSource/baseModelUri")
        .and_then(Value::as_str);
    if base.is_some_and(|base| base.contains("gemini")) {
        return Some(WireApi::Gemini);
    }
    let image = model
        .pointer("/containerSpec/imageUri")
        .and_then(Value::as_str)?
        .to_ascii_lowercase();
    ["vllm", "hf-tgi", "text-generation-inference", "sglang"]
        .iter()
        .any(|server| image.contains(server))
        .then_some(WireApi::OpenAiChat)
}

/// A dedicated endpoint's host, `None` for an endpoint on the shared host.
/// `Err` for a dedicated endpoint without a usable host under
/// `prediction.vertexai.goog`, which the shared host does not serve.
pub(super) fn dedicated_host(endpoint: &Value) -> Result<Option<String>, ()> {
    if endpoint
        .get("dedicatedEndpointEnabled")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Ok(None);
    }
    let host = endpoint
        .get("dedicatedEndpointDns")
        .and_then(Value::as_str)
        .ok_or(())?;
    let valid = host.len() <= 253
        && host.ends_with(DEDICATED_ENDPOINT_DOMAIN)
        && !host.starts_with(['.', '-'])
        && !host.contains("..")
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
    valid.then(|| Some(host.to_owned())).ok_or(())
}

/// The listed endpoint `model` with what the listing found it serves: a
/// known wire API makes it callable (`listed`, chat, the API as its
/// format); otherwise it stays `endpoint_only`.
pub(super) fn with_wire_api(
    mut model: DiscoveredModel,
    wire_api: Option<WireApi>,
) -> DiscoveredModel {
    if let Some(wire_api) = wire_api {
        model.availability = ModelAvailability::Listed;
        model.operations = Some(vec![CatalogOperation::Chat]);
        model.deployment.get_or_insert_with(Default::default).format =
            Some(wire_api.as_str().to_owned());
    }
    model
}

/// A listed partner model with its publisher's wire API as its format, so
/// an operator sees how the gateway would call it.
pub(super) fn with_publisher_format(model: DiscoveredModel, publisher: &str) -> DiscoveredModel {
    match publisher_wire_api(publisher) {
        Some(wire_api) => {
            let mut model = model;
            model.deployment.get_or_insert_with(Default::default).format =
                Some(wire_api.as_str().to_owned());
            model
        }
        None => model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_speaks_one_api_or_none() {
        let gemini_model =
            json!({"baseModelSource": {"genieSource": {"baseModelUri": "gemini-2.5-flash"}}});
        let vllm = json!({"containerSpec": {"imageUri":
            "us-docker.pkg.dev/vertex-ai/vertex-vision-model-garden-dockers/pytorch-vllm-serve:20250601"}});
        assert_eq!(model_wire_api(&gemini_model), Some(WireApi::Gemini));
        assert_eq!(model_wire_api(&vllm), Some(WireApi::OpenAiChat));
        assert_eq!(
            model_wire_api(&json!({"containerSpec": {"imageUri": "gcr.io/x/custom-predictor:1"}})),
            None
        );
        assert_eq!(model_wire_api(&json!({})), None);
        assert_eq!(
            deployed_wire_api(&json!({"model": "publishers/google/models/gemini-2.5-flash"})),
            Some(WireApi::Gemini)
        );
        assert_eq!(
            deployed_wire_api(&json!({"model": "projects/1/locations/us-central1/models/9"})),
            None
        );
        // Only models with a share of the traffic are served.
        let endpoint = json!({
            "deployedModels": [{"id": "1"}, {"id": "2"}, {"id": "3"}],
            "trafficSplit": {"1": 0, "2": 100}
        });
        let served: Vec<_> = served_models(&endpoint)
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(served, ["2"]);
        assert_eq!(
            served_models(&json!({"deployedModels": [{"id": "1"}]})).len(),
            1
        );
        assert!(served_models(&json!({})).is_empty());
        assert!(model_resource_name(
            "projects/1/locations/us-central1/models/gemma@1"
        ));
        assert!(!model_resource_name("projects/1/../../x/models/y"));
        assert!(!model_resource_name("https://evil.example/models/x"));
    }

    #[test]
    fn a_dedicated_endpoint_is_called_on_its_own_google_host_only() {
        let dedicated =
            |dns: &str| json!({"dedicatedEndpointEnabled": true, "dedicatedEndpointDns": dns});
        assert_eq!(
            dedicated_host(&dedicated("12.us-central1-345.prediction.vertexai.goog")),
            Ok(Some(
                "12.us-central1-345.prediction.vertexai.goog".to_owned()
            ))
        );
        for bad in [
            "evil.example",
            "prediction.vertexai.goog.evil.example",
            "x..prediction.vertexai.goog",
            "X.prediction.vertexai.goog",
            "a/b.prediction.vertexai.goog",
        ] {
            assert_eq!(dedicated_host(&dedicated(bad)), Err(()), "{bad}");
        }
        assert_eq!(
            dedicated_host(&json!({"dedicatedEndpointEnabled": true})),
            Err(())
        );
        assert_eq!(dedicated_host(&json!({})), Ok(None));
        let platform = AgentPlatform::new("tenant-project", "us-central1").unwrap();
        let base = Url::parse(&platform.base_url()).unwrap();
        assert_eq!(
            agent_platform_url(
                &base,
                &platform,
                Some("12.us-central1-345.prediction.vertexai.goog"),
                "endpoints/12/chat/completions"
            )
            .unwrap()
            .as_str(),
            "https://12.us-central1-345.prediction.vertexai.goog/v1/projects/tenant-project/locations/us-central1/endpoints/12/chat/completions"
        );
        assert_eq!(
            agent_platform_url(
                &base,
                &platform,
                None,
                "publishers/anthropic/models/claude-haiku-4-5:rawPredict"
            )
            .unwrap()
            .as_str(),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/tenant-project/locations/us-central1/publishers/anthropic/models/claude-haiku-4-5:rawPredict"
        );
    }

    #[test]
    fn mistral_names_its_model_without_the_version() {
        assert_eq!(unversioned("mistral-medium-3@001"), "mistral-medium-3");
        assert_eq!(unversioned("codestral-2"), "codestral-2");
    }
}
