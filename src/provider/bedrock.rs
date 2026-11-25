use async_stream::stream;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::{Client, Response, StatusCode, Url, header};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    time::Duration,
};

use super::{
    ChatEventStream, InferenceProviderAdapter, MAX_MODEL_LIST_PAGES, ValidatingChatEventStream,
    model_list::{PageShape, next_token, push_page, too_many_pages},
};
use crate::{
    config::BedrockConfig,
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

const PROVIDER: &str = "bedrock";
const MAX_ERROR_BODY: usize = 16 * 1024;
const AWS_SERVICE: &str = "bedrock";
/// Availability checks run at once during a listing.
const AVAILABILITY_CONCURRENCY: usize = 8;
/// Most foundation models one listing checks; the rest stay unchecked.
const MAX_AVAILABILITY_CHECKS: usize = 512;
/// How long a listing spends checking availability. Checks still running
/// then are dropped and their models stay unchecked, so the checks never
/// cost the listing itself.
const AVAILABILITY_BUDGET: Duration = Duration::from_secs(30);
/// Titan embeds one text per `InvokeModel`; this many run at once.
const TITAN_EMBED_CONCURRENCY: usize = 8;
/// Most texts one Cohere Embed `InvokeModel` takes.
const COHERE_EMBED_BATCH: usize = 96;
/// The input token count Bedrock reports for an `InvokeModel` call.
const INPUT_TOKEN_COUNT_HEADER: &str = "x-amzn-bedrock-input-token-count";

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    !crc
}

pub(crate) struct BedrockAdapter {
    client: Client,
    base_url: Url,
    control_base_url: Url,
    region: String,
    credentials: crate::aws_credentials::AwsCredentialSource,
    /// S3 for batch staging, path-style; `None` for the bucket's regional
    /// endpoint.
    s3_base_url: Option<Url>,
    max_response_bytes: usize,
    request_timeout: Duration,
    idle_read_timeout: Duration,
}

impl fmt::Debug for BedrockAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockAdapter")
            .field("base_url", &self.base_url)
            .field("control_base_url", &self.control_base_url)
            .field("region", &self.region)
            .field("credentials", &self.credentials)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

impl BedrockAdapter {
    pub(crate) fn new(config: &BedrockConfig) -> Result<Self, ProviderError> {
        let base_url = Url::parse(&format!("{}/", config.base_url.trim_end_matches('/')))
            .map_err(|_| protocol_error())?;
        let control_base_url = Url::parse(&format!(
            "{}/",
            config.control_base_url.trim_end_matches('/')
        ))
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
        Ok(Self {
            client,
            base_url,
            control_base_url,
            region: config.region.clone(),
            credentials: config.credentials.clone(),
            s3_base_url: config
                .s3_base_url
                .as_deref()
                .map(|url| Url::parse(&format!("{}/", url.trim_end_matches('/'))))
                .transpose()
                .map_err(|_| protocol_error())?,
            max_response_bytes: config.max_response_bytes,
            request_timeout: config.request_timeout,
            idle_read_timeout: config.request_timeout,
        })
    }

    /// Bedrock's `modelId` path parameter accepts both plain model IDs and
    /// full inference-profile / provisioned-model ARNs, the latter
    /// containing `/` and `:`. `percent_encode_path_segment` escapes every
    /// character outside the unreserved set, so the whole identifier
    /// (slashes included) is safe to encode into a single path segment.
    fn endpoint(&self, model: &str, method: &str) -> Result<Url, ProviderError> {
        if model.is_empty() || model.contains(['?', '#', ' ', '\n', '\r']) {
            return Err(internal_error());
        }
        let path = format!("model/{}/{method}", percent_encode_path_segment(model));
        self.base_url.join(&path).map_err(|_| internal_error())
    }

    fn request_body(&self, request: &ChatRequest) -> Result<Value, ProviderError> {
        validate_chat_request(request)?;
        let mut messages = Vec::with_capacity(request.messages.len());
        let mut system = None;
        // Tools map to Converse's own (D-032): an assistant message's calls
        // to `toolUse` blocks after its text, and the `tool` messages that
        // answer them to one user message of `toolResult` blocks.
        let mut results_open = false;
        for message in &request.messages {
            let is_result = message.role == ChatRole::Tool;
            match message.role {
                ChatRole::System => system = Some(message.content.clone()),
                ChatRole::User => {
                    messages.push(json!({"role": "user", "content": [{"text": message.content}]}))
                }
                ChatRole::Assistant if message.tool_calls.is_empty() => messages
                    .push(json!({"role": "assistant", "content": [{"text": message.content}]})),
                ChatRole::Assistant => {
                    let mut content = Vec::with_capacity(message.tool_calls.len() + 1);
                    if !message.content.is_empty() {
                        content.push(json!({"text": message.content}));
                    }
                    for call in &message.tool_calls {
                        content.push(json!({"toolUse": {
                            "toolUseId": call.id.as_deref().ok_or_else(invalid_request)?,
                            "name": call.name.as_deref().ok_or_else(invalid_request)?,
                            "input": super::tool_arguments(&call.arguments)?,
                        }}));
                    }
                    messages.push(json!({"role": "assistant", "content": content}));
                }
                ChatRole::Tool => {
                    // Converse refuses an empty text block: an empty result
                    // goes as an empty JSON object instead.
                    let result = if message.content.is_empty() {
                        json!({"json": {}})
                    } else {
                        json!({"text": message.content})
                    };
                    let block = json!({"toolResult": {
                        "toolUseId": message.tool_call_id.as_deref().ok_or_else(invalid_request)?,
                        "content": [result],
                    }});
                    match messages.last_mut() {
                        Some(last) if results_open => last["content"]
                            .as_array_mut()
                            .ok_or_else(invalid_request)?
                            .push(block),
                        _ => messages.push(json!({"role": "user", "content": [block]})),
                    }
                }
                ChatRole::Developer => return Err(invalid_request()),
            }
            results_open = is_result;
        }
        if messages.is_empty() {
            return Err(invalid_request());
        }
        let mut body = json!({ "messages": messages });
        if let Some(tool_config) = tool_config(request)? {
            body["toolConfig"] = tool_config;
        }
        if let Some(system) = system {
            body["system"] = json!([{"text": system}]);
        }
        let mut inference_config = serde_json::Map::new();
        if let Some(temperature) = request.temperature {
            inference_config.insert("temperature".to_owned(), json!(temperature));
        }
        if let Some(max_tokens) = request.max_tokens {
            inference_config.insert("maxTokens".to_owned(), json!(max_tokens));
        }
        if !inference_config.is_empty() {
            body["inferenceConfig"] = Value::Object(inference_config);
        }
        Ok(body)
    }

    /// Compute AWS SigV4 signing headers for the exact bytes about to be sent.
    /// The caller must send precisely `body` as the request payload: the
    /// signature covers its SHA-256 digest, not a value reserialized later.
    fn sign(
        &self,
        credentials: &crate::aws::AwsCredentials,
        method: &str,
        url: &Url,
        body: &[u8],
        amz_date: &str,
    ) -> Result<Vec<(header::HeaderName, header::HeaderValue)>, ProviderError> {
        crate::aws::sign(
            credentials,
            &self.region,
            AWS_SERVICE,
            &crate::aws::SignedRequest {
                method,
                url,
                headers: &[("content-type", "application/json")],
                body,
            },
            amz_date,
        )
        .map_err(|_| internal_error())
    }

    /// The credentials to sign with now. Failing to get them means nothing
    /// was sent; a refused role assumption is an authentication failure.
    async fn credentials(&self) -> Result<crate::aws::AwsCredentials, ProviderError> {
        self.credentials
            .get_within(self.request_timeout)
            .await
            .map_err(super::credentials_error)
    }

    async fn signed_request(
        &self,
        url: Url,
        body: Vec<u8>,
        accept: &'static str,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let credentials = self.credentials().await?;
        let amz_date = crate::aws::amz_date_now();
        let signed_headers = self.sign(&credentials, "POST", &url, &body, &amz_date)?;
        let mut builder = self
            .client
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, accept)
            .body(body);
        for (name, value) in signed_headers {
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    /// A signed, bodiless GET against the control plane.
    async fn signed_get(&self, url: Url) -> Result<reqwest::RequestBuilder, ProviderError> {
        let credentials = self.credentials().await?;
        let amz_date = crate::aws::amz_date_now();
        let signed_headers = self.sign(&credentials, "GET", &url, b"", &amz_date)?;
        let mut builder = self
            .client
            .get(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json");
        for (name, value) in signed_headers {
            builder = builder.header(name, value);
        }
        Ok(builder)
    }

    /// One control-plane listing page at `path` with `query` pairs, which
    /// must be given sorted by name.
    async fn control_page(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value, ProviderError> {
        let mut url = self
            .control_base_url
            .join(path)
            .map_err(|_| internal_error())?;
        let query = list_query(query);
        url.set_query(Some(query.as_str()).filter(|query| !query.is_empty()));
        let response = send_with_timeout(self.signed_get(url).await?, self.request_timeout).await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    async fn response_error(&self, response: Response) -> ProviderError {
        let status = response.status();
        let error_type_header = response
            .headers()
            .get("x-amzn-errortype")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let retry_after = retry_after(response.headers());
        let bytes = bounded_bytes(response, MAX_ERROR_BODY, self.idle_read_timeout)
            .await
            .unwrap_or_default();
        let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
        status_error(status, &value, error_type_header, request_id, retry_after)
    }
}

#[async_trait]
impl InferenceProviderAdapter for BedrockAdapter {
    fn validate_chat_request(
        &self,
        _context: &InferenceContext,
        request: &ChatRequest,
    ) -> Result<(), ProviderError> {
        validate_chat_request(request)
    }

    async fn chat(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let model = context.deployment.model().to_owned();
        let url = self.endpoint(&model, "converse")?;
        let body =
            serde_json::to_vec(&self.request_body(&request)?).map_err(|_| internal_error())?;
        let response = send_with_timeout(
            self.signed_request(url, body, "application/json").await?,
            self.request_timeout,
        )
        .await?;
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        parse_response(value, model, request_id)
    }

    async fn chat_stream(
        &self,
        context: InferenceContext,
        request: ChatRequest,
    ) -> Result<ChatEventStream, ProviderError> {
        let model = context.deployment.model().to_owned();
        let url = self.endpoint(&model, "converse-stream")?;
        let body =
            serde_json::to_vec(&self.request_body(&request)?).map_err(|_| internal_error())?;
        let response = send_with_timeout(
            self.signed_request(url, body, "application/vnd.amazon.eventstream")
                .await?,
            self.request_timeout,
        )
        .await?;
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let max = self.max_response_bytes;
        let idle = self.idle_read_timeout;
        let stream = bedrock_converse_stream(response.bytes_stream(), model, request_id, max, idle);
        Ok(ValidatingChatEventStream::new(stream))
    }

    /// Bedrock has no common embeddings operation: each model family takes
    /// its own `InvokeModel` body (see [`EmbeddingSchema`]). Titan embeds one
    /// text per call, so the inputs go out [`TITAN_EMBED_CONCURRENCY`] calls
    /// at a time; Cohere takes up to [`COHERE_EMBED_BATCH`] texts per call.
    /// Usage is the input token count Bedrock reports for each call, summed;
    /// it is left unknown when any call omits it.
    ///
    /// A request of several calls can fail after some were billed. The
    /// error then carries what they cost as partial usage, so it is never
    /// retried or failed over to bill the same texts twice (see [`Billed`]).
    async fn embeddings(
        &self,
        context: InferenceContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ProviderError> {
        if request.input.is_empty() {
            return Err(invalid_request());
        }
        let model = context.deployment.model().to_owned();
        let schema = embedding_schema(&model)
            .ok_or_else(|| ProviderError::new(ProviderErrorCategory::UnsupportedFeature))?;
        let mut billed = Billed::default();
        let mut vectors = Vec::with_capacity(request.input.len());
        let mut request_id = None;
        let calls: Vec<&[String]> = match schema {
            EmbeddingSchema::Titan => request.input.chunks(1).collect(),
            EmbeddingSchema::Cohere => request.input.chunks(COHERE_EMBED_BATCH).collect(),
        };
        let window = match schema {
            EmbeddingSchema::Titan => TITAN_EMBED_CONCURRENCY,
            EmbeddingSchema::Cohere => 1,
        };
        for group in calls.chunks(window) {
            // Collected first: a closure inside the future type trips
            // async_trait's `Send` lifetime inference.
            let pending: Vec<_> = group
                .iter()
                .map(|texts| self.invoke(&model, embed_body(schema, texts)))
                .collect();
            // Every call of the group runs to its end, so a failure knows
            // what the others cost; no later group is sent.
            let outcomes = futures::future::join_all(pending).await;
            let mut failure = None;
            for (texts, outcome) in group.iter().zip(outcomes) {
                match outcome {
                    Ok(call) => {
                        billed.add(schema, &call, texts);
                        if request_id.is_none() {
                            request_id.clone_from(&call.request_id);
                        }
                        match embedded_vectors(schema, &call.value, texts.len()) {
                            Ok(batch) => vectors.extend(batch),
                            Err(error) => {
                                failure.get_or_insert(error);
                            }
                        }
                    }
                    Err(error) => {
                        if !refused(&error) {
                            billed.maybe(texts);
                        }
                        failure.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = failure {
                return Err(billed.attach(error));
            }
        }
        let data = vectors
            .into_iter()
            .enumerate()
            .map(|(index, vector)| {
                Embedding::new(u32::try_from(index).map_err(|_| protocol_error())?, vector)
                    .map_err(|_| protocol_error())
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| billed.attach(error))?;
        let usage = billed
            .reported()
            .map(|tokens| {
                Usage::embedding(TokenCounter::exact(tokens))
                    .ok()
                    .and_then(|usage| UsageSnapshot::final_(usage).ok())
                    .ok_or_else(protocol_error)
            })
            .transpose()?;
        EmbeddingResponse::new(request_id, model, data, usage).map_err(|_| protocol_error())
    }

    /// Foundation models, then the system-defined cross-region inference
    /// profiles, whose identifiers a deployment may name instead, then the
    /// account's own resources by ARN (see [`OWN_RESOURCES`]). A foundation model
    /// whose `inferenceTypesSupported` lacks `ON_DEMAND` answers its own
    /// identifier only with an error, so it is listed as `ProfileOnly`: still
    /// reported (its name prices its profiles) but never offered.
    ///
    /// Each model then carries what `GetFoundationModelAvailability` says
    /// about its foundation model (a profile's, for a profile), when the
    /// check could be made: see [`Self::entitlements`].
    async fn list_models(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        self.listing(true).await
    }

    /// Batch inference jobs (D-021) stage through the tenant's S3 bucket,
    /// so they are reached only with the connection's staging.
    fn staged_batches(
        &self,
        staging: &super::batch::BatchStaging,
    ) -> Option<Box<dyn super::batch::BatchProvider + '_>> {
        batch::StagedBedrock::new(self, staging)
            .map(|staged| Box::new(staged) as Box<dyn super::batch::BatchProvider + '_>)
    }

    /// A one-token `Converse` (D-018): the cheapest call Bedrock bills, as
    /// it has no free per-account call check. Charged to the account, a
    /// fraction of a cent.
    fn probes_models(&self) -> bool {
        true
    }

    fn probes_need_entitlement(&self) -> bool {
        true
    }

    /// An embedding model takes no `Converse`, so it is probed with one
    /// short text through `InvokeModel`.
    async fn probe_model(&self, model: &str) -> Result<(), ProviderError> {
        match embedding_schema(model) {
            Some(EmbeddingSchema::Titan) => {
                return self
                    .invoke(model, json!({ "inputText": "hi" }))
                    .await
                    .map(drop);
            }
            Some(EmbeddingSchema::Cohere) => {
                return self
                    .invoke(model, cohere_embed_body(&["hi".to_owned()]))
                    .await
                    .map(drop);
            }
            None => {}
        }
        let url = self.endpoint(model, "converse")?;
        let body = serde_json::to_vec(&json!({
            "messages": [{"role": "user", "content": [{"text": "hi"}]}],
            "inferenceConfig": {"maxTokens": 1}
        }))
        .map_err(|_| internal_error())?;
        let response = send_with_timeout(
            self.signed_request(url, body, "application/json").await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        Ok(())
    }

    /// The listing without availability checks: they are optional, and
    /// verifying a credential must not wait on them.
    async fn check_access(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        self.listing(false).await
    }

    fn model_agreements(&self) -> Option<&dyn super::ModelAgreements> {
        Some(self)
    }
}

impl BedrockAdapter {
    /// One `InvokeModel` call with a JSON body.
    async fn invoke(&self, model: &str, body: Value) -> Result<Invoked, ProviderError> {
        let url = self.endpoint(model, "invoke")?;
        let body = serde_json::to_vec(&body).map_err(|_| internal_error())?;
        let response = send_with_timeout(
            self.signed_request(url, body, "application/json").await?,
            self.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(self.response_error(response).await);
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned)
        };
        let request_id = header("x-amzn-requestid");
        let input_tokens = header(INPUT_TOKEN_COUNT_HEADER).and_then(|value| value.parse().ok());
        let bytes =
            bounded_bytes(response, self.max_response_bytes, self.idle_read_timeout).await?;
        let value = serde_json::from_slice(&bytes).map_err(|_| protocol_error())?;
        Ok(Invoked {
            value,
            request_id,
            input_tokens,
        })
    }

    /// The listing, with each model's availability when `check` is set.
    async fn listing(&self, check: bool) -> Result<Vec<DiscoveredModel>, ProviderError> {
        let mut models = ModelListBuilder::default();
        // The foundation model each listed identifier calls.
        let mut bases = HashMap::new();
        let page = self.control_page("foundation-models", &[]).await?;
        push_foundation_models(&mut models, &mut bases, &page)?;
        let mut next: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let mut query = vec![("maxResults", "1000"), ("type", "SYSTEM_DEFINED")];
            if let Some(token) = next.as_deref() {
                query.push(("nextToken", token));
            }
            let page = self.control_page("inference-profiles", &query).await?;
            push_page(&mut models, &page, &INFERENCE_PROFILE_PAGE)?;
            push_profile_bases(&mut bases, &page);
            match next_token(&page, "nextToken") {
                Some(token) => next = Some(token.to_owned()),
                None => {
                    for resources in &OWN_RESOURCES {
                        self.own_resources(resources, &mut models, &mut bases)
                            .await?;
                    }
                    if !check {
                        return Ok(models.finish());
                    }
                    let models = models.finish();
                    let entitlements = self
                        .entitlements(
                            models
                                .iter()
                                .filter_map(|model| bases.get(&model.model).cloned())
                                .collect(),
                        )
                        .await;
                    return Ok(models
                        .into_iter()
                        .map(|model| {
                            let entitlement = bases
                                .get(&model.model)
                                .and_then(|base| entitlements.get(base))
                                .copied();
                            model.with_entitlement(entitlement)
                        })
                        .collect());
                }
            }
        }
        Err(too_many_pages())
    }

    /// What `GetFoundationModelAvailability` says about each of `models`
    /// (D-018), for those it answers plainly. The check is optional for a
    /// tenant's role: the first one runs alone, and a refusal of it (the
    /// role lacks `bedrock:GetFoundationModelAvailability`) or a throttle
    /// ends the checks, leaving the models unchecked rather than refused. A
    /// model the check rejects (an identifier it does not know) is skipped.
    async fn entitlements(&self, models: BTreeSet<String>) -> HashMap<String, Entitlement> {
        let deadline = tokio::time::Instant::now() + AVAILABILITY_BUDGET;
        let mut found = HashMap::new();
        let mut models = models.into_iter().take(MAX_AVAILABILITY_CHECKS);
        let Some(first) = models.next() else {
            return found;
        };
        match tokio::time::timeout_at(deadline, self.availability(&first)).await {
            Ok(Ok(Some(entitlement))) => {
                found.insert(first, entitlement);
            }
            Ok(Err(error)) if ends_checks(&error) => return found,
            Ok(_) => {}
            Err(_) => return found,
        }
        let mut checks = futures::stream::iter(models)
            .map(|model| async move {
                let result = self.availability(&model).await;
                (model, result)
            })
            .buffer_unordered(AVAILABILITY_CONCURRENCY);
        while let Ok(Some((model, result))) = tokio::time::timeout_at(deadline, checks.next()).await
        {
            match result {
                Ok(Some(entitlement)) => {
                    found.insert(model, entitlement);
                }
                Err(error) if ends_checks(&error) => break,
                _ => {}
            }
        }
        found
    }

    /// One kind of the account's own resources, following every page. A
    /// role that may not list them (403), or a region without the API
    /// (404), lists none: the permissions are optional.
    async fn own_resources(
        &self,
        resources: &OwnResources,
        models: &mut ModelListBuilder,
        bases: &mut HashMap<String, String>,
    ) -> Result<(), ProviderError> {
        let mut next: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let mut query = vec![("maxResults", "1000")];
            query.extend_from_slice(resources.query);
            if let Some(token) = next.as_deref() {
                query.push(("nextToken", token));
            }
            let page = match self.control_page(resources.path, &query).await {
                Ok(page) => page,
                Err(error)
                    if error.category() == ProviderErrorCategory::Authorization
                        || error.http_status() == Some(404) =>
                {
                    // The permissions for these lists are optional, so a refusal
                    // is not an error; log it so a missing grant can be found.
                    tracing::info!(
                        path = resources.path,
                        category = ?error.category(),
                        status = ?error.http_status(),
                        "listing the account's own Bedrock resources was refused; skipped"
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            push_own_resources(resources, models, bases, &page)?;
            match next_token(&page, "nextToken") {
                Some(token) => next = Some(token.to_owned()),
                None => return Ok(()),
            }
        }
        Err(too_many_pages())
    }

    /// One model's availability, `None` when the answer is not plain.
    async fn availability(&self, model: &str) -> Result<Option<Entitlement>, ProviderError> {
        let path = format!(
            "foundation-model-availability/{}",
            percent_encode_path_segment(model)
        );
        Ok(entitlement_of(&self.control_page(&path, &[]).await?))
    }
}

/// Model agreements (D-035) through Bedrock's control plane:
/// `ListFoundationModelAgreementOffers`, `GetFoundationModelAvailability`
/// and `CreateFoundationModelAgreement`. The first and last need the
/// connection's opt-in "accept model agreements" permissions.
#[async_trait]
impl super::ModelAgreements for BedrockAdapter {
    async fn agreement_bases(&self) -> Result<HashMap<String, String>, ProviderError> {
        let mut models = ModelListBuilder::default();
        let mut bases = HashMap::new();
        let page = self.control_page("foundation-models", &[]).await?;
        push_foundation_models(&mut models, &mut bases, &page)?;
        let mut next: Option<String> = None;
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let mut query = vec![("maxResults", "1000"), ("type", "SYSTEM_DEFINED")];
            if let Some(token) = next.as_deref() {
                query.push(("nextToken", token));
            }
            let page = self.control_page("inference-profiles", &query).await?;
            push_page(&mut models, &page, &INFERENCE_PROFILE_PAGE)?;
            push_profile_bases(&mut bases, &page);
            match next_token(&page, "nextToken") {
                Some(token) => next = Some(token.to_owned()),
                None => return Ok(bases),
            }
        }
        Err(too_many_pages())
    }

    /// Public and private offers alike (`offerType=ALL`): a private offer is
    /// one AWS or the vendor made the account.
    async fn agreement_offers(
        &self,
        model: &str,
    ) -> Result<Vec<crate::domain::model_agreement::AgreementOffer>, ProviderError> {
        let path = format!(
            "list-foundation-model-agreement-offers/{}",
            percent_encode_path_segment(model)
        );
        let page = throttled_retry(|| self.control_page(&path, &[("offerType", "ALL")])).await?;
        offers_of(&page, &self.region)
    }

    async fn agreement_availability(
        &self,
        model: &str,
    ) -> Result<Option<crate::domain::model_agreement::AgreementAvailability>, ProviderError> {
        let path = format!(
            "foundation-model-availability/{}",
            percent_encode_path_segment(model)
        );
        Ok(agreement_availability_of(
            &throttled_retry(|| self.control_page(&path, &[])).await?,
        ))
    }

    async fn create_agreement(
        &self,
        model: &str,
        token: &crate::domain::model_agreement::OfferToken,
    ) -> Result<(), ProviderError> {
        let url = self
            .control_base_url
            .join("create-foundation-model-agreement")
            .map_err(|_| internal_error())?;
        let body = serde_json::to_vec(&json!({
            "modelId": model,
            "offerToken": token.expose(),
        }))
        .map_err(|_| internal_error())?;
        // AWS throttles before it acts, so a throttled call made no
        // agreement and is safe to repeat, as AWS's own SDKs do.
        throttled_retry(|| async {
            let response = send_with_timeout(
                self.signed_request(url.clone(), body.clone(), "application/json")
                    .await?,
                self.request_timeout,
            )
            .await?;
            if !response.status().is_success() {
                return Err(self.response_error(response).await);
            }
            Ok(())
        })
        .await
    }
}

/// Retries after the first throttled answer for one agreement call.
const THROTTLE_RETRIES: u32 = 4;
/// The first wait after a throttled agreement call, doubled after each.
const THROTTLE_BACKOFF: Duration = Duration::from_millis(500);
/// The longest backoff between agreement calls AWS throttled without a
/// `Retry-After`.
const THROTTLE_BACKOFF_MAX: Duration = Duration::from_secs(4);
/// The longest `Retry-After` waited out: one agreement call's callers
/// allow it 20 seconds.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(10);

/// Runs `call`, again after a wait each time AWS throttles it: the
/// agreement APIs allow only a request or two a second per account, and
/// reading 40 models' offers runs past that. The wait is the answer's
/// `Retry-After` when given (up to [`RETRY_AFTER_MAX`]), otherwise a
/// doubling backoff with jitter so concurrent reads spread out. Callers
/// bound the whole call.
async fn throttled_retry<T, F, Fut>(mut call: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ProviderError>>,
{
    let mut backoff = THROTTLE_BACKOFF;
    let mut retries = 0;
    loop {
        match call().await {
            Err(error) if retries < THROTTLE_RETRIES && throttled(&error) => {
                let wait = match error.retry_after() {
                    // AWS's own wait, in full; one past what any caller
                    // allows a call would only time out, so it is given
                    // back now.
                    Some(wait) if wait > RETRY_AFTER_MAX => return Err(error),
                    Some(wait) => wait,
                    None => jittered(backoff),
                };
                tokio::time::sleep(wait).await;
                backoff = (backoff * 2).min(THROTTLE_BACKOFF_MAX);
                retries += 1;
            }
            result => return result,
        }
    }
}

/// Whether AWS refused a call for its request rate (not a quota, which
/// waiting does not lift).
fn throttled(error: &ProviderError) -> bool {
    error.category() == ProviderErrorCategory::RateLimited
        && error.provider_code() == Some("throttlingException")
}

/// `wait` scaled by a random factor in [0.5, 1.0].
fn jittered(wait: Duration) -> Duration {
    wait.mul_f64(rand::random_range(0.5..=1.0))
}

/// The offers of a `ListFoundationModelAgreementOffers` answer. An offer
/// without an identifier or token cannot be accepted and is left out; its
/// rate card is narrowed to `region`'s rates where the card names them.
fn offers_of(
    page: &Value,
    region: &str,
) -> Result<Vec<crate::domain::model_agreement::AgreementOffer>, ProviderError> {
    use crate::domain::model_agreement::{
        AgreementOffer, MAX_RATES, OfferTerms, OfferToken, Rate, bounded_text, offer_id,
    };
    let offers = page
        .get("offers")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    let text = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(bounded_text)
            .filter(|text| !text.is_empty())
    };
    let code = pricing_region_code(region);
    Ok(offers
        .iter()
        .filter_map(|offer| {
            let id = offer.get("offerId").and_then(Value::as_str)?;
            let token = offer.get("offerToken").and_then(Value::as_str)?;
            if !offer_id(id) || token.is_empty() {
                return None;
            }
            let card: Vec<Rate> = offer
                .pointer("/termDetails/usageBasedPricingTerm/rateCard")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|rate| {
                    Some(Rate {
                        dimension: text(rate, "/dimension")?,
                        price: text(rate, "/price"),
                        unit: text(rate, "/unit"),
                        description: text(rate, "/description"),
                    })
                })
                .collect();
            let total = card.len();
            let regional: Vec<Rate> = code
                .map(|code| {
                    card.iter()
                        .filter(|rate| {
                            rate.dimension
                                .strip_prefix(code)
                                .is_some_and(|rest| rest.starts_with('_'))
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let (rate_card, rate_card_region) = if regional.is_empty() {
                (card.into_iter().take(MAX_RATES).collect(), None)
            } else {
                (
                    regional.into_iter().take(MAX_RATES).collect(),
                    code.map(str::to_owned),
                )
            };
            Some(AgreementOffer {
                offer_id: id.to_owned(),
                token: OfferToken::new(token),
                terms: OfferTerms {
                    rate_card,
                    rate_card_region,
                    rate_card_total: total,
                    legal_url: offer
                        .pointer("/termDetails/legalTerm/url")
                        .and_then(Value::as_str)
                        .and_then(legal_url),
                    refund_policy: text(offer, "/termDetails/supportTerm/refundPolicyDescription"),
                    agreement_duration: text(offer, "/termDetails/validityTerm/agreementDuration"),
                },
            })
        })
        .collect())
}

/// The longest legal-terms link kept: AWS signs it (an S3 presigned URL with
/// a session token runs to a few kilobytes). Longer is left out, never cut.
const MAX_LEGAL_URL_BYTES: usize = 8 * 1024;

/// An offer's legal-terms link, kept whole: a valid `https` URL within
/// [`MAX_LEGAL_URL_BYTES`], else none (a cut signed link would not open).
fn legal_url(value: &str) -> Option<String> {
    if value.len() > MAX_LEGAL_URL_BYTES {
        return None;
    }
    let parsed = url::Url::parse(value).ok()?;
    (parsed.scheme() == "https" && parsed.host_str().is_some()).then(|| value.to_owned())
}

/// The region code AWS's Marketplace rate cards prefix a dimension with
/// (`USE2_input_tokens_...`), for the regions whose code is AWS's
/// documented usage-type prefix. Others show the whole card.
fn pricing_region_code(region: &str) -> Option<&'static str> {
    Some(match region {
        "us-east-1" => "USE1",
        "us-east-2" => "USE2",
        "us-west-1" => "USW1",
        "us-west-2" => "USW2",
        "ca-central-1" => "CAN1",
        "eu-west-1" => "EU",
        "eu-west-2" => "EUW2",
        "eu-west-3" => "EUW3",
        "eu-central-1" => "EUC1",
        "eu-central-2" => "EUC2",
        "eu-north-1" => "EUN1",
        "eu-south-1" => "EUS1",
        "eu-south-2" => "EUS2",
        "ap-northeast-1" => "APN1",
        "ap-northeast-2" => "APN2",
        "ap-northeast-3" => "APN3",
        "ap-southeast-1" => "APS1",
        "ap-southeast-2" => "APS2",
        "ap-south-1" => "APS3",
        "sa-east-1" => "SAE1",
        "us-gov-east-1" => "UGE1",
        "us-gov-west-1" => "UGW1",
        _ => return None,
    })
}

/// What a `GetFoundationModelAvailability` answer says about the model's
/// agreement; `None` for a status this build does not know.
fn agreement_availability_of(
    value: &Value,
) -> Option<crate::domain::model_agreement::AgreementAvailability> {
    use crate::domain::model_agreement::{
        AgreementAvailability, ProviderAgreementStatus, bounded_text,
    };
    let agreement = match value
        .pointer("/agreementAvailability/status")
        .and_then(Value::as_str)?
    {
        "AVAILABLE" => ProviderAgreementStatus::Available,
        "PENDING" => ProviderAgreementStatus::Pending,
        "NOT_AVAILABLE" => ProviderAgreementStatus::NotAvailable,
        "ERROR" => ProviderAgreementStatus::Error,
        _ => return None,
    };
    Some(AgreementAvailability {
        agreement,
        error_message: value
            .pointer("/agreementAvailability/errorMessage")
            .and_then(Value::as_str)
            .map(bounded_text)
            .filter(|text| !text.is_empty()),
        entitled: match value.get("entitlementAvailability").and_then(Value::as_str) {
            Some("AVAILABLE") => Some(true),
            Some("NOT_AVAILABLE") => Some(false),
            _ => None,
        },
        authorized: match value.get("authorizationStatus").and_then(Value::as_str) {
            Some("AUTHORIZED") => Some(true),
            Some("NOT_AUTHORIZED") => Some(false),
            _ => None,
        },
        region_available: match value.get("regionAvailability").and_then(Value::as_str) {
            Some("AVAILABLE") => Some(true),
            Some("NOT_AVAILABLE") => Some(false),
            _ => None,
        },
        entitlement: entitlement_of(value),
    })
}

/// Whether a failed availability check ends the listing's checks: the role
/// may not make them, or Bedrock asks the gateway to slow down.
fn ends_checks(error: &ProviderError) -> bool {
    matches!(
        error.category(),
        ProviderErrorCategory::Authentication
            | ProviderErrorCategory::Authorization
            | ProviderErrorCategory::RateLimited
    )
}

/// What a `GetFoundationModelAvailability` answer says. Every field must
/// hold a value this build knows and the region must serve the model, or the
/// answer says nothing (a probe settles it instead). The first thing in the
/// way is then the reason, in the order a tenant would fix it: the role's
/// IAM authorization, the account's entitlement, the model's Marketplace
/// agreement (pending or failed is still not in place).
fn entitlement_of(value: &Value) -> Option<Entitlement> {
    let text = |pointer: &str| value.pointer(pointer).and_then(Value::as_str);
    let authorized = match text("/authorizationStatus")? {
        "AUTHORIZED" => true,
        "NOT_AUTHORIZED" => false,
        _ => return None,
    };
    let entitled = match text("/entitlementAvailability")? {
        "AVAILABLE" => true,
        "NOT_AVAILABLE" => false,
        _ => return None,
    };
    let agreed = match text("/agreementAvailability/status")? {
        "AVAILABLE" => true,
        "NOT_AVAILABLE" | "PENDING" | "ERROR" => false,
        _ => return None,
    };
    if text("/regionAvailability")? != "AVAILABLE" {
        return None;
    }
    Some(if !authorized {
        Entitlement::PermissionMissing
    } else if !entitled {
        Entitlement::NotEntitled
    } else if !agreed {
        Entitlement::AgreementRequired
    } else {
        Entitlement::Entitled
    })
}

/// A kind of the account's own Bedrock resources, which a deployment names
/// by the ARN Bedrock calls it with.
struct OwnResources {
    /// The control-plane list operation's path and filter.
    path: &'static str,
    query: &'static [(&'static str, &'static str)],
    /// The page's array of entries.
    items: &'static str,
    /// The entry's ARN, display name, and the field holding the foundation
    /// model it calls, if any.
    arn: &'static str,
    name: &'static str,
    base: Option<&'static str>,
    /// The entry field and value an entry must have to be callable.
    callable: Option<(&'static str, &'static str)>,
}

/// The account's own resources, after the shared catalog: application
/// inference profiles, provisioned throughput in service, imported models,
/// registered Marketplace model endpoints and active custom model
/// deployments (each needs its read-only `bedrock:List*` permission, which
/// is optional).
const OWN_RESOURCES: [OwnResources; 5] = [
    OwnResources {
        path: "inference-profiles",
        query: &[("type", "APPLICATION")],
        items: "inferenceProfileSummaries",
        arn: "inferenceProfileArn",
        name: "inferenceProfileName",
        // Its models' ARNs, read by [`push_own_resources`].
        base: Some("models"),
        callable: Some(("type", "APPLICATION")),
    },
    OwnResources {
        path: "provisioned-model-throughputs",
        query: &[("statusEquals", "InService")],
        items: "provisionedModelSummaries",
        arn: "provisionedModelArn",
        name: "provisionedModelName",
        base: Some("foundationModelArn"),
        callable: Some(("status", "InService")),
    },
    OwnResources {
        path: "imported-models",
        query: &[],
        items: "modelSummaries",
        arn: "modelArn",
        name: "modelName",
        base: None,
        callable: None,
    },
    OwnResources {
        path: "marketplace-model/endpoints",
        query: &[],
        items: "marketplaceModelEndpoints",
        arn: "endpointArn",
        name: "modelSourceIdentifier",
        base: None,
        callable: Some(("status", "REGISTERED")),
    },
    // A custom (fine-tuned or distilled) model deployed for on-demand
    // inference, which Converse calls by the deployment's ARN.
    OwnResources {
        path: "model-customization/custom-model-deployments",
        query: &[("statusEquals", "Active")],
        items: "modelDeploymentSummaries",
        arn: "customModelDeploymentArn",
        name: "customModelDeploymentName",
        base: None,
        callable: Some(("status", "Active")),
    },
];

/// The foundation model an ARN names (`...:foundation-model/<id>`).
fn foundation_model_of(arn: &str) -> Option<&str> {
    arn.split_once(":foundation-model/").map(|(_, model)| model)
}

/// The callable entries of one page of `resources`, each with the
/// foundation model it calls when the page names one (an application
/// profile copied from a system profile names every region's model, which
/// must agree).
fn push_own_resources(
    resources: &OwnResources,
    models: &mut ModelListBuilder,
    bases: &mut HashMap<String, String>,
    page: &Value,
) -> Result<(), ProviderError> {
    let items = page
        .get(resources.items)
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    for item in items {
        let text = |field: &str| item.get(field).and_then(Value::as_str);
        if let Some((field, value)) = resources.callable
            && text(field) != Some(value)
        {
            continue;
        }
        let Some(model) =
            text(resources.arn).and_then(|arn| DiscoveredModel::new(arn, text(resources.name)))
        else {
            continue;
        };
        let base = match resources.base {
            Some("models") => {
                let mut arns = item
                    .get("models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|model| {
                        model
                            .get("modelArn")
                            .and_then(Value::as_str)
                            .and_then(foundation_model_of)
                    });
                match arns.next() {
                    Some(Some(base)) if arns.all(|other| other == Some(base)) => Some(base),
                    _ => None,
                }
            }
            Some(field) => text(field).and_then(foundation_model_of),
            None => None,
        };
        if let Some(base) = base {
            bases.insert(model.model.clone(), base.to_owned());
        }
        models.push_model(model).map_err(|_| protocol_error())?;
    }
    Ok(())
}

/// Each system inference profile of a `ListInferenceProfiles` page with the
/// foundation model it routes to, when all its regions route to one.
fn push_profile_bases(bases: &mut HashMap<String, String>, page: &Value) {
    let Some(items) = page
        .get("inferenceProfileSummaries")
        .and_then(Value::as_array)
    else {
        return;
    };
    for item in items {
        let Some(id) = item.get("inferenceProfileId").and_then(Value::as_str) else {
            continue;
        };
        let mut models = item
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|model| {
                model
                    .get("modelArn")
                    .and_then(Value::as_str)
                    .and_then(|arn| arn.split_once(":foundation-model/"))
                    .map(|(_, model)| model)
            });
        let Some(Some(base)) = models.next() else {
            continue;
        };
        if models.all(|model| model == Some(base)) {
            bases.insert(id.to_owned(), base.to_owned());
        }
    }
}

/// The foundation models of a `ListFoundationModels` page, each marked
/// `ProfileOnly` when the page states its inference types without
/// `ON_DEMAND`. Models whose page states no types stay listed.
fn push_foundation_models(
    models: &mut ModelListBuilder,
    bases: &mut HashMap<String, String>,
    page: &Value,
) -> Result<(), ProviderError> {
    let items = page
        .get("modelSummaries")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    for item in items {
        let Some(model) = item
            .get("modelId")
            .and_then(Value::as_str)
            .and_then(|id| DiscoveredModel::new(id, item.get("modelName").and_then(Value::as_str)))
        else {
            continue;
        };
        let profile_only = item
            .get("inferenceTypesSupported")
            .and_then(Value::as_array)
            .is_some_and(|types| !types.iter().any(|kind| kind.as_str() == Some("ON_DEMAND")));
        let model = if profile_only {
            model.with_availability(ModelAvailability::ProfileOnly)
        } else {
            model
        };
        let embeds = item
            .get("outputModalities")
            .and_then(Value::as_array)
            .is_some_and(|modalities| modalities.iter().any(|m| m.as_str() == Some("EMBEDDING")));
        let model = if embeds {
            model.with_operations(&[CatalogOperation::Embeddings])
        } else {
            model
        };
        bases.insert(model.model.clone(), model.model.clone());
        models.push_model(model).map_err(|_| protocol_error())?;
    }
    Ok(())
}

const INFERENCE_PROFILE_PAGE: PageShape = PageShape {
    items: "inferenceProfileSummaries",
    id: "inferenceProfileId",
    display_name: Some("inferenceProfileName"),
    id_prefix: None,
    omits_empty: false,
};

/// An `InvokeModel` response.
struct Invoked {
    value: Value,
    request_id: Option<String>,
    input_tokens: Option<u64>,
}

/// The `InvokeModel` body an embedding model takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmbeddingSchema {
    /// Amazon Titan Embeddings: `{"inputText"}` in, `{"embedding"}` out, one
    /// text per call.
    Titan,
    /// Cohere Embed: `{"texts", "input_type"}` in, `{"embeddings"}` out.
    Cohere,
}

/// The schema of an embedding model, from its identifier: a foundation
/// model ID, a cross-region profile ID (`us.cohere.embed-v4:0`) or an ARN
/// that names the foundation model. An application profile or provisioned
/// model ARN does not say which model it serves, so it has none.
fn embedding_schema(model: &str) -> Option<EmbeddingSchema> {
    let model = model.to_ascii_lowercase();
    if model.contains("amazon.titan-embed") {
        Some(EmbeddingSchema::Titan)
    } else if model.contains("cohere.embed") {
        Some(EmbeddingSchema::Cohere)
    } else {
        None
    }
}

/// The `InvokeModel` body that embeds `texts`: one text for Titan.
fn embed_body(schema: EmbeddingSchema, texts: &[String]) -> Value {
    match schema {
        EmbeddingSchema::Titan => json!({ "inputText": texts[0] }),
        EmbeddingSchema::Cohere => cohere_embed_body(texts),
    }
}

/// The vectors of one call's answer, which must hold one per text.
fn embedded_vectors(
    schema: EmbeddingSchema,
    value: &Value,
    texts: usize,
) -> Result<Vec<Vec<f32>>, ProviderError> {
    let vectors = match schema {
        EmbeddingSchema::Titan => vec![titan_vector(value)?],
        EmbeddingSchema::Cohere => cohere_vectors(value)?,
    };
    if vectors.len() == texts {
        Ok(vectors)
    } else {
        Err(protocol_error())
    }
}

/// What the calls of one embeddings request cost so far.
#[derive(Debug)]
struct Billed {
    /// Input tokens Bedrock reported.
    reported: u64,
    /// Input tokens estimated for calls that may have been billed without
    /// a report: an answer without a count, or a failure Bedrock may have
    /// run (a timeout, a server error).
    estimated: u64,
    /// Whether every answered call reported its count.
    all_reported: bool,
    /// Whether any call was answered, and so billed.
    answered: bool,
}

impl Default for Billed {
    fn default() -> Self {
        Self {
            reported: 0,
            estimated: 0,
            all_reported: true,
            answered: false,
        }
    }
}

impl Billed {
    /// An answered call: its reported count, else Titan's count in the
    /// body, else an estimate.
    fn add(&mut self, schema: EmbeddingSchema, call: &Invoked, texts: &[String]) {
        self.answered = true;
        let tokens = call.input_tokens.or_else(|| match schema {
            EmbeddingSchema::Titan => call
                .value
                .get("inputTextTokenCount")
                .and_then(Value::as_u64),
            EmbeddingSchema::Cohere => None,
        });
        match tokens {
            Some(tokens) => self.reported = self.reported.saturating_add(tokens),
            None => {
                self.all_reported = false;
                self.maybe(texts);
            }
        }
    }

    /// A call that may have been billed with no count: about four bytes a
    /// token.
    fn maybe(&mut self, texts: &[String]) {
        let bytes: usize = texts.iter().map(String::len).sum();
        let tokens = u64::try_from(bytes.div_ceil(4).max(1)).unwrap_or(u64::MAX);
        self.estimated = self.estimated.saturating_add(tokens);
    }

    /// The input tokens of a request every call of which reported them.
    fn reported(&self) -> Option<u64> {
        self.all_reported.then_some(self.reported)
    }

    /// `error`, carrying what earlier calls cost as partial usage once any
    /// was answered. A request none of whose calls was answered fails as a
    /// single call does.
    fn attach(&self, error: ProviderError) -> ProviderError {
        if !self.answered {
            return error;
        }
        let tokens = self.reported.saturating_add(self.estimated);
        let input = if self.estimated == 0 {
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
}

/// An error answer or a request never sent: Bedrock did not run the call,
/// so nothing was billed.
fn refused(error: &ProviderError) -> bool {
    error.was_not_sent()
        || error
            .http_status()
            .is_some_and(|status| (400..500).contains(&status))
}

/// The public API has no input type, so texts embed as documents: the
/// choice for building an index, and still usable for queries.
fn cohere_embed_body(texts: &[String]) -> Value {
    json!({
        "texts": texts,
        "input_type": "search_document",
        "embedding_types": ["float"],
    })
}

fn titan_vector(value: &Value) -> Result<Vec<f32>, ProviderError> {
    float_vector(value.get("embedding"))
}

/// `embeddings` is a list of vectors, or (when `embedding_types` is given,
/// as it always is here) an object keyed by type.
fn cohere_vectors(value: &Value) -> Result<Vec<Vec<f32>>, ProviderError> {
    let embeddings = value.get("embeddings").ok_or_else(protocol_error)?;
    let list = embeddings
        .get("float")
        .unwrap_or(embeddings)
        .as_array()
        .ok_or_else(protocol_error)?;
    list.iter()
        .map(|vector| float_vector(Some(vector)))
        .collect()
}

fn float_vector(value: Option<&Value>) -> Result<Vec<f32>, ProviderError> {
    value
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?
        .iter()
        .map(|value| {
            value
                .as_f64()
                .map(|value| value as f32)
                .ok_or_else(protocol_error)
        })
        .collect()
}

/// Converse's `toolConfig` for the request's tools, or none when it defines
/// none. Converse has no way to turn tools off for a turn or to ask for one
/// call at a time; a request that asks is refused, never sent without it
/// (D-032).
fn tool_config(request: &ChatRequest) -> Result<Option<Value>, ProviderError> {
    if request.tool_choice == Some(ToolChoice::None) || request.parallel_tool_calls == Some(false) {
        return Err(invalid_request());
    }
    if request.tools.is_empty() {
        return Ok(None);
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut spec = json!({
                "name": tool.name,
                "inputSchema": {"json": tool.parameters},
            });
            if let Some(description) = &tool.description {
                spec["description"] = json!(description);
            }
            json!({"toolSpec": spec})
        })
        .collect::<Vec<_>>();
    let mut config = json!({ "tools": tools });
    match &request.tool_choice {
        None => {}
        Some(ToolChoice::Auto) => config["toolChoice"] = json!({"auto": {}}),
        Some(ToolChoice::Required) => config["toolChoice"] = json!({"any": {}}),
        Some(ToolChoice::Function(name)) => config["toolChoice"] = json!({"tool": {"name": name}}),
        Some(ToolChoice::None) => return Err(invalid_request()),
    }
    Ok(Some(config))
}

/// Converse's stop reason in the contract's terms: a stop to call tools is
/// `tool_calls` (D-032).
fn finish_reason_of(reason: &str) -> FinishReason {
    FinishReason::new(if reason == "tool_use" {
        "tool_calls"
    } else {
        reason
    })
}

/// Whether a batch job can take `request` as a line: what `Converse` takes.
pub(crate) fn validate_batch_line(request: &ChatRequest) -> Result<(), ProviderError> {
    validate_chat_request(request)
}

fn validate_chat_request(request: &ChatRequest) -> Result<(), ProviderError> {
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
    tool_config(request).map(|_| ())
}

/// A SigV4 canonical query string, sorted by encoded name then value.
fn list_query(pairs: &[(&str, &str)]) -> String {
    let mut pairs = pairs
        .iter()
        .map(|(name, value)| {
            (
                percent_encode_path_segment(name),
                percent_encode_path_segment(value),
            )
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

fn parse_response(
    value: Value,
    model: String,
    request_id: Option<String>,
) -> Result<ChatResponse, ProviderError> {
    let (text, tool_calls) = extract_output(&value)?;
    let finish_reason = value
        .get("stopReason")
        .and_then(Value::as_str)
        .map(finish_reason_of);
    let usage = parse_usage(value.get("usage").ok_or_else(protocol_error)?)?;
    let choice = ChatChoice {
        index: 0,
        message: ChatMessage {
            role: ChatRole::Assistant,
            content: text,
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        },
        finish_reason,
        tool_calls,
        refusal: None,
        safety: vec![],
        annotations: vec![],
    };
    ChatResponse::new(
        request_id,
        model,
        vec![choice],
        Some(UsageSnapshot::final_(usage).map_err(|_| protocol_error())?),
        SafeMetadata::default(),
    )
    .map_err(|_| protocol_error())
}

/// The answer's text and its tool calls (`toolUse` blocks, D-032).
fn extract_output(value: &Value) -> Result<(String, Vec<ToolCall>), ProviderError> {
    let blocks = value
        .get("output")
        .and_then(|output| output.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in blocks {
        let object = block.as_object().ok_or_else(protocol_error)?;
        if object.len() != 1 {
            return Err(protocol_error());
        }
        if is_reasoning(object) {
            continue;
        }
        if let Some(call) = object.get("toolUse") {
            let id = call.get("toolUseId").and_then(Value::as_str);
            let name = call.get("name").and_then(Value::as_str);
            let input = call.get("input").filter(|input| input.is_object());
            let (Some(id), Some(name), Some(input)) = (id, name, input) else {
                return Err(protocol_error());
            };
            calls.push(ToolCall {
                id: Some(id.to_owned()),
                name: Some(name.to_owned()),
                arguments: serde_json::to_string(input).map_err(|_| protocol_error())?,
            });
            continue;
        }
        let fragment = object
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?;
        text.push_str(fragment);
    }
    Ok((text, calls))
}

/// Reasoning models (such as `openai.gpt-oss-*`) return their reasoning in a
/// `reasoningContent` block before the answer. The gateway returns only the
/// answer; the reasoning tokens are already counted in `outputTokens`.
fn is_reasoning(block: &serde_json::Map<String, Value>) -> bool {
    block.get("reasoningContent").is_some_and(Value::is_object)
}

fn parse_usage(value: &Value) -> Result<Usage, ProviderError> {
    validate_known_usage_fields(value)?;
    let input = value
        .get("inputTokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    let output = value
        .get("outputTokens")
        .and_then(Value::as_u64)
        .ok_or_else(protocol_error)?;
    usage_of(
        input,
        output,
        cache_read_tokens(value)?,
        cache_write_tokens(value)?,
    )
}

/// Converse's `inputTokens` counts only the prompt not read from or written
/// to the prompt cache: the whole prompt is `inputTokens +
/// cacheReadInputTokens + cacheWriteInputTokens`
/// (<https://docs.aws.amazon.com/bedrock/latest/userguide/prompt-caching.html>).
/// The gateway's input counts cache reads (charged at the cached-input
/// price, the rest at the input price); cache writes are the `other`
/// counter, charged on top at the cache write price.
fn usage_of(input: u64, output: u64, read: u64, write: u64) -> Result<Usage, ProviderError> {
    Usage::complete(
        TokenCounter::exact(input.checked_add(read).ok_or_else(protocol_error)?),
        TokenCounter::exact(output),
        Some(TokenCounter::exact(read)),
        None,
        Some(TokenCounter::exact(write)),
    )
    .map_err(|_| protocol_error())
}

fn cache_read_tokens(value: &Value) -> Result<u64, ProviderError> {
    cache_tokens(value, "cacheReadInputTokens", "cacheReadInputTokenCount")
}

fn cache_write_tokens(value: &Value) -> Result<u64, ProviderError> {
    cache_tokens(value, "cacheWriteInputTokens", "cacheWriteInputTokenCount")
}

/// Bedrock omits prompt-cache usage fields entirely when their value is zero
/// (rather than reporting an explicit `0`), and the raw API reports each
/// count twice, under `name` and under a `…Count` alias (seen live; the AWS
/// CLI hides the alias). Absence is an exact zero; a malformed field, or the
/// two names disagreeing, fails closed.
fn cache_tokens(value: &Value, name: &str, alias: &str) -> Result<u64, ProviderError> {
    let count = |field: &str| match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(other) => other.as_u64().map(Some).ok_or_else(protocol_error),
    };
    match (count(name)?, count(alias)?) {
        (Some(count), Some(alias)) if count != alias => Err(protocol_error()),
        (count, alias) => Ok(count.or(alias).unwrap_or(0)),
    }
}

/// Reject any usage dimension this adapter does not explicitly map, rather
/// than silently dropping a potentially billable field.
fn validate_known_usage_fields(value: &Value) -> Result<(), ProviderError> {
    let object = value.as_object().ok_or_else(protocol_error)?;
    for (key, field) in object {
        match key.as_str() {
            "inputTokens"
            | "outputTokens"
            | "totalTokens"
            | "cacheReadInputTokens"
            | "cacheReadInputTokenCount"
            | "cacheWriteInputTokens"
            | "cacheWriteInputTokenCount" => {}
            // Server-side tool use (web search and the like), which the
            // gateway never asks for: Bedrock reports it empty. Counts would
            // be billable usage this adapter does not map.
            "serverToolUsage" if no_usage(field) => {}
            _ => return Err(protocol_error()),
        }
    }
    Ok(())
}

/// An object of usage counts that are all zero (or none at all).
fn no_usage(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|counts| counts.values().all(|count| count.as_u64() == Some(0)))
}

struct AwsEventFrame {
    message_type: String,
    event_type: Option<String>,
    exception_type: Option<String>,
    payload: Vec<u8>,
}

/// Decode one `application/vnd.amazon.eventstream` frame from the front of
/// `pending`, if a complete frame is already buffered. Returns the frame and
/// the number of bytes it consumed, or `None` when more bytes are needed.
fn try_decode_event_stream_frame(
    pending: &[u8],
) -> Result<Option<(AwsEventFrame, usize)>, ProviderError> {
    if pending.len() < 12 {
        return Ok(None);
    }
    let total_len = u32::from_be_bytes(pending[0..4].try_into().unwrap()) as usize;
    let headers_len = u32::from_be_bytes(pending[4..8].try_into().unwrap()) as usize;
    let prelude_crc = u32::from_be_bytes(pending[8..12].try_into().unwrap());
    if total_len < 16 || headers_len > total_len - 16 {
        return Err(protocol_error());
    }
    if crc32(&pending[0..8]) != prelude_crc {
        return Err(protocol_error());
    }
    if pending.len() < total_len {
        return Ok(None);
    }
    let message_crc = u32::from_be_bytes(pending[total_len - 4..total_len].try_into().unwrap());
    if crc32(&pending[0..total_len - 4]) != message_crc {
        return Err(protocol_error());
    }
    let headers_bytes = &pending[12..12 + headers_len];
    let payload = pending[12 + headers_len..total_len - 4].to_vec();
    let headers = decode_headers(headers_bytes)?;
    let message_type = headers
        .get(":message-type")
        .cloned()
        .ok_or_else(protocol_error)?;
    let event_type = headers.get(":event-type").cloned();
    let exception_type = headers.get(":exception-type").cloned();
    Ok(Some((
        AwsEventFrame {
            message_type,
            event_type,
            exception_type,
            payload,
        },
        total_len,
    )))
}

fn decode_headers(mut bytes: &[u8]) -> Result<HashMap<String, String>, ProviderError> {
    let mut headers = HashMap::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or_else(protocol_error)? as usize;
        bytes = &bytes[1..];
        if bytes.len() < name_len {
            return Err(protocol_error());
        }
        let name = std::str::from_utf8(&bytes[..name_len])
            .map_err(|_| protocol_error())?
            .to_owned();
        bytes = &bytes[name_len..];
        let value_type = *bytes.first().ok_or_else(protocol_error)?;
        bytes = &bytes[1..];
        match value_type {
            0 | 1 => {}
            2 => {
                if bytes.is_empty() {
                    return Err(protocol_error());
                }
                bytes = &bytes[1..];
            }
            3 => {
                if bytes.len() < 2 {
                    return Err(protocol_error());
                }
                bytes = &bytes[2..];
            }
            4 => {
                if bytes.len() < 4 {
                    return Err(protocol_error());
                }
                bytes = &bytes[4..];
            }
            5 | 8 => {
                if bytes.len() < 8 {
                    return Err(protocol_error());
                }
                bytes = &bytes[8..];
            }
            9 => {
                if bytes.len() < 16 {
                    return Err(protocol_error());
                }
                bytes = &bytes[16..];
            }
            6 => {
                if bytes.len() < 2 {
                    return Err(protocol_error());
                }
                let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    return Err(protocol_error());
                }
                bytes = &bytes[len..];
            }
            7 => {
                if bytes.len() < 2 {
                    return Err(protocol_error());
                }
                let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                bytes = &bytes[2..];
                if bytes.len() < len {
                    return Err(protocol_error());
                }
                let value = std::str::from_utf8(&bytes[..len])
                    .map_err(|_| protocol_error())?
                    .to_owned();
                bytes = &bytes[len..];
                headers.insert(name, value);
            }
            _ => return Err(protocol_error()),
        }
    }
    Ok(headers)
}

fn bedrock_converse_stream<S>(
    mut bytes: S,
    model: String,
    request_id: Option<String>,
    max: usize,
    idle_timeout: Duration,
) -> impl Stream<Item = Result<ChatStreamEvent, ProviderError>> + Send + 'static
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    stream! {
        let mut pending = Vec::<u8>::new();
        let mut total = 0usize;
        let mut response_started = false;
        let mut finished = false;
        let mut metadata_seen = false;
        let mut input_tokens: Option<u64> = None;
        let mut output_tokens: Option<u64> = None;
        let mut cached_tokens: Option<u64> = None;
        let mut other_tokens: Option<u64> = None;
        // The open content block's tool call, by its index among the
        // response's calls, and whether its input arrived (D-032).
        let mut tool_open: Option<(u32, bool)> = None;
        let mut tool_calls = 0u32;

        'outer: loop {
            loop {
                let decoded = match try_decode_event_stream_frame(&pending) {
                    Ok(decoded) => decoded,
                    Err(error) => { yield Err(error); return; }
                };
                let Some((frame, consumed)) = decoded else { break; };
                pending.drain(..consumed);

                if frame.message_type == "exception" {
                    yield Err(stream_exception_error(frame.exception_type.as_deref(), request_id.clone()));
                    return;
                }
                if frame.message_type != "event" {
                    yield Err(protocol_error());
                    return;
                }
                let Some(event_type) = frame.event_type.clone() else {
                    yield Err(protocol_error());
                    return;
                };
                let value: Value = if frame.payload.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    match serde_json::from_slice(&frame.payload) {
                        Ok(value) => value,
                        Err(_) => { yield Err(protocol_error()); return; }
                    }
                };

                match event_type.as_str() {
                    "messageStart" => {
                        if response_started { yield Err(protocol_error()); return; }
                        response_started = true;
                        match ResponseStarted::new(request_id.clone(), model.clone(), SafeMetadata::default()) {
                            Ok(started) => yield Ok(ChatStreamEvent::ResponseStarted(started)),
                            Err(_) => { yield Err(protocol_error()); return; }
                        }
                    }
                    "contentBlockStart" => {
                        if !response_started || finished || tool_open.is_some() { yield Err(protocol_error()); return; }
                        if let Some(start) = value.get("start").and_then(|start| start.get("toolUse")) {
                            let id = start.get("toolUseId").and_then(Value::as_str);
                            let name = start.get("name").and_then(Value::as_str);
                            let (Some(id), Some(name)) = (id, name) else { yield Err(protocol_error()); return; };
                            tool_open = Some((tool_calls, false));
                            tool_calls += 1;
                            yield Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                                choice_index: 0,
                                tool_call_index: tool_calls - 1,
                                id: Some(id.to_owned()),
                                name: Some(name.to_owned()),
                                arguments_delta: String::new(),
                            }));
                        }
                    }
                    "contentBlockStop" => {
                        if !response_started || finished { yield Err(protocol_error()); return; }
                        // A call without input has an empty object.
                        if let Some((index, false)) = tool_open.take() {
                            yield Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                                choice_index: 0,
                                tool_call_index: index,
                                id: None,
                                name: None,
                                arguments_delta: "{}".to_owned(),
                            }));
                        }
                    }
                    "contentBlockDelta" if tool_open.is_some() => {
                        if !response_started || finished { yield Err(protocol_error()); return; }
                        let input = value.get("delta").and_then(|delta| delta.get("toolUse")).and_then(|tool| tool.get("input")).and_then(Value::as_str);
                        let (Some(input), Some((index, seen))) = (input, tool_open.as_mut()) else { yield Err(protocol_error()); return; };
                        *seen |= !input.is_empty();
                        yield Ok(ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                            choice_index: 0,
                            tool_call_index: *index,
                            id: None,
                            name: None,
                            arguments_delta: input.to_owned(),
                        }));
                    }
                    "contentBlockDelta" => {
                        if !response_started || finished { yield Err(protocol_error()); return; }
                        let delta = value.get("delta").and_then(Value::as_object);
                        if delta.is_some_and(|delta| delta.len() == 1 && is_reasoning(delta)) { continue; }
                        let text = delta.and_then(|delta| delta.get("text")).and_then(Value::as_str);
                        let Some(text) = text else { yield Err(protocol_error()); return; };
                        if !text.is_empty() {
                            yield Ok(ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                                choice_index: 0,
                                content_delta: Some(text.to_owned()),
                                refusal_delta: None,
                                annotations: vec![],
                            }));
                        }
                    }
                    "messageStop" => {
                        if !response_started || finished { yield Err(protocol_error()); return; }
                        let reason = match value.get("stopReason").and_then(Value::as_str) {
                            Some(reason) => reason,
                            None => { yield Err(protocol_error()); return; }
                        };
                        finished = true;
                        match ChoiceFinished::new(0, finish_reason_of(reason)) {
                            Ok(choice_finished) => yield Ok(ChatStreamEvent::ChoiceFinished(choice_finished)),
                            Err(_) => { yield Err(protocol_error()); return; }
                        }
                    }
                    "metadata" => {
                        if !finished || metadata_seen { yield Err(protocol_error()); return; }
                        metadata_seen = true;
                        let usage = match value.get("usage") {
                            Some(usage) => usage,
                            None => { yield Err(protocol_error()); return; }
                        };
                        if let Err(error) = validate_known_usage_fields(usage) { yield Err(error); return; }
                        input_tokens = usage.get("inputTokens").and_then(Value::as_u64);
                        output_tokens = usage.get("outputTokens").and_then(Value::as_u64);
                        match (cache_read_tokens(usage), cache_write_tokens(usage)) {
                            (Ok(read), Ok(write)) => {
                                cached_tokens = Some(read);
                                other_tokens = Some(write);
                            }
                            _ => { yield Err(protocol_error()); return; }
                        }
                    }
                    _ => { yield Err(protocol_error()); return; }
                }
            }

            if metadata_seen {
                break 'outer;
            }

            let Some(chunk) = (match tokio::time::timeout(idle_timeout, bytes.next()).await {
                Ok(chunk) => chunk,
                Err(_) => { yield Err(ProviderError::new(ProviderErrorCategory::Timeout)); return; }
            }) else {
                yield Err(protocol_error());
                return;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => { yield Err(network_error_simple()); return; }
            };
            total = total.saturating_add(chunk.len());
            if total > max { yield Err(protocol_error()); return; }
            pending.extend_from_slice(&chunk);
        }

        if !pending.is_empty() || !response_started || !finished || !metadata_seen {
            yield Err(protocol_error());
            return;
        }
        let input = match input_tokens { Some(v) => v, None => { yield Err(protocol_error()); return; } };
        let output = match output_tokens { Some(v) => v, None => { yield Err(protocol_error()); return; } };
        let usage = match usage_of(
            input,
            output,
            cached_tokens.unwrap_or(0),
            other_tokens.unwrap_or(0),
        ) {
            Ok(usage) => usage,
            Err(_) => { yield Err(protocol_error()); return; }
        };
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

/// AWS error type identifiers arrive either as `ExceptionShapeName`
/// (event-stream `:exception-type`/JSON `__type`, sometimes namespaced as
/// `namespace#ExceptionShapeName` or suffixed `:http://...`) or the
/// lower-camel-case form used by this function's lookup table.
fn normalize_exception_type(raw: &str) -> String {
    let trimmed = raw.split(':').next().unwrap_or(raw);
    let name = trimmed.rsplit('#').next().unwrap_or(trimmed);
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_ascii_lowercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

fn category_for_exception_type(value: &str) -> Option<ProviderErrorCategory> {
    match value {
        "accessDeniedException" => Some(ProviderErrorCategory::Authorization),
        // The credential is invalid, expired or badly signed: the
        // account's, whatever model the request named.
        "unrecognizedClientException"
        | "invalidSignatureException"
        | "incompleteSignatureException"
        | "signatureDoesNotMatch"
        | "expiredTokenException"
        | "expiredToken"
        | "invalidClientTokenId"
        | "missingAuthenticationTokenException"
        | "invalidAccessKeyId" => Some(ProviderErrorCategory::Authentication),
        "validationException" | "resourceNotFoundException" => {
            Some(ProviderErrorCategory::InvalidRequest)
        }
        "throttlingException" | "serviceQuotaExceededException" => {
            Some(ProviderErrorCategory::RateLimited)
        }
        "modelTimeoutException" => Some(ProviderErrorCategory::Timeout),
        "modelNotReadyException" | "serviceUnavailableException" | "internalServerException" => {
            Some(ProviderErrorCategory::Unavailable)
        }
        "modelErrorException" | "modelStreamErrorException" => {
            Some(ProviderErrorCategory::Provider)
        }
        _ => None,
    }
}

fn category_for_status(status: StatusCode) -> ProviderErrorCategory {
    match status.as_u16() {
        401 => ProviderErrorCategory::Authentication,
        403 => ProviderErrorCategory::Authorization,
        400 | 404 | 422 => ProviderErrorCategory::InvalidRequest,
        408 | 504 => ProviderErrorCategory::Timeout,
        424 => ProviderErrorCategory::Provider,
        429 => ProviderErrorCategory::RateLimited,
        other if (500..600).contains(&other) => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    }
}

fn status_error(
    status: StatusCode,
    body: &Value,
    error_type_header: Option<String>,
    request_id: Option<String>,
    retry_after: Option<Duration>,
) -> ProviderError {
    let body_type = body
        .get("__type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let normalized = error_type_header
        .or(body_type)
        .map(|raw| normalize_exception_type(&raw));
    let category = normalized
        .as_deref()
        .and_then(category_for_exception_type)
        .unwrap_or_else(|| category_for_status(status));
    ProviderError::from_parts(
        category,
        Some(PROVIDER.to_owned()),
        request_id,
        Some(status.as_u16()),
        normalized,
        retry_after,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

fn stream_exception_error(
    exception_type: Option<&str>,
    request_id: Option<String>,
) -> ProviderError {
    let normalized = exception_type.map(normalize_exception_type);
    let category = normalized
        .as_deref()
        .and_then(category_for_exception_type)
        .unwrap_or(ProviderErrorCategory::Provider);
    ProviderError::from_parts(
        category,
        Some(PROVIDER.to_owned()),
        request_id,
        None,
        normalized,
        None,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        CorrelationId, DeploymentModelReference, ProviderAttemptIdempotencyKey,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn test_keys() -> crate::aws::AwsCredentials {
        crate::aws::AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: zeroize::Zeroizing::new(
                "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            ),
            session_token: None,
        }
    }

    fn config(base_url: String) -> BedrockConfig {
        BedrockConfig {
            control_base_url: base_url.clone(),
            base_url,
            region: "us-east-1".into(),
            credentials: crate::aws_credentials::AwsCredentialSource::fixed(test_keys()),
            sts_endpoint: None,
            s3_base_url: None,
            request_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: 1024 * 1024,
        }
    }

    fn adapter() -> BedrockAdapter {
        BedrockAdapter::new(&config("http://127.0.0.1:9".into())).unwrap()
    }

    fn context() -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new(
                "deployment",
                "anthropic.claude-3-5-sonnet-20240620-v1:0",
            )
            .unwrap(),
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

    /// AWS's own worked SigV4 example uses a generic "service" name and GET;
    /// this adapter always POSTs to the `bedrock` service, so verify the
    /// implementation against an independently computed vector instead
    /// (Python's stdlib hmac/hashlib from scratch). The model ID's `:` is
    /// sent as `%3A` and signed as `%253A`: Bedrock, like every service but
    /// S3, signs each path segment encoded again, and rejects a signature
    /// over the path as sent (found by the live test).
    #[test]
    fn sigv4_signature_matches_independently_computed_vector() {
        let adapter = BedrockAdapter::new(&config(
            "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
        ))
        .unwrap();
        let url = adapter
            .endpoint("anthropic.claude-3-5-sonnet-20240620-v1:0", "converse")
            .unwrap();
        let body = br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}]}"#;
        let headers = adapter
            .sign(&test_keys(), "POST", &url, body, "20150830T123600Z")
            .unwrap();
        let authorization = headers
            .iter()
            .find(|(name, _)| *name == header::AUTHORIZATION)
            .map(|(_, value)| value.to_str().unwrap())
            .unwrap();
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=58775cfdd511baae92ef28b6ce7842a649515646bcbfd5f4d5e3ba74e8836c2b"
        );
    }

    /// A control-plane listing signs a bodiless GET whose canonical query is
    /// the URL's own query. The expected signature was computed separately
    /// with Python's hmac and hashlib.
    #[test]
    fn sigv4_signs_a_listing_query_in_canonical_form() {
        let mut cfg = config("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        cfg.control_base_url = "https://bedrock.us-east-1.amazonaws.com".into();
        let adapter = BedrockAdapter::new(&cfg).unwrap();
        let mut url = adapter.control_base_url.join("inference-profiles").unwrap();
        let query = list_query(&[("nextToken", "t/1="), ("maxResults", "1000")]);
        url.set_query(Some(&query));
        assert_eq!(url.query(), Some("maxResults=1000&nextToken=t%2F1%3D"));
        let headers = adapter
            .sign(&test_keys(), "GET", &url, b"", "20150830T123600Z")
            .unwrap();
        let authorization = headers
            .iter()
            .find(|(name, _)| *name == header::AUTHORIZATION)
            .map(|(_, value)| value.to_str().unwrap())
            .unwrap();
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=634f7549b6f1a10ef027e6ccd5b4d6010e90058fb861325cdb8a86c162698d06"
        );
    }

    #[test]
    fn sigv4_signs_security_token_when_present() {
        let adapter = BedrockAdapter::new(&config(
            "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
        ))
        .unwrap();
        let url = adapter
            .endpoint("anthropic.claude-3-5-sonnet-20240620-v1:0", "converse")
            .unwrap();
        let keys = crate::aws::AwsCredentials {
            session_token: Some(zeroize::Zeroizing::new("test-session-token".into())),
            ..test_keys()
        };
        let headers = adapter
            .sign(&keys, "POST", &url, b"{}", "20150830T123600Z")
            .unwrap();
        let authorization = headers
            .iter()
            .find(|(name, _)| *name == header::AUTHORIZATION)
            .map(|(_, value)| value.to_str().unwrap())
            .unwrap();
        assert!(
            authorization
                .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token")
        );
        let token_header = headers
            .iter()
            .find(|(name, _)| name.as_str() == "x-amz-security-token")
            .unwrap();
        assert!(token_header.1.is_sensitive());
    }

    #[test]
    fn endpoint_percent_encodes_inference_profile_arns_containing_slashes() {
        let adapter = adapter();
        let url = adapter
            .endpoint(
                "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-3-5-sonnet-20240620-v1:0",
                "converse",
            )
            .unwrap();
        assert_eq!(
            url.path(),
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse"
        );
    }

    #[test]
    fn endpoint_rejects_control_and_reserved_characters() {
        let adapter = adapter();
        for model in ["", "bad model", "bad\nmodel", "bad?model", "bad#model"] {
            assert_eq!(
                adapter.endpoint(model, "converse").unwrap_err().category(),
                ProviderErrorCategory::Internal
            );
        }
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
        assert_eq!(body["system"][0]["text"], "system");
        assert_eq!(body["messages"][0]["role"], "user");

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

    fn embedding_context(model: &str) -> InferenceContext {
        InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new("attempt").unwrap(),
            DeploymentModelReference::new("deployment", model).unwrap(),
        )
    }

    /// Serves `responses` one connection each (`Connection: close`) and
    /// returns the requests in the order they arrived.
    fn serve_each(
        listener: TcpListener,
        responses: Vec<String>,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut wires = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                wires.push(read_request(&mut socket).await);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            wires
        })
    }

    fn invoke_response(body: &str, tokens: Option<u64>) -> String {
        let tokens = tokens
            .map(|tokens| format!("X-Amzn-Bedrock-Input-Token-Count: {tokens}\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-amzn-RequestId: req-embed\r\n{tokens}Connection: close\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    #[test]
    fn embedding_schema_follows_the_model_family() {
        assert_eq!(
            embedding_schema("amazon.titan-embed-text-v2:0"),
            Some(EmbeddingSchema::Titan)
        );
        assert_eq!(
            embedding_schema("cohere.embed-v4:0"),
            Some(EmbeddingSchema::Cohere)
        );
        assert_eq!(
            embedding_schema("us.cohere.embed-v4:0"),
            Some(EmbeddingSchema::Cohere)
        );
        assert_eq!(
            embedding_schema(
                "arn:aws:bedrock:us-east-1::foundation-model/amazon.titan-embed-text-v2:0"
            ),
            Some(EmbeddingSchema::Titan)
        );
        assert_eq!(
            embedding_schema("anthropic.claude-3-haiku-20240307-v1:0"),
            None
        );
    }

    #[tokio::test]
    async fn embeddings_reject_a_model_without_a_known_schema() {
        let error = adapter()
            .embeddings(
                context(),
                EmbeddingRequest {
                    input: vec!["a".into()],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::UnsupportedFeature);
    }

    #[tokio::test]
    async fn embeddings_refuse_an_empty_input() {
        let error = adapter()
            .embeddings(
                embedding_context("amazon.titan-embed-text-v2:0"),
                EmbeddingRequest { input: Vec::new() },
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::InvalidRequest);
    }

    #[tokio::test]
    async fn titan_embeddings_invoke_once_per_text_and_sum_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // The second response has no header, so its body count is used.
        let server = serve_each(
            listener,
            vec![
                invoke_response(
                    r#"{"embedding":[0.5,-0.25],"inputTextTokenCount":3}"#,
                    Some(3),
                ),
                invoke_response(r#"{"embedding":[0.5,-0.25],"inputTextTokenCount":4}"#, None),
            ],
        );
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let response = adapter
            .embeddings(
                embedding_context("amazon.titan-embed-text-v2:0"),
                EmbeddingRequest {
                    input: vec!["one".into(), "two".into()],
                },
            )
            .await
            .unwrap();
        let wires = server.await.unwrap();
        assert_eq!(wires.len(), 2);
        for wire in &wires {
            assert!(wire.starts_with("POST /model/amazon.titan-embed-text-v2%3A0/invoke HTTP/1.1"));
            assert!(
                wire.to_ascii_lowercase()
                    .contains("authorization: aws4-hmac-sha256")
            );
        }
        let bodies: Vec<&str> = wires
            .iter()
            .map(|wire| wire.split("\r\n\r\n").nth(1).unwrap())
            .collect();
        assert!(bodies.contains(&r#"{"inputText":"one"}"#));
        assert!(bodies.contains(&r#"{"inputText":"two"}"#));
        assert_eq!(response.data().len(), 2);
        assert_eq!(response.data()[1].index(), 1);
        assert_eq!(response.data()[0].vector(), &[0.5, -0.25]);
        assert_eq!(response.provider_request_id(), Some("req-embed"));
        let usage = response.usage().unwrap().usage();
        assert_eq!(usage.input_tokens().unwrap().value(), Some(7));
        assert_eq!(usage.output_tokens().unwrap().value(), Some(0));
    }

    #[tokio::test]
    async fn cohere_embeddings_send_texts_as_documents_and_read_both_shapes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![invoke_response(
                r#"{"id":"x","embeddings":{"float":[[0.1,0.2],[0.3,0.4]]},"response_type":"embeddings_by_type"}"#,
                Some(5),
            )],
        );
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let response = adapter
            .embeddings(
                embedding_context("cohere.embed-v4:0"),
                EmbeddingRequest {
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .unwrap();
        let wire = server.await.unwrap().remove(0);
        assert!(wire.starts_with("POST /model/cohere.embed-v4%3A0/invoke HTTP/1.1"));
        let body: Value = serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"texts": ["a", "b"], "input_type": "search_document", "embedding_types": ["float"]})
        );
        assert_eq!(response.data().len(), 2);
        assert_eq!(response.data()[1].vector(), &[0.3, 0.4]);
        assert_eq!(
            response
                .usage()
                .unwrap()
                .usage()
                .input_tokens()
                .unwrap()
                .value(),
            Some(5)
        );
        assert_eq!(
            cohere_vectors(&json!({"embeddings": [[1.0], [2.0]]})).unwrap(),
            vec![vec![1.0], vec![2.0]]
        );
    }

    #[tokio::test]
    async fn embeddings_fail_when_the_vector_count_is_wrong_and_omit_unreported_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![
                invoke_response(r#"{"embeddings":{"float":[[0.1]]}}"#, None),
                invoke_response(r#"{"embeddings":{"float":[[0.1]]}}"#, None),
            ],
        );
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter
            .embeddings(
                embedding_context("cohere.embed-v4:0"),
                EmbeddingRequest {
                    input: vec!["a".into(), "b".into()],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
        // The answer was billed though it cannot be used.
        assert!(error.partial_usage().is_some());
        let response = adapter
            .embeddings(
                embedding_context("cohere.embed-v4:0"),
                EmbeddingRequest {
                    input: vec!["a".into()],
                },
            )
            .await
            .unwrap();
        assert!(response.usage().is_none());
        server.await.unwrap();
    }

    fn error_response(status: &str) -> String {
        let body = r#"{"message":"slow down"}"#;
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    /// A later Cohere call refused after an earlier one was billed: the
    /// error carries the billed tokens, so it is not retried.
    #[tokio::test]
    async fn a_failure_after_a_billed_call_carries_its_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let vectors = format!(
            r#"{{"embeddings":{{"float":[{}]}}}}"#,
            vec!["[0.1]"; COHERE_EMBED_BATCH].join(",")
        );
        let server = serve_each(
            listener,
            vec![
                invoke_response(&vectors, Some(10)),
                error_response("429 Too Many Requests"),
            ],
        );
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter
            .embeddings(
                embedding_context("cohere.embed-v4:0"),
                EmbeddingRequest {
                    input: vec!["text".to_owned(); COHERE_EMBED_BATCH + 1],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), Some(429));
        let input = error.partial_usage().unwrap().input_tokens().unwrap();
        assert_eq!(input, TokenCounter::exact(10));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    /// Titan's calls of a group all finish; one Bedrock may have run counts
    /// as estimated, and a first call refused alone fails plainly.
    #[tokio::test]
    async fn titan_failures_estimate_calls_that_may_have_run() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = serve_each(
            listener,
            vec![
                invoke_response(r#"{"embedding":[0.1]}"#, Some(3)),
                error_response("500 Internal Server Error"),
                error_response("429 Too Many Requests"),
            ],
        );
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter
            .embeddings(
                embedding_context("amazon.titan-embed-text-v2:0"),
                EmbeddingRequest {
                    input: vec!["abcd".into(), "efgh".into()],
                },
            )
            .await
            .unwrap_err();
        let input = error.partial_usage().unwrap().input_tokens().unwrap();
        assert_eq!(input, TokenCounter::estimated(4));
        let error = adapter
            .embeddings(
                embedding_context("amazon.titan-embed-text-v2:0"),
                EmbeddingRequest {
                    input: vec!["abcd".into()],
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status(), Some(429));
        assert!(error.partial_usage().is_none());
        server.await.unwrap();
    }

    #[test]
    fn listed_embedding_models_state_the_embeddings_operation() {
        let mut models = ModelListBuilder::default();
        let mut bases = HashMap::new();
        push_foundation_models(
            &mut models,
            &mut bases,
            &json!({"modelSummaries": [
                {"modelId": "amazon.titan-embed-text-v2:0", "outputModalities": ["EMBEDDING"]},
                {"modelId": "amazon.nova-micro-v1:0", "outputModalities": ["TEXT"]}
            ]}),
        )
        .unwrap();
        let models = models.finish();
        let operations = |id: &str| {
            models
                .iter()
                .find(|m| m.model == id)
                .unwrap()
                .operations
                .clone()
        };
        assert_eq!(
            operations("amazon.titan-embed-text-v2:0"),
            Some(vec![CatalogOperation::Embeddings])
        );
        assert_eq!(operations("amazon.nova-micro-v1:0"), None);
    }

    #[tokio::test]
    async fn mock_http_chat_uses_wire_path_headers_and_response_mapping() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let response_body = r#"{"output":{"message":{"role":"assistant","content":[{"text":"hi there"}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":2,"totalTokens":5}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-amzn-RequestId: req-1\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let wire = read_request(&mut socket).await;
            sender.send(wire).unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter.chat(context(), request()).await.unwrap();
        let wire = receiver.await.unwrap();
        assert!(wire.starts_with(
            "POST /model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse HTTP/1.1"
        ));
        assert!(
            wire.to_ascii_lowercase()
                .contains("authorization: aws4-hmac-sha256")
        );
        assert_eq!(result.choices()[0].message.content, "hi there");
        assert_eq!(result.provider_request_id(), Some("req-1"));
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
    async fn absent_cache_counters_settle_as_exact_zero() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = r#"{"output":{"message":{"role":"assistant","content":[{"text":"hi there"}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":2}}"#;
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
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter.chat(context(), request()).await.unwrap();
        let usage = result.usage().unwrap().usage();
        assert_eq!(usage.cached_input_tokens().unwrap().value(), Some(0));
        assert_eq!(usage.other_tokens().unwrap().value(), Some(0));
    }

    #[tokio::test]
    async fn non_text_content_block_fails_closed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = r#"{"output":{"message":{"role":"assistant","content":[{"image":{"format":"png","source":{"bytes":"AA=="}}}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":2}}"#;
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
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter.chat(context(), request()).await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
    }

    #[tokio::test]
    async fn reasoning_content_is_left_out_of_the_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        // Shape seen live from openai.gpt-oss-20b-1:0.
        let response_body = r#"{"output":{"message":{"role":"assistant","content":[{"reasoningContent":{"reasoningText":{"text":"The user wants pong."}}},{"text":"pong"}]}},"stopReason":"end_turn","usage":{"inputTokens":72,"outputTokens":42,"totalTokens":114}}"#;
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
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let result = adapter.chat(context(), request()).await.unwrap();
        assert_eq!(result.choices()[0].message.content, "pong");
        assert_eq!(
            result
                .usage()
                .unwrap()
                .usage()
                .output_tokens()
                .unwrap()
                .value(),
            Some(42)
        );
    }

    #[tokio::test]
    async fn error_status_maps_throttling_to_rate_limited() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let error_body = r#"{"message":"too many requests"}"#;
        let response = format!(
            "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nx-amzn-ErrorType: ThrottlingException\r\nContent-Length: {}\r\n\r\n{}",
            error_body.len(),
            error_body
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let adapter = BedrockAdapter::new(&config(format!("http://{address}"))).unwrap();
        let error = adapter.chat(context(), request()).await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        assert_eq!(error.provider_code(), Some("throttlingException"));
    }

    fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut headers_bytes = Vec::new();
        for (name, value) in headers {
            headers_bytes.push(name.len() as u8);
            headers_bytes.extend_from_slice(name.as_bytes());
            headers_bytes.push(7); // string-typed header value
            headers_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers_bytes.extend_from_slice(value.as_bytes());
        }
        let total_len = 12 + headers_bytes.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total_len);
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers_bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend_from_slice(&headers_bytes);
        frame.extend_from_slice(payload);
        let message_crc = crc32(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        frame
    }

    fn event_frame(event_type: &str, payload: &Value) -> Vec<u8> {
        encode_frame(
            &[
                (":message-type", "event"),
                (":event-type", event_type),
                (":content-type", "application/json"),
            ],
            &serde_json::to_vec(payload).unwrap(),
        )
    }

    fn valid_stream_frames() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", &json!({"role": "assistant"})));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "hi there"}}),
        ));
        body.extend(event_frame(
            "messageStop",
            &json!({"stopReason": "end_turn"}),
        ));
        body.extend(event_frame(
            "metadata",
            &json!({"usage": {"inputTokens": 1, "outputTokens": 2}}),
        ));
        body
    }

    async fn mock_stream_adapter(chunks: Vec<Vec<u8>>) -> BedrockAdapter {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.amazon.eventstream\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            for chunk in chunks {
                socket.write_all(&chunk).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        BedrockAdapter::new(&config(format!("http://{address}"))).unwrap()
    }

    #[tokio::test]
    async fn mock_http_stream_handles_fragmented_binary_frames() {
        let body = valid_stream_frames();
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
    async fn stream_absent_cache_counters_settle_as_exact_zero() {
        let adapter = mock_stream_adapter(vec![valid_stream_frames()]).await;
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
        assert_eq!(usage.other_tokens().unwrap().value(), Some(0));
    }

    #[tokio::test]
    async fn stream_skips_reasoning_deltas() {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", &json!({"role": "assistant"})));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "thinking"}}}),
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"text": "pong"}}),
        ));
        body.extend(event_frame(
            "messageStop",
            &json!({"stopReason": "end_turn"}),
        ));
        body.extend(event_frame(
            "metadata",
            &json!({"usage": {"inputTokens": 1, "outputTokens": 9}}),
        ));
        let adapter = mock_stream_adapter(vec![body]).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().all(Result::is_ok));
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ok(ChatStreamEvent::ChoiceDelta(delta)) => delta.content_delta.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["pong"]);
    }

    #[tokio::test]
    async fn premature_eof_before_message_stop_fails_closed() {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", &json!({"role": "assistant"})));
        let adapter = mock_stream_adapter(vec![body]).await;
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

    #[tokio::test]
    async fn streaming_exception_frame_maps_to_category() {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", &json!({"role": "assistant"})));
        body.extend(encode_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "throttlingException"),
                (":content-type", "application/json"),
            ],
            &serde_json::to_vec(&json!({"message": "slow down"})).unwrap(),
        ));
        let adapter = mock_stream_adapter(vec![body]).await;
        let events = adapter
            .chat_stream(context(), request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(events.iter().any(|event| matches!(
            event,
            Err(error) if error.category() == ProviderErrorCategory::RateLimited
        )));
    }

    #[test]
    fn crc32_matches_known_vector() {
        // Well-known CRC32/IEEE check value for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Bedrock's Converse usage carries an empty `serverToolUsage` (seen
    /// live); only counts this adapter does not map fail closed.
    #[test]
    fn usage_accepts_empty_server_tool_usage_and_rejects_counts() {
        let parse = |usage: Value| parse_usage(&usage).map(|usage| usage.input_tokens());
        assert!(
            parse(json!({"inputTokens": 7, "outputTokens": 2, "totalTokens": 9, "serverToolUsage": {}}))
                .is_ok()
        );
        assert!(
            parse(
                json!({"inputTokens": 7, "outputTokens": 2, "serverToolUsage": {"webSearch": 0}})
            )
            .is_ok()
        );
        for rejected in [
            json!({"inputTokens": 7, "outputTokens": 2, "serverToolUsage": {"webSearch": 1}}),
            json!({"inputTokens": 7, "outputTokens": 2, "serverToolUsage": 3}),
            json!({"inputTokens": 7, "outputTokens": 2, "unknownTokens": 0}),
        ] {
            assert!(parse(rejected.clone()).is_err(), "{rejected}");
        }
    }

    /// Converse's `inputTokens` leaves out cache reads and writes: input
    /// adds the reads back (charged as cached input, so never below them)
    /// and the writes are `other`, charged on top.
    #[test]
    fn usage_adds_cache_reads_to_the_uncached_input() {
        let usage = parse_usage(&json!({
            "inputTokens": 30, "outputTokens": 12,
            "cacheReadInputTokens": 1500, "cacheWriteInputTokens": 200
        }))
        .unwrap();
        let value = |c: Option<TokenCounter>| c.and_then(|c| c.value());
        assert_eq!(
            (
                value(usage.input_tokens()),
                value(usage.cached_input_tokens()),
                value(usage.other_tokens())
            ),
            (Some(1530), Some(1500), Some(200))
        );
        let price = crate::domain::accounting::PriceSnapshot::configured_with_breakdown(
            100,
            500,
            10,
            0,
            125,
            "test",
            crate::domain::accounting::PricingConfidence::Known,
        )
        .unwrap();
        assert_eq!(
            price.charge(&usage).unwrap().amount.value(),
            30 * 100 + 1500 * 10 + 200 * 125 + 12 * 500
        );
    }

    /// The raw Converse API reports each cache count under two names (seen
    /// live on Claude Sonnet 4.6); either is read, and a disagreement fails
    /// closed rather than billing one of them.
    #[test]
    fn usage_reads_cache_counts_under_either_name() {
        let parse = |usage: Value| {
            parse_usage(&usage).map(|usage| (usage.cached_input_tokens(), usage.other_tokens()))
        };
        let live = json!({
            "cacheReadInputTokenCount": 3, "cacheReadInputTokens": 3,
            "cacheWriteInputTokenCount": 5, "cacheWriteInputTokens": 5,
            "inputTokens": 9, "outputTokens": 20, "serverToolUsage": {}, "totalTokens": 29
        });
        let both = parse(live).unwrap();
        let alias_only = parse(json!({
            "inputTokens": 9, "outputTokens": 20,
            "cacheReadInputTokenCount": 3, "cacheWriteInputTokenCount": 5
        }))
        .unwrap();
        assert_eq!(both, alias_only);
        assert_ne!(
            both,
            parse(json!({"inputTokens": 9, "outputTokens": 20})).unwrap()
        );
        for rejected in [
            json!({"inputTokens": 9, "outputTokens": 20,
                   "cacheReadInputTokens": 3, "cacheReadInputTokenCount": 4}),
            json!({"inputTokens": 9, "outputTokens": 20, "cacheWriteInputTokenCount": "5"}),
        ] {
            assert!(parse(rejected.clone()).is_err(), "{rejected}");
        }
    }

    /// A control plane that answers each request by its path, concurrently,
    /// and reports each request line.
    async fn control_plane(
        route: fn(&str) -> (u16, &'static str, String),
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    let line = request.lines().next().unwrap_or_default().to_owned();
                    let path = line.split(' ').nth(1).unwrap_or_default().to_owned();
                    let body = request
                        .split_once("\r\n\r\n")
                        .map(|(_, body)| body.to_owned())
                        .unwrap_or_default();
                    log.lock().unwrap().push(line);
                    if !body.is_empty() {
                        log.lock().unwrap().push(format!("BODY {body}"));
                    }
                    let (status, error_type, body) = route(&path);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nx-amzn-ErrorType: {error_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://{address}"), seen)
    }

    fn availability_body(agreement: &str, authorization: &str, entitlement: &str) -> String {
        json!({
            "agreementAvailability": {"status": agreement},
            "authorizationStatus": authorization,
            "entitlementAvailability": entitlement,
            "regionAvailability": "AVAILABLE"
        })
        .to_string()
    }

    fn listing_route(path: &str) -> Option<(u16, &'static str, String)> {
        if path == "/foundation-models" {
            return Some((
                200,
                "",
                json!({"modelSummaries": [
                    {"modelId": "amazon.nova-micro-v1:0", "inferenceTypesSupported": ["ON_DEMAND"]},
                    {"modelId": "anthropic.claude-haiku-4-5-20251001-v1:0", "inferenceTypesSupported": ["INFERENCE_PROFILE"]},
                    {"modelId": "acme.unknown-v1"}
                ]})
                .to_string(),
            ));
        }
        if path.starts_with("/inference-profiles") && path.contains("type=APPLICATION") {
            return Some((200, "", r#"{"inferenceProfileSummaries":[]}"#.into()));
        }
        if let Some(items) = [
            ("/imported-models", "modelSummaries"),
            (
                "/provisioned-model-throughputs",
                "provisionedModelSummaries",
            ),
            ("/marketplace-model/endpoints", "marketplaceModelEndpoints"),
            (
                "/model-customization/custom-model-deployments",
                "modelDeploymentSummaries",
            ),
        ]
        .into_iter()
        .find_map(|(prefix, items)| path.starts_with(prefix).then_some(items))
        {
            return Some((200, "", json!({ items: [] }).to_string()));
        }
        if path.starts_with("/inference-profiles") {
            let arn = |region: &str| {
                format!(
                    "arn:aws:bedrock:{region}::foundation-model/anthropic.claude-haiku-4-5-20251001-v1:0"
                )
            };
            return Some((
                200,
                "",
                json!({"inferenceProfileSummaries": [
                    {"inferenceProfileId": "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                     "models": [{"modelArn": arn("us-east-1")}, {"modelArn": arn("us-west-2")}]},
                    {"inferenceProfileId": "us.mixed", "models": [
                        {"modelArn": "arn:aws:bedrock:us-east-1::foundation-model/a"},
                        {"modelArn": "arn:aws:bedrock:us-west-2::foundation-model/b"}]}
                ]})
                .to_string(),
            ));
        }
        None
    }

    fn entitlements(models: &[DiscoveredModel]) -> Vec<(&str, Option<Entitlement>)> {
        models
            .iter()
            .map(|model| (model.model.as_str(), model.entitlement))
            .collect()
    }

    /// `ListFoundationModelAgreementOffers` as Bedrock answers it (shape
    /// read from us-east-2 on 2026-11-19, token and terms shortened).
    fn offers_body() -> String {
        json!({
            "modelId": "anthropic.claude-haiku-5-5",
            "offers": [
                {
                    "offerId": "offer-u3aih6zr7uw5u",
                    "offerToken": "AQICAHsecret-offer-token",
                    "termDetails": {
                        "usageBasedPricingTerm": {"rateCard": [
                            {"dimension": "USE1_input_tokens_global_standard", "price": "0.1",
                             "description": "Input Tokens - Standard, Global", "unit": "Units"},
                            {"dimension": "USE1_output_tokens_global_standard", "price": "0.5",
                             "description": "Output Tokens - Standard, Global", "unit": "Units"},
                            {"dimension": "USE2_input_tokens_global_standard", "price": "0.1",
                             "description": "Input Tokens - Standard, Global", "unit": "Units"},
                            {"dimension": "EU_input_tokens_standard", "price": "0.11",
                             "description": "Input Tokens - Standard", "unit": "Units"}
                        ]},
                        "legalTerm": {"url": "https://aws-mp-offer-legal-artifacts.s3.amazonaws.com/eula"},
                        "supportTerm": {"refundPolicyDescription": "No refunds"}
                    }
                },
                // No token: it cannot be accepted, so it is not offered.
                {"offerId": "offer-other", "termDetails": {}}
            ]
        })
        .to_string()
    }

    #[tokio::test]
    async fn agreement_offers_show_the_regions_rates_and_never_the_token() {
        use crate::provider::ModelAgreements;
        let (base, seen) = control_plane(|path| match path {
            "/list-foundation-model-agreement-offers/anthropic.claude-haiku-5-5?offerType=ALL" => {
                (200, "", offers_body())
            }
            _ => (404, "ResourceNotFoundException", "{}".into()),
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        let offers = adapter
            .agreement_offers("anthropic.claude-haiku-5-5")
            .await
            .unwrap();
        assert_eq!(offers.len(), 1);
        let offer = &offers[0];
        assert_eq!(offer.offer_id, "offer-u3aih6zr7uw5u");
        assert_eq!(offer.token.expose(), "AQICAHsecret-offer-token");
        assert!(!format!("{offer:?}").contains("AQICAH"));
        // us-east-1's rates only.
        assert_eq!(offer.terms.rate_card_region.as_deref(), Some("USE1"));
        assert_eq!(offer.terms.rate_card_total, 4);
        assert_eq!(
            offer
                .terms
                .rate_card
                .iter()
                .map(|rate| rate.dimension.as_str())
                .collect::<Vec<_>>(),
            [
                "USE1_input_tokens_global_standard",
                "USE1_output_tokens_global_standard"
            ]
        );
        assert_eq!(offer.terms.refund_policy.as_deref(), Some("No refunds"));
        assert!(
            offer
                .terms
                .legal_url
                .as_deref()
                .unwrap()
                .starts_with("https://")
        );
        assert_eq!(seen.lock().unwrap().len(), 1);

        // A long signed link is kept whole; one past the limit, or not
        // https, is left out rather than cut.
        let signed = format!(
            "https://aws-mp-offer-legal-artifacts.s3.amazonaws.com/eula?X-Amz-Security-Token={}&X-Amz-Signature=abc",
            "A".repeat(3000)
        );
        assert_eq!(legal_url(&signed).as_deref(), Some(signed.as_str()));
        assert_eq!(
            legal_url(&format!("https://x.example/{}", "a".repeat(9000))),
            None
        );
        assert_eq!(legal_url("http://x.example/eula"), None);
        assert_eq!(legal_url("https:not a url"), None);
        let mut page: Value = serde_json::from_str(&offers_body()).unwrap();
        page["offers"][0]["termDetails"]["legalTerm"]["url"] = json!(signed);
        let kept = offers_of(&page, "us-east-1").unwrap();
        assert_eq!(kept[0].terms.legal_url.as_deref(), Some(signed.as_str()));

        // A region whose code the card does not name shows the whole card.
        let page: Value = serde_json::from_str(&offers_body()).unwrap();
        let whole = offers_of(&page, "me-central-1").unwrap();
        assert_eq!(whole[0].terms.rate_card_region, None);
        assert_eq!(whole[0].terms.rate_card.len(), 4);
        // eu-west-1's code is `EU`, which `EUC1_` and the like do not match.
        let eu = offers_of(&page, "eu-west-1").unwrap();
        assert_eq!(eu[0].terms.rate_card.len(), 1);
    }

    #[tokio::test]
    async fn agreements_are_created_with_the_offer_token() {
        use crate::provider::ModelAgreements;
        let (base, seen) = control_plane(|path| match path {
            "/create-foundation-model-agreement" => (
                202,
                "",
                json!({"modelId": "anthropic.claude-haiku-5-5"}).to_string(),
            ),
            _ => (404, "ResourceNotFoundException", "{}".into()),
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        adapter
            .create_agreement(
                "anthropic.claude-haiku-5-5",
                &crate::domain::model_agreement::OfferToken::new("AQICAHtoken"),
            )
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], "POST /create-foundation-model-agreement HTTP/1.1");
        let body: Value = serde_json::from_str(seen[1].strip_prefix("BODY ").unwrap()).unwrap();
        assert_eq!(
            body,
            json!({"modelId": "anthropic.claude-haiku-5-5", "offerToken": "AQICAHtoken"})
        );
    }

    #[tokio::test]
    async fn throttled_agreement_calls_are_retried() {
        use crate::provider::ModelAgreements;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static OFFERS: AtomicUsize = AtomicUsize::new(0);
        static CREATES: AtomicUsize = AtomicUsize::new(0);
        let (base, seen) = control_plane(|path| match path {
            "/list-foundation-model-agreement-offers/anthropic.claude-haiku-5-5?offerType=ALL" => {
                if OFFERS.fetch_add(1, Ordering::SeqCst) < 2 {
                    (
                        429,
                        "ThrottlingException",
                        r#"{"message":"You have sent too many requests."}"#.into(),
                    )
                } else {
                    (200, "", offers_body())
                }
            }
            "/create-foundation-model-agreement" => {
                if CREATES.fetch_add(1, Ordering::SeqCst) < 1 {
                    (
                        429,
                        "ThrottlingException",
                        r#"{"message":"You have sent too many requests."}"#.into(),
                    )
                } else {
                    (202, "", "{}".into())
                }
            }
            _ => (404, "ResourceNotFoundException", "{}".into()),
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        let offers = adapter
            .agreement_offers("anthropic.claude-haiku-5-5")
            .await
            .unwrap();
        assert_eq!(offers.len(), 1);
        assert_eq!(OFFERS.load(Ordering::SeqCst), 3);
        adapter
            .create_agreement(
                "anthropic.claude-haiku-5-5",
                &crate::domain::model_agreement::OfferToken::new("AQICAHtoken"),
            )
            .await
            .unwrap();
        assert_eq!(CREATES.load(Ordering::SeqCst), 2);
        // The repeated create sends the same request.
        let seen = seen.lock().unwrap();
        let bodies: Vec<&String> = seen
            .iter()
            .filter(|line| line.starts_with("BODY "))
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_is_waited_in_full_up_to_its_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let throttle = |after: u64| {
            ProviderError::from_parts(
                ProviderErrorCategory::RateLimited,
                None,
                None,
                Some(429),
                Some("throttlingException".into()),
                Some(Duration::from_secs(after)),
                None,
            )
            .unwrap()
        };
        // Waited out in full, past the 4-second backoff cap.
        let calls = AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        let result = throttled_retry(|| async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(throttle(7))
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(start.elapsed(), Duration::from_secs(7));
        // Past what a caller allows: given back at once.
        let calls = AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        let error = throttled_retry(|| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(throttle(30))
        })
        .await
        .unwrap_err();
        assert_eq!(error.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test]
    async fn an_exceeded_quota_is_not_retried() {
        use crate::provider::ModelAgreements;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let (base, _) = control_plane(|_| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            (
                400,
                "ServiceQuotaExceededException",
                r#"{"message":"quota"}"#.into(),
            )
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        let error = adapter
            .agreement_offers("anthropic.claude-haiku-5-5")
            .await
            .unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::RateLimited);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn agreement_refusals_keep_bedrocks_error_types() {
        use crate::provider::ModelAgreements;
        let (base, _) = control_plane(|path| match path {
            "/create-foundation-model-agreement" => (
                400,
                "ConflictException",
                r#"{"message":"An agreement already exists for this model."}"#.into(),
            ),
            _ => (
                403,
                "AccessDeniedException",
                r#"{"message":"User is not authorized to perform: bedrock:ListFoundationModelAgreementOffers"}"#.into(),
            ),
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        let conflict = adapter
            .create_agreement(
                "anthropic.claude-haiku-5-5",
                &crate::domain::model_agreement::OfferToken::new("t"),
            )
            .await
            .unwrap_err();
        assert_eq!(conflict.error_type(), Some("conflictException"));
        let denied = adapter
            .agreement_offers("anthropic.claude-haiku-5-5")
            .await
            .unwrap_err();
        assert_eq!(denied.category(), ProviderErrorCategory::Authorization);
        assert_eq!(denied.error_type(), Some("accessDeniedException"));
    }

    #[tokio::test]
    async fn agreement_availability_and_bases_are_read() {
        use crate::domain::model_agreement::ProviderAgreementStatus;
        use crate::provider::ModelAgreements;
        let (base, _) = control_plane(|path| {
            if let Some(response) = listing_route(path) {
                return response;
            }
            match path.strip_prefix("/foundation-model-availability/") {
                Some("anthropic.claude-haiku-4-5-20251001-v1%3A0") => (
                    200,
                    "",
                    json!({
                        "agreementAvailability": {"status": "ERROR", "errorMessage": "Payment\nmethod missing"},
                        "authorizationStatus": "AUTHORIZED",
                        "entitlementAvailability": "NOT_AVAILABLE",
                        "regionAvailability": "AVAILABLE"
                    })
                    .to_string(),
                ),
                _ => (
                    200,
                    "",
                    availability_body("PENDING", "AUTHORIZED", "AVAILABLE"),
                ),
            }
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        let failed = adapter
            .agreement_availability("anthropic.claude-haiku-4-5-20251001-v1:0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.agreement, ProviderAgreementStatus::Error);
        assert_eq!(
            failed.error_message.as_deref(),
            Some("Payment method missing")
        );
        assert_eq!(failed.entitled, Some(false));
        let pending = adapter
            .agreement_availability("amazon.nova-micro-v1:0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.agreement, ProviderAgreementStatus::Pending);
        assert_eq!(pending.entitled, Some(true));
        let bases = adapter.agreement_bases().await.unwrap();
        assert_eq!(
            bases["us.anthropic.claude-haiku-4-5-20251001-v1:0"],
            "anthropic.claude-haiku-4-5-20251001-v1:0"
        );
        assert!(!bases.contains_key("us.mixed"));
    }

    #[tokio::test]
    async fn listed_models_carry_their_foundation_models_availability() {
        let (base, seen) = control_plane(|path| {
            if let Some(response) = listing_route(path) {
                return response;
            }
            match path.strip_prefix("/foundation-model-availability/") {
                Some("amazon.nova-micro-v1%3A0") => (
                    200,
                    "",
                    availability_body("AVAILABLE", "AUTHORIZED", "AVAILABLE"),
                ),
                Some("anthropic.claude-haiku-4-5-20251001-v1%3A0") => (
                    200,
                    "",
                    availability_body("NOT_AVAILABLE", "AUTHORIZED", "AVAILABLE"),
                ),
                _ => (
                    400,
                    "ValidationException",
                    r#"{"message":"The provided model identifier is invalid."}"#.into(),
                ),
            }
        })
        .await;
        let models = BedrockAdapter::new(&config(base))
            .unwrap()
            .list_models()
            .await
            .unwrap();
        assert_eq!(
            entitlements(&models),
            [
                ("acme.unknown-v1", None),
                ("amazon.nova-micro-v1:0", Some(Entitlement::Entitled)),
                (
                    "anthropic.claude-haiku-4-5-20251001-v1:0",
                    Some(Entitlement::AgreementRequired)
                ),
                // A profile carries its foundation model's answer; one that
                // routes to several models carries none.
                (
                    "us.anthropic.claude-haiku-4-5-20251001-v1:0",
                    Some(Entitlement::AgreementRequired)
                ),
                ("us.mixed", None),
            ]
        );
        let mut checked = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("/foundation-model-availability/"))
            .cloned()
            .collect::<Vec<_>>();
        checked.sort();
        // Each foundation model is checked once, profiles through theirs.
        assert_eq!(
            checked,
            [
                "GET /foundation-model-availability/acme.unknown-v1 HTTP/1.1",
                "GET /foundation-model-availability/amazon.nova-micro-v1%3A0 HTTP/1.1",
                "GET /foundation-model-availability/anthropic.claude-haiku-4-5-20251001-v1%3A0 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn a_role_that_may_not_check_availability_leaves_models_unchecked() {
        let (base, seen) = control_plane(|path| {
            listing_route(path).unwrap_or((
                403,
                "AccessDeniedException",
                r#"{"message":"not authorized to perform: bedrock:GetFoundationModelAvailability"}"#
                    .into(),
            ))
        })
        .await;
        let models = BedrockAdapter::new(&config(base))
            .unwrap()
            .list_models()
            .await
            .unwrap();
        assert_eq!(models.len(), 5);
        assert!(models.iter().all(|model| model.entitlement.is_none()));
        // The first refusal ends the checks.
        let checks = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("/foundation-model-availability/"))
            .count();
        assert_eq!(checks, 1);
    }

    #[tokio::test]
    async fn the_accounts_own_resources_are_listed_by_arn() {
        const ACCOUNT: &str = "arn:aws:bedrock:us-east-1:123456789012";
        let (base, seen) = control_plane(|path| {
            let foundation = |model: &str| format!("arn:aws:bedrock:us-east-1::foundation-model/{model}");
            let page = |items: &str, entries: Value| json!({ items: entries }).to_string();
            if path.starts_with("/inference-profiles") && path.contains("type=APPLICATION") {
                let body = if path.contains("nextToken") {
                    page("inferenceProfileSummaries", json!([
                        {"inferenceProfileArn": format!("{ACCOUNT}:application-inference-profile/mixed"),
                         "type": "APPLICATION", "models": [
                            {"modelArn": foundation("a")}, {"modelArn": foundation("b")}]}
                    ]))
                } else {
                    let mut page: Value = serde_json::from_str(&page("inferenceProfileSummaries", json!([
                        {"inferenceProfileArn": format!("{ACCOUNT}:application-inference-profile/team-a"),
                         "inferenceProfileName": "team-a", "type": "APPLICATION",
                         "models": [{"modelArn": foundation("amazon.nova-micro-v1:0")}]},
                        // The filter is the request's; an entry of another
                        // type is not the account's.
                        {"inferenceProfileArn": "arn:aws:bedrock:us-east-1::inference-profile/us.x",
                         "type": "SYSTEM_DEFINED"}
                    ]))).unwrap();
                    page["nextToken"] = json!("p2");
                    page.to_string()
                };
                return (200, "", body);
            }
            if path.starts_with("/provisioned-model-throughputs") {
                return (200, "", page("provisionedModelSummaries", json!([
                    {"provisionedModelArn": format!("{ACCOUNT}:provisioned-model/pt1"),
                     "provisionedModelName": "pt1", "status": "InService",
                     "foundationModelArn": foundation("amazon.nova-micro-v1:0")},
                    {"provisionedModelArn": format!("{ACCOUNT}:provisioned-model/pt2"),
                     "status": "Creating"}
                ])));
            }
            if path.starts_with("/imported-models") {
                // The role may not list imported models: none are listed.
                return (
                    403,
                    "AccessDeniedException",
                    r#"{"message":"not authorized to perform: bedrock:ListImportedModels"}"#.into(),
                );
            }
            if path.starts_with("/marketplace-model/endpoints") {
                return (200, "", page("marketplaceModelEndpoints", json!([
                    {"endpointArn": format!("{ACCOUNT}:marketplace/endpoint/ep1"),
                     "modelSourceIdentifier": "acme-model", "status": "REGISTERED"},
                    {"endpointArn": format!("{ACCOUNT}:marketplace/endpoint/ep2"),
                     "status": "INCOMPATIBLE_ENDPOINT"}
                ])));
            }
            if path.starts_with("/model-customization/custom-model-deployments") {
                return (200, "", page("modelDeploymentSummaries", json!([
                    {"customModelDeploymentArn": format!("{ACCOUNT}:custom-model-deployment/cmd1"),
                     "customModelDeploymentName": "support-tuned", "status": "Active",
                     "modelArn": format!("{ACCOUNT}:custom-model/amazon.nova-micro-v1:0:128k/abc")},
                    // Not yet callable.
                    {"customModelDeploymentArn": format!("{ACCOUNT}:custom-model-deployment/cmd2"),
                     "customModelDeploymentName": "starting", "status": "Creating"}
                ])));
            }
            if let Some(response) = listing_route(path) {
                return response;
            }
            match path.strip_prefix("/foundation-model-availability/") {
                Some("amazon.nova-micro-v1%3A0") => (
                    200,
                    "",
                    availability_body("AVAILABLE", "NOT_AUTHORIZED", "AVAILABLE"),
                ),
                _ => (
                    400,
                    "ValidationException",
                    r#"{"message":"The provided model identifier is invalid."}"#.into(),
                ),
            }
        })
        .await;
        let models = BedrockAdapter::new(&config(base))
            .unwrap()
            .list_models()
            .await
            .unwrap();
        let own: Vec<(&str, Option<&str>, Option<Entitlement>)> = models
            .iter()
            .filter(|model| model.model.starts_with(ACCOUNT))
            .map(|model| {
                (
                    &model.model[ACCOUNT.len()..],
                    model.display_name.as_deref(),
                    model.entitlement,
                )
            })
            .collect();
        assert_eq!(
            own,
            [
                // Profiles and throughput carry their foundation model's
                // availability; a profile routing to several models none.
                (":application-inference-profile/mixed", None, None),
                (
                    ":application-inference-profile/team-a",
                    Some("team-a"),
                    Some(Entitlement::PermissionMissing)
                ),
                (":custom-model-deployment/cmd1", Some("support-tuned"), None),
                (":marketplace/endpoint/ep1", Some("acme-model"), None),
                (
                    ":provisioned-model/pt1",
                    Some("pt1"),
                    Some(Entitlement::PermissionMissing)
                ),
            ]
        );
        assert!(
            models
                .iter()
                .filter(|model| model.model.starts_with(ACCOUNT))
                .all(|model| model.availability == ModelAvailability::Listed)
        );
        let lines = seen.lock().unwrap().clone();
        for request in [
            "GET /inference-profiles?maxResults=1000&type=APPLICATION HTTP/1.1",
            "GET /inference-profiles?maxResults=1000&nextToken=p2&type=APPLICATION HTTP/1.1",
            "GET /provisioned-model-throughputs?maxResults=1000&statusEquals=InService HTTP/1.1",
            "GET /imported-models?maxResults=1000 HTTP/1.1",
            "GET /marketplace-model/endpoints?maxResults=1000 HTTP/1.1",
            "GET /model-customization/custom-model-deployments?maxResults=1000&statusEquals=Active HTTP/1.1",
        ] {
            assert!(
                lines.iter().any(|line| line == request),
                "{request}: {lines:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_own_resource_list_failing_otherwise_fails_the_listing() {
        let (base, _seen) = control_plane(|path| {
            if path.starts_with("/provisioned-model-throughputs") {
                return (
                    500,
                    "InternalServerException",
                    r#"{"message":"boom"}"#.into(),
                );
            }
            listing_route(path).unwrap_or((404, "", "{}".into()))
        })
        .await;
        assert!(
            BedrockAdapter::new(&config(base))
                .unwrap()
                .list_models()
                .await
                .is_err()
        );
    }

    #[test]
    fn availability_is_read_in_the_order_a_tenant_fixes_it() {
        let read = |agreement, authorization, entitlement| {
            entitlement_of(
                &serde_json::from_str(&availability_body(agreement, authorization, entitlement))
                    .unwrap(),
            )
        };
        use Entitlement::*;
        assert_eq!(read("AVAILABLE", "AUTHORIZED", "AVAILABLE"), Some(Entitled));
        assert_eq!(
            read("NOT_AVAILABLE", "NOT_AUTHORIZED", "NOT_AVAILABLE"),
            Some(PermissionMissing)
        );
        assert_eq!(
            read("NOT_AVAILABLE", "AUTHORIZED", "NOT_AVAILABLE"),
            Some(NotEntitled)
        );
        for agreement in ["NOT_AVAILABLE", "PENDING", "ERROR"] {
            assert_eq!(
                read(agreement, "AUTHORIZED", "AVAILABLE"),
                Some(AgreementRequired)
            );
        }
        // Values this build does not know, and a region that does not serve
        // the model, say nothing.
        assert_eq!(read("SOMETHING_NEW", "AUTHORIZED", "AVAILABLE"), None);
        assert_eq!(read("AVAILABLE", "MAYBE", "AVAILABLE"), None);
        let mut region = json!({
            "agreementAvailability": {"status": "AVAILABLE"},
            "authorizationStatus": "AUTHORIZED",
            "entitlementAvailability": "AVAILABLE",
            "regionAvailability": "NOT_AVAILABLE"
        });
        assert_eq!(entitlement_of(&region), None);
        // ... even when an earlier field is in the way.
        region["agreementAvailability"]["status"] = json!("NOT_AVAILABLE");
        region["authorizationStatus"] = json!("NOT_AUTHORIZED");
        assert_eq!(entitlement_of(&region), None);
        region.as_object_mut().unwrap().remove("regionAvailability");
        assert_eq!(entitlement_of(&region), None);
        // A value this build does not know after the first blocking one.
        assert_eq!(read("NOT_AVAILABLE", "NOT_AUTHORIZED", "MAYBE"), None);
        assert_eq!(read("SOMETHING_NEW", "NOT_AUTHORIZED", "AVAILABLE"), None);
    }

    #[tokio::test]
    async fn verifying_a_credential_checks_no_availability() {
        let (base, seen) = control_plane(|path| {
            listing_route(path).unwrap_or((500, "InternalServerException", "{}".into()))
        })
        .await;
        let models = BedrockAdapter::new(&config(base))
            .unwrap()
            .check_access()
            .await
            .unwrap();
        assert_eq!(models.len(), 5);
        assert!(models.iter().all(|model| model.entitlement.is_none()));
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("/foundation-model-availability/"))
        );
    }

    #[tokio::test]
    async fn a_probe_is_a_one_token_converse_to_the_model() {
        let (base, seen) = control_plane(|path| match path {
            "/model/xai.grok-4.7/converse" => (
                403,
                "AccessDeniedException",
                r#"{"message":"xai.grok-4.7 is not available for this account."}"#.into(),
            ),
            _ => (
                200,
                "",
                r#"{"output":{"message":{"role":"assistant","content":[{"text":"H"}]}},"stopReason":"max_tokens","usage":{"inputTokens":8,"outputTokens":1,"totalTokens":9}}"#.into(),
            ),
        })
        .await;
        let adapter = BedrockAdapter::new(&config(base)).unwrap();
        adapter
            .probe_model("us.amazon.nova-micro-v1:0")
            .await
            .unwrap();
        let refused = adapter.probe_model("xai.grok-4.7").await.unwrap_err();
        assert_eq!(refused.category(), ProviderErrorCategory::Authorization);
        assert_eq!(refused.http_status(), Some(403));
        let lines = seen.lock().unwrap().clone();
        let body = r#"BODY {"inferenceConfig":{"maxTokens":1},"messages":[{"content":[{"text":"hi"}],"role":"user"}]}"#;
        assert_eq!(
            lines,
            [
                "POST /model/us.amazon.nova-micro-v1%3A0/converse HTTP/1.1",
                body,
                "POST /model/xai.grok-4.7/converse HTTP/1.1",
                body,
            ]
        );
    }

    #[test]
    fn a_rejected_credential_is_the_accounts_not_the_models() {
        for raw in [
            "UnrecognizedClientException",
            "ExpiredTokenException",
            "InvalidSignatureException",
        ] {
            let error = status_error(
                StatusCode::FORBIDDEN,
                &Value::Null,
                Some(raw.into()),
                None,
                None,
            );
            assert_eq!(
                error.category(),
                ProviderErrorCategory::Authentication,
                "{raw}"
            );
            assert_eq!(
                crate::domain::model_evidence::Verdict::of_error(&error),
                None,
                "{raw}"
            );
        }
        let denied = status_error(
            StatusCode::FORBIDDEN,
            &Value::Null,
            Some("AccessDeniedException".into()),
            None,
            None,
        );
        assert_eq!(denied.category(), ProviderErrorCategory::Authorization);
    }

    #[test]
    fn tools_map_to_tool_config_tool_use_and_tool_results() {
        use crate::provider::tools::tool_conversation;
        let body = adapter().request_body(&tool_conversation()).unwrap();
        assert_eq!(
            body["toolConfig"],
            json!({"tools": [{"toolSpec": {"name": "get_weather", "description": "Current weather",
                "inputSchema": {"json": {"type": "object", "properties": {"city": {"type": "string"}}}}}}]})
        );
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"text": "weather in Paris and Rome?"}]},
                {"role": "assistant", "content": [
                    {"text": "Checking."},
                    {"toolUse": {"toolUseId": "toolu_01", "name": "get_weather", "input": {"city": "Paris"}}},
                    {"toolUse": {"toolUseId": "toolu_02", "name": "get_weather", "input": {"city": "Rome"}}}
                ]},
                {"role": "user", "content": [
                    {"toolResult": {"toolUseId": "toolu_01", "content": [{"text": "18C"}]}},
                    {"toolResult": {"toolUseId": "toolu_02", "content": [{"text": "24C"}]}}
                ]}
            ])
        );
        // An empty result: Converse refuses an empty text block, so it
        // goes as an empty JSON object, in a batch line as in a call (the
        // batch input line is this body).
        let mut empty = tool_conversation();
        let last = empty.messages.len() - 1;
        empty.messages[last].content.clear();
        let body = adapter().request_body(&empty).unwrap();
        assert_eq!(
            body["messages"][2]["content"],
            json!([
                {"toolResult": {"toolUseId": "toolu_01", "content": [{"text": "18C"}]}},
                {"toolResult": {"toolUseId": "toolu_02", "content": [{"json": {}}]}}
            ])
        );
        use crate::provider::batch::{BatchProvider, BatchStaging, BatchTag};
        let batching = adapter();
        let staged = batch::StagedBedrock::new(
            &batching,
            &BatchStaging {
                bucket: "bucket".into(),
                prefix: String::new(),
                region: Some("us-east-1".into()),
                role_arn: Some("arn:aws:iam::123456789012:role/batch".into()),
            },
        )
        .unwrap();
        let tag = BatchTag {
            batch_id: uuid::Uuid::nil(),
            job_id: uuid::Uuid::nil(),
        };
        let line = staged
            .input_line(
                tag,
                1,
                crate::domain::batch::BatchEndpoint::ChatCompletions,
                "model",
                &crate::domain::batch::LineRequest::Chat(empty),
            )
            .unwrap();
        let line: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(line["modelInput"]["messages"], body["messages"]);
        let choice = |tool_choice| {
            adapter()
                .request_body(&ChatRequest {
                    tool_choice: Some(tool_choice),
                    ..tool_conversation()
                })
                .map(|body| body["toolConfig"]["toolChoice"].clone())
        };
        assert_eq!(choice(ToolChoice::Auto).unwrap(), json!({"auto": {}}));
        assert_eq!(choice(ToolChoice::Required).unwrap(), json!({"any": {}}));
        assert_eq!(
            choice(ToolChoice::Function("get_weather".into())).unwrap(),
            json!({"tool": {"name": "get_weather"}})
        );
        // Converse cannot turn tools off or ask for one call at a time.
        assert_eq!(
            choice(ToolChoice::None).unwrap_err().category(),
            ProviderErrorCategory::InvalidRequest
        );
        let serial = ChatRequest {
            parallel_tool_calls: Some(false),
            ..tool_conversation()
        };
        assert!(validate_chat_request(&serial).is_err());
        assert!(validate_batch_line(&serial).is_err());
        assert!(validate_batch_line(&tool_conversation()).is_ok());
    }

    #[test]
    fn tool_use_blocks_are_tool_calls() {
        // Converse's documented response to a tool request.
        let value = json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": "Checking."},
                {"toolUse": {"toolUseId": "tooluse_kZJMlvQmRJ6eAyJE5GIl7Q", "name": "top_song",
                             "input": {"sign": "WZPZ"}}}
            ]}},
            "stopReason": "tool_use",
            "usage": {"inputTokens": 30, "outputTokens": 20, "totalTokens": 50}
        });
        let response = parse_response(value, "m".into(), None).unwrap();
        let choice = &response.choices()[0];
        assert_eq!(choice.message.content, "Checking.");
        assert_eq!(choice.finish_reason.as_ref().unwrap().0, "tool_calls");
        assert_eq!(
            choice.tool_calls,
            vec![ToolCall {
                id: Some("tooluse_kZJMlvQmRJ6eAyJE5GIl7Q".into()),
                name: Some("top_song".into()),
                arguments: r#"{"sign":"WZPZ"}"#.into(),
            }]
        );
    }

    #[tokio::test]
    async fn streamed_tool_use_becomes_tool_call_deltas() {
        let mut body = Vec::new();
        body.extend(event_frame("messageStart", &json!({"role": "assistant"})));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 0, "delta": {"text": "Checking"}}),
        ));
        body.extend(event_frame(
            "contentBlockStop",
            &json!({"contentBlockIndex": 0}),
        ));
        body.extend(event_frame(
            "contentBlockStart",
            &json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "tooluse_1", "name": "top_song"}}}),
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"sign\": \"WZ"}}}),
        ));
        body.extend(event_frame(
            "contentBlockDelta",
            &json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "PZ\"}"}}}),
        ));
        body.extend(event_frame(
            "contentBlockStop",
            &json!({"contentBlockIndex": 1}),
        ));
        body.extend(event_frame(
            "contentBlockStart",
            &json!({"contentBlockIndex": 2, "start": {"toolUse": {"toolUseId": "tooluse_2", "name": "now"}}}),
        ));
        body.extend(event_frame(
            "contentBlockStop",
            &json!({"contentBlockIndex": 2}),
        ));
        body.extend(event_frame(
            "messageStop",
            &json!({"stopReason": "tool_use"}),
        ));
        body.extend(event_frame(
            "metadata",
            &json!({"usage": {"inputTokens": 1, "outputTokens": 2}}),
        ));
        let events = bedrock_converse_stream(
            futures::stream::iter(vec![Ok(bytes::Bytes::from(body))]),
            "m".into(),
            None,
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
        assert_eq!(deltas[0].id.as_deref(), Some("tooluse_1"));
        assert_eq!(deltas[0].name.as_deref(), Some("top_song"));
        let arguments = |index| {
            deltas
                .iter()
                .filter(|delta| delta.tool_call_index == index)
                .map(|delta| delta.arguments_delta.as_str())
                .collect::<String>()
        };
        assert_eq!(arguments(0), r#"{"sign": "WZPZ"}"#);
        assert_eq!(arguments(1), "{}");
        assert!(events.iter().any(|event| matches!(
            event,
            ChatStreamEvent::ChoiceFinished(finished) if finished.finish_reason().0 == "tool_calls"
        )));
    }
}
