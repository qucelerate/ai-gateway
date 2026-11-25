//! Provider-independent inference contracts.
//!
//! Request and response content is intentionally redacted from `Debug`. These
//! values may be logged by application code, but this module makes accidental
//! debug logging of prompts, completions, tool arguments, and vectors safe.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashSet, fmt, time::Duration};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CorrelationId(pub Uuid);

impl CorrelationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CorrelationId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct ProviderAttemptIdempotencyKey(String);

impl ProviderAttemptIdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ContextError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ContextError::EmptyIdempotencyKey);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderAttemptIdempotencyKey {
    type Error = ContextError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct DeploymentModelReference {
    deployment: String,
    model: String,
}

impl DeploymentModelReference {
    pub fn new(
        deployment: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let reference = Self {
            deployment: deployment.into(),
            model: model.into(),
        };
        if reference.deployment.trim().is_empty() || reference.model.trim().is_empty() {
            return Err(ContextError::EmptyDeploymentReference);
        }
        Ok(reference)
    }
    pub fn deployment(&self) -> &str {
        &self.deployment
    }
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl<'de> Deserialize<'de> for DeploymentModelReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            deployment: String,
            model: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.deployment, wire.model).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InferenceContext {
    pub correlation_id: CorrelationId,
    pub provider_attempt_idempotency_key: ProviderAttemptIdempotencyKey,
    pub deployment: DeploymentModelReference,
    /// Trusted application routing identity. Public callers never supply this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_instance_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_family: Option<String>,
}

impl InferenceContext {
    pub fn new(
        correlation_id: CorrelationId,
        provider_attempt_idempotency_key: ProviderAttemptIdempotencyKey,
        deployment: DeploymentModelReference,
    ) -> Self {
        Self {
            correlation_id,
            provider_attempt_idempotency_key,
            deployment,
            adapter_instance_key: None,
            provider_family: None,
        }
    }

    pub fn with_adapter_instance_key(mut self, key: impl Into<String>) -> Self {
        self.adapter_instance_key = Some(key.into());
        self
    }

    pub fn with_provider_family(mut self, family: impl Into<String>) -> Self {
        self.provider_family = Some(family.into());
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ContextError {
    EmptyIdempotencyKey,
    EmptyDeploymentReference,
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdempotencyKey => f.write_str("empty provider idempotency key"),
            Self::EmptyDeploymentReference => f.write_str("empty deployment/model reference"),
        }
    }
}
impl std::error::Error for ContextError {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ChatRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The calls an assistant message made (D-032): each has an id and a
    /// name, and its arguments are a JSON object as text. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl ChatMessage {
    /// A message of `role` with text `content` and nothing else.
    pub fn text(role: ChatRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }
}

impl fmt::Debug for ChatMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatMessage")
            .field("role", &self.role)
            .field("content", &"REDACTED")
            .field("name", &self.name)
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_calls", &self.tool_calls)
            .finish()
    }
}

/// A function the model may call (D-032), as the client defined it.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The arguments' JSON Schema: an object schema.
    pub parameters: Value,
}

impl ToolDefinition {
    /// The bytes the definition adds to a request: its name, description
    /// and parameters as compact JSON.
    pub fn bytes(&self) -> usize {
        self.name
            .len()
            .saturating_add(self.description.as_ref().map_or(0, String::len))
            .saturating_add(serde_json::to_string(&self.parameters).map_or(0, |text| text.len()))
    }
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field(
                "description",
                &self.description.as_ref().map(|_| "REDACTED"),
            )
            .field("parameters", &"REDACTED")
            .finish()
    }
}

/// Whether and which tools the model must call (D-032).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides (the default when tools are defined).
    Auto,
    /// The model calls no tool.
    None,
    /// The model calls at least one tool.
    Required,
    /// The model calls this tool.
    Function(String),
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Functions the model may call (D-032).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// `Some(false)` asks for at most one tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl ChatRequest {
    /// Whether the request defines tools or carries tool calls or results:
    /// only a deployment that takes tools can serve it (D-032).
    pub fn uses_tools(&self) -> bool {
        !self.tools.is_empty()
            || self.tool_choice.is_some()
            || self.parallel_tool_calls.is_some()
            || self
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Tool || !message.tool_calls.is_empty())
    }
}

impl fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatRequest")
            .field(
                "messages",
                &format_args!("[{} redacted]", self.messages.len()),
            )
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("tools", &self.tools)
            .field("tool_choice", &self.tool_choice)
            .field("parallel_tool_calls", &self.parallel_tool_calls)
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
}

impl fmt::Debug for ToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCall")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("arguments", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCallDelta {
    pub choice_index: u32,
    pub tool_call_index: u32,
    pub id: Option<String>,
    pub name: Option<String>,
    /// Opaque provider argument fragment; it must be concatenated in order.
    pub arguments_delta: String,
}

impl fmt::Debug for ToolCallDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallDelta")
            .field("choice_index", &self.choice_index)
            .field("tool_call_index", &self.tool_call_index)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("arguments_delta", &"REDACTED")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SafetyAnnotation {
    pub choice_index: u32,
    pub category: String,
    pub blocked: bool,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct Annotation {
    pub kind: String,
    pub value: String,
}

impl fmt::Debug for Annotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Annotation")
            .field("kind", &self.kind)
            .field("value", &"REDACTED")
            .finish()
    }
}

/// Provider finish reasons are opaque and forward-compatible.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct FinishReason(pub String);

impl FinishReason {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.trim().is_empty() {
            return Err(serde::de::Error::custom("finish reason cannot be empty"));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UsageQuality {
    Exact,
    Estimated,
    Unknown,
}

pub type CounterQuality = UsageQuality;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UsageCompleteness {
    Complete,
    Partial,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UsageFinality {
    Interim,
    Final,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TokenCounter {
    /// `None` is the only representation of an unknown count; unknown is
    /// never silently represented as zero.
    value: Option<u64>,
    quality: UsageQuality,
}

impl TokenCounter {
    pub fn exact(value: u64) -> Self {
        Self {
            value: Some(value),
            quality: UsageQuality::Exact,
        }
    }
    pub fn estimated(value: u64) -> Self {
        Self {
            value: Some(value),
            quality: UsageQuality::Estimated,
        }
    }
    pub fn unknown() -> Self {
        Self {
            value: None,
            quality: UsageQuality::Unknown,
        }
    }
    pub fn value(&self) -> Option<u64> {
        self.value
    }
    pub fn quality(&self) -> UsageQuality {
        self.quality
    }
}

impl<'de> Deserialize<'de> for TokenCounter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            value: Option<u64>,
            quality: UsageQuality,
        }
        let wire = Wire::deserialize(deserializer)?;
        match (wire.value, wire.quality) {
            (None, UsageQuality::Unknown) => Ok(Self::unknown()),
            (Some(value), UsageQuality::Exact) => Ok(Self::exact(value)),
            (Some(value), UsageQuality::Estimated) => Ok(Self::estimated(value)),
            _ => Err(serde::de::Error::custom(
                "token counter value and quality disagree",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Usage {
    input_tokens: Option<TokenCounter>,
    output_tokens: Option<TokenCounter>,
    cached_input_tokens: Option<TokenCounter>,
    reasoning_tokens: Option<TokenCounter>,
    other_tokens: Option<TokenCounter>,
    quality: UsageQuality,
    completeness: UsageCompleteness,
}

impl<'de> Deserialize<'de> for Usage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            input_tokens: Option<TokenCounter>,
            output_tokens: Option<TokenCounter>,
            cached_input_tokens: Option<TokenCounter>,
            reasoning_tokens: Option<TokenCounter>,
            other_tokens: Option<TokenCounter>,
            quality: UsageQuality,
            completeness: UsageCompleteness,
        }
        let wire = Wire::deserialize(deserializer)?;
        let usage = Self::from_counters(
            wire.input_tokens,
            wire.output_tokens,
            wire.cached_input_tokens,
            wire.reasoning_tokens,
            wire.other_tokens,
            wire.completeness,
        );
        if usage.quality != wire.quality || usage.completeness != wire.completeness {
            return Err(serde::de::Error::custom(
                "usage quality or completeness is inconsistent",
            ));
        }
        if usage.completeness == UsageCompleteness::Complete
            && [usage.input_tokens, usage.output_tokens]
                .iter()
                .any(Option::is_none)
        {
            return Err(serde::de::Error::custom(
                "complete usage requires input and output counters",
            ));
        }
        Ok(usage)
    }
}

impl Usage {
    fn from_counters(
        input_tokens: Option<TokenCounter>,
        output_tokens: Option<TokenCounter>,
        cached_input_tokens: Option<TokenCounter>,
        reasoning_tokens: Option<TokenCounter>,
        other_tokens: Option<TokenCounter>,
        completeness: UsageCompleteness,
    ) -> Self {
        let counters = [
            input_tokens,
            output_tokens,
            cached_input_tokens,
            reasoning_tokens,
            other_tokens,
        ];
        let quality = counters.iter().flatten().map(TokenCounter::quality).fold(
            if counters.iter().all(Option::is_none) {
                UsageQuality::Unknown
            } else {
                UsageQuality::Exact
            },
            |quality, next| match (quality, next) {
                (UsageQuality::Unknown, _) | (_, UsageQuality::Unknown) => UsageQuality::Unknown,
                (UsageQuality::Estimated, _) | (_, UsageQuality::Estimated) => {
                    UsageQuality::Estimated
                }
                _ => UsageQuality::Exact,
            },
        );
        Self {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            reasoning_tokens,
            other_tokens,
            quality,
            completeness,
        }
    }

    pub fn unavailable() -> Self {
        Self::from_counters(None, None, None, None, None, UsageCompleteness::Unavailable)
    }
    pub fn partial(
        input_tokens: Option<TokenCounter>,
        output_tokens: Option<TokenCounter>,
        cached_input_tokens: Option<TokenCounter>,
        reasoning_tokens: Option<TokenCounter>,
        other_tokens: Option<TokenCounter>,
    ) -> Self {
        Self::from_counters(
            input_tokens,
            output_tokens,
            cached_input_tokens,
            reasoning_tokens,
            other_tokens,
            UsageCompleteness::Partial,
        )
    }
    /// Usage of an embeddings call that read `input_tokens`. Embeddings
    /// generate no output and cache no input, and have no reasoning or other
    /// tokens, so each of those is an exact zero: a price that charges one
    /// (a cached-input price copied from the input price) charges nothing
    /// for it, rather than finding the usage unknown and refusing to settle.
    pub fn embedding(input_tokens: TokenCounter) -> Result<Self, UsageError> {
        let zero = TokenCounter::exact(0);
        Self::complete(input_tokens, zero, Some(zero), Some(zero), Some(zero))
    }
    pub fn complete(
        input_tokens: TokenCounter,
        output_tokens: TokenCounter,
        cached_input_tokens: Option<TokenCounter>,
        reasoning_tokens: Option<TokenCounter>,
        other_tokens: Option<TokenCounter>,
    ) -> Result<Self, UsageError> {
        let usage = Self::from_counters(
            Some(input_tokens),
            Some(output_tokens),
            cached_input_tokens,
            reasoning_tokens,
            other_tokens,
            UsageCompleteness::Complete,
        );
        if usage.quality == UsageQuality::Unknown {
            Err(UsageError::CompleteUsageUnknown)
        } else {
            Ok(usage)
        }
    }
    pub fn input_tokens(&self) -> Option<TokenCounter> {
        self.input_tokens
    }
    pub fn output_tokens(&self) -> Option<TokenCounter> {
        self.output_tokens
    }
    pub fn cached_input_tokens(&self) -> Option<TokenCounter> {
        self.cached_input_tokens
    }
    pub fn reasoning_tokens(&self) -> Option<TokenCounter> {
        self.reasoning_tokens
    }
    pub fn other_tokens(&self) -> Option<TokenCounter> {
        self.other_tokens
    }
    pub fn quality(&self) -> UsageQuality {
        self.quality
    }
    pub fn completeness(&self) -> UsageCompleteness {
        self.completeness
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageError {
    CompleteUsageUnknown,
    InvalidSnapshot,
    InvalidErrorUsage,
}
impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid usage contract")
    }
}
impl std::error::Error for UsageError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct UsageSnapshot {
    usage: Usage,
    finality: UsageFinality,
}

impl<'de> Deserialize<'de> for UsageSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            usage: Usage,
            finality: UsageFinality,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.usage, wire.finality).map_err(serde::de::Error::custom)
    }
}

impl UsageSnapshot {
    pub fn interim(usage: Usage) -> Self {
        Self {
            usage,
            finality: UsageFinality::Interim,
        }
    }
    pub fn final_(usage: Usage) -> Result<Self, UsageError> {
        Self::new(usage, UsageFinality::Final)
    }
    pub fn partial(usage: Usage) -> Result<Self, UsageError> {
        Self::new(usage, UsageFinality::Partial)
    }
    pub fn new(usage: Usage, finality: UsageFinality) -> Result<Self, UsageError> {
        match finality {
            UsageFinality::Final if usage.completeness != UsageCompleteness::Complete => {
                Err(UsageError::InvalidSnapshot)
            }
            UsageFinality::Partial if usage.completeness != UsageCompleteness::Partial => {
                Err(UsageError::InvalidSnapshot)
            }
            _ => Ok(Self { usage, finality }),
        }
    }
    pub fn usage(&self) -> Usage {
        self.usage
    }
    pub fn finality(&self) -> UsageFinality {
        self.finality
    }
    pub fn cumulative(&self) -> bool {
        true
    }
}

/// Opaque structured metadata selected by a trusted adapter mapping. The
/// underlying JSON object preserves forward-compatible nested values, while
/// remaining inaccessible to callers and never participating in audit details.
#[derive(Clone, Default, Eq, PartialEq, Serialize)]
pub struct SafeMetadata(Map<String, Value>);

impl fmt::Debug for SafeMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|key| (key, &"REDACTED")))
            .finish()
    }
}

impl SafeMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adapter mapping boundary. No public arbitrary JSON/string constructor is
    /// exposed; only in-crate provider mappings can normalize selected values.
    #[allow(dead_code)]
    pub(crate) fn from_trusted_mapping(
        metadata: Map<String, Value>,
    ) -> Result<Self, MetadataError> {
        validate_metadata_object(&metadata)?;
        Ok(Self(metadata))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    ForbiddenField,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("provider metadata contains a forbidden field")
    }
}
impl std::error::Error for MetadataError {}

#[allow(dead_code)]
fn validate_metadata_object(metadata: &Map<String, Value>) -> Result<(), MetadataError> {
    for (key, value) in metadata {
        let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
        if [
            "authorization",
            "header",
            "headers",
            "body",
            "credential",
            "secret",
            "password",
            "bearer",
            "token",
            "prompt",
            "completion",
            "tool",
            "apikey",
        ]
        .iter()
        .any(|word| normalized.contains(word))
        {
            return Err(MetadataError::ForbiddenField);
        }
        validate_metadata_value(value)?;
    }
    Ok(())
}

#[allow(dead_code)]
fn validate_metadata_value(value: &Value) -> Result<(), MetadataError> {
    match value {
        Value::Object(object) => validate_metadata_object(object),
        Value::Array(values) => values.iter().try_for_each(validate_metadata_value),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<FinishReason>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub safety: Vec<SafetyAnnotation>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

impl fmt::Debug for ChatChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatChoice")
            .field("index", &self.index)
            .field("message", &self.message)
            .field("finish_reason", &self.finish_reason)
            .field("tool_calls", &self.tool_calls)
            .field("refusal", &self.refusal.as_ref().map(|_| "REDACTED"))
            .field("safety", &self.safety)
            .field("annotations", &self.annotations)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ChatResponse {
    provider_request_id: Option<String>,
    model: String,
    choices: Vec<ChatChoice>,
    usage: Option<UsageSnapshot>,
    metadata: SafeMetadata,
}

impl ChatResponse {
    pub fn new(
        provider_request_id: Option<String>,
        model: String,
        choices: Vec<ChatChoice>,
        usage: Option<UsageSnapshot>,
        metadata: SafeMetadata,
    ) -> Result<Self, ResponseError> {
        validate_ordered_indices(choices.iter().map(|choice| choice.index))?;
        if usage
            .as_ref()
            .is_some_and(|snapshot| snapshot.finality() != UsageFinality::Final)
        {
            return Err(ResponseError::UsageMustBeFinal);
        }
        if model.trim().is_empty() {
            return Err(ResponseError::EmptyModel);
        }
        Ok(Self {
            provider_request_id,
            model,
            choices,
            usage,
            metadata,
        })
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn choices(&self) -> &[ChatChoice] {
        &self.choices
    }
    pub fn usage(&self) -> Option<UsageSnapshot> {
        self.usage
    }
    pub fn metadata(&self) -> &SafeMetadata {
        &self.metadata
    }
}

impl<'de> Deserialize<'de> for ChatResponse {
    // Deliberately reject the public serde boundary. Adapter mappings must
    // construct responses through validated constructors and trusted metadata.
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "normalized responses must be constructed by a trusted provider adapter",
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseError {
    EmptyModel,
    UsageMustBeFinal,
    InvalidIndices,
    EmptyVector,
    NonFiniteVector,
}
impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid inference response: {:?}", self)
    }
}
impl std::error::Error for ResponseError {}

fn validate_ordered_indices(indices: impl Iterator<Item = u32>) -> Result<(), ResponseError> {
    for (expected, actual) in indices.enumerate() {
        if actual != expected as u32 {
            return Err(ResponseError::InvalidIndices);
        }
    }
    Ok(())
}

impl fmt::Debug for ChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatResponse")
            .field("provider_request_id", &self.provider_request_id)
            .field("model", &self.model)
            .field("choices", &self.choices)
            .field("usage", &self.usage)
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct StreamChoiceDelta {
    pub choice_index: u32,
    pub content_delta: Option<String>,
    pub refusal_delta: Option<String>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
}

impl fmt::Debug for StreamChoiceDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamChoiceDelta")
            .field("choice_index", &self.choice_index)
            .field(
                "content_delta",
                &self.content_delta.as_ref().map(|_| "REDACTED"),
            )
            .field(
                "refusal_delta",
                &self.refusal_delta.as_ref().map(|_| "REDACTED"),
            )
            .field("annotations", &self.annotations)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamLifecycleError {
    MissingResponseStarted,
    InterruptedBeforeTerminal,
    EventAfterTerminal,
    DuplicateResponseStarted,
    ChoiceEventAfterFinished,
    DuplicateChoiceFinished,
    MissingChoiceFinished,
    StandaloneFinalUsage,
    NonCumulativeUsage,
    EmptyFinishReason,
    InvalidCompletionUsage,
    InvalidErrorUsage,
}
impl fmt::Display for StreamLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid chat stream lifecycle: {:?}", self)
    }
}
impl std::error::Error for StreamLifecycleError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResponseStarted {
    provider_request_id: Option<String>,
    model: String,
    metadata: SafeMetadata,
}

impl ResponseStarted {
    pub fn new(
        provider_request_id: Option<String>,
        model: String,
        metadata: SafeMetadata,
    ) -> Result<Self, ResponseError> {
        if model.trim().is_empty() {
            return Err(ResponseError::EmptyModel);
        }
        Ok(Self {
            provider_request_id,
            model,
            metadata,
        })
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn metadata(&self) -> &SafeMetadata {
        &self.metadata
    }
}

impl<'de> Deserialize<'de> for ResponseStarted {
    // Deliberately reject the public serde boundary for the same reason.
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "stream responses must be constructed by a trusted provider adapter",
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChoiceFinished {
    choice_index: u32,
    finish_reason: FinishReason,
}

impl<'de> Deserialize<'de> for ChoiceFinished {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            choice_index: u32,
            finish_reason: FinishReason,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.choice_index, wire.finish_reason).map_err(serde::de::Error::custom)
    }
}
impl ChoiceFinished {
    pub fn new(
        choice_index: u32,
        finish_reason: FinishReason,
    ) -> Result<Self, StreamLifecycleError> {
        if finish_reason.as_str().trim().is_empty() {
            return Err(StreamLifecycleError::EmptyFinishReason);
        }
        Ok(Self {
            choice_index,
            finish_reason,
        })
    }
    pub fn choice_index(&self) -> u32 {
        self.choice_index
    }
    pub fn finish_reason(&self) -> &FinishReason {
        &self.finish_reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResponseCompletion {
    provider_request_id: Option<String>,
    model: String,
    usage: Option<UsageSnapshot>,
}

impl<'de> Deserialize<'de> for ResponseCompletion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider_request_id: Option<String>,
            model: String,
            usage: Option<UsageSnapshot>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.provider_request_id, wire.model, wire.usage)
            .map_err(serde::de::Error::custom)
    }
}
impl ResponseCompletion {
    pub fn new(
        provider_request_id: Option<String>,
        model: String,
        usage: Option<UsageSnapshot>,
    ) -> Result<Self, ResponseError> {
        if model.trim().is_empty() {
            return Err(ResponseError::EmptyModel);
        }
        if usage
            .as_ref()
            .is_some_and(|snapshot| snapshot.finality() != UsageFinality::Final)
        {
            return Err(ResponseError::UsageMustBeFinal);
        }
        Ok(Self {
            provider_request_id,
            model,
            usage,
        })
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn usage(&self) -> Option<UsageSnapshot> {
        self.usage
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum ChatStreamEvent {
    ResponseStarted(ResponseStarted),
    ChoiceDelta(StreamChoiceDelta),
    ToolCallDelta(ToolCallDelta),
    ChoiceFinished(ChoiceFinished),
    SafetyAnnotation(SafetyAnnotation),
    Usage(UsageSnapshot),
    /// The only clean terminal event.
    ResponseCompleted(ResponseCompletion),
}

impl<'de> Deserialize<'de> for ChatStreamEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        enum Wire {
            ResponseStarted(ResponseStarted),
            ChoiceDelta(StreamChoiceDelta),
            ToolCallDelta(ToolCallDelta),
            ChoiceFinished(ChoiceFinished),
            SafetyAnnotation(SafetyAnnotation),
            Usage(UsageSnapshot),
            ResponseCompleted(ResponseCompletion),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::ResponseStarted(value) => Self::ResponseStarted(value),
            Wire::ChoiceDelta(value) => Self::ChoiceDelta(value),
            Wire::ToolCallDelta(value) => Self::ToolCallDelta(value),
            Wire::ChoiceFinished(value) => Self::ChoiceFinished(value),
            Wire::SafetyAnnotation(value) => Self::SafetyAnnotation(value),
            Wire::Usage(value) => Self::Usage(value),
            Wire::ResponseCompleted(value) => Self::ResponseCompleted(value),
        })
    }
}

impl ChatStreamEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::ResponseCompleted(_))
    }
}

/// Validate a finite sequence using the same rules as the provider stream
/// wrapper. Normal consumers should use the wrapper rather than collecting.
pub fn validate_chat_stream(
    events: &[Result<ChatStreamEvent, ProviderError>],
    eof: bool,
) -> Result<(), StreamLifecycleError> {
    let mut terminal = match events.first() {
        Some(Ok(ChatStreamEvent::ResponseStarted(_))) => false,
        Some(Err(error)) => {
            if error
                .partial_usage()
                .is_some_and(|usage| usage.completeness() != UsageCompleteness::Partial)
            {
                return Err(StreamLifecycleError::InvalidErrorUsage);
            }
            true
        }
        _ => return Err(StreamLifecycleError::MissingResponseStarted),
    };
    let mut observed = HashSet::new();
    let mut finished = HashSet::new();
    let mut last_usage = None;
    for event in events.iter().skip(1) {
        if terminal {
            return Err(StreamLifecycleError::EventAfterTerminal);
        }
        match event {
            Err(error) => {
                if error
                    .partial_usage()
                    .is_some_and(|usage| usage.completeness() != UsageCompleteness::Partial)
                {
                    return Err(StreamLifecycleError::InvalidErrorUsage);
                }
                terminal = true;
            }
            Ok(ChatStreamEvent::ResponseStarted(_)) => {
                return Err(StreamLifecycleError::DuplicateResponseStarted);
            }
            Ok(ChatStreamEvent::ChoiceDelta(delta)) => {
                observed.insert(delta.choice_index);
                if finished.contains(&delta.choice_index) {
                    return Err(StreamLifecycleError::ChoiceEventAfterFinished);
                }
            }
            Ok(ChatStreamEvent::ToolCallDelta(delta)) => {
                observed.insert(delta.choice_index);
                if finished.contains(&delta.choice_index) {
                    return Err(StreamLifecycleError::ChoiceEventAfterFinished);
                }
            }
            Ok(ChatStreamEvent::ChoiceFinished(finished_choice)) => {
                if !finished.insert(finished_choice.choice_index()) {
                    return Err(StreamLifecycleError::DuplicateChoiceFinished);
                }
            }
            Ok(ChatStreamEvent::Usage(snapshot)) => {
                if snapshot.finality() == UsageFinality::Final {
                    return Err(StreamLifecycleError::StandaloneFinalUsage);
                }
                if last_usage
                    .is_some_and(|previous| !cumulative_not_decreasing(previous, snapshot.usage()))
                {
                    return Err(StreamLifecycleError::NonCumulativeUsage);
                }
                last_usage = Some(snapshot.usage());
            }
            Ok(ChatStreamEvent::ResponseCompleted(completion)) => {
                if completion
                    .usage()
                    .is_some_and(|usage| usage.finality() != UsageFinality::Final)
                {
                    return Err(StreamLifecycleError::InvalidCompletionUsage);
                }
                if !observed.is_subset(&finished) {
                    return Err(StreamLifecycleError::MissingChoiceFinished);
                }
                if completion.usage().is_some_and(|snapshot| {
                    last_usage.is_some_and(|previous| {
                        !cumulative_not_decreasing(previous, snapshot.usage())
                    })
                }) {
                    return Err(StreamLifecycleError::NonCumulativeUsage);
                }
                terminal = true;
            }
            Ok(ChatStreamEvent::SafetyAnnotation(_)) => {}
        }
    }
    if eof && !terminal {
        return Err(StreamLifecycleError::InterruptedBeforeTerminal);
    }
    Ok(())
}

fn cumulative_not_decreasing(previous: Usage, next: Usage) -> bool {
    fn okay(before: Option<TokenCounter>, after: Option<TokenCounter>) -> bool {
        match (
            before.and_then(|counter| counter.value()),
            after.and_then(|counter| counter.value()),
        ) {
            (Some(before), Some(after)) => after >= before,
            (Some(_), None) => false,
            (None, Some(_)) | (None, None) => true,
        }
    }

    okay(previous.input_tokens(), next.input_tokens())
        && okay(previous.output_tokens(), next.output_tokens())
        && okay(previous.cached_input_tokens(), next.cached_input_tokens())
        && okay(previous.reasoning_tokens(), next.reasoning_tokens())
        && okay(previous.other_tokens(), next.other_tokens())
}

#[cfg(test)]
mod stream_validation_tests {
    use super::*;

    #[test]
    fn only_identifier_like_provider_codes_are_error_types() {
        let with = |code: &str| {
            ProviderError::from_parts(
                ProviderErrorCategory::InvalidRequest,
                None,
                None,
                Some(400),
                Some(code.to_owned()),
                None,
                None,
            )
            .unwrap()
        };
        assert_eq!(
            with("ValidationException").error_type(),
            Some("ValidationException")
        );
        assert_eq!(
            with("aws:ThrottlingException").error_type(),
            Some("aws:ThrottlingException")
        );
        assert_eq!(with("the prompt said: hello").error_type(), None);
        assert_eq!(with("").error_type(), None);
        assert_eq!(with(&"x".repeat(65)).error_type(), None);
        assert_eq!(ProviderErrorCategory::RateLimited.as_str(), "rate_limited");
    }

    #[test]
    fn invalid_initial_provider_error_usage_is_rejected() {
        let complete_usage = Usage::complete(
            TokenCounter::exact(1),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        let error = ProviderError {
            category: ProviderErrorCategory::Provider,
            provider: None,
            provider_request_id: None,
            http_status: None,
            provider_code: None,
            retry_after: None,
            partial_usage: Some(complete_usage),
            not_sent: false,
        };
        assert_eq!(
            validate_chat_stream(&[Err(error)], true),
            Err(StreamLifecycleError::InvalidErrorUsage)
        );
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderErrorCategory {
    Authentication,
    Authorization,
    InvalidRequest,
    UnsupportedFeature,
    RateLimited,
    Unavailable,
    Timeout,
    Cancelled,
    Provider,
    Protocol,
    Internal,
}

impl ProviderErrorCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Authorization => "authorization",
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedFeature => "unsupported_feature",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Provider => "provider",
            Self::Protocol => "protocol",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ProviderError {
    category: ProviderErrorCategory,
    provider: Option<String>,
    provider_request_id: Option<String>,
    http_status: Option<u16>,
    provider_code: Option<String>,
    retry_after: Option<Duration>,
    partial_usage: Option<Usage>,
    /// The request provably never reached the provider (the connection was
    /// never established), so trying again cannot run it twice.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    not_sent: bool,
}

impl<'de> Deserialize<'de> for ProviderError {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            category: ProviderErrorCategory,
            provider: Option<String>,
            provider_request_id: Option<String>,
            http_status: Option<u16>,
            provider_code: Option<String>,
            retry_after: Option<Duration>,
            partial_usage: Option<Usage>,
            #[serde(default)]
            not_sent: bool,
        }
        let wire = Wire::deserialize(deserializer)?;
        let error = Self {
            category: wire.category,
            provider: wire.provider,
            provider_request_id: wire.provider_request_id,
            http_status: wire.http_status,
            provider_code: wire.provider_code,
            retry_after: wire.retry_after,
            partial_usage: wire.partial_usage,
            not_sent: wire.not_sent,
        };
        error.validate().map_err(serde::de::Error::custom)?;
        Ok(error)
    }
}

impl ProviderError {
    pub fn new(category: ProviderErrorCategory) -> Self {
        Self {
            category,
            provider: None,
            provider_request_id: None,
            http_status: None,
            provider_code: None,
            retry_after: None,
            partial_usage: None,
            not_sent: false,
        }
    }
    /// The error of a request that never reached the provider: a connection
    /// that could not be opened.
    pub fn not_sent(category: ProviderErrorCategory) -> Self {
        Self {
            not_sent: true,
            ..Self::new(category)
        }
    }
    pub fn was_not_sent(&self) -> bool {
        self.not_sent
    }
    /// The same error, reported as another provider's: an adapter that
    /// serves a request through another one's protocol names itself.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }
    pub fn with_partial_usage(mut self, usage: Usage) -> Result<Self, UsageError> {
        if usage.completeness() != UsageCompleteness::Partial {
            return Err(UsageError::InvalidErrorUsage);
        }
        self.partial_usage = Some(usage);
        Ok(self)
    }
    pub fn from_parts(
        category: ProviderErrorCategory,
        provider: Option<String>,
        provider_request_id: Option<String>,
        http_status: Option<u16>,
        provider_code: Option<String>,
        retry_after: Option<Duration>,
        partial_usage: Option<Usage>,
    ) -> Result<Self, UsageError> {
        let error = Self {
            category,
            provider,
            provider_request_id,
            http_status,
            provider_code,
            retry_after,
            partial_usage,
            not_sent: false,
        };
        error.validate()?;
        Ok(error)
    }
    pub fn category(&self) -> ProviderErrorCategory {
        self.category
    }
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn http_status(&self) -> Option<u16> {
        self.http_status
    }
    pub fn provider_code(&self) -> Option<&str> {
        self.provider_code.as_deref()
    }
    /// The provider's error type when it reads as an identifier
    /// (`ValidationException`, `rate_limit_exceeded`), so it can be logged
    /// and stored; `None` for anything that might be free text.
    pub fn error_type(&self) -> Option<&str> {
        self.provider_code.as_deref().filter(|code| {
            (1..=64).contains(&code.len())
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
        })
    }
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }
    pub fn partial_usage(&self) -> Option<Usage> {
        self.partial_usage
    }
    pub fn validate(&self) -> Result<(), UsageError> {
        if self
            .partial_usage
            .as_ref()
            .is_some_and(|usage| usage.completeness() != UsageCompleteness::Partial)
        {
            Err(UsageError::InvalidErrorUsage)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderError")
            .field("category", &self.category)
            .field("provider", &self.provider)
            .field("provider_request_id", &self.provider_request_id)
            .field("http_status", &self.http_status)
            .field("provider_code", &self.provider_code)
            .field("retry_after", &self.retry_after)
            .field("partial_usage", &self.partial_usage)
            .field("not_sent", &self.not_sent)
            .finish()
    }
}
impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "provider error: {:?}", self.category)
    }
}
impl std::error::Error for ProviderError {}

/// Embedding input. The model is intentionally absent: `InferenceContext`
/// deployment/model resolution is authoritative for every operation.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmbeddingRequest {
    pub input: Vec<String>,
}
impl fmt::Debug for EmbeddingRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbeddingRequest")
            .field("input", &format_args!("[{} redacted]", self.input.len()))
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize)]
pub struct Embedding {
    index: u32,
    vector: Vec<f32>,
}
impl<'de> Deserialize<'de> for Embedding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            index: u32,
            vector: Vec<f32>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.index, wire.vector).map_err(serde::de::Error::custom)
    }
}
impl Embedding {
    pub fn new(index: u32, vector: Vec<f32>) -> Result<Self, ResponseError> {
        if vector.is_empty() {
            return Err(ResponseError::EmptyVector);
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(ResponseError::NonFiniteVector);
        }
        Ok(Self { index, vector })
    }
    pub fn index(&self) -> u32 {
        self.index
    }
    pub fn vector(&self) -> &[f32] {
        &self.vector
    }
}
impl fmt::Debug for Embedding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedding")
            .field("index", &self.index)
            .field(
                "vector",
                &format_args!("[{} values redacted]", self.vector.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize)]
pub struct EmbeddingResponse {
    provider_request_id: Option<String>,
    model: String,
    data: Vec<Embedding>,
    usage: Option<UsageSnapshot>,
}
impl EmbeddingResponse {
    pub fn new(
        provider_request_id: Option<String>,
        model: String,
        data: Vec<Embedding>,
        usage: Option<UsageSnapshot>,
    ) -> Result<Self, ResponseError> {
        validate_ordered_indices(data.iter().map(Embedding::index))?;
        if model.trim().is_empty() {
            return Err(ResponseError::EmptyModel);
        }
        if usage
            .as_ref()
            .is_some_and(|snapshot| snapshot.finality() != UsageFinality::Final)
        {
            return Err(ResponseError::UsageMustBeFinal);
        }
        Ok(Self {
            provider_request_id,
            model,
            data,
            usage,
        })
    }
    pub fn provider_request_id(&self) -> Option<&str> {
        self.provider_request_id.as_deref()
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn data(&self) -> &[Embedding] {
        &self.data
    }
    pub fn usage(&self) -> Option<UsageSnapshot> {
        self.usage
    }
}
impl<'de> Deserialize<'de> for EmbeddingResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider_request_id: Option<String>,
            model: String,
            data: Vec<Embedding>,
            usage: Option<UsageSnapshot>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.provider_request_id, wire.model, wire.data, wire.usage)
            .map_err(serde::de::Error::custom)
    }
}
impl fmt::Debug for EmbeddingResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbeddingResponse")
            .field("model", &self.model)
            .field(
                "data",
                &format_args!("[{} vectors redacted]", self.data.len()),
            )
            .field("usage", &self.usage)
            .finish()
    }
}

/// Compatibility names used by callers that refer to the request context more
/// generally than inference does.
pub type RequestContext = InferenceContext;
pub type DeploymentReference = DeploymentModelReference;
pub type InvocationContext = InferenceContext;
pub type EmbeddingData = Embedding;

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn trusted_nested_metadata_round_trips_and_stays_redacted() {
        let value = serde_json::json!({
            "future_object": {"nested": [true, 7, {"unknown": "provider-value"}]},
            "future_number": 12.5,
            "future_boolean": false,
        });
        let metadata =
            SafeMetadata::from_trusted_mapping(value.as_object().unwrap().clone()).unwrap();
        let response = ChatResponse::new(None, "model".into(), vec![], None, metadata).unwrap();
        assert_eq!(serde_json::to_value(&response).unwrap()["metadata"], value);
        assert!(!format!("{response:?}").contains("provider-value"));
    }

    #[test]
    fn trusted_metadata_rejects_forbidden_nested_fields() {
        let value = serde_json::json!({"future": [{"authorization": "secret"}]});
        assert!(SafeMetadata::from_trusted_mapping(value.as_object().unwrap().clone()).is_err());
    }
}
