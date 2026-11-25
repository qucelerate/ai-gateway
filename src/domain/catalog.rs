use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};
use thiserror::Error;

use super::{accounting::Currency, identity::OrganizationId};

pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_MANIFEST_ITEMS: usize = 256;

/// Declares a fieldless enum whose `ALL`, `as_str` and `FromStr` all come from
/// one variant list, so a new variant cannot be missing from `ALL` and every
/// matrix test that iterates it.
macro_rules! catalog_enum {
    (
        $(#[$meta:meta])*
        $name:ident, unknown = $unknown:expr,
        { $($variant:ident => $text:literal),+ $(,)? }
    ) => {
        $(#[$meta])*
        pub enum $name {
            $($variant),+
        }
        impl $name {
            pub const ALL: [Self; [$($text),+].len()] = [$(Self::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }
        impl FromStr for $name {
            type Err = CatalogError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    _ => Err($unknown),
                }
            }
        }
    };
}

catalog_enum! {
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    ProviderFamily, unknown = CatalogError::UnknownProviderFamily, {
        OpenAi => "openai",
        Anthropic => "anthropic",
        Gemini => "gemini",
        Bedrock => "bedrock",
        AzureOpenAi => "azure-openai",
        AzureAiFoundry => "azure-ai-foundry",
        OpenAiCompatible => "openai-compatible",
    }
}

catalog_enum! {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    CatalogOperation, unknown = CatalogError::UnknownOperation, {
        Chat => "chat",
        ChatStream => "chat_stream",
        Embeddings => "embeddings",
        ChatBatch => "chat_batch",
        EmbeddingsBatch => "embeddings_batch",
    }
}

impl CatalogOperation {
    /// The operations a manifest declares. Streaming is a delivery mode of
    /// chat (D-020): a streamed request takes the alias's `chat` route.
    /// `chat_stream` remains a variant only so releases stored before D-020
    /// still read, and route as they did.
    pub const DECLARED: [Self; 4] = [
        Self::Chat,
        Self::Embeddings,
        Self::ChatBatch,
        Self::EmbeddingsBatch,
    ];

    /// The batch operation (D-021) whose lines are requests of `self`.
    pub fn batched(self) -> Option<Self> {
        match self {
            Self::Chat => Some(Self::ChatBatch),
            Self::Embeddings => Some(Self::EmbeddingsBatch),
            Self::ChatStream | Self::ChatBatch | Self::EmbeddingsBatch => None,
        }
    }

    /// The single-request operation a batch operation's lines are.
    pub fn unbatched(self) -> Self {
        match self {
            Self::ChatBatch => Self::Chat,
            Self::EmbeddingsBatch => Self::Embeddings,
            other => other,
        }
    }

    pub fn is_batch(self) -> bool {
        matches!(self, Self::ChatBatch | Self::EmbeddingsBatch)
    }
}

impl ProviderFamily {
    /// Operations the family's adapter implements. The catalog refuses any
    /// other, and the database guard `gateway.prevent_anthropic_embeddings()`
    /// enforces the same rule.
    pub fn supports(self, operation: CatalogOperation) -> bool {
        if operation.is_batch() {
            // Native batches (D-021) only where the adapter submits them;
            // the other families gain them milestone by milestone.
            // Bedrock's embedding models take one text per batch record and
            // Agent Platform's take no `generateContent`: their embedding
            // batches run emulated, on the embeddings route.
            if matches!(self, Self::Bedrock | Self::Gemini)
                && operation == CatalogOperation::EmbeddingsBatch
            {
                return false;
            }
            return self.supports_batch() && self.supports(operation.unbatched());
        }
        !matches!(
            (self, operation),
            (Self::Anthropic, CatalogOperation::Embeddings)
        )
    }

    /// Whether the family's adapter runs provider batch jobs.
    pub fn supports_batch(self) -> bool {
        matches!(
            self,
            Self::OpenAi | Self::AzureOpenAi | Self::Anthropic | Self::Bedrock | Self::Gemini
        )
    }

    /// Whether the adapter counts chat input tokens exactly through a
    /// provider API. Only these families may declare `provider_api_exact`;
    /// every other adapter returns `UnsupportedFeature` from
    /// `count_chat_input_tokens`.
    pub fn counts_chat_input_exactly(self) -> bool {
        matches!(self, Self::Anthropic | Self::Gemini)
    }

    /// Whether `price` charges for a usage dimension the adapter never
    /// reports. Such a price must be zero; the database guard enforces the
    /// same rule.
    fn prices_unreported_usage(self, price: &PriceManifest) -> bool {
        self.charges_unreported_usage(
            price.cached_input_minor_per_token,
            price.reasoning_minor_per_token,
            price.other_minor_per_token,
        )
    }

    /// [`Self::prices_unreported_usage`] for bare per-token prices, as a
    /// price override carries them. Every adapter reports cached input and
    /// other tokens (Anthropic's and Bedrock's cache reads and writes), so
    /// only reasoning is checked today.
    pub fn charges_unreported_usage(self, _cached_input: i64, reasoning: i64, _other: i64) -> bool {
        match self {
            // Both count thinking as output, never apart.
            Self::Anthropic | Self::Bedrock => reasoning != 0,
            Self::OpenAi
            | Self::Gemini
            | Self::AzureOpenAi
            | Self::AzureAiFoundry
            | Self::OpenAiCompatible => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatInputCountMode {
    ProviderApiExact,
    Unsupported,
}

impl ChatInputCountMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderApiExact => "provider_api_exact",
            Self::Unsupported => "unsupported",
        }
    }
}

impl FromStr for ChatInputCountMode {
    type Err = CatalogError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "provider_api_exact" => Ok(Self::ProviderApiExact),
            "unsupported" => Ok(Self::Unsupported),
            _ => Err(CatalogError::InvalidGraph),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogManifest {
    pub release: ReleaseManifest,
    pub providers: Vec<ProviderManifest>,
    pub deployments: Vec<DeploymentManifest>,
    pub prices: Vec<PriceManifest>,
    pub routes: Vec<RouteManifest>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub version: String,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderManifest {
    pub adapter_instance_key: String,
    pub family: String,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentManifest {
    pub name: String,
    pub model: String,
    pub location: Option<String>,
    pub adapter_instance_key: String,
    #[serde(default)]
    pub capabilities: Vec<CapabilityManifest>,
}

/// Capability policy is catalog data, not an environment routing default.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityManifest {
    pub operation: String,
    pub max_input_bytes: u64,
    pub max_output_tokens: u32,
    pub max_tokens_per_byte: u32,
    pub tokenizer: String,
    pub reservation_ttl_seconds: u64,
    #[serde(alias = "chat_input_count_mode")]
    pub chat_input_count_mode: String,
    /// Reasoning tokens reserved on top of a request's output cap, for a
    /// model whose reasoning the cap does not bound (xAI's Grok reports it
    /// apart from `completion_tokens` and reasons past `max_tokens`). Zero,
    /// the default, means reasoning falls within the output cap. Omitted
    /// from the manifest when zero, so releases without it keep their
    /// digest.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_allowance_tokens: u32,
    /// Whether the deployment takes function tools (D-032). Only chat
    /// capabilities may; false, the default, refuses requests that use
    /// tools. Omitted from the manifest when false, so releases without it
    /// keep their digest.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tools: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The wire API a deployment's tool calls travel over (D-032): its family's,
/// except on a Gemini (Agent Platform) account, where a partner model is
/// called through its publisher's API (Claude through Anthropic's Messages
/// API) and a project's endpoint through what it serves, known only when it
/// is called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolWire {
    /// OpenAI-shaped chat completions: OpenAI, Azure OpenAI, Foundry,
    /// OpenAI-compatible servers, and Agent Platform's OpenAI-compatible
    /// and Mistral partner models.
    OpenAi,
    /// Anthropic's Messages API: Anthropic, and Claude on Agent Platform.
    /// (Foundry's Claude deployments are told apart only when called; their
    /// ids and options are the same as OpenAI's for this purpose.)
    Anthropic,
    /// Bedrock's Converse API.
    Bedrock,
    /// Gemini's `generateContent`: Google's models.
    Gemini,
    /// An Agent Platform endpoint, whose wire is read when it is called.
    Endpoint,
}

/// How a wire's tool call ids look, which decides whether a conversation's
/// calls can be replayed to another deployment (D-032).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallIds {
    /// Gemini's: made by the gateway, carrying the call's thought signature
    /// (`call_1~sig~…`), which no other provider accepts and without which
    /// Gemini 3 refuses a replayed call.
    Gemini,
    /// Short ids of letters, digits, `_` and `-`, as OpenAI, Anthropic and
    /// Bedrock make and take them.
    Plain,
    /// An endpoint's: unknown until it is called.
    Unknown,
}

impl ToolWire {
    /// The wire `model` on a `family` account is called through.
    pub fn of(family: ProviderFamily, model: &str) -> Self {
        use crate::domain::agent_platform::{AgentPlatformModel, WireApi};
        match family {
            ProviderFamily::Anthropic => Self::Anthropic,
            ProviderFamily::Bedrock => Self::Bedrock,
            ProviderFamily::Gemini => match AgentPlatformModel::parse(model) {
                Some(AgentPlatformModel::Endpoint(_)) => Self::Endpoint,
                Some(AgentPlatformModel::Partner {
                    wire_api: Some(WireApi::AnthropicMessages),
                    ..
                }) => Self::Anthropic,
                Some(AgentPlatformModel::Partner {
                    wire_api: Some(WireApi::MistralChat | WireApi::OpenAiChat),
                    ..
                }) => Self::OpenAi,
                // Google's models, and identifiers the adapter refuses anyway.
                _ => Self::Gemini,
            },
            _ => Self::OpenAi,
        }
    }

    /// The tool options this wire refuses rather than drops (D-032):
    /// Bedrock's Converse has no tool choice of none, and neither Converse
    /// nor Gemini can limit a response to one call. An endpoint's are
    /// checked when it is called.
    pub fn option_refusals(self) -> ToolOptionRefusals {
        match self {
            Self::Bedrock => ToolOptionRefusals {
                tool_choice_none: Some(
                    "Bedrock's Converse API has no tool_choice none; leave out tools instead",
                ),
                serial_calls: Some(
                    "Bedrock's Converse API cannot limit a response to one tool call (parallel_tool_calls false)",
                ),
            },
            Self::Gemini => ToolOptionRefusals {
                tool_choice_none: None,
                serial_calls: Some(
                    "Gemini cannot limit a response to one tool call (parallel_tool_calls false)",
                ),
            },
            Self::OpenAi | Self::Anthropic | Self::Endpoint => ToolOptionRefusals::default(),
        }
    }

    /// Why this wire cannot serve `request`'s tool options as asked, or
    /// `None` when it can.
    pub fn option_refusal(
        self,
        request: &crate::domain::inference::ChatRequest,
    ) -> Option<&'static str> {
        self.option_refusals().refusal(request)
    }

    pub fn call_ids(self) -> ToolCallIds {
        match self {
            Self::Gemini => ToolCallIds::Gemini,
            Self::Endpoint => ToolCallIds::Unknown,
            Self::OpenAi | Self::Anthropic | Self::Bedrock => ToolCallIds::Plain,
        }
    }
}

/// The tool options some deployment of a route refuses (D-032), each with
/// why: a route refuses an option if any deployment it may reach does, so
/// a failover never meets a refusal after the request was reserved.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ToolOptionRefusals {
    /// `tool_choice: none`.
    pub tool_choice_none: Option<&'static str>,
    /// `parallel_tool_calls: false`.
    pub serial_calls: Option<&'static str>,
}

impl ToolOptionRefusals {
    /// What a deployment of `model` on a `family` account (as the catalog
    /// names it) refuses; a family the gateway does not know refuses
    /// nothing here (its adapter refuses the route).
    pub fn of(family: &str, model: &str) -> Self {
        family
            .parse::<ProviderFamily>()
            .map(|family| ToolWire::of(family, model).option_refusals())
            .unwrap_or_default()
    }

    /// What either refuses.
    pub fn or(self, other: Self) -> Self {
        Self {
            tool_choice_none: self.tool_choice_none.or(other.tool_choice_none),
            serial_calls: self.serial_calls.or(other.serial_calls),
        }
    }

    /// Why `request`'s tool options are refused, or `None`.
    pub fn refusal(self, request: &crate::domain::inference::ChatRequest) -> Option<&'static str> {
        use crate::domain::inference::ToolChoice;
        if request.tool_choice == Some(ToolChoice::None) {
            if let Some(why) = self.tool_choice_none {
                return Some(why);
            }
        }
        if request.parallel_tool_calls == Some(false) {
            if let Some(why) = self.serial_calls {
                return Some(why);
            }
        }
        None
    }
}

/// The call id dialect of a deployment of `model` on a `family` account
/// (the family as the catalog names it); `None` for a family the gateway
/// does not know.
pub fn tool_call_ids(family: &str, model: &str) -> Option<ToolCallIds> {
    family
        .parse::<ProviderFamily>()
        .ok()
        .map(|family| ToolWire::of(family, model).call_ids())
}

/// Whether `model` on `family` certainly takes function tools, so an
/// automatic offer may set its chat capability to take them (D-032). No
/// provider's model listing says which models take tools; every Claude
/// model does, on Anthropic's API and on Bedrock's Converse (including
/// Bedrock's inference profiles, `us.anthropic.claude-…`). Others are left
/// for an admin to set.
pub fn takes_tools(family: ProviderFamily, model: &str) -> bool {
    match family {
        ProviderFamily::Anthropic => model.starts_with("claude-"),
        ProviderFamily::Bedrock => model
            .split('.')
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair[0] == "anthropic" && pair[1].starts_with("claude-")),
        _ => false,
    }
}

/// The most reasoning tokens a capability may reserve beyond its cap.
pub const MAX_REASONING_ALLOWANCE_TOKENS: u32 = 1_000_000;

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl CapabilityManifest {
    pub fn operation(&self) -> &str {
        &self.operation
    }
    pub fn limits(&self) -> (u64, u32, u32, &str, u64, &str) {
        (
            self.max_input_bytes,
            self.max_output_tokens,
            self.max_tokens_per_byte,
            &self.tokenizer,
            self.reservation_ttl_seconds,
            &self.chat_input_count_mode,
        )
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PriceManifest {
    pub deployment: String,
    pub operation: String,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub currency: String,
    pub source: String,
    pub source_version: String,
    pub confidence: String,
}
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteManifest {
    pub alias: String,
    pub operation: String,
    pub deployment: String,
    /// This deployment's share of its priority's traffic (D-023), relative to
    /// the other deployments the alias routes the operation to at the same
    /// priority. Omitted from the manifest when 1, the default, so releases
    /// stored before weighted routes keep their digest.
    #[serde(
        default = "default_route_number",
        skip_serializing_if = "is_default_route_number"
    )]
    pub weight: u32,
    /// Lower is preferred: a request goes to the lowest priority that has a
    /// healthy deployment. Omitted when 1, the default.
    #[serde(
        default = "default_route_number",
        skip_serializing_if = "is_default_route_number"
    )]
    pub priority: u32,
}

/// The largest weight a route's deployment may have.
pub const MAX_ROUTE_WEIGHT: u32 = 1000;
/// The largest (least preferred) priority a route's deployment may have.
pub const MAX_ROUTE_PRIORITY: u32 = 10;
/// The most deployments one alias may route one operation to.
pub const MAX_ROUTE_DEPLOYMENTS: usize = 8;

fn default_route_number() -> u32 {
    1
}

fn is_default_route_number(value: &u32) -> bool {
    *value == 1
}

impl RouteManifest {
    /// A route of `alias` and `operation` to one deployment, at weight and
    /// priority 1.
    pub fn new(
        alias: impl Into<String>,
        operation: impl Into<String>,
        deployment: impl Into<String>,
    ) -> Self {
        Self {
            alias: alias.into(),
            operation: operation.into(),
            deployment: deployment.into(),
            weight: 1,
            priority: 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogDigest(pub [u8; 32]);

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("catalog manifest is too large or has too many entries")]
    Bounds,
    #[error("catalog manifest contains an invalid string")]
    InvalidString,
    #[error("unknown provider family")]
    UnknownProviderFamily,
    #[error("unknown catalog operation")]
    UnknownOperation,
    #[error("provider family does not support this operation")]
    UnsupportedOperation,
    #[error("catalog graph is invalid")]
    InvalidGraph,
    /// A `chat_stream` entry that says something its `chat` entry does not.
    /// Streaming takes the chat route (D-020), so the two must agree.
    #[error(
        "`chat_stream` is no longer a separate operation: streamed requests use the `chat` route, price and limits, but {0} declares a different `chat_stream` {1}; remove the `chat_stream` entry or make it match"
    )]
    StreamingDiffers(String, &'static str),
    /// A route whose deployments cannot serve one alias as one contract
    /// (D-023); the text says which alias and why.
    #[error("{0}")]
    Route(String),
    /// A deployment its account cannot call as declared; the text says which
    /// deployment and what to change.
    #[error("{0}")]
    Deployment(String),
    #[error("catalog JSON is invalid")]
    InvalidJson(#[from] serde_json::Error),
}

impl CatalogManifest {
    pub fn parse(bytes: &[u8]) -> Result<(Self, CatalogDigest), CatalogError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(CatalogError::Bounds);
        }
        let mut manifest: Self = serde_json::from_slice(bytes)?;
        manifest.fold_streaming()?;
        manifest.validate()?;
        let digest = manifest.digest()?;
        Ok((manifest, digest))
    }

    /// Folds every `chat_stream` entry into `chat` (D-020), for a manifest
    /// written before streaming became a mode of chat. An entry matching its
    /// `chat` counterpart is dropped; one without a counterpart becomes the
    /// `chat` entry; one that differs is refused, since the release would
    /// otherwise route or price streamed requests differently from what it
    /// says. Stored releases are never folded: they keep their digest.
    pub fn fold_streaming(&mut self) -> Result<(), CatalogError> {
        self.fold(true)
    }

    /// [`Self::fold_streaming`] for a stored release a new one is planned
    /// from (an automatic offer): a `chat_stream` entry that differs from its
    /// `chat` one is dropped, and chat wins, as the console does, so the next
    /// release has no `chat_stream` and is never refused for an old one.
    pub fn fold_streaming_into_chat(&mut self) {
        self.fold(false)
            .expect("folding without refusing a difference cannot fail");
    }

    fn fold(&mut self, strict: bool) -> Result<(), CatalogError> {
        const CHAT: &str = "chat";
        const STREAM: &str = "chat_stream";
        // Compared as JSON with the operation renamed, so every field counts,
        // including ones added later.
        fn as_chat<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, CatalogError> {
            let mut value = serde_json::to_value(value)?;
            value["operation"] = CHAT.into();
            Ok(value)
        }
        for deployment in &mut self.deployments {
            let chat = deployment
                .capabilities
                .iter()
                .find(|capability| capability.operation == CHAT)
                .map(as_chat)
                .transpose()?;
            let mut kept = Vec::with_capacity(deployment.capabilities.len());
            for mut capability in std::mem::take(&mut deployment.capabilities) {
                if capability.operation == STREAM {
                    match &chat {
                        Some(chat) if as_chat(&capability)? == *chat => continue,
                        Some(_) if !strict => continue,
                        Some(_) => {
                            return Err(CatalogError::StreamingDiffers(
                                format!("deployment `{}`", deployment.name),
                                "capability",
                            ));
                        }
                        None => capability.operation = CHAT.to_owned(),
                    }
                }
                kept.push(capability);
            }
            deployment.capabilities = kept;
        }
        let chat_prices = self
            .prices
            .iter()
            .filter(|price| price.operation == CHAT)
            .map(|price| Ok((price.deployment.clone(), as_chat(price)?)))
            .collect::<Result<std::collections::HashMap<_, _>, CatalogError>>()?;
        let mut prices = Vec::with_capacity(self.prices.len());
        for mut price in std::mem::take(&mut self.prices) {
            if price.operation == STREAM {
                match chat_prices.get(&price.deployment) {
                    Some(chat) if as_chat(&price)? == *chat => continue,
                    Some(_) if !strict => continue,
                    Some(_) => {
                        return Err(CatalogError::StreamingDiffers(
                            format!("deployment `{}`", price.deployment),
                            "price",
                        ));
                    }
                    None => price.operation = CHAT.to_owned(),
                }
            }
            prices.push(price);
        }
        self.prices = prices;
        // An alias's chat routes, each as JSON with its operation, so a
        // `chat_stream` entry folds only into an identical one.
        let mut chat_routes: std::collections::HashMap<String, Vec<serde_json::Value>> =
            std::collections::HashMap::new();
        for route in self.routes.iter().filter(|route| route.operation == CHAT) {
            chat_routes
                .entry(route.alias.clone())
                .or_default()
                .push(as_chat(route)?);
        }
        let mut routes = Vec::with_capacity(self.routes.len());
        for mut route in std::mem::take(&mut self.routes) {
            if route.operation == STREAM {
                match chat_routes.get(&route.alias) {
                    Some(chat) if chat.contains(&as_chat(&route)?) => continue,
                    Some(_) if !strict => continue,
                    Some(_) => {
                        return Err(CatalogError::StreamingDiffers(
                            format!("alias `{}`", route.alias),
                            "route",
                        ));
                    }
                    None => route.operation = CHAT.to_owned(),
                }
            }
            routes.push(route);
        }
        self.routes = routes;
        Ok(())
    }

    pub fn digest(&self) -> Result<CatalogDigest, CatalogError> {
        self.validate()?;
        let canonical = canonical_json(self).map_err(CatalogError::InvalidJson)?;
        Ok(CatalogDigest(Sha256::digest(canonical).into()))
    }

    pub fn validate(&self) -> Result<(), CatalogError> {
        if self.providers.len() > MAX_MANIFEST_ITEMS
            || self.deployments.len() > MAX_MANIFEST_ITEMS
            || self.prices.len() > MAX_MANIFEST_ITEMS
            || self.routes.len() > MAX_MANIFEST_ITEMS
        {
            return Err(CatalogError::Bounds);
        }
        valid_string(&self.release.version, 128)?;
        let mut providers = std::collections::HashMap::new();
        for provider in &self.providers {
            valid_string(&provider.adapter_instance_key, 128)?;
            let family = ProviderFamily::from_str(&provider.family)?;
            if providers
                .insert(provider.adapter_instance_key.clone(), family)
                .is_some()
            {
                return Err(CatalogError::InvalidGraph);
            }
        }
        let mut deployments = std::collections::HashMap::new();
        for deployment in &self.deployments {
            valid_string(&deployment.name, 255)?;
            valid_string(&deployment.model, 255)?;
            if let Some(location) = &deployment.location {
                valid_string(location, 255)?;
            }
            let family = providers
                .get(&deployment.adapter_instance_key)
                .ok_or(CatalogError::InvalidGraph)?;
            let mut capabilities = std::collections::HashSet::new();
            for capability in &deployment.capabilities {
                let operation = CatalogOperation::from_str(capability.operation())?;
                if !family.supports(operation) {
                    return Err(CatalogError::UnsupportedOperation);
                }
                let (input, output, tokens_per_byte, tokenizer, reservation_ttl, count_mode) =
                    capability.limits();
                if input == 0
                    || output == 0
                    || tokens_per_byte == 0
                    || reservation_ttl == 0
                    || reservation_ttl > 86_400
                {
                    return Err(CatalogError::InvalidGraph);
                }
                valid_string(tokenizer, 128)?;
                if tokenizer != "configured" {
                    return Err(CatalogError::InvalidGraph);
                }
                let count_mode = ChatInputCountMode::from_str(count_mode)?;
                // `ProviderApiExact` on a family without a provider-side
                // counting endpoint would fail every request at runtime.
                // `Unsupported` works for every family: accounting reserves
                // against the catalog-configured bytes-per-token upper bound
                // (the same mechanism embeddings use) and settles against the
                // provider's reported usage. See
                // `AccountingService::count_chat_input_tokens`.
                if count_mode == ChatInputCountMode::ProviderApiExact
                    && !family.counts_chat_input_exactly()
                {
                    return Err(CatalogError::InvalidGraph);
                }
                // Only chat generates reasoning; embeddings reserve no output.
                if capability.reasoning_allowance_tokens > MAX_REASONING_ALLOWANCE_TOKENS
                    || (capability.reasoning_allowance_tokens > 0
                        && matches!(
                            operation,
                            CatalogOperation::Embeddings | CatalogOperation::EmbeddingsBatch
                        ))
                {
                    return Err(CatalogError::InvalidGraph);
                }
                // Tools are a chat feature (D-032).
                if capability.tools
                    && !matches!(
                        operation,
                        CatalogOperation::Chat
                            | CatalogOperation::ChatStream
                            | CatalogOperation::ChatBatch
                    )
                {
                    return Err(CatalogError::InvalidGraph);
                }
                if !capabilities.insert(capability.operation()) {
                    return Err(CatalogError::InvalidGraph);
                }
            }
            if *family == ProviderFamily::Gemini {
                validate_agent_platform_deployment(deployment)?;
            }
            if deployments
                .insert(deployment.name.clone(), *family)
                .is_some()
            {
                return Err(CatalogError::InvalidGraph);
            }
        }
        let mut price_keys = std::collections::HashSet::new();
        for price in &self.prices {
            let family = deployments
                .get(&price.deployment)
                .ok_or(CatalogError::InvalidGraph)?;
            let operation = CatalogOperation::from_str(&price.operation)?;
            if !price_keys.insert((price.deployment.clone(), price.operation.clone())) {
                return Err(CatalogError::InvalidGraph);
            }
            let deployment_manifest = self
                .deployments
                .iter()
                .find(|d| d.name == price.deployment)
                .ok_or(CatalogError::InvalidGraph)?;
            if !family.supports(operation) {
                return Err(CatalogError::UnsupportedOperation);
            }
            if family.prices_unreported_usage(price) {
                return Err(CatalogError::InvalidGraph);
            }
            if !deployment_manifest
                .capabilities
                .iter()
                .any(|capability| capability.operation() == price.operation)
            {
                return Err(CatalogError::InvalidGraph);
            }
            if [
                price.input_minor_per_token,
                price.output_minor_per_token,
                price.cached_input_minor_per_token,
                price.reasoning_minor_per_token,
                price.other_minor_per_token,
            ]
            .iter()
            .any(|value| *value < 0)
            {
                return Err(CatalogError::InvalidGraph);
            }
            Currency::new(price.currency.clone()).map_err(|_| CatalogError::InvalidGraph)?;
            if !matches!(price.confidence.as_str(), "known" | "conservative") {
                return Err(CatalogError::InvalidGraph);
            }
            for value in [&price.source, &price.source_version, &price.confidence] {
                valid_string(value, 128)?;
            }
        }
        let mut route_keys = std::collections::HashSet::new();
        // Each alias and operation's deployments, in manifest order.
        let mut groups: std::collections::BTreeMap<(&str, &str), Vec<&RouteManifest>> =
            std::collections::BTreeMap::new();
        for route in &self.routes {
            valid_string(&route.alias, 128)?;
            let family = deployments
                .get(&route.deployment)
                .ok_or(CatalogError::InvalidGraph)?;
            let operation = CatalogOperation::from_str(&route.operation)?;
            let deployment_manifest = self
                .deployments
                .iter()
                .find(|d| d.name == route.deployment)
                .ok_or(CatalogError::InvalidGraph)?;
            if !deployment_manifest
                .capabilities
                .iter()
                .any(|capability| capability.operation() == route.operation)
            {
                return Err(CatalogError::InvalidGraph);
            }
            if !route_keys.insert((
                route.alias.clone(),
                route.operation.clone(),
                route.deployment.clone(),
            )) {
                return Err(CatalogError::InvalidGraph);
            }
            if !family.supports(operation) {
                return Err(CatalogError::UnsupportedOperation);
            }
            if !self.prices.iter().any(|price| {
                price.deployment == route.deployment && price.operation == route.operation
            }) {
                return Err(CatalogError::InvalidGraph);
            }
            if !(1..=MAX_ROUTE_WEIGHT).contains(&route.weight) {
                return Err(CatalogError::Route(format!(
                    "alias `{}` gives deployment `{}` weight {}; a weight is from 1 to {MAX_ROUTE_WEIGHT}",
                    route.alias, route.deployment, route.weight
                )));
            }
            if !(1..=MAX_ROUTE_PRIORITY).contains(&route.priority) {
                return Err(CatalogError::Route(format!(
                    "alias `{}` gives deployment `{}` priority {}; a priority is from 1 to {MAX_ROUTE_PRIORITY}",
                    route.alias, route.deployment, route.priority
                )));
            }
            groups
                .entry((route.alias.as_str(), route.operation.as_str()))
                .or_default()
                .push(route);
        }
        for ((alias, operation), group) in groups {
            if group.len() > MAX_ROUTE_DEPLOYMENTS {
                return Err(CatalogError::Route(format!(
                    "alias `{alias}` routes {operation} to {} deployments; at most {MAX_ROUTE_DEPLOYMENTS}",
                    group.len()
                )));
            }
            // Budgets never convert currencies (§6), so a route with no
            // single currency would have no highest price to reserve.
            let mut currencies = group.iter().filter_map(|route| {
                self.prices
                    .iter()
                    .find(|price| {
                        price.deployment == route.deployment && price.operation == route.operation
                    })
                    .map(|price| price.currency.as_str())
            });
            let first = currencies.next();
            if currencies.any(|currency| Some(currency) != first) {
                return Err(CatalogError::Route(format!(
                    "alias `{alias}` routes {operation} to deployments priced in different currencies; every deployment of a route is priced in one currency"
                )));
            }
        }
        Ok(())
    }
}

/// A Gemini deployment of a partner model or an endpoint
/// (`docs/custom-deployments-design.md`): the publisher must be one the
/// gateway calls, the deployment may only chat (neither embeddings nor batch
/// jobs reach these models), and an exact input count needs a wire API with a
/// free count (Anthropic's; an endpoint's is not relied on). Whether the
/// account is on Agent Platform is the account's credential, which a release
/// does not carry: the adapter refuses the call on a Gemini API key.
fn validate_agent_platform_deployment(deployment: &DeploymentManifest) -> Result<(), CatalogError> {
    use crate::domain::agent_platform::AgentPlatformModel;
    let name = &deployment.name;
    let counts_exactly = match AgentPlatformModel::parse(&deployment.model) {
        Some(AgentPlatformModel::Google(_)) => return Ok(()),
        // Gemini API identifiers outside Agent Platform's shapes are left
        // to the provider, as before.
        None if !deployment.model.contains('/') => return Ok(()),
        None => {
            return Err(CatalogError::Deployment(format!(
                "deployment `{name}` names `{}`, which is neither a Google model, `<publisher>/<model>` nor `endpoints/<id>`",
                deployment.model
            )));
        }
        Some(AgentPlatformModel::Partner {
            publisher: "google",
            ..
        }) => {
            return Err(CatalogError::Deployment(format!(
                "deployment `{name}` names `{}`; name a Google model without `google/`",
                deployment.model
            )));
        }
        Some(AgentPlatformModel::Partner {
            publisher,
            wire_api: None,
            ..
        }) => {
            return Err(CatalogError::Deployment(format!(
                "deployment `{name}`: the gateway does not call Model Garden models of publisher `{publisher}`; it calls Google's own models (named without a publisher) and those of anthropic, mistralai, xai, meta, deepseek-ai, qwen and openai"
            )));
        }
        Some(AgentPlatformModel::Partner {
            wire_api: Some(wire_api),
            ..
        }) => wire_api.counts_input_exactly(),
        Some(AgentPlatformModel::Endpoint(_)) => false,
    };
    for capability in &deployment.capabilities {
        if capability.operation() != CatalogOperation::Chat.as_str() {
            return Err(CatalogError::Deployment(format!(
                "deployment `{name}` declares `{}`; a Model Garden partner model or an endpoint serves only `chat`",
                capability.operation()
            )));
        }
        let (_, _, _, _, _, count_mode) = capability.limits();
        if !counts_exactly && count_mode == ChatInputCountMode::ProviderApiExact.as_str() {
            return Err(CatalogError::Deployment(format!(
                "deployment `{name}` declares `chat_input_count_mode: provider_api_exact`, but `{}` has no free token count; use `unsupported`",
                deployment.model
            )));
        }
    }
    Ok(())
}

/// Canonical JSON used for the release digest. Object keys are sorted at every
/// level, while arrays retain their manifest order (the manifest validator
/// rejects duplicate graph identities).
fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    fn sort(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut sorted = serde_json::Map::new();
                let mut entries: Vec<_> = map.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (key, value) in entries {
                    sorted.insert(key, sort(value));
                }
                serde_json::Value::Object(sorted)
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(sort).collect())
            }
            other => other,
        }
    }
    serde_json::to_vec(&sort(serde_json::to_value(value)?))
}

fn valid_string(value: &str, max: usize) -> Result<(), CatalogError> {
    if value.trim().is_empty() || value.len() > max || !value.is_char_boundary(value.len()) {
        Err(CatalogError::InvalidString)
    } else {
        Ok(())
    }
}

impl fmt::Display for CatalogDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct CatalogOrganization(pub OrganizationId);

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> CatalogManifest {
        serde_json::from_value(serde_json::json!({
            "release":{"version":"1"},
            "providers":[{"adapter_instance_key":"openai-main","family":"openai"}],
        "deployments":[{"name":"chat","model":"gpt","location":null,"adapter_instance_key":"openai-main","capabilities":[{"operation":"chat","max_input_bytes":131072,"max_output_tokens":4096,"max_tokens_per_byte":1,"tokenizer":"configured","reservation_ttl_seconds":300,"chat_input_count_mode":"unsupported"}]}],
            "prices":[{"deployment":"chat","operation":"chat","input_minor_per_token":1,"output_minor_per_token":1,"cached_input_minor_per_token":1,"reasoning_minor_per_token":0,"other_minor_per_token":0,"currency":"USD","source":"operator","source_version":"1","confidence":"known"}],
            "routes":[{"alias":"chat-public","operation":"chat","deployment":"chat"}]
        })).unwrap()
    }

    #[test]
    fn listed_values_round_trip_through_their_names() {
        for family in ProviderFamily::ALL {
            assert_eq!(ProviderFamily::from_str(family.as_str()).unwrap(), family);
        }
        for operation in CatalogOperation::ALL {
            assert_eq!(
                CatalogOperation::from_str(operation.as_str()).unwrap(),
                operation
            );
        }
    }

    #[test]
    fn manifest_digest_is_deterministic_and_graph_is_strict() {
        let bytes = serde_json::to_vec(&manifest()).unwrap();
        let (_, first) = CatalogManifest::parse(&bytes).unwrap();
        let (_, second) = CatalogManifest::parse(&bytes).unwrap();
        assert_eq!(first, second);
    }

    fn single_route(
        family: ProviderFamily,
        operation: CatalogOperation,
        count_mode: ChatInputCountMode,
    ) -> serde_json::Value {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["providers"][0]["family"] = family.as_str().into();
        value["deployments"][0]["capabilities"][0]["operation"] = operation.as_str().into();
        value["deployments"][0]["capabilities"][0]["chat_input_count_mode"] =
            count_mode.as_str().into();
        value["prices"][0]["operation"] = operation.as_str().into();
        value["prices"][0]["cached_input_minor_per_token"] = 0.into();
        value["routes"][0]["operation"] = operation.as_str().into();
        value
    }

    fn parse(value: &serde_json::Value) -> Result<(), CatalogError> {
        CatalogManifest::parse(&serde_json::to_vec(value).unwrap()).map(|_| ())
    }

    /// `manifest()` with a `chat_stream` capability, price and route beside
    /// its `chat` ones, as manifests were written before D-020.
    fn with_streaming() -> serde_json::Value {
        let mut value = serde_json::to_value(manifest()).unwrap();
        let mut capability = value["deployments"][0]["capabilities"][0].clone();
        capability["operation"] = "chat_stream".into();
        value["deployments"][0]["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(capability);
        let mut price = value["prices"][0].clone();
        price["operation"] = "chat_stream".into();
        value["prices"].as_array_mut().unwrap().push(price);
        let mut route = value["routes"][0].clone();
        route["operation"] = "chat_stream".into();
        value["routes"].as_array_mut().unwrap().push(route);
        value
    }

    #[test]
    fn a_matching_chat_stream_entry_folds_into_chat() {
        let (folded, digest) =
            CatalogManifest::parse(&serde_json::to_vec(&with_streaming()).unwrap()).unwrap();
        assert_eq!(folded.deployments[0].capabilities.len(), 1);
        assert_eq!(folded.prices.len(), 1);
        assert_eq!(folded.routes.len(), 1);
        assert!(folded.routes.iter().all(|route| route.operation == "chat"));
        // The same release as one written without it.
        let plain = CatalogManifest::parse(&serde_json::to_vec(&manifest()).unwrap()).unwrap();
        assert_eq!(digest, plain.1);
    }

    #[test]
    fn a_lone_chat_stream_entry_becomes_chat() {
        let streaming = single_route(
            ProviderFamily::OpenAi,
            CatalogOperation::ChatStream,
            ChatInputCountMode::Unsupported,
        );
        let (folded, _) = CatalogManifest::parse(&serde_json::to_vec(&streaming).unwrap()).unwrap();
        assert_eq!(folded.deployments[0].capabilities[0].operation, "chat");
        assert_eq!(folded.prices[0].operation, "chat");
        assert_eq!(folded.routes[0].operation, "chat");
    }

    #[test]
    fn a_stored_release_folds_with_chat_winning() {
        let mut value = with_streaming();
        value["prices"][1]["input_minor_per_token"] = 2.into();
        let mut stored: CatalogManifest = serde_json::from_value(value).unwrap();
        stored.fold_streaming_into_chat();
        assert_eq!(stored.prices.len(), 1);
        assert_eq!(stored.prices[0].operation, "chat");
        assert_eq!(stored.prices[0].input_minor_per_token, 1);
        assert_eq!(stored.routes.len(), 1);
        assert!(stored.validate().is_ok());
    }

    #[test]
    fn a_chat_stream_entry_that_differs_from_chat_is_refused() {
        let mut price = with_streaming();
        price["prices"][1]["input_minor_per_token"] = 2.into();
        assert!(matches!(
            parse(&price),
            Err(CatalogError::StreamingDiffers(_, "price"))
        ));
        let mut capability = with_streaming();
        capability["deployments"][0]["capabilities"][1]["max_output_tokens"] = 8.into();
        assert!(matches!(
            parse(&capability),
            Err(CatalogError::StreamingDiffers(_, "capability"))
        ));
        let mut route = with_streaming();
        let mut other = route["deployments"][0].clone();
        other["name"] = "other".into();
        route["deployments"].as_array_mut().unwrap().push(other);
        let mut other_price = route["prices"][0].clone();
        other_price["deployment"] = "other".into();
        route["prices"].as_array_mut().unwrap().push(other_price);
        route["routes"][1]["deployment"] = "other".into();
        let error = parse(&route).unwrap_err();
        assert!(matches!(error, CatalogError::StreamingDiffers(_, "route")));
        assert!(error.to_string().contains("alias `chat-public`"));
    }

    #[test]
    fn a_reasoning_allowance_is_chat_only_bounded_and_absent_when_zero() {
        // Zero is omitted, so releases written before the field keep their
        // digest.
        let plain = serde_json::to_value(manifest()).unwrap();
        assert!(
            plain["deployments"][0]["capabilities"][0]
                .get("reasoning_allowance_tokens")
                .is_none()
        );
        let mut explicit = plain.clone();
        explicit["deployments"][0]["capabilities"][0]["reasoning_allowance_tokens"] = 0.into();
        let digest = |value: &serde_json::Value| {
            CatalogManifest::parse(&serde_json::to_vec(value).unwrap())
                .unwrap()
                .1
        };
        assert_eq!(digest(&plain), digest(&explicit));

        let mut chat = plain.clone();
        chat["deployments"][0]["capabilities"][0]["reasoning_allowance_tokens"] = 4096.into();
        assert!(parse(&chat).is_ok());
        assert_ne!(digest(&plain), digest(&chat));
        chat["deployments"][0]["capabilities"][0]["reasoning_allowance_tokens"] =
            (MAX_REASONING_ALLOWANCE_TOKENS + 1).into();
        assert!(matches!(parse(&chat), Err(CatalogError::InvalidGraph)));

        let mut embeddings = single_route(
            ProviderFamily::OpenAi,
            CatalogOperation::Embeddings,
            ChatInputCountMode::Unsupported,
        );
        assert!(parse(&embeddings).is_ok());
        embeddings["deployments"][0]["capabilities"][0]["reasoning_allowance_tokens"] = 16.into();
        assert!(matches!(
            parse(&embeddings),
            Err(CatalogError::InvalidGraph)
        ));
    }

    #[test]
    fn batch_operations_are_priced_and_routed_apart_where_the_family_runs_batches() {
        assert_eq!(
            CatalogOperation::Chat.batched(),
            Some(CatalogOperation::ChatBatch)
        );
        assert_eq!(
            CatalogOperation::EmbeddingsBatch.unbatched(),
            CatalogOperation::Embeddings
        );
        assert_eq!(CatalogOperation::ChatStream.batched(), None);
        // Native batches where the family has a batch API (D-021); the
        // others' lines run emulated, on their single routes.
        assert!(ProviderFamily::OpenAi.supports(CatalogOperation::ChatBatch));
        assert!(ProviderFamily::OpenAi.supports(CatalogOperation::EmbeddingsBatch));
        assert!(ProviderFamily::AzureOpenAi.supports(CatalogOperation::ChatBatch));
        assert!(ProviderFamily::AzureOpenAi.supports(CatalogOperation::EmbeddingsBatch));
        assert!(ProviderFamily::Anthropic.supports(CatalogOperation::ChatBatch));
        assert!(!ProviderFamily::Anthropic.supports(CatalogOperation::EmbeddingsBatch));
        assert!(ProviderFamily::Bedrock.supports(CatalogOperation::ChatBatch));
        assert!(ProviderFamily::Gemini.supports(CatalogOperation::ChatBatch));
        assert!(!ProviderFamily::Gemini.supports(CatalogOperation::EmbeddingsBatch));
        assert!(ProviderFamily::Bedrock.supports(CatalogOperation::Embeddings));
        assert!(!ProviderFamily::Bedrock.supports(CatalogOperation::EmbeddingsBatch));
        assert!(!ProviderFamily::Anthropic.supports(CatalogOperation::Embeddings));
        assert!(!ProviderFamily::OpenAiCompatible.supports(CatalogOperation::ChatBatch));

        // A batch capability, price and route beside the chat ones.
        let mut value = serde_json::to_value(manifest()).unwrap();
        let mut capability = value["deployments"][0]["capabilities"][0].clone();
        capability["operation"] = "chat_batch".into();
        capability["reservation_ttl_seconds"] = 86_400.into();
        value["deployments"][0]["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(capability);
        let mut price = value["prices"][0].clone();
        price["operation"] = "chat_batch".into();
        value["prices"].as_array_mut().unwrap().push(price);
        let mut route = value["routes"][0].clone();
        route["operation"] = "chat_batch".into();
        value["routes"].as_array_mut().unwrap().push(route);
        let (parsed, _) = CatalogManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.routes.len(), 2);

        // A batch route needs its own batch price: the chat price is not it.
        let mut unpriced = value.clone();
        unpriced["prices"].as_array_mut().unwrap().pop();
        assert!(matches!(parse(&unpriced), Err(CatalogError::InvalidGraph)));

        // Embedding batches reserve no reasoning.
        let mut embeddings = single_route(
            ProviderFamily::OpenAi,
            CatalogOperation::EmbeddingsBatch,
            ChatInputCountMode::Unsupported,
        );
        assert!(parse(&embeddings).is_ok());
        embeddings["deployments"][0]["capabilities"][0]["reasoning_allowance_tokens"] = 16.into();
        assert!(matches!(
            parse(&embeddings),
            Err(CatalogError::InvalidGraph)
        ));
    }

    #[test]
    fn gemini_partner_models_and_endpoints_are_validated_by_what_calls_them() {
        let gemini = |model: &str, operation, count_mode| {
            let mut value = single_route(ProviderFamily::Gemini, operation, count_mode);
            value["deployments"][0]["model"] = model.into();
            value
        };
        let deployment = |value: Result<(), CatalogError>| match value {
            Err(CatalogError::Deployment(text)) => Some(text),
            _ => None,
        };
        use CatalogOperation::{Chat, ChatBatch, Embeddings};
        use ChatInputCountMode::{ProviderApiExact, Unsupported};
        // Claude counts for free; Google models are as before.
        for model in [
            "gemini-2.5-flash",
            "anthropic/claude-haiku-4-5",
            "anthropic/claude-haiku-4-5@20251001",
        ] {
            assert!(
                parse(&gemini(model, Chat, ProviderApiExact)).is_ok(),
                "{model}"
            );
        }
        for model in [
            "mistralai/codestral-2",
            "xai/grok-4.7",
            "endpoints/1234567890",
        ] {
            assert!(parse(&gemini(model, Chat, Unsupported)).is_ok(), "{model}");
            let text = deployment(parse(&gemini(model, Chat, ProviderApiExact))).unwrap();
            assert!(text.contains("unsupported"), "{text}");
        }
        assert!(deployment(parse(&gemini("google/gemini-2.5-flash", Chat, Unsupported))).is_some());
        let text = deployment(parse(&gemini("ai21/jamba-1.5", Chat, Unsupported))).unwrap();
        assert!(text.contains("`ai21`"), "{text}");
        let text = deployment(parse(&gemini(
            "anthropic/claude-haiku-4-5",
            Embeddings,
            Unsupported,
        )))
        .unwrap();
        assert!(text.contains("only `chat`"), "{text}");
        assert!(deployment(parse(&gemini("endpoints/12", ChatBatch, Unsupported))).is_some());
        assert!(deployment(parse(&gemini("a/b/c", Chat, Unsupported))).is_some());
        // Another family's model identifiers are not Agent Platform's.
        let mut bedrock = single_route(ProviderFamily::Bedrock, Chat, Unsupported);
        bedrock["deployments"][0]["model"] =
            "arn:aws:bedrock:us-east-2:1:custom-model-deployment/x".into();
        assert!(parse(&bedrock).is_ok());
    }

    #[test]
    fn catalog_accepts_exactly_the_operations_each_family_supports() {
        for family in ProviderFamily::ALL {
            for operation in CatalogOperation::ALL {
                let result = parse(&single_route(
                    family,
                    operation,
                    ChatInputCountMode::Unsupported,
                ));
                let label = format!("{} {}", family.as_str(), operation.as_str());
                if family.supports(operation) {
                    assert!(result.is_ok(), "{label}: {result:?}");
                } else {
                    assert!(
                        matches!(result, Err(CatalogError::UnsupportedOperation)),
                        "{label}: {result:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unrouted_unsupported_capability_is_still_rejected() {
        let mut value = single_route(
            ProviderFamily::Anthropic,
            CatalogOperation::Chat,
            ChatInputCountMode::Unsupported,
        );
        let mut embeddings = value["deployments"][0]["capabilities"][0].clone();
        embeddings["operation"] = "embeddings".into();
        value["deployments"][0]["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(embeddings);
        assert!(matches!(
            parse(&value),
            Err(CatalogError::UnsupportedOperation)
        ));
    }

    #[test]
    fn exact_input_counting_is_accepted_only_where_the_adapter_counts() {
        for family in ProviderFamily::ALL {
            for operation in [CatalogOperation::Chat, CatalogOperation::ChatStream] {
                let result = parse(&single_route(
                    family,
                    operation,
                    ChatInputCountMode::ProviderApiExact,
                ));
                let label = format!("{} {}", family.as_str(), operation.as_str());
                if family.counts_chat_input_exactly() {
                    assert!(result.is_ok(), "{label}: {result:?}");
                } else {
                    assert!(
                        matches!(result, Err(CatalogError::InvalidGraph)),
                        "{label}: {result:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn prices_for_unreported_usage_are_rejected() {
        let dimensions = [
            "cached_input_minor_per_token",
            "reasoning_minor_per_token",
            "other_minor_per_token",
        ];
        for family in ProviderFamily::ALL {
            for dimension in dimensions {
                let mut value = single_route(
                    family,
                    CatalogOperation::Chat,
                    ChatInputCountMode::Unsupported,
                );
                value["prices"][0][dimension] = 1.into();
                let unreported = match family {
                    ProviderFamily::Anthropic | ProviderFamily::Bedrock => {
                        dimension == "reasoning_minor_per_token"
                    }
                    _ => false,
                };
                let result = parse(&value);
                let label = format!("{} {dimension}", family.as_str());
                if unreported {
                    assert!(
                        matches!(result, Err(CatalogError::InvalidGraph)),
                        "{label}: {result:?}"
                    );
                } else {
                    assert!(result.is_ok(), "{label}: {result:?}");
                }
            }
        }
    }

    #[test]
    fn an_alias_may_route_an_operation_to_several_deployments_in_one_currency() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        let mut second = value["deployments"][0].clone();
        second["name"] = "chat-b".into();
        value["deployments"].as_array_mut().unwrap().push(second);
        let mut price = value["prices"][0].clone();
        price["deployment"] = "chat-b".into();
        value["prices"].as_array_mut().unwrap().push(price);
        value["routes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"alias":"chat-public","operation":"chat","deployment":"chat-b","weight":3,"priority":2}));
        let (parsed, _) = CatalogManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            parsed
                .routes
                .iter()
                .map(|route| (route.deployment.as_str(), route.weight, route.priority))
                .collect::<Vec<_>>(),
            vec![("chat", 1, 1), ("chat-b", 3, 2)]
        );

        // The same deployment twice is one route listed twice.
        let mut twice = value.clone();
        twice["routes"][1]["deployment"] = "chat".into();
        assert!(matches!(parse(&twice), Err(CatalogError::InvalidGraph)));

        for (field, bad) in [
            ("weight", 0),
            ("weight", 1001),
            ("priority", 0),
            ("priority", 11),
        ] {
            let mut out_of_range = value.clone();
            out_of_range["routes"][1][field] = bad.into();
            let error = parse(&out_of_range).unwrap_err();
            assert!(
                matches!(&error, CatalogError::Route(text) if text.contains(field)),
                "{field} {bad}: {error}"
            );
        }

        let mut currencies = value.clone();
        currencies["prices"][1]["currency"] = "EUR".into();
        let error = parse(&currencies).unwrap_err();
        assert!(error.to_string().contains("one currency"), "{error}");

        let mut many = value.clone();
        for index in 0..MAX_ROUTE_DEPLOYMENTS {
            let name = format!("extra-{index}");
            let mut deployment = many["deployments"][0].clone();
            deployment["name"] = name.clone().into();
            many["deployments"].as_array_mut().unwrap().push(deployment);
            let mut price = many["prices"][0].clone();
            price["deployment"] = name.clone().into();
            many["prices"].as_array_mut().unwrap().push(price);
            many["routes"].as_array_mut().unwrap().push(
                serde_json::json!({"alias":"chat-public","operation":"chat","deployment":name}),
            );
        }
        let error = parse(&many).unwrap_err();
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn a_route_at_the_default_weight_and_priority_keeps_its_digest() {
        let route = RouteManifest::new("chat-public", "chat", "chat");
        assert_eq!(
            serde_json::to_value(&route).unwrap(),
            serde_json::json!({"alias":"chat-public","operation":"chat","deployment":"chat"})
        );
        let mut explicit = serde_json::to_value(manifest()).unwrap();
        explicit["routes"][0]["weight"] = 1.into();
        explicit["routes"][0]["priority"] = 1.into();
        let (_, explicit) =
            CatalogManifest::parse(&serde_json::to_vec(&explicit).unwrap()).unwrap();
        let (_, implicit) =
            CatalogManifest::parse(&serde_json::to_vec(&manifest()).unwrap()).unwrap();
        assert_eq!(explicit, implicit);
    }

    #[test]
    fn checked_manifest_example_is_valid() {
        let bytes = include_bytes!("../../docs/catalog-manifest.example.json");
        CatalogManifest::parse(bytes).unwrap();
    }

    #[test]
    fn tool_support_is_known_for_claude_and_option_refusals_name_the_provider() {
        use crate::domain::inference::{ChatRequest, ToolChoice};
        assert!(takes_tools(ProviderFamily::Anthropic, "claude-opus-4-1"));
        assert!(takes_tools(
            ProviderFamily::Bedrock,
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        ));
        assert!(takes_tools(
            ProviderFamily::Bedrock,
            "anthropic.claude-3-haiku-20240307-v1:0"
        ));
        assert!(!takes_tools(
            ProviderFamily::Bedrock,
            "amazon.nova-pro-v1:0"
        ));
        assert!(!takes_tools(ProviderFamily::OpenAi, "gpt-4o"));
        let none = ChatRequest {
            tool_choice: Some(ToolChoice::None),
            ..Default::default()
        };
        let serial = ChatRequest {
            parallel_tool_calls: Some(false),
            ..Default::default()
        };
        let refusal = |family, model: &str, request: &ChatRequest| {
            ToolWire::of(family, model).option_refusal(request)
        };
        assert!(refusal(ProviderFamily::Bedrock, "amazon.nova-pro-v1:0", &none).is_some());
        assert!(refusal(ProviderFamily::Bedrock, "amazon.nova-pro-v1:0", &serial).is_some());
        assert!(refusal(ProviderFamily::Gemini, "gemini-2.5-pro", &serial).is_some());
        assert!(refusal(ProviderFamily::Gemini, "gemini-2.5-pro", &none).is_none());
        assert!(refusal(ProviderFamily::Anthropic, "claude-sonnet-4-5", &serial).is_none());
        assert!(refusal(ProviderFamily::OpenAi, "gpt-4o", &none).is_none());
        // A partner model on an Agent Platform account follows its
        // publisher's wire, not Gemini's.
        assert!(
            refusal(
                ProviderFamily::Gemini,
                "anthropic/claude-sonnet-4-5@20250929",
                &serial
            )
            .is_none()
        );
        assert!(
            refusal(
                ProviderFamily::Gemini,
                "mistralai/mistral-medium-3",
                &serial
            )
            .is_none()
        );
        assert_eq!(
            ToolWire::of(
                ProviderFamily::Gemini,
                "anthropic/claude-sonnet-4-5@20250929"
            ),
            ToolWire::Anthropic
        );
        assert_eq!(
            ToolWire::of(ProviderFamily::Gemini, "endpoints/123"),
            ToolWire::Endpoint
        );
        assert!(refusal(ProviderFamily::Gemini, "endpoints/123", &serial).is_none());
    }

    #[test]
    fn tool_call_ids_tell_gemini_from_the_others() {
        assert_eq!(
            tool_call_ids("gemini", "gemini-2.5-pro"),
            Some(ToolCallIds::Gemini)
        );
        assert_eq!(
            tool_call_ids("gemini", "google/gemini-3-pro-preview"),
            Some(ToolCallIds::Gemini)
        );
        assert_eq!(
            tool_call_ids("gemini", "anthropic/claude-sonnet-4-5@20250929"),
            Some(ToolCallIds::Plain)
        );
        assert_eq!(
            tool_call_ids("gemini", "endpoints/123"),
            Some(ToolCallIds::Unknown)
        );
        for (family, model) in [
            ("bedrock", "anthropic.claude-sonnet-4-5-20250929-v1:0"),
            ("anthropic", "claude-sonnet-4-5"),
            ("openai", "gpt-4o"),
            ("azure-openai", "gpt-4o"),
            ("azure-ai-foundry", "claude-opus-4-1"),
        ] {
            assert_eq!(
                tool_call_ids(family, model),
                Some(ToolCallIds::Plain),
                "{family}"
            );
        }
        assert_eq!(tool_call_ids("nope", "m"), None);
    }

    #[test]
    fn tools_are_a_chat_capability_left_out_of_the_digest_when_off() {
        let (_, without) =
            CatalogManifest::parse(&serde_json::to_vec(&manifest()).unwrap()).unwrap();
        let mut value = serde_json::to_value(manifest()).unwrap();
        assert!(
            value["deployments"][0]["capabilities"][0]
                .get("tools")
                .is_none()
        );
        value["deployments"][0]["capabilities"][0]["tools"] = false.into();
        let (_, explicit_off) =
            CatalogManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(without, explicit_off);
        value["deployments"][0]["capabilities"][0]["tools"] = true.into();
        assert!(parse(&value).is_ok());
        let mut embeddings = single_route(
            ProviderFamily::OpenAi,
            CatalogOperation::Embeddings,
            ChatInputCountMode::Unsupported,
        );
        embeddings["deployments"][0]["capabilities"][0]["tools"] = true.into();
        assert!(matches!(
            parse(&embeddings),
            Err(CatalogError::InvalidGraph)
        ));
    }
}
