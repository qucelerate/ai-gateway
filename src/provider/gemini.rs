use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, Response, StatusCode, Url, header};
use serde_json::{Value, json};
use std::{collections::HashMap, fmt, time::Duration};

use super::{
    ChatEventStream, InferenceProviderAdapter, MAX_MODEL_LIST_PAGES, ValidatingChatEventStream,
    model_list::{PageShape, next_token, push_page, too_many_pages},
};
use crate::{
    config::{AgentPlatform, GeminiAuth, GeminiConfig},
    domain::catalog::CatalogOperation,
    domain::discovery::{DiscoveredModel, ModelAvailability, ModelListBuilder},
    domain::inference::{
        ChatChoice, ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent,
        ChoiceFinished, Embedding, EmbeddingRequest, EmbeddingResponse, FinishReason,
        InferenceContext, ProviderError, ProviderErrorCategory, ResponseCompletion,
        ResponseStarted, SafeMetadata, StreamChoiceDelta, TokenCounter, ToolCall, ToolCallDelta,
        ToolChoice, Usage, UsageSnapshot,
    },
    domain::model_evidence::Entitlement,
};

mod batch;
mod partner;

const PROVIDER: &str = "gemini";
const MAX_ERROR_BODY: usize = 16 * 1024;

pub(crate) struct GeminiAdapter {
    client: Client,
    base_url: Url,
    auth: GeminiAuth,
    /// On Agent Platform, in this project and location, whose model listing
    /// and embeddings differ from the Gemini API's.
    agent_platform: Option<AgentPlatform>,
    /// Service Usage, which reports the project's quotas.
    service_usage_url: Url,
    /// The Organization Policy API, which reports the project's effective
    /// `vertexai.allowedModels` policy.
    org_policy_url: Url,
    /// Cloud Storage, where batch jobs stage on Agent Platform.
    storage_url: Url,
    /// Claude on Agent Platform (Model Garden), through Anthropic's Messages
    /// API under the account's project and location; `None` off it.
    claude: Option<super::anthropic::AnthropicAdapter>,
    /// The project's endpoints as last read for calling them.
    served_endpoints: partner::ServedEndpoints,
    max_response_bytes: usize,
    request_timeout: Duration,
    idle_read_timeout: Duration,
}

impl fmt::Debug for GeminiAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiAdapter")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl GeminiAdapter {
    pub(crate) fn new(config: &GeminiConfig) -> Result<Self, ProviderError> {
        let base_url = Url::parse(&format!("{}/", config.base_url.trim_end_matches('/')))
            .map_err(|_| protocol_error())?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            // A total timeout would terminate an otherwise healthy long-lived
            // stream. Bound each idle read instead; connect timeout remains a
            // separate bound for establishing the request.
            .read_timeout(config.request_timeout)
            .build()
            .map_err(|_| internal_error())?;
        // Unit tests serve every Google API from one local server.
        #[cfg(test)]
        let control_plane = |url: &str| match base_url.host_str() {
            Some("127.0.0.1" | "localhost") => {
                Url::parse(&base_url.origin().ascii_serialization()).map_err(|_| internal_error())
            }
            _ => Url::parse(url).map_err(|_| internal_error()),
        };
        #[cfg(not(test))]
        let control_plane = |url: &str| Url::parse(url).map_err(|_| internal_error());
        Ok(Self {
            client,
            service_usage_url: control_plane(SERVICE_USAGE_URL)?,
            org_policy_url: control_plane(ORG_POLICY_URL)?,
            storage_url: match &config.storage_base_url {
                Some(url) => Url::parse(&format!("{}/", url.trim_end_matches('/')))
                    .map_err(|_| internal_error())?,
                None => control_plane(STORAGE_URL)?,
            },
            claude: partner::claude_adapter(&base_url, config)?,
            served_endpoints: Default::default(),
            base_url,
            auth: config.auth.clone(),
            agent_platform: config.agent_platform.clone(),
            max_response_bytes: config.max_response_bytes,
            request_timeout: config.request_timeout,
            idle_read_timeout: config.request_timeout,
        })
    }

    fn endpoint(&self, model: &str, method: &str, streaming: bool) -> Result<Url, ProviderError> {
        if model.is_empty() || model.contains(['/', '?', '#', ' ', '\n', '\r']) {
            return Err(internal_error());
        }
        let path = if streaming {
            format!("models/{model}:{method}?alt=sse")
        } else {
            format!("models/{model}:{method}")
        };
        self.base_url.join(&path).map_err(|_| internal_error())
    }

    /// `builder` with the key, or a Google token and the project it bills.
    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        google_authorize(&self.auth, builder, self.request_timeout).await
    }

    fn request_body(&self, request: &ChatRequest) -> Result<Value, ProviderError> {
        validate_text_request(request)?;
        validate_gemini_tool_options(request)?;
        let mut contents = Vec::with_capacity(request.messages.len());
        let mut system = None;
        // Tools map to Gemini's own (D-032): an assistant message's calls to
        // `functionCall` parts after its text, with the thought signature
        // the call's id carries, and the `tool` messages that answer them to
        // one user turn of `functionResponse` parts, named after the call.
        let mut results_open = false;
        for (index, message) in request.messages.iter().enumerate() {
            let is_result = message.role == ChatRole::Tool;
            match message.role {
                ChatRole::System => system = Some(message.content.clone()),
                ChatRole::User => {
                    contents.push(json!({"role": "user", "parts": [{"text": message.content}]}))
                }
                ChatRole::Assistant if message.tool_calls.is_empty() => {
                    contents.push(json!({"role": "model", "parts": [{"text": message.content}]}))
                }
                ChatRole::Assistant => {
                    let mut parts = Vec::with_capacity(message.tool_calls.len() + 1);
                    if !message.content.is_empty() {
                        parts.push(json!({"text": message.content}));
                    }
                    for call in &message.tool_calls {
                        let (id, signature) =
                            split_call_id(call.id.as_deref().ok_or_else(invalid_request)?);
                        let mut part = json!({"functionCall": {
                            "id": id,
                            "name": call.name.as_deref().ok_or_else(invalid_request)?,
                            "args": super::tool_arguments(&call.arguments)?,
                        }});
                        if let Some(signature) = signature {
                            part["thoughtSignature"] = json!(signature);
                        }
                        parts.push(part);
                    }
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                ChatRole::Tool => {
                    let call_id = message
                        .tool_call_id
                        .as_deref()
                        .ok_or_else(invalid_request)?;
                    let name = called_name(&request.messages[..index], call_id)
                        .ok_or_else(invalid_request)?;
                    let part = json!({"functionResponse": {
                        "id": split_call_id(call_id).0,
                        "name": name,
                        "response": {"output": message.content},
                    }});
                    match contents.last_mut() {
                        Some(last) if results_open => last["parts"]
                            .as_array_mut()
                            .ok_or_else(invalid_request)?
                            .push(part),
                        _ => contents.push(json!({"role": "user", "parts": [part]})),
                    }
                }
                ChatRole::Developer => return Err(invalid_request()),
            }
            results_open = is_result;
        }
        if contents.is_empty() {
            return Err(invalid_request());
        }
        let mut body = json!({ "contents": contents });
        if !request.tools.is_empty() {
            body["tools"] = json!([{"functionDeclarations": request
                .tools
                .iter()
                .map(|tool| {
                    let mut declaration = json!({
                        "name": tool.name,
                        "parametersJsonSchema": tool.parameters,
                    });
                    if let Some(description) = &tool.description {
                        declaration["description"] = json!(description);
                    }
                    declaration
                })
                .collect::<Vec<_>>()}]);
            if let Some(choice) = &request.tool_choice {
                body["toolConfig"] = json!({"functionCallingConfig": match choice {
                    ToolChoice::Auto => json!({"mode": "AUTO"}),
                    ToolChoice::None => json!({"mode": "NONE"}),
                    ToolChoice::Required => json!({"mode": "ANY"}),
                    ToolChoice::Function(name) => {
                        json!({"mode": "ANY", "allowedFunctionNames": [name]})
                    }
                }});
            }
        }
        if let Some(system) = system {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }
        let mut generation_config = serde_json::Map::new();
        if let Some(temperature) = request.temperature {
            generation_config.insert("temperature".to_owned(), json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            generation_config.insert("maxOutputTokens".to_owned(), json!(max_tokens));
        }
        if !generation_config.is_empty() {
            body["generationConfig"] = Value::Object(generation_config);
        }
        Ok(body)
    }

    async fn request(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let method = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let url = self.endpoint(context.deployment.model(), method, stream)?;
        self.request_at(url, request, stream).await
    }

    /// A Gemini request posted to `url`: a Google model's, or an endpoint
    /// serving Gemini.
    async fn request_at(
        &self,
        url: Url,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let body = self.request_body(request)?;
        self.authorize(
            self.client
                .post(url)
                .header(header::CONTENT_TYPE, "application/json")
                .header(
                    header::ACCEPT,
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
                .json(&body),
        )
        .await
    }

    /// A Gemini chat at `url`, answered for `model`.
    async fn gemini_chat_at(
        &self,
        url: Url,
        model: &str,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let response = send_with_timeout(
            self.request_at(url, request, false).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_response(value, model.to_owned())
    }

    /// [`Self::gemini_chat_at`], streamed.
    async fn gemini_stream_at(
        &self,
        url: Url,
        model: &str,
        request: &ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let response = send_with_timeout(
            self.request_at(url, request, true).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let stream = gemini_sse_stream(
            response.bytes_stream(),
            model.to_owned(),
            self.max_response_bytes,
            self.idle_read_timeout,
        );
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn count_embedding_tokens(
        &self,
        model: &str,
        input: &[String],
    ) -> Result<u64, ProviderError> {
        let contents: Vec<Value> = input
            .iter()
            .map(|text| json!({"parts": [{"text": text}]}))
            .collect();
        let body = json!({ "contents": contents });
        let url = self.endpoint(model, "countTokens", false)?;
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .post(url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .json(&body),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_count_tokens(value)
    }

    /// One page of `GET models`.
    async fn models_page(&self, page_token: Option<&str>) -> Result<Value, ProviderError> {
        let mut url = self.base_url.join("models").map_err(|_| internal_error())?;
        url.query_pairs_mut().append_pair("pageSize", "1000");
        if let Some(page_token) = page_token {
            url.query_pairs_mut().append_pair("pageToken", page_token);
        }
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .get(url)
                    .header(header::ACCEPT, "application/json"),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    /// One page of a publisher's models on Agent Platform. Google publishes
    /// one catalog for every location (there is no listing per project), so
    /// it is listed under `v1beta1`, the version that has it.
    async fn publisher_models_page(
        &self,
        publisher: &str,
        page_token: Option<&str>,
    ) -> Result<Value, ProviderError> {
        self.get_json(self.publisher_models_url(publisher, page_token))
            .await
    }

    /// On the endpoint inference uses, global or regional.
    fn publisher_models_url(&self, publisher: &str, page_token: Option<&str>) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(&format!("/v1beta1/publishers/{publisher}/models"));
        url.set_query(None);
        url.query_pairs_mut().append_pair("pageSize", "300");
        if let Some(page_token) = page_token {
            url.query_pairs_mut().append_pair("pageToken", page_token);
        }
        url
    }

    /// Google's Gemini and embedding models on Agent Platform, following
    /// every page. Their operations are known only where the model family
    /// makes them certain (see [`publisher_model_operations`]): the listing
    /// states none. It cannot say which models the account's location
    /// offers, so each is recorded with unknown availability.
    ///
    /// Partner models (see [`PARTNER_PUBLISHERS`]) follow, as
    /// `<publisher>/<model>` with unknown operations and the wire API the
    /// adapter calls them with as their format (see [`partner`]); like
    /// Google's, their availability is unknown, so they are never offered
    /// automatically. Each with its own request quota in the project
    /// carries [`Entitlement::QuotaZero`] when it is zero, else
    /// [`Entitlement::Entitled`].
    async fn publisher_models(
        &self,
        platform: &AgentPlatform,
    ) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let mut models = ModelListBuilder::default();
        self.google_models(&mut models).await?;
        let mut partners = Vec::new();
        for publisher in PARTNER_PUBLISHERS {
            self.partner_models(publisher, &mut partners).await?;
        }
        if !partners.is_empty() {
            let quota = self.partner_quota(platform).await;
            for model in partners {
                // A positive limit is recorded too, so a raised quota
                // replaces an earlier `quota_zero`.
                let entitlement = quota.as_ref().and_then(|quota| {
                    quota
                        .get(&model.model.replacen('/', "-", 1))
                        .map(|&limit| match limit {
                            0 => Entitlement::QuotaZero,
                            _ => Entitlement::Entitled,
                        })
                });
                models
                    .push_model(model.with_entitlement(entitlement))
                    .map_err(|_| protocol_error())?;
            }
        }
        self.endpoints(platform, &mut models).await?;
        let mut models = models.finish();
        if let Some(policy) = self.model_policy(platform).await {
            for model in &mut models {
                if let Some(allowed) = policy_value(&model.model).map(|value| policy.allows(&value))
                {
                    model.entitlement = Some(match (allowed, model.entitlement) {
                        (false, _) => Entitlement::PolicyDenied,
                        // A zero quota still blocks a model the policy allows.
                        (true, Some(other)) => other,
                        (true, None) => Entitlement::Entitled,
                    });
                }
            }
        }
        Ok(models)
    }

    /// The project's effective `vertexai.allowedModels` organization policy
    /// (D-018), which decides which models it may call: an explanation for
    /// refusals, read only where the tenant allows it (the identity needs
    /// `orgpolicy.policy.get`, in Organization Policy Viewer, and the project
    /// the Organization Policy API). `None`, leaving models unchecked, when it
    /// cannot be read or depends on conditions (tags) the gateway does not
    /// evaluate.
    async fn model_policy(&self, platform: &AgentPlatform) -> Option<ModelPolicy> {
        let url = self
            .org_policy_url
            .join(&format!(
                "v2/projects/{}/policies/vertexai.allowedModels:getEffectivePolicy",
                platform.project
            ))
            .ok()?;
        match self.get_json(url).await {
            Ok(policy) => ModelPolicy::from_effective(&policy),
            Err(error) => {
                tracing::debug!(
                    category = ?error.category(),
                    status = ?error.http_status(),
                    "reading the model access policy failed; models left unchecked"
                );
                None
            }
        }
    }

    /// The project's own endpoints in the location (D-018): models it
    /// deployed to dedicated machines, such as Model Garden deployments and
    /// tuned models, as `endpoints/<id>` with what each serves and on what
    /// machines (see [`listed_endpoint`]). One whose deployed models speak a
    /// wire API the adapter calls is [`ModelAvailability::Listed`], for
    /// chat, with that API as its format; the others are
    /// [`ModelAvailability::EndpointOnly`]: listed, never offered or probed.
    /// Listing them needs `aiplatform.endpoints.list` and reading their
    /// models `aiplatform.models.get`, in `roles/aiplatform.user`, which
    /// inference already requires.
    async fn endpoints(
        &self,
        platform: &AgentPlatform,
        models: &mut ModelListBuilder,
    ) -> Result<(), ProviderError> {
        let mut page_token: Option<String> = None;
        let mut reads = partner::MAX_LISTING_MODEL_READS;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self
                .get_json(self.endpoints_url(platform, page_token.as_deref()))
                .await?;
            // Proto3 JSON omits an empty page's array; anything else that
            // is not a page of endpoints is not an empty listing, which
            // would mark every endpoint recorded before missing.
            let endpoints = match page
                .as_object()
                .ok_or_else(protocol_error)?
                .get("endpoints")
            {
                None => &[][..],
                Some(endpoints) => endpoints.as_array().ok_or_else(protocol_error)?,
            };
            for endpoint in endpoints {
                let Some(model) = listed_endpoint(endpoint) else {
                    continue;
                };
                // A dedicated endpoint without a usable host cannot be
                // called, whatever it serves.
                let wire_api = match partner::dedicated_host(endpoint) {
                    Ok(_) => self.endpoint_wire_api(endpoint, &mut reads).await,
                    Err(()) => None,
                };
                models
                    .push_model(partner::with_wire_api(model, wire_api))
                    .map_err(|_| protocol_error())?;
            }
            match next_token(&page, "nextPageToken") {
                Some(token) => page_token = Some(token.to_owned()),
                None => return Ok(()),
            }
        }
        Err(too_many_pages())
    }

    /// On the endpoint inference uses, global or regional.
    fn endpoints_url(&self, platform: &AgentPlatform, page_token: Option<&str>) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(&format!(
            "/v1/projects/{}/locations/{}/endpoints",
            platform.project, platform.location
        ));
        url.set_query(None);
        url.query_pairs_mut().append_pair("pageSize", "100");
        if let Some(page_token) = page_token {
            url.query_pairs_mut().append_pair("pageToken", page_token);
        }
        url
    }

    async fn google_models(&self, models: &mut ModelListBuilder) -> Result<(), ProviderError> {
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self
                .publisher_models_page("google", page_token.as_deref())
                .await?;
            let mut listed = ModelListBuilder::default();
            push_page(&mut listed, &page, &PUBLISHER_MODEL_PAGE)?;
            for model in listed.finish() {
                let Some(operations) = publisher_model_operations(&model.model) else {
                    continue;
                };
                let model = model
                    .with_operations(operations.unwrap_or_default())
                    .with_availability(ModelAvailability::Unknown);
                models.push_model(model).map_err(|_| protocol_error())?;
            }
            match next_token(&page, "nextPageToken") {
                Some(token) => page_token = Some(token.to_owned()),
                None => return Ok(()),
            }
        }
        Err(too_many_pages())
    }

    /// One partner publisher's models, as `<publisher>/<model>`.
    async fn partner_models(
        &self,
        publisher: &str,
        models: &mut Vec<DiscoveredModel>,
    ) -> Result<(), ProviderError> {
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self
                .publisher_models_page(publisher, page_token.as_deref())
                .await?;
            let mut listed = ModelListBuilder::default();
            push_page(&mut listed, &page, &PARTNER_MODEL_PAGE)?;
            for model in listed.finish() {
                let Some(id) = partner_model_id(publisher, &model.model) else {
                    continue;
                };
                models.push(partner::with_publisher_format(
                    DiscoveredModel { model: id, ..model }
                        .with_availability(ModelAvailability::Unknown),
                    publisher,
                ));
            }
            match next_token(&page, "nextPageToken") {
                Some(token) => page_token = Some(token.to_owned()),
                None => return Ok(()),
            }
        }
        Err(too_many_pages())
    }

    /// The project's per-model request quota for partner models where
    /// requests go, by Service Usage's `base_model` dimension
    /// (`anthropic-claude-haiku-4-5`): the lowest limit of each, 0 where
    /// none is in effect. `None` when it cannot be read (the identity needs
    /// `serviceusage.quotas.get`, in Service Usage Viewer), so the models
    /// are left unchecked.
    async fn partner_quota(&self, platform: &AgentPlatform) -> Option<HashMap<String, i64>> {
        let global = platform.location == "global";
        let metric = if global {
            "global_online_prediction_requests_per_base_model"
        } else {
            "online_prediction_requests_per_base_model"
        };
        let url = self
            .service_usage_url
            .join(&format!(
                "v1beta1/projects/{}/services/aiplatform.googleapis.com/consumerQuotaMetrics/aiplatform.googleapis.com%2F{metric}?view=FULL",
                platform.project
            ))
            .ok()?;
        match self.get_json(url).await {
            Ok(page) => Some(base_model_limits(
                &page,
                (!global).then_some(platform.location.as_str()),
            )),
            Err(error) => {
                tracing::warn!(
                    category = ?error.category(),
                    status = ?error.http_status(),
                    "reading partner model quota failed; partner models left unchecked"
                );
                None
            }
        }
    }

    async fn get_json(&self, url: Url) -> Result<Value, ProviderError> {
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .get(url)
                    .header(header::ACCEPT, "application/json"),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    async fn post_json(&self, url: Url, body: &Value) -> Result<Value, ProviderError> {
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .post(url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .json(body),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    /// Embeddings on Agent Platform. Every chunk is counted (`countTokens`
    /// is free) before the first billable call, so a counting failure bills
    /// nothing; then each chunk is embedded in turn and the request settles
    /// once, on the tokens Google reports, else as estimated from the
    /// counts. A chunk failing after others were billed returns the error
    /// with their usage as partial usage, which is never retried.
    async fn agent_platform_embeddings(
        &self,
        model: &str,
        input: &[String],
    ) -> Result<EmbeddingResponse, ProviderError> {
        let method = AgentPlatformEmbedding::for_model(model)
            .ok_or_else(|| ProviderError::new(ProviderErrorCategory::UnsupportedFeature))?;
        let chunks: Vec<&[String]> = input.chunks(method.max_inputs_per_call()).collect();
        if chunks.len() > MAX_EMBEDDING_CALLS {
            return Err(invalid_request());
        }
        let mut counted = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            counted.push(match method {
                AgentPlatformEmbedding::Predict { .. } => {
                    let body = json!({ "instances": predict_instances(chunk) });
                    let url = self.endpoint(model, "countTokens", false)?;
                    parse_count_tokens(self.post_json(url, &body).await?)?
                }
                AgentPlatformEmbedding::EmbedContent => {
                    match self.count_embedding_tokens(model, chunk).await {
                        Ok(tokens) => tokens,
                        // Google refuses to count for some embedding models
                        // ("CountTokens for embedding model
                        // `gemini-embedding-2` is not yet enabled", 400).
                        // Their `embedContent` reports usage, and the count
                        // only stands in for an unreported one, so take
                        // the conservative bound of a token per byte.
                        Err(error) if error.http_status() == Some(400) => byte_bound(chunk),
                        Err(error) => return Err(error),
                    }
                }
            });
        }
        let mut data = Vec::with_capacity(input.len());
        // Tokens charged for the chunks embedded so far (Google's report,
        // else the count), and whether Google reported every one.
        let mut billed: u64 = 0;
        let mut exact = true;
        for (chunk, counted) in chunks.iter().zip(&counted) {
            let outcome = match method {
                AgentPlatformEmbedding::Predict { .. } => {
                    self.predict_embeddings(model, chunk).await
                }
                AgentPlatformEmbedding::EmbedContent => self.embed_content(model, &chunk[0]).await,
            };
            let embedded = match outcome {
                Ok(embedded) => embedded,
                // Nothing billed yet: the error as any single call fails.
                Err(ChunkFailure::Call(error)) if data.is_empty() => return Err(error),
                Err(failure) => {
                    // What this chunk may have cost: nothing if Google
                    // refused it, else its own report or, failing that, its
                    // count.
                    let (error, cost) = match failure {
                        ChunkFailure::Billed(error, Some(tokens)) => (error, Some((tokens, true))),
                        ChunkFailure::Billed(error, None) => (error, Some((*counted, false))),
                        ChunkFailure::Call(error) if refused(&error) => (error, None),
                        ChunkFailure::Call(error) => (error, Some((*counted, false))),
                    };
                    let (charged, exact) = match cost {
                        Some((tokens, reported)) => {
                            (billed.saturating_add(tokens), exact && reported)
                        }
                        None => (billed, exact),
                    };
                    return Err(with_billed_input(error, charged, exact));
                }
            };
            match embedded.tokens {
                Some(tokens) => billed = billed.saturating_add(tokens),
                None => {
                    billed = billed.saturating_add(*counted);
                    exact = false;
                }
            }
            for vector in embedded.vectors {
                let index = u32::try_from(data.len()).map_err(|_| protocol_error())?;
                data.push(Embedding::new(index, vector).map_err(|_| protocol_error())?);
            }
        }
        let input_tokens = if exact {
            TokenCounter::exact(billed)
        } else {
            TokenCounter::estimated(billed)
        };
        let usage = Usage::embedding(input_tokens).map_err(|_| protocol_error())?;
        EmbeddingResponse::new(
            None,
            model.to_owned(),
            data,
            Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?),
        )
        .map_err(|_| protocol_error())
    }

    async fn predict_embeddings(
        &self,
        model: &str,
        input: &[String],
    ) -> Result<EmbeddedChunk, ChunkFailure> {
        let body = json!({
            "instances": predict_instances(input),
            "parameters": {"autoTruncate": false},
        });
        let url = self
            .endpoint(model, "predict", false)
            .map_err(ChunkFailure::Call)?;
        let value = self
            .post_json(url, &body)
            .await
            .map_err(ChunkFailure::Call)?;
        parse_predict_embeddings(&value, input.len())
    }

    async fn embed_content(&self, model: &str, text: &str) -> Result<EmbeddedChunk, ChunkFailure> {
        let body = json!({
            "content": {"role": "user", "parts": [{"text": text}]},
            "embedContentConfig": {"autoTruncate": false},
        });
        let url = self
            .endpoint(model, "embedContent", false)
            .map_err(ChunkFailure::Call)?;
        let value = self
            .post_json(url, &body)
            .await
            .map_err(ChunkFailure::Call)?;
        parse_embed_content(&value)
    }

    async fn response_error(&self, response: Response) -> ProviderError {
        response_error(response, self.idle_read_timeout).await
    }
}

/// `builder` signed for a Gemini account: the key, or a Google token and the
/// project it bills. Shared with the codecs the adapter embeds for partner
/// models, so every call is the account's.
pub(super) async fn google_authorize(
    auth: &GeminiAuth,
    builder: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::RequestBuilder, ProviderError> {
    // Sensitivity redacts diagnostics only; the request still sends the
    // raw credential.
    let sensitive = |value: &str| {
        header::HeaderValue::from_str(value)
            .map(|mut value| {
                value.set_sensitive(true);
                value
            })
            .map_err(|_| internal_error())
    };
    Ok(match auth {
        GeminiAuth::ApiKey(api_key) => builder.header("x-goog-api-key", sensitive(api_key)?),
        GeminiAuth::Google {
            token,
            quota_project,
        } => {
            let token = token
                .get_within(timeout)
                .await
                .map_err(super::credentials_error)?;
            let builder = builder.header(
                header::AUTHORIZATION,
                sensitive(&format!("Bearer {}", token.as_str()))?,
            );
            match quota_project {
                Some(project) => builder.header(
                    "x-goog-user-project",
                    header::HeaderValue::from_str(project).map_err(|_| internal_error())?,
                ),
                None => builder,
            }
        }
    })
}

/// A refused call's error, from Google's error envelope (or, for a partner
/// model's own refusal, its publisher's).
pub(super) async fn response_error(
    response: Response,
    idle_read_timeout: Duration,
) -> ProviderError {
    let status = response.status();
    let retry_after = retry_after(response.headers());
    let bytes = bounded_bytes(response, MAX_ERROR_BODY, idle_read_timeout)
        .await
        .unwrap_or_default();
    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    status_error(status, &value, retry_after)
}

#[async_trait]
impl InferenceProviderAdapter for GeminiAdapter {
    /// Batch prediction jobs (D-021) on Agent Platform, staged through the
    /// tenant's Cloud Storage bucket. The Gemini API's own batches are not
    /// used: a key's lines run emulated.
    fn staged_batches(
        &self,
        staging: &super::batch::BatchStaging,
    ) -> Option<Box<dyn super::batch::BatchProvider + '_>> {
        batch::StagedGemini::new(self, staging)
            .map(|staged| Box::new(staged) as Box<dyn super::batch::BatchProvider + '_>)
    }

    fn validate_chat_request(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        // A partner model or an endpoint needs Agent Platform and a
        // publisher the gateway calls: refused before any reservation.
        let target = self.target(context.deployment.model())?;
        validate_text_request(request)?;
        // Gemini's tool options bind Google's models only: a partner model
        // is called through its publisher's API (Claude's takes one call at
        // a time), and an endpoint serving Gemini's is checked when its
        // body is built.
        if target == partner::Target::Google {
            validate_gemini_tool_options(request)?;
        }
        Ok(())
    }

    async fn count_chat_input_tokens(
        &self,
        context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<u64, ProviderError> {
        if let Some(counted) = self.routed_count(context, request).await {
            return counted;
        }
        let mut body = self.request_body(request)?;
        if let Some(object) = body.as_object_mut() {
            object.remove("generationConfig");
        }
        let url = self.endpoint(context.deployment.model(), "countTokens", false)?;
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .post(url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .json(&body),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_count_tokens(value)
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        if let Some(response) = self.routed_chat(&context, &request).await {
            return response;
        }
        let response = send_with_timeout(
            self.request(&context, &request, false).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let model = context.deployment.model().to_owned();
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_response(value, model)
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        if let Some(stream) = self.routed_chat_stream(&context, &request).await {
            return stream;
        }
        let response = send_with_timeout(
            self.request(&context, &request, true).await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let model = context.deployment.model().to_owned();
        let bytes = response.bytes_stream();
        let max = self.max_response_bytes;
        let stream = gemini_sse_stream(bytes, model, max, self.idle_read_timeout);
        Ok(ValidatingChatEventStream::new(stream))
    }

    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        if request.input.is_empty() {
            return Err(invalid_request());
        }
        let model = context.deployment.model();
        // Partner models and endpoints are called for chat only.
        if model.contains('/') {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        }
        if self.agent_platform.is_some() {
            return self.agent_platform_embeddings(model, &request.input).await;
        }
        // batchEmbedContents reports no token usage, so source an exact
        // input count from countTokens before generating embeddings. Doing
        // this first (rather than as a fallible follow-up) means a failure
        // here never discards an already-completed embedding, and a retry
        // after failure has not yet issued the billable embedding call.
        let input_tokens = self.count_embedding_tokens(model, &request.input).await?;
        let requests: Vec<Value> = request
            .input
            .iter()
            .map(|text| {
                json!({
                    "model": format!("models/{model}"),
                    "content": {"parts": [{"text": text}]},
                })
            })
            .collect();
        let body = json!({ "requests": requests });
        let url = self.endpoint(model, "batchEmbedContents", false)?;
        let response = send_with_timeout(
            self.authorize(
                self.client
                    .post(url)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json")
                    .json(&body),
            )
            .await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let expected = request.input.len();
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        let data = parse_embedding_vectors(value, expected)?;
        let usage =
            Usage::embedding(TokenCounter::exact(input_tokens)).map_err(|_| protocol_error())?;
        EmbeddingResponse::new(
            None,
            model.to_owned(),
            data,
            Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?),
        )
        .map_err(|_| protocol_error())
    }

    /// Identifiers are listed as `models/{model}`; a deployment names the
    /// model without the prefix.
    /// On Agent Platform, which has no listing for a project: Google's
    /// models (proving the sign-in, impersonation included), then a token
    /// count, which is free, in the project and location requests use
    /// (proving the role there). Listed models may be retired, and Google
    /// serves its newest only at `global` at first (Gemini 4), so a model the
    /// location does not have is skipped and left out, up to
    /// [`MAX_ACCESS_COUNTS`] of them.
    async fn check_access(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let Some(platform) = &self.agent_platform else {
            return self.list_models().await;
        };
        let mut models = self.publisher_models(platform).await?;
        let mut chat_models: Vec<String> = models
            .iter()
            .filter(|model| {
                // A model the project's policy forbids cannot prove access.
                model.entitlement != Some(Entitlement::PolicyDenied)
                    && model
                        .operations
                        .as_ref()
                        .is_some_and(|operations| operations.contains(&CatalogOperation::Chat))
            })
            .map(|model| model.model.clone())
            .collect();
        // Newest first: Google lists retired models (404 in every location)
        // first, and a project may be served only the newest.
        chat_models.sort_by_key(|model| std::cmp::Reverse(gemini_version(model)));
        // Models the location turned out not to have are not offered.
        let mut missing = Vec::new();
        for model in chat_models.iter().take(MAX_ACCESS_COUNTS) {
            match self.count_embedding_tokens(model, &["ok".to_owned()]).await {
                Ok(_) => {
                    models.retain(|listed| !missing.contains(&listed.model));
                    return Ok(models);
                }
                Err(error) if error.http_status() == Some(404) => missing.push(model.clone()),
                Err(error) => return Err(error),
            }
        }
        // Signed in, but no model to count with: the role is unproven.
        Err(ProviderError::new(
            ProviderErrorCategory::UnsupportedFeature,
        ))
    }

    /// Token counting is free on both Gemini APIs, so it is the probe: it
    /// names the model in the project and location requests use (on Agent
    /// Platform), and answers 404 where the model is not served there, as
    /// generating does (D-018). Models counted with `instances` (embedding
    /// models on Agent Platform's `predict`) reject it, which shows nothing.
    fn probes_models(&self) -> bool {
        true
    }

    /// Partner models (`<publisher>/<model>`) of a publisher the gateway
    /// calls, and endpoints (`endpoints/<id>`) whose wire API discovery found
    /// (the others are `endpoint_only`, never probed). Claude is probed with
    /// its free count, the rest with a one-token call.
    fn probes_model(&self, model: &str) -> bool {
        self.probes(model)
    }

    /// A one-token call to a partner model without a free count, or to an
    /// endpoint, is charged to the tenant: made only once a release deploys
    /// it, never for automatic offers.
    fn probes_only_when_deployed(&self, model: &str) -> bool {
        self.probe_is_billed(model)
    }

    async fn probe_model(&self, model: &str) -> Result<(), ProviderError> {
        if let Some(probed) = self.routed_probe(model).await {
            return probed;
        }
        self.count_embedding_tokens(model, &["hi".to_owned()])
            .await
            .map(|_| ())
    }

    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        if let Some(platform) = &self.agent_platform {
            return self.publisher_models(platform).await;
        }
        let mut models = ModelListBuilder::default();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.models_page(page_token.as_deref()).await?;
            push_page(&mut models, &page, &GEMINI_MODEL_PAGE)?;
            match next_token(&page, "nextPageToken") {
                Some(token) => page_token = Some(token.to_owned()),
                None => return Ok(models.finish()),
            }
        }
        Err(too_many_pages())
    }
}

/// The chat models the access check counts tokens with before it gives up,
/// newest first. Counting is free and quick, and a regional location may
/// lack every model newer than the ones it serves.
const MAX_ACCESS_COUNTS: usize = 16;

/// The Organization Policy API's endpoint.
const ORG_POLICY_URL: &str = "https://orgpolicy.googleapis.com/";

/// A project's effective `vertexai.allowedModels` policy, as far as the
/// gateway evaluates it.
#[derive(Debug, PartialEq)]
enum ModelPolicy {
    AllowAll,
    DenyAll,
    /// Listed values (`publishers/<publisher>/models/<model>:<action>`): a
    /// denied value is blocked; with any allowed value, every other is too.
    Values {
        allowed: std::collections::HashSet<String>,
        denied: std::collections::HashSet<String>,
    },
}

impl ModelPolicy {
    /// From `getEffectivePolicy`'s answer: its `spec.rules`, merged down the
    /// hierarchy. `None` for an answer without rules, or with a rule that
    /// has a condition, which depends on tags.
    fn from_effective(policy: &Value) -> Option<Self> {
        let rules = policy.get("spec")?.get("rules")?.as_array()?;
        if rules.is_empty() || rules.iter().any(|rule| rule.get("condition").is_some()) {
            return None;
        }
        let flag = |rule: &Value, key: &str| rule.get(key).and_then(Value::as_bool) == Some(true);
        if rules.iter().any(|rule| flag(rule, "denyAll")) {
            return Some(Self::DenyAll);
        }
        let mut allowed = std::collections::HashSet::new();
        let mut denied = std::collections::HashSet::new();
        let mut allow_all = false;
        for rule in rules {
            if flag(rule, "allowAll") {
                allow_all = true;
                continue;
            }
            let values = rule.get("values")?;
            for (key, into) in [
                ("allowedValues", &mut allowed),
                ("deniedValues", &mut denied),
            ] {
                let Some(listed) = values.get(key) else {
                    continue;
                };
                for value in listed.as_array()? {
                    let value = value.as_str()?;
                    into.insert(value.strip_prefix("is:").unwrap_or(value).to_owned());
                }
            }
        }
        if allow_all {
            allowed.clear();
        }
        Some(if allowed.is_empty() && denied.is_empty() {
            Self::AllowAll
        } else {
            Self::Values { allowed, denied }
        })
    }

    /// Whether the policy lets the project call the model named by `value`.
    /// An explicit denial outranks an allowance.
    fn allows(&self, value: &str) -> bool {
        match self {
            Self::AllowAll => true,
            Self::DenyAll => false,
            Self::Values { allowed, denied } => {
                !denied.contains(value) && (allowed.is_empty() || allowed.contains(value))
            }
        }
    }
}

/// The policy value that governs calling a listed model:
/// `publishers/google/models/<model>:predict`, or
/// `publishers/<publisher>/models/<model>:predict` for a partner model
/// (`<publisher>/<model>`). `None` for an endpoint, which the policy's
/// `deploy` action governed when it was created.
fn policy_value(model: &str) -> Option<String> {
    if model.starts_with("endpoints/") {
        return None;
    }
    Some(match model.split_once('/') {
        Some((publisher, model)) => format!("publishers/{publisher}/models/{model}:predict"),
        None => format!("publishers/google/models/{model}:predict"),
    })
}

/// Service Usage's endpoint.
const SERVICE_USAGE_URL: &str = "https://serviceusage.googleapis.com/";
const STORAGE_URL: &str = "https://storage.googleapis.com/";

/// Partner publishers whose models Agent Platform lists in Model Garden and
/// serves under their own quota and terms.
const PARTNER_PUBLISHERS: [&str; 3] = ["anthropic", "mistralai", "xai"];

/// A partner's models: `publishers/<publisher>/models/<id>`, kept as
/// `<publisher>/models/<id>` for [`partner_model_id`].
const PARTNER_MODEL_PAGE: PageShape = PageShape {
    items: "publisherModels",
    id: "name",
    display_name: None,
    id_prefix: Some("publishers/"),
    omits_empty: true,
};

/// A listed endpoint as `endpoints/<id>`, named by its display name, with
/// the deployed model that takes the most of its traffic: that model's
/// name, version, machines (`g2-standard-12 + 1 NVIDIA_L4`) and minimum
/// replicas, as Google reports them. `None` for an endpoint serving nothing
/// (being created, or with every model undeployed), which has nothing to
/// call, or one whose name is not `.../endpoints/<id>`. It is
/// `endpoint_only` until [`partner::with_wire_api`] records what it speaks.
fn listed_endpoint(endpoint: &Value) -> Option<DiscoveredModel> {
    fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        value.get(key).and_then(Value::as_str)
    }
    let (_, id) = text(endpoint, "name")?.rsplit_once("/endpoints/")?;
    if id.is_empty() || id.contains('/') {
        return None;
    }
    let deployed = endpoint.get("deployedModels")?.as_array()?;
    let share = |model: &Value| {
        text(model, "id")
            .and_then(|id| endpoint.get("trafficSplit")?.get(id)?.as_i64())
            .unwrap_or(0)
    };
    // The first of those with the largest share, where several tie.
    let served = deployed.iter().rev().max_by_key(|model| share(model))?;
    let resources = served
        .get("dedicatedResources")
        .or_else(|| served.get("automaticResources"));
    let machine = resources.and_then(|resources| resources.get("machineSpec"));
    let machine = machine.and_then(|spec| {
        let machine_type = text(spec, "machineType")?;
        Some(
            match (
                text(spec, "acceleratorType"),
                spec.get("acceleratorCount").and_then(Value::as_i64),
            ) {
                (Some(accelerator), Some(count)) if count > 0 => {
                    format!("{machine_type} + {count} {accelerator}")
                }
                (Some(accelerator), None) => format!("{machine_type} + {accelerator}"),
                _ => machine_type.to_owned(),
            },
        )
    });
    let model = text(served, "displayName").or_else(|| {
        text(served, "model").and_then(|name| name.rsplit_once("/models/").map(|(_, id)| id))
    });
    let replicas = resources
        .and_then(|resources| resources.get("minReplicaCount"))
        .and_then(Value::as_i64);
    let details = crate::domain::discovery::DeploymentDetails::new(
        None,
        model,
        text(served, "modelVersionId"),
        machine.as_deref(),
        replicas,
    );
    Some(
        DiscoveredModel::new(&format!("endpoints/{id}"), text(endpoint, "displayName"))?
            .with_availability(ModelAvailability::EndpointOnly)
            .with_deployment(details),
    )
}

/// `<publisher>/<id>` for a listed `<publisher>/models/<id>`, or `None` for
/// an entry of another publisher or another shape.
fn partner_model_id(publisher: &str, listed: &str) -> Option<String> {
    let id = listed.strip_prefix(publisher)?.strip_prefix("/models/")?;
    (!id.is_empty() && !id.contains('/')).then(|| format!("{publisher}/{id}"))
}

/// The lowest limit Service Usage reports for each `base_model` in a
/// `consumerQuotaMetrics` answer, in `region` (or the metric with no
/// region). A bucket without `effectiveLimit` is 0: proto3 JSON omits
/// zero. Buckets for other regions, or for no model, are skipped.
fn base_model_limits(page: &Value, region: Option<&str>) -> HashMap<String, i64> {
    let mut limits = HashMap::new();
    let buckets = page
        .get("consumerQuotaLimits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|limit| limit.get("quotaBuckets").and_then(Value::as_array))
        .flatten();
    for bucket in buckets {
        let dimensions = bucket.get("dimensions");
        let dimension = |name: &str| {
            dimensions
                .and_then(|dimensions| dimensions.get(name))
                .and_then(Value::as_str)
        };
        let Some(model) = dimension("base_model") else {
            continue;
        };
        if dimension("region") != region {
            continue;
        }
        let limit = match bucket.get("effectiveLimit") {
            None => 0,
            Some(limit) => match limit
                .as_str()
                .and_then(|limit| limit.parse::<i64>().ok())
                .or_else(|| limit.as_i64())
            {
                Some(limit) => limit,
                // An unreadable limit says nothing.
                None => continue,
            },
        };
        limits
            .entry(model.to_owned())
            .and_modify(|lowest: &mut i64| *lowest = (*lowest).min(limit))
            .or_insert(limit);
    }
    limits
}

/// Google's models on Agent Platform: `publishers/google/models/<id>`.
const PUBLISHER_MODEL_PAGE: PageShape = PageShape {
    items: "publisherModels",
    id: "name",
    display_name: None,
    id_prefix: Some("publishers/google/models/"),
    omits_empty: true,
};

/// A Gemini model's version, `gemini-<major>[.<minor>]-…`, for ordering.
fn gemini_version(model: &str) -> (u32, u32) {
    let version = model
        .strip_prefix("gemini-")
        .and_then(|rest| rest.split('-').next())
        .unwrap_or("");
    let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
    (major.parse().unwrap_or(0), minor.parse().unwrap_or(0))
}

/// What the gateway can say of a model in Google's catalog: `None` for a
/// model this adapter does not call (Imagen, Veo, partner and open models);
/// otherwise its operations where the family makes them certain, else
/// `Some(None)`, unknown.
///
/// The listing states no operations (`supportedActions` names console and
/// deployment actions, not API methods), so they come from the identifier
/// alone: embedding families embed and nothing else, and a Gemini text
/// model named `gemini-<version>-<pro|flash|flash-lite>`, optionally with a
/// `-NNN` version or a `-preview` date, chats, streamed or not. Any other
/// Gemini model (image, speech, live, computer use, an alias, an
/// experiment) has unknown operations, so routing fails closed on it until
/// an operator sets them in the catalog.
fn publisher_model_operations(model: &str) -> Option<Option<&'static [CatalogOperation]>> {
    const CHAT: &[CatalogOperation] = &[CatalogOperation::Chat];
    const EMBEDDINGS: &[CatalogOperation] = &[CatalogOperation::Embeddings];
    if AgentPlatformEmbedding::for_model(model).is_some() {
        return Some(Some(EMBEDDINGS));
    }
    let rest = model.strip_prefix("gemini-")?;
    let chat = rest.split_once('-').is_some_and(|(version, rest)| {
        let numeric = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        let version_ok = match version.split_once('.') {
            Some((major, minor)) => numeric(major) && numeric(minor),
            None => numeric(version),
        };
        let suffix = ["flash-lite", "flash", "pro"]
            .iter()
            .find_map(|tier| rest.strip_prefix(tier));
        version_ok
            && suffix.is_some_and(|suffix| {
                suffix.is_empty()
                    || suffix
                        .strip_prefix('-')
                        .is_some_and(|version| version.len() == 3 && numeric(version))
                    || suffix.strip_prefix("-preview").is_some_and(|date| {
                        date.is_empty()
                            || date
                                .strip_prefix('-')
                                .is_some_and(|date| date.split('-').all(numeric))
                    })
            })
    });
    Some(chat.then_some(CHAT))
}

/// Most calls one embeddings request is split into. With one input a call
/// (`gemini-embedding-*`), a request may carry at most this many inputs;
/// more are refused as `invalid_request` before anything is sent.
const MAX_EMBEDDING_CALLS: usize = 16;

/// How Agent Platform embeds text with a model, after Google's Gen AI SDK
/// (`google-genai`, `models.embed_content` and
/// `_transformers.t_is_vertex_embed_content_model`, 2026-10) and the
/// `aiplatform` v1 discovery document (revision 20261007):
///
/// - `text-embedding-*`, `text-multilingual-embedding-*` and
///   `gemini-embedding-001`: `POST .../models/{model}:predict` with
///   `{"instances": [{"content": ...}], "parameters": {"autoTruncate":
///   false}}`, answering `{"predictions": [{"embeddings": {"values": [...],
///   "statistics": {"token_count": n, "truncated": bool}}}]}`. Google's
///   text-embeddings guide limits a call to 250 inputs and 20,000 tokens (a
///   larger call is refused, 400) and `gemini-embedding-001` to one input;
///   Google's quota page has allowed it more, so one is the safe reading.
/// - Other `gemini-embedding-*`: `POST .../models/{model}:embedContent`
///   with `{"content": {"role": "user", "parts": [{"text": ...}]},
///   "embedContentConfig": {"autoTruncate": false}}`, one input a call
///   (the SDK refuses more), answering `{"embedding":
///   {"values": [...]}, "usageMetadata": {"promptTokenCount": n},
///   "truncated": bool}`.
///
/// `countTokens` takes the same `instances` for a `predict` model, and
/// `contents` for the others. The inference API has no task type or output
/// dimensionality, so neither is sent: Google's defaults apply.
/// `autoTruncate: false` makes Google refuse an input longer than the model
/// takes (400, `invalid_request`) rather than embed part of it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentPlatformEmbedding {
    Predict { max_inputs: usize },
    EmbedContent,
}

impl AgentPlatformEmbedding {
    fn for_model(model: &str) -> Option<Self> {
        if model.starts_with("text-embedding-") || model.starts_with("text-multilingual-embedding-")
        {
            Some(Self::Predict { max_inputs: 250 })
        } else if model == "gemini-embedding-001" {
            Some(Self::Predict { max_inputs: 1 })
        } else if model.starts_with("gemini-embedding-") {
            Some(Self::EmbedContent)
        } else {
            None
        }
    }

    fn max_inputs_per_call(self) -> usize {
        match self {
            Self::Predict { max_inputs } => max_inputs,
            Self::EmbedContent => 1,
        }
    }
}

/// One chunk's vectors and the tokens Google reported for it, if it did.
struct EmbeddedChunk {
    vectors: Vec<Vec<f32>>,
    tokens: Option<u64>,
}

enum ChunkFailure {
    /// The call failed; whether Google ran it depends on how.
    Call(ProviderError),
    /// Google answered and billed, but the answer cannot be returned (an
    /// input it truncated, a malformed body); with the tokens it reported.
    Billed(ProviderError, Option<u64>),
}

/// An error answer: Google refused the call, so nothing was billed.
fn refused(error: &ProviderError) -> bool {
    error.was_not_sent()
        || error
            .http_status()
            .is_some_and(|status| (400..500).contains(&status))
}

/// `error` carrying the input tokens billed before it as partial usage.
fn with_billed_input(error: ProviderError, tokens: u64, exact: bool) -> ProviderError {
    let input = if exact {
        TokenCounter::exact(tokens)
    } else {
        TokenCounter::estimated(tokens)
    };
    error
        .with_partial_usage(Usage::partial(
            Some(input),
            Some(TokenCounter::exact(0)),
            None,
            None,
            None,
        ))
        .unwrap_or_else(|_| protocol_error())
}

/// At most one token per byte of `input`: the bound when Google will not
/// count it.
fn byte_bound(input: &[String]) -> u64 {
    let bytes: usize = input.iter().map(String::len).sum();
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn predict_instances(input: &[String]) -> Vec<Value> {
    input
        .iter()
        .map(|text| json!({ "content": text }))
        .collect()
}

/// A token count as Google writes it: an integer, sometimes as a float.
fn token_count(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|count| count.fract() == 0.0 && *count >= 0.0 && *count <= u32::MAX as f64)
            .map(|count| count as u64)
    })
}

/// A vector `Embedding::new` accepts: not empty, every value finite as an
/// `f32`. Checked here, so a billed answer with an unusable vector fails
/// its chunk with the tokens it cost.
fn vector(value: Option<&Value>) -> Result<Vec<f32>, ProviderError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|value| value as f32)
                .filter(|value| value.is_finite())
                .ok_or_else(protocol_error)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if values.is_empty() {
        return Err(protocol_error());
    }
    Ok(values)
}

/// A `predict` answer for `expected` inputs. Google reports each input's
/// tokens; if any is missing, the chunk's tokens are unknown. A truncated
/// input fails the chunk, billed.
fn parse_predict_embeddings(value: &Value, expected: usize) -> Result<EmbeddedChunk, ChunkFailure> {
    let billed = |tokens| ChunkFailure::Billed(protocol_error(), tokens);
    let Some(predictions) = value.get("predictions").and_then(Value::as_array) else {
        return Err(billed(None));
    };
    let mut tokens = Some(0_u64);
    let mut truncated = false;
    let mut vectors = Vec::with_capacity(predictions.len());
    let mut malformed = predictions.len() != expected;
    for prediction in predictions {
        let embeddings = prediction.get("embeddings");
        let statistics = embeddings.and_then(|embeddings| embeddings.get("statistics"));
        let count = statistics
            .and_then(|statistics| statistics.get("token_count"))
            .and_then(token_count);
        tokens = tokens
            .zip(count)
            .map(|(sum, count)| sum.saturating_add(count));
        truncated |= statistics
            .and_then(|statistics| statistics.get("truncated"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match vector(embeddings.and_then(|embeddings| embeddings.get("values"))) {
            Ok(values) => vectors.push(values),
            Err(_) => malformed = true,
        }
    }
    if truncated || malformed {
        return Err(billed(tokens));
    }
    Ok(EmbeddedChunk { vectors, tokens })
}

/// An `embedContent` answer: one vector, and `usageMetadata` when Google
/// reports it.
fn parse_embed_content(value: &Value) -> Result<EmbeddedChunk, ChunkFailure> {
    let tokens = value
        .get("usageMetadata")
        .and_then(|usage| usage.get("promptTokenCount"))
        .and_then(token_count);
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let values = vector(
        value
            .get("embedding")
            .and_then(|embedding| embedding.get("values")),
    );
    match values {
        Ok(values) if !truncated => Ok(EmbeddedChunk {
            vectors: vec![values],
            tokens,
        }),
        _ => Err(ChunkFailure::Billed(protocol_error(), tokens)),
    }
}

const GEMINI_MODEL_PAGE: PageShape = PageShape {
    items: "models",
    id: "name",
    display_name: Some("displayName"),
    id_prefix: Some("models/"),
    omits_empty: true,
};

/// Whether a batch prediction job can take `request` as a line: what
/// `generateContent` takes.
pub(crate) fn validate_batch_line(request: &ChatRequest) -> Result<(), ProviderError> {
    validate_text_request(request)?;
    validate_gemini_tool_options(request)
}

fn validate_text_request(request: &ChatRequest) -> Result<(), ProviderError> {
    let mut system_seen = false;
    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            ChatRole::System if index == 0 && !system_seen => system_seen = true,
            ChatRole::System => return Err(invalid_request()),
            ChatRole::Developer => return Err(invalid_request()),
            ChatRole::User | ChatRole::Assistant | ChatRole::Tool => {}
        }
    }
    if request.messages.is_empty() || request.messages.iter().all(|m| m.role == ChatRole::System) {
        return Err(invalid_request());
    }
    Ok(())
}

/// Gemini cannot be asked for one call at a time; refused rather than sent
/// without it (D-032). Only what goes over Gemini's own API.
fn validate_gemini_tool_options(request: &ChatRequest) -> Result<(), ProviderError> {
    if request.parallel_tool_calls == Some(false) {
        return Err(invalid_request());
    }
    Ok(())
}

/// What joins a call's id to the thought signature Gemini gave with it.
/// Gemini 3 needs the signature back with the call when the conversation
/// is replayed, and the contract's call has only its id to carry it in
/// (D-032). Base64 signatures never contain it.
const SIGNATURE_SEPARATOR: &str = "~sig~";

/// A call's id as the contract carries it: Gemini's own, or one made from
/// the call's place in the response when Gemini gives none, with its
/// thought signature if any.
fn call_id(id: Option<&str>, index: u32, signature: Option<&str>) -> String {
    let id = id.map_or_else(|| format!("call_{index}"), str::to_owned);
    match signature {
        Some(signature) => format!("{id}{SIGNATURE_SEPARATOR}{signature}"),
        None => id,
    }
}

/// A call id's Gemini id and thought signature.
fn split_call_id(id: &str) -> (&str, Option<&str>) {
    match id.split_once(SIGNATURE_SEPARATOR) {
        Some((id, signature)) => (id, Some(signature)),
        None => (id, None),
    }
}

/// The name of the call `call_id` answers, from the assistant messages
/// before it: Gemini matches a response to its call by name.
fn called_name<'a>(before: &'a [ChatMessage], call_id: &str) -> Option<&'a str> {
    before
        .iter()
        .rev()
        .flat_map(|message| &message.tool_calls)
        .find(|call| call.id.as_deref() == Some(call_id))
        .and_then(|call| call.name.as_deref())
}

/// A candidate's `functionCall` parts as tool calls, numbered from `first`
/// for the ids Gemini does not give (D-032).
fn extract_calls(candidate: &Value, first: u32) -> Result<Vec<ToolCall>, ProviderError> {
    let Some(parts) = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
    else {
        return Ok(Vec::new());
    };
    let mut calls = Vec::new();
    for part in parts {
        let Some(call) = part.get("functionCall") else {
            continue;
        };
        let name = call
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?;
        let arguments = match call.get("args") {
            None | Some(Value::Null) => "{}".to_owned(),
            Some(args) if args.is_object() => {
                serde_json::to_string(args).map_err(|_| protocol_error())?
            }
            Some(_) => return Err(protocol_error()),
        };
        let index = first + u32::try_from(calls.len()).map_err(|_| protocol_error())?;
        calls.push(ToolCall {
            id: Some(call_id(
                call.get("id").and_then(Value::as_str),
                index,
                part.get("thoughtSignature").and_then(Value::as_str),
            )),
            name: Some(name.to_owned()),
            arguments,
        });
    }
    Ok(calls)
}

/// Gemini's finish reason in the contract's terms: a turn that stops with
/// calls finished to call tools (D-032).
fn finish_reason_of(reason: &str, called: bool) -> FinishReason {
    FinishReason::new(if called && reason == "STOP" {
        "tool_calls"
    } else {
        reason
    })
}

fn parse_response(value: Value, model: String) -> Result<ChatResponse, ProviderError> {
    let id = value
        .get("responseId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let candidates = value
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    if candidates.len() != 1 {
        return Err(protocol_error());
    }
    let candidate = &candidates[0];
    if candidate
        .get("index")
        .is_some_and(|index| index.as_u64() != Some(0))
    {
        return Err(protocol_error());
    }
    let text = extract_text(candidate)?;
    let tool_calls = extract_calls(candidate, 0)?;
    let reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map(|reason| finish_reason_of(reason, !tool_calls.is_empty()));
    let usage = parse_usage(value.get("usageMetadata").ok_or_else(protocol_error)?)?;
    let choice = ChatChoice {
        index: 0,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content: text,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
        finish_reason: reason,
        tool_calls,
        refusal: None,
        safety: vec![],
        annotations: vec![],
    };
    ChatResponse::new(
        id,
        model,
        vec![choice],
        Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?),
        SafeMetadata::default(),
    )
    .map_err(|_| protocol_error())
}

fn extract_text(candidate: &Value) -> Result<String, ProviderError> {
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    let mut text = String::new();
    for part in parts {
        // Calls are read by `extract_calls` (D-032).
        if part.get("functionCall").is_some() {
            continue;
        }
        let fragment = part
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?;
        text.push_str(fragment);
    }
    Ok(text)
}

fn parse_usage(value: &Value) -> Result<Usage, ProviderError> {
    validate_unsupported_usage(value)?;
    let input = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let output = value
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let cached = Some(optional_zero_counter(value, "cachedContentTokenCount")?);
    let reasoning = Some(optional_zero_counter(value, "thoughtsTokenCount")?);
    Usage::complete(
        TokenCounter::exact(input),
        TokenCounter::exact(output),
        cached,
        reasoning,
        None,
    )
    .map_err(|_| protocol_error())
}

/// Gemini omits optional usage fields entirely when their value is zero
/// (rather than reporting an explicit `0`). Map that absence to an exact
/// zero counter so settlement can bill a nonzero-priced dimension safely;
/// only a present-but-malformed field fails closed.
fn optional_zero_counter(value: &Value, field: &str) -> Result<TokenCounter, ProviderError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(TokenCounter::exact(0)),
        Some(other) => other
            .as_u64()
            .map(TokenCounter::exact)
            .ok_or_else(protocol_error),
    }
}

/// Reject any usage dimension this adapter does not explicitly map (for
/// example tool-use prompt tokens) rather than silently dropping it. The
/// per-modality breakdowns (`promptTokensDetails`, `candidatesTokensDetails`,
/// `cacheTokensDetails`) only split a total this adapter does map, so they
/// are accepted when their counts add up to it.
fn validate_unsupported_usage(value: &Value) -> Result<(), ProviderError> {
    for field in ["toolUsePromptTokenCount", "toolUsePromptTokensDetails"] {
        let Some(field_value) = value.get(field) else {
            continue;
        };
        let empty = match field_value {
            Value::Null => true,
            Value::Number(number) => number.as_u64() == Some(0),
            Value::Array(values) => values.is_empty(),
            _ => false,
        };
        if !empty {
            return Err(protocol_error());
        }
    }
    for (details, total) in [
        ("promptTokensDetails", "promptTokenCount"),
        ("candidatesTokensDetails", "candidatesTokenCount"),
        ("cacheTokensDetails", "cachedContentTokenCount"),
    ] {
        validate_breakdown(value, details, total)?;
    }
    Ok(())
}

/// `details`, if present, is a list of `{modality, tokenCount}` whose counts
/// sum to `total` (absent meaning zero).
fn validate_breakdown(value: &Value, details: &str, total: &str) -> Result<(), ProviderError> {
    let entries = match value.get(details) {
        None | Some(Value::Null) => return Ok(()),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(protocol_error()),
    };
    let mut sum: u64 = 0;
    for entry in entries {
        let count = entry
            .get("tokenCount")
            .map_or(Some(0), Value::as_u64)
            .ok_or_else(protocol_error)?;
        sum = sum.checked_add(count).ok_or_else(protocol_error)?;
    }
    let expected = match value.get(total) {
        None | Some(Value::Null) => 0,
        Some(total) => total.as_u64().ok_or_else(protocol_error)?,
    };
    if sum != expected {
        return Err(protocol_error());
    }
    Ok(())
}

/// A `countTokens` answer: `totalTokens`, with at most the breakdown and
/// billing details Agent Platform adds.
fn parse_count_tokens(value: Value) -> Result<u64, ProviderError> {
    let object = value.as_object().ok_or_else(protocol_error)?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "totalTokens"
                | "totalBillableCharacters"
                | "promptTokensDetails"
                | "cachedContentTokenCount"
                | "cacheTokensDetails"
        )
    }) {
        return Err(protocol_error());
    }
    let total = object
        .get("totalTokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    if let Some(details) = object.get("promptTokensDetails") {
        validate_breakdown(
            &json!({"promptTokensDetails": details, "promptTokenCount": total}),
            "promptTokensDetails",
            "promptTokenCount",
        )?;
    }
    Ok(total)
}

fn parse_embedding_vectors(value: Value, expected: usize) -> Result<Vec<Embedding>, ProviderError> {
    let embeddings = value
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    if embeddings.len() != expected {
        return Err(protocol_error());
    }
    embeddings
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let vector = item
                .get("values")
                .and_then(Value::as_array)
                .ok_or_else(protocol_error)?
                .iter()
                .map(|value| {
                    value
                        .as_f64()
                        .map(|value| value as f32)
                        .ok_or_else(protocol_error)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Embedding::new(index as u32, vector).map_err(|_| protocol_error())
        })
        .collect::<Result<Vec<_>, _>>()
}

fn gemini_sse_stream<S>(
    mut bytes: S,
    model: String,
    max: usize,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    stream! {
        let mut pending = Vec::<u8>::new();
        let mut data = String::new();
        let mut data_seen = false;
        let mut total = 0usize;
        let mut response_started = false;
        let mut request_id: Option<String> = None;
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;
        let mut cached_tokens: Option<u64> = None;
        let mut reasoning_tokens: Option<u64> = None;
        let mut finish_reason: Option<FinishReason> = None;
        let mut finished = false;
        // Calls so far: Gemini streams each call whole, in one chunk (D-032).
        let mut tool_calls = 0u32;
        loop {
            let Some(chunk) = (match tokio::time::timeout(idle_timeout, bytes.next()).await {
                Ok(chunk) => chunk,
                Err(_) => { yield Err(ProviderError::new(ProviderErrorCategory::Timeout)); return; }
            }) else { break; };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => { yield Err(network_error_simple()); return; }
            };
            total = total.saturating_add(chunk.len());
            if total > max { yield Err(protocol_error()); return; }
            pending.extend_from_slice(&chunk);
            while let Some(position) = pending.iter().position(|byte| *byte == b'\n') {
                let line = pending.drain(..=position).collect::<Vec<_>>();
                let line = match decode_sse_line(line) {
                    Ok(line) => line,
                    Err(error) => { yield Err(error); return; }
                };
                if line.is_empty() {
                    if !data_seen { continue; }
                    if finished {
                        yield Err(protocol_error());
                        return;
                    }
                    let value: Value = match serde_json::from_str(&data) {
                        Ok(value) => value,
                        Err(_) => { yield Err(protocol_error()); return; }
                    };
                    data.clear();
                    data_seen = false;

                    if value.get("error").is_some() {
                        yield Err(stream_error(&value, request_id.clone()));
                        return;
                    }

                    let candidates = match value.get("candidates").and_then(Value::as_array) {
                        Some(candidates) if candidates.len() == 1 => candidates,
                        _ => { yield Err(protocol_error()); return; }
                    };
                    let candidate = &candidates[0];
                    if candidate.get("index").is_some_and(|index| index.as_u64() != Some(0)) {
                        yield Err(protocol_error());
                        return;
                    }

                    if !response_started {
                        response_started = true;
                        request_id = value.get("responseId").and_then(Value::as_str).map(str::to_owned);
                        match ResponseStarted::new(request_id.clone(), model.clone(), SafeMetadata::default()) {
                            Ok(started) => yield Ok(ChatStreamEvent::ResponseStarted(started)),
                            Err(_) => { yield Err(protocol_error()); return; }
                        }
                    }

                    if let Some(usage) = value.get("usageMetadata") {
                        if let Err(error) = validate_unsupported_usage(usage) {
                            yield Err(error);
                            return;
                        }
                        if let Some(v) = usage.get("promptTokenCount").and_then(Value::as_u64) {
                            input_tokens = Some(v);
                        }
                        if let Some(v) = usage.get("candidatesTokenCount").and_then(Value::as_u64) {
                            output_tokens = Some(v);
                        }
                        match usage.get("cachedContentTokenCount") {
                            None | Some(Value::Null) => {}
                            Some(other) => match other.as_u64() {
                                Some(v) => cached_tokens = Some(v),
                                None => { yield Err(protocol_error()); return; }
                            },
                        }
                        match usage.get("thoughtsTokenCount") {
                            None | Some(Value::Null) => {}
                            Some(other) => match other.as_u64() {
                                Some(v) => reasoning_tokens = Some(v),
                                None => { yield Err(protocol_error()); return; }
                            },
                        }
                    }

                    let text = match extract_text(candidate) {
                        Ok(text) => text,
                        Err(error) => { yield Err(error); return; }
                    };
                    if !text.is_empty() {
                        yield Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                            choice_index: 0,
                            content_delta: Some(text),
                            refusal_delta: None,
                            annotations: vec![],
                        }));
                    }

                    let calls = match extract_calls(candidate, tool_calls) {
                        Ok(calls) => calls,
                        Err(error) => { yield Err(error); return; }
                    };
                    for call in calls {
                        yield Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                            choice_index: 0,
                            tool_call_index: tool_calls,
                            id: call.id,
                            name: call.name,
                            arguments_delta: call.arguments,
                        }));
                        tool_calls += 1;
                    }

                    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                        finish_reason = Some(finish_reason_of(reason, tool_calls > 0));
                        finished = true;
                    }
                } else if let Some(value) = line.strip_prefix("data: ") {
                    if data_seen { data.push('\n'); }
                    data.push_str(value);
                    data_seen = true;
                } else if !line.starts_with(':') {
                    yield Err(protocol_error());
                    return;
                }
            }
        }
        if !pending.is_empty() || data_seen || !finished || !response_started {
            yield Err(protocol_error());
            return;
        }
        let reason = match finish_reason {
            Some(reason) => reason,
            None => { yield Err(protocol_error()); return; }
        };
        let input = match input_tokens {
            Some(value) => value,
            None => { yield Err(protocol_error()); return; }
        };
        let output = match output_tokens {
            Some(value) => value,
            None => { yield Err(protocol_error()); return; }
        };
        let usage = match Usage::complete(
            TokenCounter::exact(input),
            TokenCounter::exact(output),
            Some(TokenCounter::exact(cached_tokens.unwrap_or(0))),
            Some(TokenCounter::exact(reasoning_tokens.unwrap_or(0))),
            None,
        ) {
            Ok(usage) => usage,
            Err(_) => { yield Err(protocol_error()); return; }
        };
        match ChoiceFinished::new(0, reason) {
            Ok(finished) => yield Ok(ChatStreamEvent::ChoiceFinished(finished)),
            Err(_) => { yield Err(protocol_error()); return; }
        }
        let snapshot = match UsageSnapshot::final_(usage) {
            Ok(snapshot) => snapshot,
            Err(_) => { yield Err(protocol_error()); return; }
        };
        match ResponseCompletion::new(request_id, model, Some(snapshot)) {
            Ok(completion) => yield Ok(ChatStreamEvent::ResponseCompleted(completion)),
            Err(_) => yield Err(protocol_error()),
        }
    }
}

fn decode_sse_line(mut line: Vec<u8>) -> Result<String, ProviderError> {
    if line.pop() != Some(b'\n') {
        return Err(protocol_error());
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.contains(&b'\r') {
        return Err(protocol_error());
    }
    String::from_utf8(line).map_err(|_| protocol_error())
}

async fn send_with_timeout(
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<reqwest::Response, ProviderError> {
    tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
        .map_err(network_error)
}

async fn bounded_bytes(
    response: reqwest::Response,
    max: usize,
    idle_timeout: Duration,
) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(idle_timeout, stream.next())
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
    {
        let chunk = chunk.map_err(network_error)?;
        if bytes.len().saturating_add(chunk.len()) > max {
            return Err(protocol_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn network_error(error: reqwest::Error) -> ProviderError {
    // A connection that was never opened carried nothing to the provider.
    if error.is_connect() {
        return ProviderError::not_sent(if error.is_timeout() {
            ProviderErrorCategory::Timeout
        } else {
            ProviderErrorCategory::Unavailable
        });
    }
    ProviderError::new(if error.is_timeout() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::Provider
    })
}
fn network_error_simple() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Provider)
}
fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Protocol)
}
fn invalid_request() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::InvalidRequest)
}
fn internal_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Internal)
}

fn retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    super::retry_after(headers)
}

fn category_for_status(status: StatusCode) -> ProviderErrorCategory {
    match status {
        StatusCode::UNAUTHORIZED => ProviderErrorCategory::Authentication,
        StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ProviderErrorCategory::InvalidRequest
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorCategory::RateLimited,
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => ProviderErrorCategory::Timeout,
        status if status.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    }
}

fn category_for_error_status(value: &str) -> Option<ProviderErrorCategory> {
    match value {
        "UNAUTHENTICATED" => Some(ProviderErrorCategory::Authentication),
        "PERMISSION_DENIED" => Some(ProviderErrorCategory::Authorization),
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "OUT_OF_RANGE" => {
            Some(ProviderErrorCategory::InvalidRequest)
        }
        "RESOURCE_EXHAUSTED" => Some(ProviderErrorCategory::RateLimited),
        "UNAVAILABLE" => Some(ProviderErrorCategory::Unavailable),
        "DEADLINE_EXCEEDED" => Some(ProviderErrorCategory::Timeout),
        _ => None,
    }
}

pub(super) fn status_error(
    status: StatusCode,
    body: &Value,
    retry_after: Option<Duration>,
) -> ProviderError {
    let error = body.get("error");
    let error_status = error
        .and_then(|error| error.get("status"))
        .and_then(Value::as_str);
    let category = error_status
        .and_then(category_for_error_status)
        .unwrap_or_else(|| category_for_status(status));
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let code =
        match error_status {
            Some("RESOURCE_EXHAUSTED") if message.contains("per_base_model") => {
                Some(crate::domain::model_evidence::BASE_MODEL_QUOTA_CODE.to_owned())
            }
            Some(
                error_status @ ("FAILED_PRECONDITION" | "PERMISSION_DENIED" | "INVALID_ARGUMENT"),
            ) if model_garden_terms(message) => Some(format!(
                "{error_status}{}",
                crate::domain::model_evidence::MODEL_GARDEN_CODE_SUFFIX
            )),
            Some(error_status) => Some(error_status.to_owned()),
            // A partner model's own refusal, in its publisher's shape
            // (Anthropic's `{"type": "error", "error": {"type": ...}}`).
            None => error
                .and_then(|error| error.get("type"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        };
    ProviderError::from_parts(
        category,
        Some(PROVIDER.to_owned()),
        None,
        Some(status.as_u16()),
        code,
        retry_after,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

/// Whether Google's message says the project has not enabled a Model Garden
/// model (its terms are not accepted). Google words it in prose only, so
/// this reads the words its consoles use (*to verify* on a live refusal,
/// `docs/custom-deployments-design.md`).
fn model_garden_terms(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("model garden") || message.contains("terms of service")
}

fn stream_error(value: &Value, request_id: Option<String>) -> ProviderError {
    let error_status = value
        .get("error")
        .and_then(|error| error.get("status"))
        .and_then(Value::as_str);
    let category = error_status
        .and_then(category_for_error_status)
        .unwrap_or(ProviderErrorCategory::Provider);
    ProviderError::from_parts(
        category,
        Some(PROVIDER.to_owned()),
        request_id,
        None,
        error_status.map(ToOwned::to_owned),
        None,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::discovery::DeploymentDetails;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn config(base_url: String) -> GeminiConfig {
        GeminiConfig {
            base_url,
            auth: crate::config::GeminiAuth::ApiKey("test-gemini-key".into()),
            identity: None,
            agent_platform: None,
            agent_platform_location: None,
            storage_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        }
    }

    fn adapter() -> GeminiAdapter {
        GeminiAdapter::new(&config("http://127.0.0.1:9".into())).unwrap()
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new("deployment", "gemini-test").unwrap(),
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "hello".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end;
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..count]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
        }
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..count]);
        }
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn api_key_header_is_marked_sensitive() {
        let request = adapter()
            .request(&context(), &request(), false)
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(
            request
                .headers()
                .get("x-goog-api-key")
                .unwrap()
                .is_sensitive()
        );
    }

    #[tokio::test]
    async fn google_auth_sends_a_bearer_token_and_the_quota_project() {
        use crate::gcp_credentials::{CLOUD_PLATFORM_SCOPE, tests as google};
        let fake = google::start().await;
        let dir = google::TempDir::new();
        let identity = google::identity(
            &fake,
            &dir.write("adc.json", &google::user_credentials(&fake)),
        );
        let google_config = |quota_project: Option<&str>| GeminiConfig {
            auth: GeminiAuth::Google {
                token: identity.token(CLOUD_PLATFORM_SCOPE),
                quota_project: quota_project.map(str::to_owned),
            },
            identity: Some(identity.clone()),
            agent_platform: None,
            agent_platform_location: None,
            storage_base_url: None,
            ..config("http://127.0.0.1:9".into())
        };
        let built = GeminiAdapter::new(&google_config(Some("billing-project")))
            .unwrap()
            .request(&context(), &request(), false)
            .await
            .unwrap()
            .build()
            .unwrap();
        let headers = built.headers();
        assert_eq!(headers["authorization"], "Bearer token-1");
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(headers["x-goog-user-project"], "billing-project");
        assert!(headers.get("x-goog-api-key").is_none());
        // Without a quota project, the token's own project is billed.
        let built = GeminiAdapter::new(&google_config(None))
            .unwrap()
            .request(&context(), &request(), false)
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(built.headers().get("x-goog-user-project").is_none());

        // A token Google refuses: nothing is sent, and it is an
        // authentication failure.
        fake.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let refused = GeminiAdapter::new(&GeminiConfig {
            auth: GeminiAuth::Google {
                token: identity
                    .impersonate("t-sa@t-proj.iam.gserviceaccount.com", CLOUD_PLATFORM_SCOPE)
                    .unwrap(),
                quota_project: None,
            },
            ..google_config(None)
        })
        .unwrap();
        let error = refused.chat(context(), request()).await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Authentication);
        assert!(error.was_not_sent());
    }

    #[tokio::test]
    async fn a_stalled_token_service_is_bounded_by_the_request_timeout() {
        // A metadata server that accepts and never answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                held.push(socket);
            }
        });
        let identity =
            crate::gcp_credentials::GoogleIdentity::from_env(&move |name: &str| match name {
                "GCE_METADATA_HOST" => Some(host.clone()),
                "HOME" => Some("/nonexistent".to_owned()),
                _ => None,
            })
            .unwrap();
        let adapter = GeminiAdapter::new(&GeminiConfig {
            auth: GeminiAuth::Google {
                token: identity.token(crate::gcp_credentials::CLOUD_PLATFORM_SCOPE),
                quota_project: None,
            },
            request_timeout: Duration::from_millis(200),
            ..config("http://127.0.0.1:9".into())
        })
        .unwrap();
        let started = std::time::Instant::now();
        let error = adapter.chat(context(), request()).await.unwrap_err();
        // Well before the credential client's own ten-second limit.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(error.category(), ProviderErrorCategory::Timeout);
        assert!(error.was_not_sent());
    }

    #[test]
    fn request_mapping_keeps_system_separate_and_rejects_tools() {
        let adapter = adapter();
        let mapped = ChatRequest {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "system".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "hello".into(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                },
            ],
            temperature: Some(0.2),
            max_tokens: Some(12),
            ..Default::default()
        };
        let body = adapter.request_body(&mapped).unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
        assert_eq!(body["contents"][0]["role"], "user");

        let tool_request = ChatRequest {
            messages: vec![ChatMessage {
                role: ChatRole::Tool,
                content: "x".into(),
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
            }],
            temperature: None,
            max_tokens: Some(1),
            ..Default::default()
        };
        assert_eq!(
            adapter.request_body(&tool_request).unwrap_err().category(),
            ProviderErrorCategory::InvalidRequest
        );
    }

    #[tokio::test]
    async fn mock_http_chat_uses_wire_path_headers_and_response_mapping() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let response_body = r#"{"responseId":"resp-1","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"hi there"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = read_request(&mut socket).await;
            sender.send(wire).unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter.chat(context(), request()).await.unwrap();
        let wire = receiver.await.unwrap();
        assert!(wire.starts_with("POST /models/gemini-test:generateContent HTTP/1.1"));
        assert!(
            wire.to_ascii_lowercase()
                .contains("x-goog-api-key: test-gemini-key")
        );
        assert_eq!(result.choices()[0].message.content, "hi there");
        assert_eq!(result.provider_request_id(), Some("resp-1"));
        assert_eq!(
            result
                .usage()
                .unwrap()
                .usage()
                .input_tokens()
                .unwrap()
                .value(),
            Some(3)
        );
    }

    #[tokio::test]
    async fn absent_cached_and_reasoning_counters_settle_as_exact_zero() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // No cachedContentTokenCount/thoughtsTokenCount: Gemini omits these
        // fields rather than reporting zero explicitly.
        let response_body = r#"{"responseId":"resp-1","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"hi there"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter.chat(context(), request()).await.unwrap();
        let usage = result.usage().unwrap().usage();
        assert_eq!(usage.cached_input_tokens().unwrap().value(), Some(0));
        assert_eq!(usage.reasoning_tokens().unwrap().value(), Some(0));
    }

    #[tokio::test]
    async fn count_tokens_uses_wire_path_and_strips_generation_config() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let response_body = r#"{"totalTokens":7}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = read_request(&mut socket).await;
            sender.send(wire).unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let count = adapter
            .count_chat_input_tokens(&context(), &request())
            .await
            .unwrap();
        assert_eq!(count, 7);
        let wire = receiver.await.unwrap();
        assert!(wire.starts_with("POST /models/gemini-test:countTokens HTTP/1.1"));
        let body = wire.split("\r\n\r\n").nth(1).unwrap();
        assert!(!body.contains("generationConfig"));
    }

    #[tokio::test]
    async fn embeddings_happy_path_maps_batch_response_and_settles_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let embed_body = r#"{"embeddings":[{"values":[0.1,0.2]},{"values":[0.3,0.4]}]}"#;
        let embed_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            embed_body.len(),
            embed_body
        );
        let count_body = r#"{"totalTokens":9}"#;
        let count_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            count_body.len(),
            count_body
        );
        tokio::spawn(async move {
            let mut wires = Vec::new();
            let (mut socket, _) = listener.accept().await.unwrap();
            wires.push(read_request(&mut socket).await);
            socket.write_all(count_response.as_bytes()).await.unwrap();
            // reqwest may reuse this keep-alive connection for the follow-up
            // batchEmbedContents call, or open a fresh one; handle either.
            match tokio::time::timeout(Duration::from_millis(200), read_request(&mut socket)).await
            {
                Ok(wire) => {
                    wires.push(wire);
                    socket.write_all(embed_response.as_bytes()).await.unwrap();
                }
                Err(_) => {
                    let (mut second, _) = listener.accept().await.unwrap();
                    wires.push(read_request(&mut second).await);
                    second.write_all(embed_response.as_bytes()).await.unwrap();
                }
            }
            sender.send(wires).unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter
            .embeddings(
                context(),
                EmbeddingRequest {
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .unwrap();
        let wires = receiver.await.unwrap();
        assert!(
            wires
                .iter()
                .any(|w| w.starts_with("POST /models/gemini-test:batchEmbedContents HTTP/1.1"))
        );
        assert!(
            wires
                .iter()
                .any(|w| w.starts_with("POST /models/gemini-test:countTokens HTTP/1.1"))
        );
        assert_eq!(result.data()[0].vector(), &[0.1, 0.2]);
        assert_eq!(result.data()[1].vector(), &[0.3, 0.4]);
        assert_eq!(
            result
                .usage()
                .unwrap()
                .usage()
                .input_tokens()
                .unwrap()
                .value(),
            Some(9)
        );
    }

    #[tokio::test]
    async fn embeddings_never_calls_batch_embed_when_count_tokens_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let error_body = r#"{"error":{"code":500,"message":"boom","status":"UNAVAILABLE"}}"#;
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            error_body.len(),
            error_body
        );
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            // Confirm no batchEmbedContents request ever arrives, whether on
            // a reused connection or a fresh one.
            let mut buf = [0_u8; 16];
            let more_on_same_socket = tokio::time::timeout(
                Duration::from_millis(150),
                AsyncReadExt::read(&mut socket, &mut buf),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .is_some_and(|count| count > 0);
            let second_connection =
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_ok();
            sender
                .send((wire, more_on_same_socket, second_connection))
                .unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter
            .embeddings(
                context(),
                EmbeddingRequest {
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Unavailable);
        let (wire, more_on_same_socket, second_connection) = receiver.await.unwrap();
        assert!(wire.starts_with("POST /models/gemini-test:countTokens HTTP/1.1"));
        assert!(
            !more_on_same_socket,
            "batchEmbedContents must not reuse the connection"
        );
        assert!(
            !second_connection,
            "batchEmbedContents must not open a new connection"
        );
    }

    #[tokio::test]
    async fn error_status_maps_resource_exhausted_to_rate_limited() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let error_body =
            r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            error_body.len(),
            error_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = GeminiAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter.chat(context(), request()).await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        assert_eq!(error.provider_code(), Some("RESOURCE_EXHAUSTED"));
    }

    fn valid_stream_body() -> &'static str {
        concat!(
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
            "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" there\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":2}}\n\n"
        )
    }

    async fn mock_stream_adapter(chunks: Vec<Vec<u8>>) -> GeminiAdapter {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            for chunk in chunks {
                socket.write_all(&chunk).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        GeminiAdapter::new(&config(format!("http://{address}"))).unwrap()
    }

    #[tokio::test]
    async fn mock_http_stream_handles_fragmented_multiline_sse() {
        let body = valid_stream_body().as_bytes();
        let split = body.len() / 2;
        let adapter =
            mock_stream_adapter(vec![body[..split].to_vec(), body[split..].to_vec()]).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        assert!(matches!(
            events.last(),
            Some(Ok(ChatStreamEvent::ResponseCompleted(_)))
        ));
    }

    #[tokio::test]
    async fn stream_absent_cached_and_reasoning_counters_settle_as_exact_zero() {
        let adapter = mock_stream_adapter(vec![valid_stream_body().as_bytes().to_vec()]).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let completion = events
            .into_iter()
            .find_map(|event| match event {
                Ok(ChatStreamEvent::ResponseCompleted(completion)) => Some(completion),
                _ => None,
            })
            .expect("response completed");
        let usage = completion.usage().unwrap().usage();
        assert_eq!(usage.cached_input_tokens().unwrap().value(), Some(0));
        assert_eq!(usage.reasoning_tokens().unwrap().value(), Some(0));
    }

    #[tokio::test]
    async fn premature_eof_before_finish_reason_fails_closed() {
        let start_only = "data: {\"responseId\":\"stream-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"hi\"}]}}]}\n\n";
        let adapter = mock_stream_adapter(vec![start_only.as_bytes().to_vec()]).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(error) if error.category() == ProviderErrorCategory::Protocol
        )));
    }

    /// Agent Platform and the current Gemini API split usage by modality;
    /// the split is accepted when it adds up, and tool use still is not.
    #[test]
    fn usage_breakdowns_must_add_up() {
        let parse = |usage: Value| parse_usage(&usage).is_ok();
        assert!(parse(json!({
            "promptTokenCount": 7, "candidatesTokenCount": 2, "totalTokenCount": 9,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 7}],
            "candidatesTokensDetails": [{"modality": "TEXT", "tokenCount": 2}],
            "trafficType": "ON_DEMAND"
        })));
        assert!(!parse(json!({
            "promptTokenCount": 7, "candidatesTokenCount": 2,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 6}]
        })));
        assert!(!parse(json!({
            "promptTokenCount": 7, "candidatesTokenCount": 2,
            "promptTokensDetails": {"modality": "TEXT"}
        })));
        assert!(!parse(json!({
            "promptTokenCount": 7, "candidatesTokenCount": 2, "toolUsePromptTokenCount": 3
        })));
        assert_eq!(
            parse_count_tokens(json!({
                "totalTokens": 7, "totalBillableCharacters": 30,
                "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 7}]
            }))
            .unwrap(),
            7
        );
        assert!(parse_count_tokens(json!({"totalTokens": 7, "other": 1})).is_err());
    }

    /// Serves `responses` in order, one per connection, and reports each
    /// request, head and body.
    async fn script(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                sender.send(read_request(&mut socket).await).unwrap();
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), receiver)
    }

    fn requests(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn request_body(request: &str) -> Value {
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    const PUBLISHER: &str = "/v1/projects/tenant-project/locations/us-central1/publishers/google";

    /// Gemini on Agent Platform in `tenant-project`, `us-central1`, served
    /// by `server`.
    fn platform(server: &str) -> GeminiAdapter {
        let mut adapter = GeminiAdapter::new(&GeminiConfig {
            agent_platform: crate::config::AgentPlatform::new("tenant-project", "us-central1"),
            ..config(format!("{server}{PUBLISHER}"))
        })
        .unwrap();
        adapter.service_usage_url = Url::parse(&format!("{server}/")).unwrap();
        adapter
    }

    /// No partner models: an empty page for each partner publisher.
    fn no_partners() -> Vec<(u16, String)> {
        vec![(200, "{}".to_owned()); PARTNER_PUBLISHERS.len()]
    }

    /// No endpoints in the project's location: an empty page.
    fn no_endpoints() -> Vec<(u16, String)> {
        vec![(200, "{}".to_owned())]
    }

    /// A model access policy the identity may not read: models unchecked.
    fn no_policy() -> Vec<(u16, String)> {
        vec![(403, error_body(403, "PERMISSION_DENIED"))]
    }

    fn embedding_context(model: &str) -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new("deployment", model).unwrap(),
        )
    }

    fn texts(count: usize) -> EmbeddingRequest {
        EmbeddingRequest {
            input: (0..count).map(|index| format!("text {index}")).collect(),
        }
    }

    fn error_body(code: u16, status: &str) -> String {
        json!({"error": {"code": code, "message": "refused", "status": status}}).to_string()
    }

    /// A `predict` answer: one vector per entry of `tokens`, each with its
    /// statistics when the count is given.
    fn predicted(tokens: &[Option<u64>], truncated: bool) -> String {
        let predictions: Vec<Value> = tokens
            .iter()
            .enumerate()
            .map(|(index, tokens)| {
                let mut embeddings = json!({"values": [index as f64 + 0.5, 0.25]});
                if let Some(tokens) = tokens {
                    embeddings["statistics"] =
                        json!({"token_count": tokens, "truncated": truncated});
                }
                json!({"embeddings": embeddings})
            })
            .collect();
        json!({"predictions": predictions, "metadata": {"billableCharacterCount": 12}}).to_string()
    }

    fn counted(tokens: u64) -> String {
        json!({"totalTokens": tokens, "totalBillableCharacters": 6}).to_string()
    }

    fn input_tokens(response: &EmbeddingResponse) -> TokenCounter {
        response.usage().unwrap().usage().input_tokens().unwrap()
    }

    #[test]
    fn publisher_models_are_classified_by_family_only_where_certain() {
        use CatalogOperation::{Chat, Embeddings};
        let chat: Option<&[CatalogOperation]> = Some(&[Chat]);
        let embeddings: Option<&[CatalogOperation]> = Some(&[Embeddings]);
        for model in [
            "gemini-2.5-flash",
            "gemini-2.5-pro",
            "gemini-2.0-flash-lite-001",
            "gemini-2.0-flash-001",
            "gemini-3-pro-preview",
            "gemini-2.5-flash-preview-09-2025",
        ] {
            assert_eq!(publisher_model_operations(model), Some(chat), "{model}");
        }
        for model in [
            "gemini-embedding-001",
            "gemini-embedding-2-preview",
            "text-embedding-005",
            "text-multilingual-embedding-002",
        ] {
            assert_eq!(
                publisher_model_operations(model),
                Some(embeddings),
                "{model}"
            );
        }
        // Gemini, but its operations are not certain from the name.
        for model in [
            "gemini-2.5-flash-image",
            "gemini-2.5-flash-preview-tts",
            "gemini-live-2.5-flash",
            "gemini-2.5-computer-use-preview-10-2025",
            "gemini-flash-latest",
            "gemini-2.0-flash-exp",
        ] {
            assert_eq!(publisher_model_operations(model), Some(None), "{model}");
        }
        // Not called by this adapter at all.
        for model in [
            "imagen-4.0-generate-001",
            "veo-3.0-generate-001",
            "chirp-2",
            "multimodalembedding",
        ] {
            assert_eq!(publisher_model_operations(model), None, "{model}");
        }
    }

    #[test]
    fn agent_platform_calls_the_global_or_the_regional_endpoint() {
        for (location, host) in [
            ("global", "aiplatform.googleapis.com"),
            ("europe-west4", "europe-west4-aiplatform.googleapis.com"),
        ] {
            let platform = crate::config::AgentPlatform::new("tenant-project", location).unwrap();
            let adapter = GeminiAdapter::new(&GeminiConfig {
                agent_platform: Some(platform.clone()),
                ..config(platform.base_url())
            })
            .unwrap();
            assert_eq!(
                adapter
                    .publisher_models_url("google", Some("next"))
                    .as_str(),
                format!(
                    "https://{host}/v1beta1/publishers/google/models?pageSize=300&pageToken=next"
                )
            );
            assert_eq!(
                adapter
                    .endpoint("text-embedding-005", "predict", false)
                    .unwrap()
                    .as_str(),
                format!(
                    "https://{host}/v1/projects/tenant-project/locations/{location}/publishers/google/models/text-embedding-005:predict"
                )
            );
            assert_eq!(
                adapter
                    .endpoint("gemini-embedding-2-preview", "embedContent", false)
                    .unwrap()
                    .path(),
                format!(
                    "/v1/projects/tenant-project/locations/{location}/publishers/google/models/gemini-embedding-2-preview:embedContent"
                )
            );
        }
    }

    #[tokio::test]
    async fn agent_platform_lists_google_models_across_pages() {
        let (server, mut seen) = script(
            vec![
                (
                    200,
                    json!({"publisherModels": [
                    {"name": "publishers/google/models/gemini-2.5-flash", "launchStage": "GA"},
                    {"name": "publishers/google/models/text-embedding-005"},
                    {"name": "publishers/google/models/imagen-4.0-generate-001"},
                    {"name": "publishers/google/models/gemini-2.5-flash-image"}
                ], "nextPageToken": "page-2"})
                    .to_string(),
                ),
                (
                    200,
                    json!({"publisherModels": [
                    {"name": "publishers/google/models/gemini-embedding-001"},
                    {"name": "publishers/google/models/text-multilingual-embedding-002"},
                    {"name": "publishers/google/models/gemini-2.0-flash-lite-001"}
                ], "nextPageToken": "page-3"})
                    .to_string(),
                ),
                // Proto3 JSON omits an empty last page's array.
                (200, "{}".to_owned()),
            ]
            .into_iter()
            .chain(no_partners())
            .chain(no_endpoints())
            .collect(),
        )
        .await;
        let models = platform(&server).list_models().await.unwrap();
        let heads: Vec<String> = requests(&mut seen)
            .iter()
            .map(|request| request.lines().next().unwrap().to_owned())
            .collect();
        assert_eq!(
            heads,
            [
                "GET /v1beta1/publishers/google/models?pageSize=300 HTTP/1.1",
                "GET /v1beta1/publishers/google/models?pageSize=300&pageToken=page-2 HTTP/1.1",
                "GET /v1beta1/publishers/google/models?pageSize=300&pageToken=page-3 HTTP/1.1",
                "GET /v1beta1/publishers/anthropic/models?pageSize=300 HTTP/1.1",
                "GET /v1beta1/publishers/mistralai/models?pageSize=300 HTTP/1.1",
                "GET /v1beta1/publishers/xai/models?pageSize=300 HTTP/1.1",
                "GET /v1/projects/tenant-project/locations/us-central1/endpoints?pageSize=100 HTTP/1.1",
            ]
        );
        use CatalogOperation::{Chat, Embeddings};
        let listed: Vec<(&str, Option<Vec<CatalogOperation>>)> = models
            .iter()
            .map(|model| (model.model.as_str(), model.operations.clone()))
            .collect();
        assert_eq!(
            listed,
            [
                ("gemini-2.0-flash-lite-001", Some(vec![Chat])),
                ("gemini-2.5-flash", Some(vec![Chat])),
                // Gemini, with operations the family does not make certain.
                ("gemini-2.5-flash-image", None),
                ("gemini-embedding-001", Some(vec![Embeddings])),
                ("text-embedding-005", Some(vec![Embeddings])),
                ("text-multilingual-embedding-002", Some(vec![Embeddings])),
            ]
        );
        // Google's catalog does not say what the location offers.
        assert!(
            models
                .iter()
                .all(|model| model.availability == ModelAvailability::Unknown)
        );
    }

    #[tokio::test]
    async fn agent_platform_listing_stops_at_the_page_cap() {
        let page = json!({
            "publisherModels": [{"name": "publishers/google/models/gemini-2.5-flash"}],
            "nextPageToken": "again"
        })
        .to_string();
        let (server, mut seen) = script(vec![(200, page); MAX_MODEL_LIST_PAGES]).await;
        let error = platform(&server).list_models().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        assert_eq!(requests(&mut seen).len(), MAX_MODEL_LIST_PAGES);
    }

    #[tokio::test]
    async fn agent_platform_lists_partner_models_with_their_quota_where_requests_go() {
        let partner = |publisher: &str, models: &[&str]| {
            let models: Vec<Value> = models
                .iter()
                .map(|model| json!({"name": format!("publishers/{publisher}/models/{model}")}))
                .collect();
            (200, json!({ "publisherModels": models }).to_string())
        };
        let bucket = |region: &str, model: &str, limit: Option<&str>| {
            let mut bucket = json!({"dimensions": {"region": region, "base_model": model}});
            if let Some(limit) = limit {
                bucket["effectiveLimit"] = json!(limit);
            }
            bucket
        };
        let quota = json!({"consumerQuotaLimits": [{
            "unit": "1/min/{project}/{region}/{base_model}",
            "quotaBuckets": [
                // The default bucket names no model: it says nothing.
                {},
                // Proto3 JSON omits a zero limit.
                bucket("us-central1", "anthropic-claude-haiku-4-5", None),
                bucket("us-central1", "anthropic-claude-sonnet-4-6", Some("60")),
                // Zero elsewhere is not zero where requests go.
                bucket("us-east5", "anthropic-claude-sonnet-4-6", Some("0")),
                bucket("us-central1", "mistralai-codestral-2", Some("0")),
            ]
        }]});
        let (server, mut seen) = script(vec![
            (200, "{}".to_owned()),
            partner(
                "anthropic",
                &["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-5"],
            ),
            partner("mistralai", &["codestral-2"]),
            // Another publisher's entry is not this one's.
            partner("xai", &["../google/models/gemini-2.5-flash"]),
            (200, quota.to_string()),
            (200, "{}".to_owned()),
        ])
        .await;
        let models = platform(&server).list_models().await.unwrap();
        let seen = requests(&mut seen);
        assert!(seen[4].starts_with(
            "GET /v1beta1/projects/tenant-project/services/aiplatform.googleapis.com/consumerQuotaMetrics/aiplatform.googleapis.com%2Fonline_prediction_requests_per_base_model?view=FULL "
        ));
        let listed: Vec<(&str, Option<Entitlement>)> = models
            .iter()
            .map(|model| (model.model.as_str(), model.entitlement))
            .collect();
        assert_eq!(
            listed,
            [
                ("anthropic/claude-haiku-4-5", Some(Entitlement::QuotaZero)),
                // No bucket of its own: unchecked.
                ("anthropic/claude-opus-5", None),
                // A raised quota replaces an earlier `quota_zero`.
                ("anthropic/claude-sonnet-4-6", Some(Entitlement::Entitled)),
                ("mistralai/codestral-2", Some(Entitlement::QuotaZero)),
            ]
        );
        // Listed for what the tenant would need to do; never called.
        assert!(models.iter().all(|model| {
            model.operations.is_none() && model.availability == ModelAvailability::Unknown
        }));
    }

    #[tokio::test]
    async fn partner_models_are_left_unchecked_when_their_quota_cannot_be_read() {
        let (server, _seen) = script(vec![
            (200, "{}".to_owned()),
            (
                200,
                json!({"publisherModels": [
                    {"name": "publishers/anthropic/models/claude-haiku-4-5"}
                ]})
                .to_string(),
            ),
            (200, "{}".to_owned()),
            (200, "{}".to_owned()),
            (403, error_body(403, "PERMISSION_DENIED")),
            (200, "{}".to_owned()),
        ])
        .await;
        let models = platform(&server).list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "anthropic/claude-haiku-4-5");
        assert_eq!(models[0].entitlement, None);
    }

    #[tokio::test]
    async fn agent_platform_lists_the_projects_endpoints_where_requests_go() {
        let name = |id: &str| format!("projects/123/locations/us-central1/endpoints/{id}");
        let first = json!({"endpoints": [
            {
                "name": name("mg-endpoint-1"),
                "displayName": "gemma",
                "deployedModels": [{
                    "id": "7",
                    "model": "projects/123/locations/us-central1/models/gemma3-1b",
                    "modelVersionId": "1",
                    "displayName": "gemma-3-1b-it",
                    "dedicatedResources": {
                        "machineSpec": {
                            "machineType": "g2-standard-12",
                            "acceleratorType": "NVIDIA_L4",
                            "acceleratorCount": 1
                        },
                        "minReplicaCount": 1,
                        "maxReplicaCount": 2
                    }
                }],
                "trafficSplit": {"7": 100}
            },
            // Being created: serves nothing yet, so there is nothing to call.
            {"name": name("mg-endpoint-2"), "displayName": "starting"},
            // Not an endpoint's name.
            {"name": "projects/123/locations/us-central1/models/9",
             "deployedModels": [{"id": "1"}]}
        ], "nextPageToken": "page-2"});
        let second = json!({"endpoints": [{
            "name": name("4455"),
            "deployedModels": [
                {"id": "1", "displayName": "old", "dedicatedResources": {
                    "machineSpec": {"machineType": "n1-standard-4"}}},
                // Takes most of the traffic, so it is the one described.
                {"id": "2", "model": "projects/123/locations/us-central1/models/tuned-9",
                 "automaticResources": {"minReplicaCount": 3}}
            ],
            "trafficSplit": {"1": 10, "2": 90}
        }]});
        // The Gemma endpoint's deployed model, served by vLLM.
        let gemma = json!({"name": "projects/123/locations/us-central1/models/gemma3-1b",
            "containerSpec": {"imageUri":
                "us-docker.pkg.dev/vertex-ai/vertex-vision-model-garden-dockers/pytorch-vllm-serve:20250601"}});
        let (server, mut seen) = script(
            vec![(200, "{}".to_owned())]
                .into_iter()
                .chain(no_partners())
                .chain([
                    (200, first.to_string()),
                    (200, gemma.to_string()),
                    (200, second.to_string()),
                ])
                .collect(),
        )
        .await;
        let models = platform(&server).list_models().await.unwrap();
        let seen = requests(&mut seen);
        assert!(
            seen[5].starts_with("GET /v1/projects/123/locations/us-central1/models/gemma3-1b ")
        );
        assert!(seen[6].starts_with(
            "GET /v1/projects/tenant-project/locations/us-central1/endpoints?pageSize=100&pageToken=page-2 "
        ));
        let listed: Vec<(&str, Option<&str>, Option<DeploymentDetails>)> = models
            .iter()
            .map(|model| {
                (
                    model.model.as_str(),
                    model.display_name.as_deref(),
                    model.deployment.clone(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "endpoints/4455",
                    None,
                    Some(DeploymentDetails {
                        model: Some("tuned-9".into()),
                        capacity: Some(3),
                        ..DeploymentDetails::default()
                    })
                ),
                (
                    "endpoints/mg-endpoint-1",
                    Some("gemma"),
                    Some(DeploymentDetails {
                        // How the gateway calls it.
                        format: Some("openai_chat".into()),
                        model: Some("gemma-3-1b-it".into()),
                        version: Some("1".into()),
                        sku: Some("g2-standard-12 + 1 NVIDIA_L4".into()),
                        capacity: Some(1),
                        ..DeploymentDetails::default()
                    })
                ),
            ]
        );
        // The one serving a known API is callable, and probed only once
        // deployed; the other (a model of unknown kind beside one with no
        // registry model) is listed for what it serves.
        let adapter = platform(&server);
        let called: Vec<(&str, ModelAvailability, Option<Vec<CatalogOperation>>)> = models
            .iter()
            .map(|model| {
                (
                    model.model.as_str(),
                    model.availability,
                    model.operations.clone(),
                )
            })
            .collect();
        assert_eq!(
            called,
            [
                ("endpoints/4455", ModelAvailability::EndpointOnly, None),
                (
                    "endpoints/mg-endpoint-1",
                    ModelAvailability::Listed,
                    Some(vec![CatalogOperation::Chat])
                ),
            ]
        );
        assert!(models.iter().all(|model| model.entitlement.is_none()
            && adapter.probes_model(&model.model)
            && adapter.probes_only_when_deployed(&model.model)));
    }

    #[tokio::test]
    async fn a_malformed_endpoint_page_fails_the_listing() {
        for page in [json!({"endpoints": {}}), json!(null), json!([])] {
            let (server, _seen) = script(
                vec![(200, "{}".to_owned())]
                    .into_iter()
                    .chain(no_partners())
                    .chain([(200, page.to_string())])
                    .collect(),
            )
            .await;
            let error = platform(&server).list_models().await.unwrap_err();
            assert_eq!(error.category(), ProviderErrorCategory::Protocol, "{page}");
        }
    }

    #[tokio::test]
    async fn an_endpoint_listing_that_fails_fails_the_listing() {
        let (server, _seen) = script(
            vec![(200, "{}".to_owned())]
                .into_iter()
                .chain(no_partners())
                .chain([(403, error_body(403, "PERMISSION_DENIED"))])
                .collect(),
        )
        .await;
        let error = platform(&server).list_models().await.unwrap_err();
        assert_eq!(error.http_status(), Some(403));
    }

    #[test]
    fn the_model_policy_is_read_from_its_effective_rules() {
        let rules = |rules: Value| json!({"name": "projects/1/policies/vertexai.allowedModels", "spec": {"rules": rules}});
        let flash = "publishers/google/models/gemini-2.5-flash:predict";
        let pro = "publishers/google/models/gemini-2.5-pro:predict";
        // No policy set: Google answers with `allowAll`.
        let open = ModelPolicy::from_effective(&rules(json!([{"allowAll": true}]))).unwrap();
        assert_eq!(open, ModelPolicy::AllowAll);
        assert!(open.allows(flash));
        let closed = ModelPolicy::from_effective(&rules(json!([{"denyAll": true}]))).unwrap();
        assert!(!closed.allows(flash));
        // A deny list blocks what it names, and nothing else.
        let deny = ModelPolicy::from_effective(&rules(json!([
            {"values": {"deniedValues": [format!("is:{flash}")]}}
        ])))
        .unwrap();
        assert!(!deny.allows(flash));
        assert!(deny.allows(pro));
        // An allow list blocks everything else; a denial outranks it.
        let allow = ModelPolicy::from_effective(&rules(json!([
            {"values": {"allowedValues": [flash, pro], "deniedValues": [pro]}}
        ])))
        .unwrap();
        assert!(allow.allows(flash));
        assert!(!allow.allows(pro));
        assert!(!allow.allows("publishers/anthropic/models/claude-opus-5:predict"));
        // Conditions depend on tags, which the gateway does not evaluate;
        // an answer without rules says nothing.
        for unknown in [
            rules(
                json!([{"values": {"deniedValues": [flash]}, "condition": {"expression": "true"}}]),
            ),
            rules(json!([])),
            json!({"name": "projects/1/policies/vertexai.allowedModels"}),
            rules(json!([{"values": {"deniedValues": "not a list"}}])),
        ] {
            assert_eq!(ModelPolicy::from_effective(&unknown), None, "{unknown}");
        }
    }

    #[test]
    fn a_model_is_governed_by_its_predict_value() {
        assert_eq!(
            policy_value("gemini-2.5-flash").as_deref(),
            Some("publishers/google/models/gemini-2.5-flash:predict")
        );
        assert_eq!(
            policy_value("anthropic/claude-haiku-4-5").as_deref(),
            Some("publishers/anthropic/models/claude-haiku-4-5:predict")
        );
        assert_eq!(policy_value("endpoints/mg-endpoint-1"), None);
    }

    #[tokio::test]
    async fn the_projects_model_policy_explains_what_it_forbids() {
        let policy = json!({"spec": {"rules": [{"values": {"deniedValues": [
            "publishers/google/models/gemini-2.5-pro:predict",
            "publishers/anthropic/models/claude-haiku-4-5:predict"
        ]}}]}});
        let quota = json!({"consumerQuotaLimits": [{"quotaBuckets": [
            {"dimensions": {"region": "us-central1", "base_model": "anthropic-claude-haiku-4-5"}, "effectiveLimit": "60"},
            {"dimensions": {"region": "us-central1", "base_model": "anthropic-claude-opus-5"}}
        ]}]});
        let (server, mut seen) = script(vec![
            (
                200,
                json!({"publisherModels": [
                    {"name": "publishers/google/models/gemini-2.5-flash"},
                    {"name": "publishers/google/models/gemini-2.5-pro"}
                ]})
                .to_string(),
            ),
            (
                200,
                json!({"publisherModels": [
                    {"name": "publishers/anthropic/models/claude-haiku-4-5"},
                    {"name": "publishers/anthropic/models/claude-opus-5"}
                ]})
                .to_string(),
            ),
            (200, "{}".to_owned()),
            (200, "{}".to_owned()),
            (200, quota.to_string()),
            (
                200,
                json!({"endpoints": [{
                    "name": "projects/1/locations/us-central1/endpoints/e-1",
                    "deployedModels": [{"id": "1", "displayName": "gemma"}]
                }]})
                .to_string(),
            ),
            (200, policy.to_string()),
        ])
        .await;
        let models = platform(&server).list_models().await.unwrap();
        let seen = requests(&mut seen);
        assert!(seen[6].starts_with(
            "GET /v2/projects/tenant-project/policies/vertexai.allowedModels:getEffectivePolicy "
        ));
        let listed: Vec<(&str, Option<Entitlement>)> = models
            .iter()
            .map(|model| (model.model.as_str(), model.entitlement))
            .collect();
        assert_eq!(
            listed,
            [
                // Denied by the policy, whatever its quota.
                (
                    "anthropic/claude-haiku-4-5",
                    Some(Entitlement::PolicyDenied)
                ),
                // Allowed, but its quota is still zero.
                ("anthropic/claude-opus-5", Some(Entitlement::QuotaZero)),
                // Not the policy's to judge.
                ("endpoints/e-1", None),
                ("gemini-2.5-flash", Some(Entitlement::Entitled)),
                ("gemini-2.5-pro", Some(Entitlement::PolicyDenied)),
            ]
        );
    }

    #[tokio::test]
    async fn access_is_not_proven_with_a_model_the_policy_forbids() {
        let policy = json!({"spec": {"rules": [{"values": {"deniedValues": [
            "publishers/google/models/gemini-3.5-flash:predict"
        ]}}]}});
        let (server, mut seen) = script(
            vec![(
                200,
                json!({"publisherModels": [
                    {"name": "publishers/google/models/gemini-2.5-flash"},
                    {"name": "publishers/google/models/gemini-3.5-flash"}
                ]})
                .to_string(),
            )]
            .into_iter()
            .chain(no_partners())
            .chain(no_endpoints())
            .chain([(200, policy.to_string()), (200, counted(1))])
            .collect(),
        )
        .await;
        platform(&server).check_access().await.unwrap();
        let seen = requests(&mut seen);
        // The newer model is denied, so the older one is counted.
        assert!(seen[6].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-2.5-flash:countTokens "
        )));
    }

    #[test]
    fn the_global_quota_metric_has_no_region() {
        let page = json!({"consumerQuotaLimits": [
            {"quotaBuckets": [
                {"dimensions": {"base_model": "xai-grok-4.7"}, "effectiveLimit": "30"},
                {"dimensions": {"base_model": "anthropic-claude-opus-5"}}
            ]},
            {"quotaBuckets": [
                {"dimensions": {"base_model": "xai-grok-4.7"}, "effectiveLimit": "0"}
            ]}
        ]});
        let limits = base_model_limits(&page, None);
        // The lowest limit stands.
        assert_eq!(limits["xai-grok-4.7"], 0);
        assert_eq!(limits["anthropic-claude-opus-5"], 0);
        assert!(base_model_limits(&page, Some("us-central1")).is_empty());
    }

    #[test]
    fn a_partner_model_is_named_by_its_publisher() {
        assert_eq!(
            partner_model_id("xai", "xai/models/grok-4.7").as_deref(),
            Some("xai/grok-4.7")
        );
        assert_eq!(partner_model_id("xai", "xaix/models/grok-4.7"), None);
        assert_eq!(partner_model_id("xai", "xai/models/"), None);
        assert_eq!(partner_model_id("xai", "xai/models/a/b"), None);
    }

    #[tokio::test]
    async fn agent_platform_access_is_proven_with_a_token_count_where_requests_go() {
        let (server, mut seen) = script(
            vec![(
                200,
                json!({"publisherModels": [
                    {"name": "publishers/google/models/gemini-1.5-pro-002"},
                    {"name": "publishers/google/models/gemini-2.5-flash"},
                    {"name": "publishers/google/models/gemini-3.5-flash"},
                    {"name": "publishers/google/models/text-embedding-005"}
                ]})
                .to_string(),
            )]
            .into_iter()
            .chain(no_partners())
            .chain(no_endpoints())
            .chain(no_policy())
            .chain([
                // Newest first; one not served in the location is skipped
                // and left out.
                (404, error_body(404, "NOT_FOUND")),
                (200, counted(1)),
            ])
            .collect(),
        )
        .await;
        let models = platform(&server).check_access().await.unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.model.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gemini-1.5-pro-002",
                "gemini-2.5-flash",
                "text-embedding-005"
            ]
        );
        let seen = requests(&mut seen);
        assert!(seen[6].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-3.5-flash:countTokens "
        )));
        assert!(seen[7].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-2.5-flash:countTokens "
        )));
    }

    #[tokio::test]
    async fn agent_platform_access_skips_newer_models_a_region_lacks() {
        // Five newer models only at `global`, then one the region serves.
        let listed: Vec<Value> = [
            "gemini-4-flash",
            "gemini-3.9-flash",
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-2.5-flash",
        ]
        .iter()
        .map(|model| json!({"name": format!("publishers/google/models/{model}")}))
        .collect();
        let (server, mut seen) = script(
            vec![(200, json!({ "publisherModels": listed }).to_string())]
                .into_iter()
                .chain(no_partners())
                .chain(no_endpoints())
                .chain(no_policy())
                .chain(vec![(404, error_body(404, "NOT_FOUND")); 5])
                .chain([(200, counted(1))])
                .collect(),
        )
        .await;
        let models = platform(&server).check_access().await.unwrap();
        let ids: Vec<&str> = models.iter().map(|model| model.model.as_str()).collect();
        assert_eq!(ids, ["gemini-2.5-flash"]);
        let seen = requests(&mut seen);
        assert!(seen[11].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-2.5-flash:countTokens "
        )));
    }

    #[tokio::test]
    async fn agent_platform_access_gives_up_after_the_cap() {
        let listed: Vec<Value> = (0..MAX_ACCESS_COUNTS + 1)
            .map(
                |minor| json!({"name": format!("publishers/google/models/gemini-3.{minor}-flash")}),
            )
            .collect();
        let (server, mut seen) = script(
            vec![(200, json!({ "publisherModels": listed }).to_string())]
                .into_iter()
                .chain(no_partners())
                .chain(no_endpoints())
                .chain(no_policy())
                .chain(vec![(404, error_body(404, "NOT_FOUND")); MAX_ACCESS_COUNTS])
                .collect(),
        )
        .await;
        let error = platform(&server).check_access().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
        assert_eq!(requests(&mut seen).len(), 6 + MAX_ACCESS_COUNTS);
    }

    #[test]
    fn gemini_versions_order_newest_first() {
        assert_eq!(gemini_version("gemini-3.5-flash"), (3, 5));
        assert_eq!(gemini_version("gemini-3-flash-preview"), (3, 0));
        assert_eq!(gemini_version("gemini-4-argon"), (4, 0));
        assert_eq!(gemini_version("gemini-1.5-pro-002"), (1, 5));
        assert!(gemini_version("gemini-2.5-flash") < gemini_version("gemini-3-flash-preview"));
    }

    #[tokio::test]
    async fn agent_platform_embeds_counts_first_and_settles_reported_tokens() {
        let (server, mut seen) = script(vec![
            (200, counted(4)),
            (200, predicted(&[Some(2), Some(3)], false)),
        ])
        .await;
        let response = platform(&server)
            .embeddings(embedding_context("text-embedding-005"), texts(2))
            .await
            .unwrap();
        let seen = requests(&mut seen);
        assert!(seen[0].starts_with(&format!(
            "POST {PUBLISHER}/models/text-embedding-005:countTokens "
        )));
        assert_eq!(
            request_body(&seen[0]),
            json!({"instances": [{"content": "text 0"}, {"content": "text 1"}]})
        );
        assert!(seen[1].starts_with(&format!(
            "POST {PUBLISHER}/models/text-embedding-005:predict "
        )));
        assert_eq!(
            request_body(&seen[1]),
            json!({
                "instances": [{"content": "text 0"}, {"content": "text 1"}],
                "parameters": {"autoTruncate": false}
            })
        );
        assert_eq!(response.model(), "text-embedding-005");
        assert_eq!(response.data().len(), 2);
        assert_eq!(response.data()[1].index(), 1);
        assert_eq!(response.data()[1].vector(), &[1.5, 0.25]);
        // Google's own count, not the estimate.
        assert_eq!(input_tokens(&response), TokenCounter::exact(5));
        let usage = response.usage().unwrap().usage();
        assert_eq!(usage.output_tokens(), Some(TokenCounter::exact(0)));
    }

    #[tokio::test]
    async fn agent_platform_embeddings_without_reported_usage_settle_as_estimated() {
        let (server, _seen) = script(vec![
            (200, counted(4)),
            (200, predicted(&[Some(2), None], false)),
        ])
        .await;
        let response = platform(&server)
            .embeddings(
                embedding_context("text-multilingual-embedding-002"),
                texts(2),
            )
            .await
            .unwrap();
        assert_eq!(input_tokens(&response), TokenCounter::estimated(4));
    }

    #[tokio::test]
    async fn one_input_models_are_called_once_per_input_and_settle_as_one_request() {
        let (server, mut seen) = script(vec![
            (200, counted(3)),
            (200, counted(4)),
            (200, counted(5)),
            (200, predicted(&[Some(3)], false)),
            (200, predicted(&[Some(4)], false)),
            (200, predicted(&[Some(5)], false)),
        ])
        .await;
        let response = platform(&server)
            .embeddings(embedding_context("gemini-embedding-001"), texts(3))
            .await
            .unwrap();
        let seen = requests(&mut seen);
        // Every count before the first billable call.
        for (index, request) in seen.iter().enumerate() {
            let method = if index < 3 { "countTokens" } else { "predict" };
            assert!(
                request.starts_with(&format!(
                    "POST {PUBLISHER}/models/gemini-embedding-001:{method} "
                )),
                "{index}"
            );
            assert_eq!(
                request_body(request)["instances"],
                json!([{"content": format!("text {}", index % 3)}])
            );
        }
        let indices: Vec<u32> = response.data().iter().map(Embedding::index).collect();
        assert_eq!(indices, [0, 1, 2]);
        assert_eq!(input_tokens(&response), TokenCounter::exact(12));
    }

    #[tokio::test]
    async fn embed_content_models_report_usage_metadata() {
        let (server, mut seen) = script(vec![
            (200, counted(6)),
            (
                200,
                json!({
                    "embedding": {"values": [0.5, 0.25]},
                    "usageMetadata": {"promptTokenCount": 7, "totalTokenCount": 7},
                    "truncated": false
                })
                .to_string(),
            ),
        ])
        .await;
        let response = platform(&server)
            .embeddings(embedding_context("gemini-embedding-2-preview"), texts(1))
            .await
            .unwrap();
        let seen = requests(&mut seen);
        assert!(seen[0].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-embedding-2-preview:countTokens "
        )));
        assert_eq!(
            request_body(&seen[0]),
            json!({"contents": [{"parts": [{"text": "text 0"}]}]})
        );
        assert!(seen[1].starts_with(&format!(
            "POST {PUBLISHER}/models/gemini-embedding-2-preview:embedContent "
        )));
        assert_eq!(
            request_body(&seen[1]),
            json!({
                "content": {"role": "user", "parts": [{"text": "text 0"}]},
                "embedContentConfig": {"autoTruncate": false}
            })
        );
        assert_eq!(input_tokens(&response), TokenCounter::exact(7));
    }

    /// Google refuses `countTokens` for `gemini-embedding-2`; the embedding
    /// still goes out, settling on Google's report, else on a byte bound. A
    /// refusal other than 400 still fails the request.
    #[tokio::test]
    async fn embed_content_models_embed_when_google_will_not_count() {
        let refused = json!({"error": {
            "code": 400,
            "message": "CountTokens for embedding model `gemini-embedding-2` is not yet enabled.",
            "status": "INVALID_ARGUMENT"
        }})
        .to_string();
        let (server, mut seen) = script(vec![
            (400, refused.clone()),
            (
                200,
                json!({
                    "embedding": {"values": [0.5, 0.25]},
                    "usageMetadata": {"promptTokenCount": 2, "totalTokenCount": 2}
                })
                .to_string(),
            ),
            (400, refused),
            (
                200,
                json!({"embedding": {"values": [0.5, 0.25]}}).to_string(),
            ),
        ])
        .await;
        let adapter = platform(&server);
        let response = adapter
            .embeddings(embedding_context("gemini-embedding-2"), texts(1))
            .await
            .unwrap();
        assert_eq!(input_tokens(&response), TokenCounter::exact(2));
        // Unreported: "text 0" is six bytes, so at most six tokens.
        let response = adapter
            .embeddings(embedding_context("gemini-embedding-2"), texts(1))
            .await
            .unwrap();
        assert_eq!(input_tokens(&response), TokenCounter::estimated(6));
        assert_eq!(requests(&mut seen).len(), 4);

        let (server, mut seen) = script(vec![(
            403,
            json!({"error": {"code": 403, "status": "PERMISSION_DENIED"}}).to_string(),
        )])
        .await;
        let error = platform(&server)
            .embeddings(embedding_context("gemini-embedding-2"), texts(1))
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), Some(403));
        assert_eq!(requests(&mut seen).len(), 1);
    }

    #[tokio::test]
    async fn more_inputs_than_the_calls_allowed_are_refused_unsent() {
        // Nothing listens on this port; a call would fail as unavailable.
        let adapter = platform("http://127.0.0.1:9");
        let error = adapter
            .embeddings(
                embedding_context("gemini-embedding-001"),
                texts(MAX_EMBEDDING_CALLS + 1),
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::InvalidRequest);
        assert!(error.http_status().is_none());
        // A model the gateway does not know how to embed with.
        let error = adapter
            .embeddings(embedding_context("gemini-2.5-flash"), texts(1))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[tokio::test]
    async fn a_truncated_input_fails_with_its_billed_tokens() {
        let (server, _seen) = script(vec![
            (200, counted(9)),
            (200, predicted(&[Some(2), Some(4)], true)),
        ])
        .await;
        let error = platform(&server)
            .embeddings(embedding_context("text-embedding-005"), texts(2))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        let usage = error.partial_usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(TokenCounter::exact(6)));
    }

    #[tokio::test]
    async fn agent_platform_embedding_errors_map_as_chat_errors_do() {
        for (status, body, category) in [
            // The model is not offered in this location.
            (
                404,
                error_body(404, "NOT_FOUND"),
                ProviderErrorCategory::Provider,
            ),
            // The service account lacks the role.
            (
                403,
                error_body(403, "PERMISSION_DENIED"),
                ProviderErrorCategory::Authorization,
            ),
            (
                429,
                error_body(429, "RESOURCE_EXHAUSTED"),
                ProviderErrorCategory::RateLimited,
            ),
            // Blocked or overlong input.
            (
                400,
                error_body(400, "INVALID_ARGUMENT"),
                ProviderErrorCategory::InvalidRequest,
            ),
        ] {
            // Refused at the count: nothing billable was sent.
            let (server, mut seen) = script(vec![(status, body.clone())]).await;
            let error = platform(&server)
                .embeddings(embedding_context("text-embedding-005"), texts(2))
                .await
                .unwrap_err();
            assert_eq!(error.category(), category, "{status}");
            assert_eq!(error.http_status(), Some(status));
            assert!(error.partial_usage().is_none());
            assert_eq!(requests(&mut seen).len(), 1);
            // Refused at the call.
            let (server, _seen) = script(vec![(200, counted(4)), (status, body)]).await;
            let error = platform(&server)
                .embeddings(embedding_context("text-embedding-005"), texts(2))
                .await
                .unwrap_err();
            assert_eq!(error.category(), category, "{status}");
            assert!(error.partial_usage().is_none());
        }
    }

    #[tokio::test]
    async fn a_later_call_failing_reports_what_earlier_calls_billed() {
        let (server, _seen) = script(vec![
            (200, counted(3)),
            (200, counted(4)),
            (200, predicted(&[Some(3)], false)),
            (429, error_body(429, "RESOURCE_EXHAUSTED")),
        ])
        .await;
        let error = platform(&server)
            .embeddings(embedding_context("gemini-embedding-001"), texts(2))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        let usage = error.partial_usage().unwrap();
        assert_eq!(usage.input_tokens(), Some(TokenCounter::exact(3)));

        // A billed answer with an unusable vector keeps what was billed.
        let (server, _seen) = script(vec![
            (200, counted(3)),
            (200, counted(4)),
            (200, predicted(&[Some(3)], false)),
            (
                200,
                json!({"predictions": [{"embeddings": {"values": [],
                    "statistics": {"token_count": 4, "truncated": false}}}]})
                .to_string(),
            ),
        ])
        .await;
        let error = platform(&server)
            .embeddings(embedding_context("gemini-embedding-001"), texts(2))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        assert_eq!(
            error.partial_usage().unwrap().input_tokens(),
            Some(TokenCounter::exact(7))
        );
        // Google's JSON past f32's range is unusable too.
        assert!(vector(Some(&json!([1e300]))).is_err());

        // A call that may have run (a 5xx) is charged its count, estimated.
        let (server, _seen) = script(vec![
            (200, counted(3)),
            (200, counted(4)),
            (200, predicted(&[Some(3)], false)),
            (503, error_body(503, "UNAVAILABLE")),
        ])
        .await;
        let error = platform(&server)
            .embeddings(embedding_context("gemini-embedding-001"), texts(2))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Unavailable);
        assert_eq!(
            error.partial_usage().unwrap().input_tokens(),
            Some(TokenCounter::estimated(7))
        );
    }

    #[tokio::test]
    async fn a_probe_counts_tokens_in_the_projects_location() {
        let (server, mut seen) = script(vec![
            (200, r#"{"totalTokens":1}"#.into()),
            (
                404,
                r#"{"error":{"code":404,"message":"Publisher model ... was not found or your project does not have access to it.","status":"NOT_FOUND"}}"#.into(),
            ),
        ])
        .await;
        let adapter = platform(&server);
        assert!(adapter.probes_models());
        assert!(adapter.probes_model("gemini-4-argon"));
        // A publisher the gateway does not call.
        assert!(!adapter.probes_model("ai21/jamba-1.5"));
        adapter.probe_model("gemini-4-argon").await.unwrap();
        let missing = adapter.probe_model("gemini-4-argon").await.unwrap_err();
        assert_eq!(missing.http_status(), Some(404));
        assert_eq!(
            crate::domain::model_evidence::Verdict::of_error(&missing),
            Some(crate::domain::model_evidence::Verdict::Refused(
                crate::domain::model_evidence::EvidenceReason::NotFound
            ))
        );
        let requests = requests(&mut seen);
        assert!(
            requests[0].starts_with(&format!(
                "POST {PUBLISHER}/models/gemini-4-argon:countTokens HTTP/1.1"
            )),
            "{requests:?}"
        );
        assert_eq!(
            request_body(&requests[0]),
            json!({"contents": [{"parts": [{"text": "hi"}]}]})
        );
    }

    const PROJECT: &str = "/v1/projects/tenant-project/locations/us-central1";

    /// What keeps a test's Google token service up: the fake, the
    /// credentials file and the identity the token source refers to.
    type GoogleGuards = (
        crate::gcp_credentials::tests::FakeGoogle,
        crate::gcp_credentials::tests::TempDir,
        crate::gcp_credentials::GoogleIdentity,
    );

    /// `platform` signed with a Google token (`token-1`), as tenants' service
    /// accounts are.
    async fn google_platform(server: &str) -> (GeminiAdapter, GoogleGuards) {
        use crate::gcp_credentials::{CLOUD_PLATFORM_SCOPE, tests as google};
        let fake = google::start().await;
        let dir = google::TempDir::new();
        let identity = google::identity(
            &fake,
            &dir.write("adc.json", &google::user_credentials(&fake)),
        );
        let mut adapter = GeminiAdapter::new(&GeminiConfig {
            auth: GeminiAuth::Google {
                token: identity.token(CLOUD_PLATFORM_SCOPE),
                quota_project: None,
            },
            identity: Some(identity.clone()),
            agent_platform: crate::config::AgentPlatform::new("tenant-project", "us-central1"),
            ..config(format!("{server}{PUBLISHER}"))
        })
        .unwrap();
        adapter.service_usage_url = Url::parse(&format!("{server}/")).unwrap();
        (adapter, (fake, dir, identity))
    }

    fn head(request: &str) -> &str {
        request.lines().next().unwrap()
    }

    fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn openai_answer() -> String {
        json!({"id": "chatcmpl-1", "object": "chat.completion", "model": "m",
               "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}})
        .to_string()
    }

    #[tokio::test]
    async fn a_partner_model_takes_its_publishers_tool_options_not_geminis() {
        use crate::provider::tools::tool_conversation;
        let message = json!({"type": "message", "id": "msg_vrtx_2", "role": "assistant",
            "model": "claude-haiku-4-5", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 9, "output_tokens": 2}});
        let (server, mut seen) = script(vec![(200, message.to_string())]).await;
        let (adapter, _guards) = google_platform(&server).await;
        let serial = ChatRequest {
            parallel_tool_calls: Some(false),
            ..tool_conversation()
        };
        // Claude takes one call at a time: sent, as Anthropic's API asks.
        let claude = embedding_context("anthropic/claude-haiku-4-5@20251001");
        adapter.validate_chat_request(&claude, &serial).unwrap();
        adapter.chat(claude, serial.clone()).await.unwrap();
        let body = request_body(&requests(&mut seen)[0]);
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
        // Google's own models cannot, and are refused unsent.
        let error = adapter
            .validate_chat_request(&embedding_context("gemini-2.5-flash"), &serial)
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::InvalidRequest);
        adapter
            .validate_chat_request(&embedding_context("gemini-2.5-flash"), &tool_conversation())
            .unwrap();
    }

    #[tokio::test]
    async fn claude_is_called_through_raw_predict_with_the_vertex_version() {
        let message = json!({"type": "message", "id": "msg_vrtx_1", "role": "assistant",
            "model": "claude-haiku-4-5", "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 9, "output_tokens": 2}});
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let (server, mut seen) = script(vec![
            (200, message.to_string()),
            (200, r#"{"input_tokens":9}"#.to_owned()),
            (200, stream.to_owned()),
        ])
        .await;
        let (adapter, _guards) = google_platform(&server).await;
        let context = embedding_context("anthropic/claude-haiku-4-5@20251001");
        adapter.validate_chat_request(&context, &request()).unwrap();
        let response = adapter.chat(context.clone(), request()).await.unwrap();
        assert_eq!(response.choices()[0].message.content, "ok");
        let usage = response.usage().unwrap().usage();
        assert_eq!(usage.input_tokens(), Some(TokenCounter::exact(9)));
        assert_eq!(
            adapter
                .count_chat_input_tokens(&context, &request())
                .await
                .unwrap(),
            9
        );
        let mut events = adapter.chat_stream(context, request()).await.unwrap();
        while let Some(event) = events.next().await {
            event.unwrap();
        }
        let seen = requests(&mut seen);
        assert_eq!(
            head(&seen[0]),
            format!(
                "POST {PROJECT}/publishers/anthropic/models/claude-haiku-4-5@20251001:rawPredict HTTP/1.1"
            )
        );
        assert_eq!(header(&seen[0], "authorization"), Some("Bearer token-1"));
        // The version is in the body, which names no model.
        assert_eq!(header(&seen[0], "anthropic-version"), None);
        let body = request_body(&seen[0]);
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert!(body.get("model").is_none());
        assert_eq!(body["max_tokens"], 12);
        assert_eq!(body["stream"], false);
        // The free count names the model it counts for.
        assert_eq!(
            head(&seen[1]),
            format!("POST {PROJECT}/publishers/anthropic/models/count-tokens:rawPredict HTTP/1.1")
        );
        let count = request_body(&seen[1]);
        assert_eq!(count["model"], "claude-haiku-4-5@20251001");
        assert!(count.get("anthropic_version").is_none());
        assert!(count.get("max_tokens").is_none());
        assert_eq!(
            head(&seen[2]),
            format!(
                "POST {PROJECT}/publishers/anthropic/models/claude-haiku-4-5@20251001:streamRawPredict HTTP/1.1"
            )
        );
        assert_eq!(request_body(&seen[2])["stream"], true);
    }

    #[tokio::test]
    async fn mistral_and_xai_refusals_keep_agent_platforms_model_garden_code() {
        use crate::domain::model_evidence::{EvidenceReason, MODEL_GARDEN_CODE_SUFFIX, Verdict};
        let terms = |code: u16, status: &str| {
            json!({"error": {"code": code, "status": status,
                "message": "Please enable the model in Model Garden and accept its terms of service."}})
            .to_string()
        };
        let denied = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "Permission 'aiplatform.endpoints.predict' denied."}});
        // A publisher's own refusal, in OpenAI's shape, is read as before.
        let own = json!({"error": {"message": "bad tool", "type": "invalid_request_error", "code": "bad_tool"}});
        let (server, _seen) = script(vec![
            (400, terms(400, "FAILED_PRECONDITION")),
            (403, terms(403, "PERMISSION_DENIED")),
            (400, terms(400, "FAILED_PRECONDITION")),
            (403, terms(403, "PERMISSION_DENIED")),
            (403, denied.to_string()),
            (400, own.to_string()),
        ])
        .await;
        let (adapter, _guards) = google_platform(&server).await;
        for (model, status, code) in [
            ("mistralai/mistral-medium-3@001", 400, "FAILED_PRECONDITION"),
            ("mistralai/mistral-medium-3@001", 403, "PERMISSION_DENIED"),
            ("xai/grok-4.7", 400, "FAILED_PRECONDITION"),
            ("xai/grok-4.7", 403, "PERMISSION_DENIED"),
        ] {
            let error = adapter
                .chat(embedding_context(model), request())
                .await
                .unwrap_err();
            assert_eq!(error.http_status(), Some(status), "{model}");
            assert_eq!(
                error.error_type(),
                Some(format!("{code}{MODEL_GARDEN_CODE_SUFFIX}").as_str()),
                "{model}"
            );
            assert_eq!(
                Verdict::of_error(&error),
                Some(Verdict::RequiresAction(EvidenceReason::EnableInModelGarden)),
                "{model} {status}"
            );
        }
        let error = adapter
            .chat(embedding_context("xai/grok-4.7"), request())
            .await
            .unwrap_err();
        assert_eq!(error.error_type(), Some("PERMISSION_DENIED"));
        assert_eq!(
            Verdict::of_error(&error),
            Some(Verdict::Refused(EvidenceReason::AccessDenied))
        );
        let error = adapter
            .chat(embedding_context("xai/grok-4.7"), request())
            .await
            .unwrap_err();
        assert_eq!(error.error_type(), Some("bad_tool"));
    }

    #[tokio::test]
    async fn mistral_and_openai_compatible_partners_take_their_own_bodies() {
        let (server, mut seen) = script(vec![(200, openai_answer()), (200, openai_answer())]).await;
        let (adapter, _guards) = google_platform(&server).await;
        let response = adapter
            .chat(
                embedding_context("mistralai/mistral-medium-3@001"),
                request(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.usage().unwrap().usage().output_tokens(),
            Some(TokenCounter::exact(1))
        );
        adapter
            .chat(embedding_context("xai/grok-4.7"), request())
            .await
            .unwrap();
        let seen = requests(&mut seen);
        assert_eq!(
            head(&seen[0]),
            format!(
                "POST {PROJECT}/publishers/mistralai/models/mistral-medium-3@001:rawPredict HTTP/1.1"
            )
        );
        assert_eq!(header(&seen[0], "authorization"), Some("Bearer token-1"));
        // Mistral's body names the model without its version.
        assert_eq!(request_body(&seen[0])["model"], "mistral-medium-3");
        assert_eq!(
            head(&seen[1]),
            format!("POST {PROJECT}/endpoints/openapi/chat/completions HTTP/1.1")
        );
        assert_eq!(request_body(&seen[1])["model"], "xai/grok-4.7");
        assert_eq!(request_body(&seen[1])["max_tokens"], 12);
        // Neither counts for free.
        for model in ["mistralai/mistral-medium-3", "xai/grok-4.7", "endpoints/12"] {
            let error = adapter
                .count_chat_input_tokens(&embedding_context(model), &request())
                .await
                .unwrap_err();
            assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
        }
    }

    #[tokio::test]
    async fn partner_models_and_endpoints_need_agent_platform_and_a_known_publisher() {
        // A Gemini API key cannot call them: refused before any call.
        let keyed = adapter();
        for model in ["anthropic/claude-haiku-4-5", "xai/grok-4.7", "endpoints/12"] {
            let error = keyed
                .validate_chat_request(&embedding_context(model), &request())
                .unwrap_err();
            assert_eq!(
                error.category(),
                ProviderErrorCategory::UnsupportedFeature,
                "{model}"
            );
            assert!(!keyed.probes_model(model));
        }
        let platform = platform("http://127.0.0.1:9");
        for model in ["ai21/jamba-1.5", "google/gemini-2.5-flash", "a/b/c"] {
            let error = platform
                .chat(embedding_context(model), request())
                .await
                .unwrap_err();
            assert_eq!(
                error.category(),
                ProviderErrorCategory::UnsupportedFeature,
                "{model}"
            );
        }
        // Chat only.
        let error = platform
            .embeddings(embedding_context("anthropic/claude-haiku-4-5"), texts(1))
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
        // The probes that cost the tenant wait for a deployment.
        assert!(!platform.probes_only_when_deployed("gemini-2.5-flash"));
        assert!(!platform.probes_only_when_deployed("anthropic/claude-haiku-4-5"));
        assert!(platform.probes_only_when_deployed("mistralai/codestral-2"));
        assert!(platform.probes_only_when_deployed("xai/grok-4.7"));
        assert!(platform.probes_only_when_deployed("endpoints/12"));
    }

    #[tokio::test]
    async fn an_endpoint_is_called_through_what_it_serves_and_read_again_when_gone() {
        let gemini_endpoint = json!({
            "name": "projects/123/locations/us-central1/endpoints/12",
            "deployedModels": [{"id": "1", "model": "publishers/google/models/gemini-2.5-flash"}]
        });
        let answer = json!({"candidates": [{"content": {"parts": [{"text": "ok"}]},
            "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1}});
        let (server, mut seen) = script(vec![
            (200, gemini_endpoint.to_string()),
            (200, answer.to_string()),
            (200, answer.to_string()),
            (404, error_body(404, "NOT_FOUND")),
            (200, gemini_endpoint.to_string()),
            (200, answer.to_string()),
        ])
        .await;
        let adapter = platform(&server);
        let context = embedding_context("endpoints/12");
        for _ in 0..2 {
            adapter.chat(context.clone(), request()).await.unwrap();
        }
        let gone = adapter.chat(context.clone(), request()).await.unwrap_err();
        assert_eq!(gone.http_status(), Some(404));
        adapter.chat(context, request()).await.unwrap();
        let heads: Vec<String> = requests(&mut seen)
            .iter()
            .map(|request| head(request).to_owned())
            .collect();
        let get = format!("GET {PROJECT}/endpoints/12 HTTP/1.1");
        let post = format!("POST {PROJECT}/endpoints/12:generateContent HTTP/1.1");
        // Read once, called twice from the cache; after the 404, read again.
        assert_eq!(
            heads,
            [&get, &post, &post, &post, &get, &post].map(String::clone)
        );
    }

    #[tokio::test]
    async fn an_endpoint_serving_openai_chat_is_called_without_a_model() {
        let endpoint = json!({
            "name": "projects/123/locations/us-central1/endpoints/7",
            "deployedModels": [
                {"id": "1", "model": "projects/123/locations/us-central1/models/gemma3-1b"},
                // No share of the traffic: not served.
                {"id": "2", "model": "projects/123/locations/us-central1/models/custom"}
            ],
            "trafficSplit": {"1": 100, "2": 0}
        });
        let gemma =
            json!({"containerSpec": {"imageUri": "us-docker.pkg.dev/x/pytorch-vllm-serve:1"}});
        let (server, mut seen) = script(vec![
            (200, endpoint.to_string()),
            (200, gemma.to_string()),
            (200, openai_answer()),
            (200, openai_answer()),
        ])
        .await;
        let (adapter, _guards) = google_platform(&server).await;
        let context = embedding_context("endpoints/7");
        adapter.chat(context.clone(), request()).await.unwrap();
        // The probe, a one-token call.
        adapter.probe_model("endpoints/7").await.unwrap();
        let seen = requests(&mut seen);
        assert_eq!(
            head(&seen[0]),
            format!("GET {PROJECT}/endpoints/7 HTTP/1.1")
        );
        assert_eq!(
            head(&seen[1]),
            "GET /v1/projects/123/locations/us-central1/models/gemma3-1b HTTP/1.1"
        );
        assert_eq!(
            head(&seen[2]),
            format!("POST {PROJECT}/endpoints/7/chat/completions HTTP/1.1")
        );
        assert!(request_body(&seen[2]).get("model").is_none());
        assert_eq!(request_body(&seen[3])["max_tokens"], 1);
    }

    #[tokio::test]
    async fn an_endpoint_of_unknown_or_mixed_apis_is_not_called() {
        let mixed = json!({
            "deployedModels": [
                {"id": "1", "model": "publishers/google/models/gemini-2.5-flash"},
                {"id": "2", "model": "projects/123/locations/us-central1/models/gemma"}
            ],
            "trafficSplit": {"1": 50, "2": 50}
        });
        let gemma =
            json!({"containerSpec": {"imageUri": "us-docker.pkg.dev/x/pytorch-vllm-serve:1"}});
        let (server, mut seen) =
            script(vec![(200, mixed.to_string()), (200, gemma.to_string())]).await;
        let adapter = platform(&server);
        let error = adapter
            .chat(embedding_context("endpoints/9"), request())
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
        // Nothing was posted.
        assert!(
            requests(&mut seen)
                .iter()
                .all(|request| request.starts_with("GET "))
        );
    }

    #[tokio::test]
    async fn partner_probes_record_quota_and_model_garden_refusals() {
        use crate::domain::model_evidence::{
            BASE_MODEL_QUOTA_CODE, EvidenceKey, EvidenceReason, Observation, Verdict,
        };
        let quota = json!({"error": {"code": 429, "status": "RESOURCE_EXHAUSTED",
            "message": "Quota exceeded for aiplatform.googleapis.com/online_prediction_requests_per_base_model with base model: anthropic-count-tokens."}});
        let terms = json!({"error": {"code": 400, "status": "FAILED_PRECONDITION",
            "message": "Please enable the model in Model Garden and accept its terms of service."}});
        let (server, mut seen) = script(vec![
            (200, r#"{"input_tokens":1}"#.to_owned()),
            (429, quota.to_string()),
            (400, terms.to_string()),
            (200, openai_answer()),
        ])
        .await;
        let adapter = platform(&server);
        assert!(adapter.probes_model("anthropic/claude-haiku-4-5"));
        adapter
            .probe_model("anthropic/claude-haiku-4-5")
            .await
            .unwrap();
        let exhausted = adapter
            .probe_model("anthropic/claude-haiku-4-5")
            .await
            .unwrap_err();
        assert_eq!(exhausted.error_type(), Some(BASE_MODEL_QUOTA_CODE));
        let key = EvidenceKey {
            organization_id: crate::domain::identity::OrganizationId(uuid::Uuid::nil()),
            adapter_instance_key: "gemini-1".into(),
            family: "gemini".into(),
            model: "anthropic/claude-haiku-4-5".into(),
        };
        assert_eq!(
            Observation::of_probe(key, Err(&exhausted), chrono::Utc::now())
                .unwrap()
                .verdict,
            Verdict::RequiresAction(EvidenceReason::QuotaZero)
        );
        let not_enabled = adapter
            .probe_model("anthropic/claude-haiku-4-5")
            .await
            .unwrap_err();
        assert_eq!(
            Verdict::of_error(&not_enabled),
            Some(Verdict::RequiresAction(EvidenceReason::EnableInModelGarden))
        );
        adapter.probe_model("mistralai/codestral-2").await.unwrap();
        let seen = requests(&mut seen);
        assert_eq!(
            head(&seen[0]),
            format!("POST {PROJECT}/publishers/anthropic/models/count-tokens:rawPredict HTTP/1.1")
        );
        assert_eq!(
            request_body(&seen[0]),
            json!({"model": "claude-haiku-4-5",
                   "messages": [{"role": "user", "content": "hi"}]})
        );
        assert_eq!(
            head(&seen[3]),
            format!("POST {PROJECT}/publishers/mistralai/models/codestral-2:rawPredict HTTP/1.1")
        );
        assert_eq!(request_body(&seen[3])["max_tokens"], 1);
    }

    #[test]
    fn partner_models_are_listed_with_the_api_that_calls_them() {
        let listed = DiscoveredModel::new("anthropic/claude-haiku-4-5", None).unwrap();
        let listed = partner::with_publisher_format(listed, "anthropic");
        assert_eq!(
            listed.deployment.unwrap().format.as_deref(),
            Some("anthropic_messages")
        );
        let other = partner::with_publisher_format(
            DiscoveredModel::new("ai21/jamba", None).unwrap(),
            "ai21",
        );
        assert!(other.deployment.is_none());
    }

    #[test]
    fn tools_map_to_function_declarations_calls_and_responses() {
        use crate::provider::tools::tool_conversation;
        let mut request = tool_conversation();
        // The first call carries the thought signature Gemini gave with it.
        request.messages[2].tool_calls[0].id = Some("toolu_01~sig~c2lnbmF0dXJl".into());
        request.messages[3].tool_call_id = Some("toolu_01~sig~c2lnbmF0dXJl".into());
        let body = adapter().request_body(&request).unwrap();
        assert_eq!(
            body["tools"],
            json!([{"functionDeclarations": [{"name": "get_weather", "description": "Current weather",
                "parametersJsonSchema": {"type": "object", "properties": {"city": {"type": "string"}}}}]}])
        );
        assert!(body.get("toolConfig").is_none());
        assert_eq!(
            body["contents"],
            json!([
                {"role": "user", "parts": [{"text": "weather in Paris and Rome?"}]},
                {"role": "model", "parts": [
                    {"text": "Checking."},
                    {"functionCall": {"id": "toolu_01", "name": "get_weather", "args": {"city": "Paris"}},
                     "thoughtSignature": "c2lnbmF0dXJl"},
                    {"functionCall": {"id": "toolu_02", "name": "get_weather", "args": {"city": "Rome"}}}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "toolu_01", "name": "get_weather", "response": {"output": "18C"}}},
                    {"functionResponse": {"id": "toolu_02", "name": "get_weather", "response": {"output": "24C"}}}
                ]}
            ])
        );
        let config = |tool_choice| {
            adapter()
                .request_body(&ChatRequest {
                    tool_choice: Some(tool_choice),
                    ..tool_conversation()
                })
                .unwrap()["toolConfig"]
                .clone()
        };
        assert_eq!(
            config(ToolChoice::Auto),
            json!({"functionCallingConfig": {"mode": "AUTO"}})
        );
        assert_eq!(
            config(ToolChoice::None),
            json!({"functionCallingConfig": {"mode": "NONE"}})
        );
        assert_eq!(
            config(ToolChoice::Required),
            json!({"functionCallingConfig": {"mode": "ANY"}})
        );
        assert_eq!(
            config(ToolChoice::Function("get_weather".into())),
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}})
        );
        // Gemini cannot be asked for one call at a time.
        let serial = ChatRequest {
            parallel_tool_calls: Some(false),
            ..tool_conversation()
        };
        assert!(validate_batch_line(&serial).is_err());
        assert!(validate_batch_line(&tool_conversation()).is_ok());
    }

    #[test]
    fn function_call_parts_are_tool_calls_with_their_signatures() {
        // generateContent's documented response to a tool request: Gemini 3
        // gives a thought signature with the first call and may give no id.
        let value = json!({
            "responseId": "resp-1",
            "candidates": [{"index": 0, "content": {"role": "model", "parts": [
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}},
                 "thoughtSignature": "c2lnbmF0dXJl"},
                {"functionCall": {"id": "fc-2", "name": "now"}}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
        });
        let response = parse_response(value, "gemini-3".into()).unwrap();
        let choice = &response.choices()[0];
        assert_eq!(choice.message.content, "");
        assert_eq!(choice.finish_reason.as_ref().unwrap().0, "tool_calls");
        assert_eq!(
            choice.tool_calls,
            vec![
                ToolCall {
                    id: Some("call_0~sig~c2lnbmF0dXJl".into()),
                    name: Some("get_weather".into()),
                    arguments: r#"{"city":"Paris"}"#.into(),
                },
                ToolCall {
                    id: Some("fc-2".into()),
                    name: Some("now".into()),
                    arguments: "{}".into(),
                },
            ]
        );
        assert_eq!(
            split_call_id("call_0~sig~c2lnbmF0dXJl"),
            ("call_0", Some("c2lnbmF0dXJl"))
        );
    }

    #[tokio::test]
    async fn streamed_function_calls_become_whole_tool_call_deltas() {
        let body = concat!(
            "data: {\"responseId\":\"s-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Checking\"}]}}]}\n\n",
            "data: {\"responseId\":\"s-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Paris\"}},\"thoughtSignature\":\"c2ln\"}]}}]}\n\n",
            "data: {\"responseId\":\"s-1\",\"candidates\":[{\"index\":0,\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"now\",\"args\":{}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":2}}\n\n"
        );
        let events = gemini_sse_stream(
            futures::stream::iter(vec![Ok(bytes::Bytes::from(body))]),
            "gemini-3".into(),
            1 << 16,
            Duration::from_secs(1),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
        let deltas = events
            .iter()
            .filter_map(|event| match event {
                ChatStreamEvent::ToolCallDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas.len(), 2);
        assert_eq!(
            (
                deltas[0].tool_call_index,
                deltas[0].id.as_deref(),
                deltas[0].name.as_deref()
            ),
            (0, Some("call_0~sig~c2ln"), Some("get_weather"))
        );
        assert_eq!(deltas[0].arguments_delta, r#"{"city":"Paris"}"#);
        assert_eq!(
            (deltas[1].tool_call_index, deltas[1].id.as_deref()),
            (1, Some("call_1"))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ChatStreamEvent::ChoiceFinished(finished) if finished.finish_reason().0 == "tool_calls"
        )));
    }
}
