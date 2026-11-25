//! Models a provider account reports it can serve.
//!
//! A provider's model list is untrusted input: identifiers and names are
//! bounded in bytes, as catalog text is, and free of control characters, and a list is bounded in size.
//! Discovery records what a provider offers; it never grants a capability or
//! a price, which stay catalog data. Where a listing states a model's
//! operations, or its model family makes them certain, discovery records
//! them for operators to read; otherwise they are unknown.

use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{
    catalog::CatalogOperation, credentials::validate_adapter_instance_key,
    inference::ProviderErrorCategory,
};

/// Most models one provider account may report.
pub const MAX_DISCOVERED_MODELS: usize = 10_000;
/// Longest model identifier in bytes, matching a catalog deployment's
/// `model`.
pub const MAX_MODEL_ID_BYTES: usize = 255;
const MAX_DISPLAY_NAME_BYTES: usize = 255;
/// Longest text of a deployment's details (format, model, version, SKU).
pub const MAX_DEPLOYMENT_TEXT_BYTES: usize = 128;

/// A model as its provider lists it. `model` is the identifier a catalog
/// deployment names to call it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredModel {
    pub model: String,
    pub display_name: Option<String>,
    /// The operations the model supports, when the listing states them or
    /// the model's family makes them certain; `None` when unknown. Never
    /// `Some` and empty.
    pub operations: Option<Vec<CatalogOperation>>,
    pub availability: ModelAvailability,
    /// What a read-only control-plane check during listing says about
    /// calling it (D-018); `None` when the family checks nothing. Recorded as
    /// evidence, not stored with the listing.
    pub entitlement: Option<crate::domain::model_evidence::Entitlement>,
    /// What the deployment serves and on what terms, where the listing is a
    /// list of the account's own deployments (Azure); `None` otherwise.
    pub deployment: Option<DeploymentDetails>,
}

/// A listed deployment's details as its provider reports them, each bounded
/// and free of control characters; one that is not is left out.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeploymentDetails {
    /// The model's format, which says which API serves it (Azure: `OpenAI`,
    /// `Anthropic`, `xAI`, ...).
    pub format: Option<String>,
    /// The model the deployment serves, which can differ from its name.
    pub model: Option<String>,
    pub version: Option<String>,
    /// The deployment type or SKU (Azure: `GlobalStandard`,
    /// `DataZoneStandard`, `ProvisionedManaged`, ...).
    pub sku: Option<String>,
    /// The capacity bought, in the SKU's unit (Azure: thousands of tokens
    /// per minute for standard SKUs, provisioned throughput units for
    /// provisioned ones).
    pub capacity: Option<i32>,
    /// The content filter policy the deployment applies (Azure's
    /// `raiPolicyName`), when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_filter: Option<String>,
    /// That policy's settings, when the account's policy list could be read
    /// and names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_filter_policy: Option<ContentFilterPolicy>,
}

/// The most filters kept of one content filter policy.
pub const MAX_CONTENT_FILTERS: usize = 64;

/// A content filter policy as the cloud reports it (Azure's `raiPolicies`):
/// its base, its mode and each filter's settings, every text bounded.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentFilterPolicy {
    pub name: String,
    /// The policy it is based on (`Microsoft.DefaultV2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_policy: Option<String>,
    /// How it filters streams (`Default`, `Asynchronous_filter`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Whether the cloud reports it as its own default policy rather than
    /// one the account made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_policy: Option<bool>,
    #[serde(default)]
    pub filters: Vec<ContentFilterRule>,
}

/// One filter of a policy: what it filters, on which side of the call, and
/// how.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentFilterRule {
    /// `Hate`, `Sexual`, `Violence`, `Selfharm`, `Jailbreak`, ...
    pub name: String,
    /// `Prompt` or `Completion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    /// The lowest severity filtered (`Low`, `Medium`, `High`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_threshold: Option<String>,
}

impl ContentFilterPolicy {
    /// Reads one entry of Azure's `{account}/raiPolicies` list: `{"name",
    /// "properties": {"basePolicyName", "mode", "type", "contentFilters":
    /// [{"name", "source", "enabled", "blocking", "severityThreshold"}]}}`.
    /// `None` without a usable name; a filter without one is left out.
    pub fn from_azure(entry: &serde_json::Value) -> Option<Self> {
        let text = |value: Option<&serde_json::Value>| {
            value
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| bounded_text(value, MAX_DEPLOYMENT_TEXT_BYTES))
                .map(ToOwned::to_owned)
        };
        let properties = entry.get("properties");
        let field = |name: &str| properties.and_then(|properties| properties.get(name));
        let filters = field("contentFilters")
            .and_then(serde_json::Value::as_array)
            .map(|filters| {
                filters
                    .iter()
                    .filter_map(|filter| {
                        Some(ContentFilterRule {
                            name: text(filter.get("name"))?,
                            source: text(filter.get("source")),
                            enabled: filter.get("enabled").and_then(serde_json::Value::as_bool),
                            blocking: filter.get("blocking").and_then(serde_json::Value::as_bool),
                            severity_threshold: text(filter.get("severityThreshold")),
                        })
                    })
                    .take(MAX_CONTENT_FILTERS)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            name: text(entry.get("name"))?,
            base_policy: text(field("basePolicyName")),
            mode: text(field("mode")),
            system_policy: text(field("type"))
                .map(|kind| kind.eq_ignore_ascii_case("SystemManaged")),
            filters,
        })
    }
}

impl DeploymentDetails {
    /// `None` when nothing usable is reported.
    pub fn new(
        format: Option<&str>,
        model: Option<&str>,
        version: Option<&str>,
        sku: Option<&str>,
        capacity: Option<i64>,
    ) -> Option<Self> {
        let text = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| bounded_text(value, MAX_DEPLOYMENT_TEXT_BYTES))
                .map(ToOwned::to_owned)
        };
        let details = Self {
            format: text(format),
            model: text(model),
            version: text(version),
            sku: text(sku),
            capacity: capacity
                .filter(|capacity| *capacity >= 0)
                .and_then(|capacity| i32::try_from(capacity).ok()),
            content_filter: None,
            content_filter_policy: None,
        };
        (details != Self::default()).then_some(details)
    }

    /// With the content filter policy the deployment names, if usable.
    pub fn with_content_filter(mut self, name: Option<&str>) -> Self {
        self.content_filter = name
            .map(str::trim)
            .filter(|value| bounded_text(value, MAX_DEPLOYMENT_TEXT_BYTES))
            .map(ToOwned::to_owned);
        self
    }
}

/// Whether a listing says the account can call the model where the gateway
/// calls it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    /// The account's own listing reports it.
    Listed,
    /// The listing is a catalog wider than the account's location (Google's
    /// publisher models on Agent Platform), so it cannot say.
    Unknown,
    /// The account's listing reports it, but only an inference profile can
    /// call it (a Bedrock foundation model without on-demand throughput):
    /// its own identifier is never offered, while its profiles are.
    ProfileOnly,
    /// One of the account's own Agent Platform endpoints (a model it
    /// deployed to dedicated machines) serving what the adapter does not
    /// call, so it is listed for what it serves and never offered. An
    /// endpoint the adapter can call is [`Self::Listed`].
    EndpointOnly,
}

impl ModelAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::Unknown => "unknown",
            Self::ProfileOnly => "profile_only",
            Self::EndpointOnly => "endpoint_only",
        }
    }
}

impl FromStr for ModelAvailability {
    type Err = UnknownValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "listed" => Ok(Self::Listed),
            "unknown" => Ok(Self::Unknown),
            "profile_only" => Ok(Self::ProfileOnly),
            "endpoint_only" => Ok(Self::EndpointOnly),
            _ => Err(UnknownValue),
        }
    }
}

impl DiscoveredModel {
    /// `None` when `model` is empty, too long, or contains a control
    /// character. An unusable display name is dropped rather than rejecting
    /// the model. Its operations are unknown and it is listed.
    pub fn new(model: &str, display_name: Option<&str>) -> Option<Self> {
        if !bounded_text(model, MAX_MODEL_ID_BYTES) || model.trim() != model {
            return None;
        }
        let display_name = display_name
            .map(str::trim)
            .filter(|name| bounded_text(name, MAX_DISPLAY_NAME_BYTES))
            .map(ToOwned::to_owned);
        Some(Self {
            model: model.to_owned(),
            display_name,
            operations: None,
            availability: ModelAvailability::Listed,
            entitlement: None,
            deployment: None,
        })
    }

    /// The model with known operations; an empty list leaves them unknown.
    pub fn with_operations(mut self, operations: &[CatalogOperation]) -> Self {
        self.operations = (!operations.is_empty()).then(|| operations.to_vec());
        self
    }

    pub fn with_entitlement(
        mut self,
        entitlement: Option<crate::domain::model_evidence::Entitlement>,
    ) -> Self {
        self.entitlement = entitlement;
        self
    }

    pub fn with_deployment(mut self, deployment: Option<DeploymentDetails>) -> Self {
        self.deployment = deployment;
        self
    }

    pub fn with_availability(mut self, availability: ModelAvailability) -> Self {
        self.availability = availability;
        self
    }
}

fn bounded_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

/// Most SKUs one deployable model may report.
pub const MAX_DEPLOYABLE_SKUS: usize = 32;

/// A model the provider's catalog lets this account deploy (D-018): Azure's
/// account model list, per format, model and version. Deploying it is the
/// tenant's action; the gateway never creates deployments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeployableModel {
    pub format: String,
    pub model: String,
    pub version: String,
    /// The provider's lifecycle status (Azure: `GenerallyAvailable`,
    /// `Preview`, `Deprecating`, `Legacy`, `Deprecated`); `None` when not
    /// reported.
    pub lifecycle_status: Option<String>,
    /// Whether this is the version a deployment gets by default.
    pub default_version: bool,
    /// The deployment types (SKUs) it can be deployed with.
    pub skus: Vec<String>,
    /// The most capacity one deployment of it may have, in its SKU's unit.
    pub max_capacity: Option<i32>,
    /// When inference on it retires.
    pub retires_at: Option<DateTime<Utc>>,
    /// The operations it supports, when the catalog states them; never
    /// `Some` and empty.
    pub operations: Option<Vec<CatalogOperation>>,
}

impl DeployableModel {
    /// `None` when the format, model or version is empty, too long, or
    /// contains a control character. The rest starts unknown.
    pub fn new(format: &str, model: &str, version: &str) -> Option<Self> {
        let usable = |value: &str, max: usize| bounded_text(value, max) && value.trim() == value;
        if !usable(format, MAX_DEPLOYMENT_TEXT_BYTES)
            || !usable(model, MAX_MODEL_ID_BYTES)
            || !usable(version, MAX_DEPLOYMENT_TEXT_BYTES)
        {
            return None;
        }
        Some(Self {
            format: format.to_owned(),
            model: model.to_owned(),
            version: version.to_owned(),
            lifecycle_status: None,
            default_version: false,
            skus: Vec::new(),
            max_capacity: None,
            retires_at: None,
            operations: None,
        })
    }

    /// Unusable values are dropped: a lifecycle status or SKU name that is
    /// too long or has control characters, a negative or oversized
    /// capacity, SKUs past [`MAX_DEPLOYABLE_SKUS`] or repeated.
    pub fn with_terms(
        mut self,
        lifecycle_status: Option<&str>,
        default_version: bool,
        skus: &[&str],
        max_capacity: Option<i64>,
        retires_at: Option<DateTime<Utc>>,
    ) -> Self {
        self.lifecycle_status = lifecycle_status
            .map(str::trim)
            .filter(|status| bounded_text(status, MAX_DEPLOYMENT_TEXT_BYTES))
            .map(ToOwned::to_owned);
        self.default_version = default_version;
        self.skus = Vec::new();
        for sku in skus.iter().map(|sku| sku.trim()) {
            if self.skus.len() < MAX_DEPLOYABLE_SKUS
                && bounded_text(sku, MAX_DEPLOYMENT_TEXT_BYTES)
                && !self.skus.iter().any(|known| known == sku)
            {
                self.skus.push(sku.to_owned());
            }
        }
        self.max_capacity = max_capacity
            .filter(|capacity| *capacity >= 0)
            .and_then(|capacity| i32::try_from(capacity).ok());
        self.retires_at = retires_at;
        self
    }

    /// The model with known operations; an empty list leaves them unknown.
    pub fn with_operations(mut self, operations: &[CatalogOperation]) -> Self {
        self.operations = (!operations.is_empty()).then(|| operations.to_vec());
        self
    }
}

/// Accumulates a provider's deployable models across pages: a repeated
/// format, model and version keeps its first entry, and the result is
/// ordered by them.
#[derive(Debug, Default)]
pub struct DeployableListBuilder {
    models: BTreeMap<(String, String, String), DeployableModel>,
}

impl DeployableListBuilder {
    pub fn push(&mut self, model: DeployableModel) -> Result<(), TooManyModels> {
        let key = (
            model.format.clone(),
            model.model.clone(),
            model.version.clone(),
        );
        if !self.models.contains_key(&key) && self.models.len() >= MAX_DISCOVERED_MODELS {
            return Err(TooManyModels);
        }
        self.models.entry(key).or_insert(model);
        Ok(())
    }

    pub fn finish(self) -> Vec<DeployableModel> {
        self.models.into_values().collect()
    }
}

/// A model an account may deploy, and the account's listed deployments that
/// serve it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployableModelView {
    pub adapter_instance_key: String,
    pub provider_family: String,
    pub format: String,
    pub model: String,
    pub version: String,
    pub lifecycle_status: Option<String>,
    pub default_version: bool,
    pub skus: Vec<String>,
    pub max_capacity: Option<i32>,
    pub retires_at: Option<DateTime<Utc>>,
    pub operations: Option<Vec<String>>,
    /// Names of the account's listed deployments serving this format, model
    /// and version; empty when it has none, which deploying it would fix.
    pub deployments: Vec<String>,
    /// When the latest listing reported it.
    pub listed_at: DateTime<Utc>,
}

/// Filters for the deployable-model collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeployableModelQuery {
    pub adapter_instance_key: Option<String>,
    /// Only models some listed deployment serves (`true`) or none does.
    pub deployed: Option<bool>,
    /// Only models in this lifecycle status, compared without case.
    pub lifecycle_status: Option<String>,
    /// Only models stating this operation.
    pub operation: Option<CatalogOperation>,
}

impl DeployableModelQuery {
    pub fn new(
        adapter_instance_key: Option<String>,
        deployed: Option<bool>,
        lifecycle_status: Option<String>,
        operation: Option<&str>,
    ) -> Result<Self, DiscoveryQueryError> {
        if let Some(key) = adapter_instance_key.as_deref() {
            validate_adapter_instance_key(key)
                .map_err(|_| DiscoveryQueryError::AdapterInstanceKey)?;
        }
        if let Some(status) = lifecycle_status.as_deref()
            && !bounded_text(status, MAX_DEPLOYMENT_TEXT_BYTES)
        {
            return Err(DiscoveryQueryError::LifecycleStatus);
        }
        let operation = operation
            .map(|operation| {
                operation
                    .parse::<CatalogOperation>()
                    .map_err(|_| DiscoveryQueryError::Operation)
            })
            .transpose()?;
        Ok(Self {
            adapter_instance_key,
            deployed,
            lifecycle_status,
            operation,
        })
    }

    /// The canonical filter string a page token is bound to.
    pub fn canonical(&self) -> String {
        serde_json::json!([
            self.adapter_instance_key,
            self.deployed,
            self.lifecycle_status.as_deref().map(str::to_lowercase),
            self.operation.map(CatalogOperation::as_str),
        ])
        .to_string()
    }
}

/// Accumulates a provider's model list across pages: unusable entries are
/// skipped, a repeated identifier keeps its first display name, and the
/// result is ordered by identifier.
#[derive(Debug, Default)]
pub struct ModelListBuilder {
    models: BTreeMap<String, DiscoveredModel>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the provider reported more than {MAX_DISCOVERED_MODELS} models")]
pub struct TooManyModels;

impl ModelListBuilder {
    pub fn push(&mut self, model: &str, display_name: Option<&str>) -> Result<(), TooManyModels> {
        match DiscoveredModel::new(model, display_name) {
            Some(model) => self.push_model(model),
            None => Ok(()),
        }
    }

    /// Adds a model already checked by [`DiscoveredModel::new`].
    pub fn push_model(&mut self, model: DiscoveredModel) -> Result<(), TooManyModels> {
        if !self.models.contains_key(&model.model) && self.models.len() >= MAX_DISCOVERED_MODELS {
            return Err(TooManyModels);
        }
        self.models.entry(model.model.clone()).or_insert(model);
        Ok(())
    }

    pub fn finish(self) -> Vec<DiscoveredModel> {
        self.models.into_values().collect()
    }
}

/// Why a sync job ran.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncTrigger {
    Scheduled,
    Manual,
}

impl SyncTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }
}

impl FromStr for SyncTrigger {
    type Err = UnknownValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "scheduled" => Ok(Self::Scheduled),
            "manual" => Ok(Self::Manual),
            _ => Err(UnknownValue),
        }
    }
}

/// Where a sync job stands. `Unsupported` finishes a job whose provider
/// family has no listing API usable with inference credentials.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Unsupported,
}

impl SyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }
}

impl FromStr for SyncStatus {
    type Err = UnknownValue;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "unsupported" => Ok(Self::Unsupported),
            _ => Err(UnknownValue),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("stored discovery value is not recognized")]
pub struct UnknownValue;

/// The fixed code a failed job records. Provider messages are never stored,
/// since they may echo request details.
pub fn error_code(category: ProviderErrorCategory) -> &'static str {
    match category {
        ProviderErrorCategory::Authentication => "authentication",
        ProviderErrorCategory::Authorization => "authorization",
        ProviderErrorCategory::InvalidRequest => "invalid_request",
        ProviderErrorCategory::UnsupportedFeature => "unsupported_feature",
        ProviderErrorCategory::RateLimited => "rate_limited",
        ProviderErrorCategory::Unavailable => "unavailable",
        ProviderErrorCategory::Timeout => "timeout",
        ProviderErrorCategory::Cancelled => "cancelled",
        ProviderErrorCategory::Provider => "provider",
        ProviderErrorCategory::Protocol => "protocol",
        ProviderErrorCategory::Internal => "internal",
    }
}

/// Neither a stored credential nor process configuration serves the account.
pub const ERROR_NOT_CONFIGURED: &str = "not_configured";
/// Every attempt's worker stopped before finishing the job.
pub const ERROR_ATTEMPTS_EXHAUSTED: &str = "attempts_exhausted";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncJobView {
    pub id: Uuid,
    pub adapter_instance_key: String,
    pub provider_family: String,
    pub trigger: SyncTrigger,
    pub status: SyncStatus,
    pub attempts: u32,
    pub error_code: Option<String>,
    /// Counts from a successful listing: models reported, models reported
    /// for the first time, and previously reported models now absent.
    pub models_listed: Option<u32>,
    pub models_added: Option<u32>,
    pub models_missing: Option<u32>,
    pub requested_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// A manual sync request for one provider account in the active release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncJobRequest {
    pub adapter_instance_key: String,
}

impl SyncJobRequest {
    pub fn validate(&self) -> Result<(), DiscoveryQueryError> {
        validate_adapter_instance_key(&self.adapter_instance_key)
            .map_err(|_| DiscoveryQueryError::AdapterInstanceKey)
    }
}

/// A model a provider account has listed, and whether a deployment in the
/// active release names it on that account.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveredModelView {
    pub adapter_instance_key: String,
    pub provider_family: String,
    pub model: String,
    pub display_name: Option<String>,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub missing_since: Option<DateTime<Utc>>,
    pub deployed: bool,
    /// What the listing says the model supports; `None` when unknown.
    pub operations: Option<Vec<String>>,
    pub availability: ModelAvailability,
    /// What the gateway itself has seen about calling it on this account
    /// (D-018); `None` when nothing yet.
    pub evidence: Option<crate::domain::model_evidence::EvidenceView>,
    /// The listed deployment's details (Azure); `None` for listings of
    /// models rather than deployments.
    pub deployment: Option<DeploymentDetails>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingState {
    /// Reported by the latest successful listing.
    Listed,
    /// Reported before but absent from a later successful listing.
    Missing,
}

/// Filters for the discovered-model collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveredModelQuery {
    pub adapter_instance_key: Option<String>,
    pub state: Option<ListingState>,
    pub deployed: Option<bool>,
    /// Only models with one of these evidence states (D-018), or with
    /// none; never empty.
    pub evidence: Option<Vec<EvidenceFilter>>,
}

/// The evidence a discovered model may be filtered by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceFilter {
    State(crate::domain::model_evidence::EvidenceState),
    /// No evidence yet.
    None,
}

impl EvidenceFilter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::State(state) => state.as_str(),
            Self::None => "none",
        }
    }
}

impl DiscoveredModelQuery {
    pub fn new(
        adapter_instance_key: Option<String>,
        state: Option<&str>,
        deployed: Option<bool>,
    ) -> Result<Self, DiscoveryQueryError> {
        Self::filtered(adapter_instance_key, state, deployed, None)
    }

    pub fn filtered(
        adapter_instance_key: Option<String>,
        state: Option<&str>,
        deployed: Option<bool>,
        evidence: Option<&str>,
    ) -> Result<Self, DiscoveryQueryError> {
        // A comma-separated set, kept sorted and without repeats so a
        // page token does not depend on how it was written.
        let evidence = evidence
            .map(|values| {
                let mut filters = values
                    .split(',')
                    .map(|value| match value {
                        "none" => Ok(EvidenceFilter::None),
                        value => value
                            .parse()
                            .map(EvidenceFilter::State)
                            .map_err(|_| DiscoveryQueryError::Evidence),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                filters.sort_by_key(|filter| filter.as_str());
                filters.dedup();
                Ok(filters)
            })
            .transpose()?;
        if let Some(key) = adapter_instance_key.as_deref() {
            validate_adapter_instance_key(key)
                .map_err(|_| DiscoveryQueryError::AdapterInstanceKey)?;
        }
        let state = match state {
            None => None,
            Some("listed") => Some(ListingState::Listed),
            Some("missing") => Some(ListingState::Missing),
            Some(_) => return Err(DiscoveryQueryError::State),
        };
        Ok(Self {
            adapter_instance_key,
            state,
            deployed,
            evidence,
        })
    }

    /// The canonical filter string a page token is bound to. JSON keeps a
    /// key containing separators from matching another filter set.
    pub fn canonical(&self) -> String {
        serde_json::json!([
            self.adapter_instance_key,
            self.state,
            self.deployed,
            self.evidence.as_ref().map(|filters| filters
                .iter()
                .map(|filter| filter.as_str())
                .collect::<Vec<_>>()),
        ])
        .to_string()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DiscoveryQueryError {
    #[error("adapter_instance_key must be 1-128 bytes without control characters")]
    AdapterInstanceKey,
    #[error("state must be listed or missing")]
    State,
    #[error(
        "evidence must be a comma-separated set of verified, refused, entitled, requires_action and none"
    )]
    Evidence,
    #[error("lifecycle_status must be 1-128 bytes without control characters")]
    LifecycleStatus,
    #[error("operation must be chat, chat_stream or embeddings")]
    Operation,
}

/// How the active release compares with what its provider accounts list.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveryDrift {
    pub release_id: Uuid,
    pub accounts: Vec<AccountDrift>,
}

/// One provider account of the active release. Until a listing of the
/// account has succeeded, its drift is unknown and both drift fields are
/// null.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountDrift {
    pub adapter_instance_key: String,
    pub provider_family: String,
    pub last_job: Option<SyncJobView>,
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Listed models no deployment names.
    pub undeployed_models: Option<u64>,
    /// Deployments whose model the account no longer lists or never listed.
    pub unlisted_deployments: Option<Vec<UnlistedDeployment>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnlistedDeployment {
    pub deployment: String,
    pub model: String,
    /// When the account stopped listing the model; null if it never did.
    pub missing_since: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unusable_identifiers_are_skipped_and_names_dropped() {
        assert!(DiscoveredModel::new("", None).is_none());
        assert!(DiscoveredModel::new(" gpt", None).is_none());
        assert!(DiscoveredModel::new("gpt\n4", None).is_none());
        assert!(DiscoveredModel::new(&"m".repeat(256), None).is_none());
        assert!(DiscoveredModel::new(&"é".repeat(128), None).is_none());
        let model = DiscoveredModel::new(&"m".repeat(255), Some("bad\u{7}name")).unwrap();
        assert_eq!(model.display_name, None);
        let model = DiscoveredModel::new("gpt-4o", Some("  GPT-4o ")).unwrap();
        assert_eq!(model.display_name.as_deref(), Some("GPT-4o"));
    }

    #[test]
    fn builder_orders_deduplicates_and_bounds() {
        let mut builder = ModelListBuilder::default();
        builder.push("b", Some("first")).unwrap();
        builder.push("a", None).unwrap();
        builder.push("b", Some("second")).unwrap();
        builder.push("", None).unwrap();
        assert_eq!(
            builder.finish(),
            vec![
                DiscoveredModel::new("a", None).unwrap(),
                DiscoveredModel::new("b", Some("first")).unwrap(),
            ]
        );

        let mut builder = ModelListBuilder::default();
        for index in 0..MAX_DISCOVERED_MODELS {
            builder.push(&format!("m{index}"), None).unwrap();
        }
        builder.push("m0", None).unwrap();
        assert_eq!(builder.push("one-more", None), Err(TooManyModels));
    }

    #[test]
    fn stored_values_round_trip() {
        for status in [
            SyncStatus::Queued,
            SyncStatus::Running,
            SyncStatus::Succeeded,
            SyncStatus::Failed,
            SyncStatus::Unsupported,
        ] {
            assert_eq!(status.as_str().parse(), Ok(status));
        }
        for trigger in [SyncTrigger::Scheduled, SyncTrigger::Manual] {
            assert_eq!(trigger.as_str().parse(), Ok(trigger));
        }
        assert_eq!("paused".parse::<SyncStatus>(), Err(UnknownValue));
        for availability in [
            ModelAvailability::Listed,
            ModelAvailability::Unknown,
            ModelAvailability::ProfileOnly,
            ModelAvailability::EndpointOnly,
        ] {
            assert_eq!(availability.as_str().parse(), Ok(availability));
        }
        assert_eq!("maybe".parse::<ModelAvailability>(), Err(UnknownValue));
    }

    #[test]
    fn operations_are_known_or_absent_never_empty() {
        let model = DiscoveredModel::new("gemini-2.5-flash", None).unwrap();
        assert_eq!(model.operations, None);
        assert_eq!(model.availability, ModelAvailability::Listed);
        assert_eq!(model.clone().with_operations(&[]).operations, None);
        let model = model
            .with_operations(&[CatalogOperation::Embeddings])
            .with_availability(ModelAvailability::Unknown);
        assert_eq!(model.operations, Some(vec![CatalogOperation::Embeddings]));
        assert_eq!(model.availability, ModelAvailability::Unknown);
    }

    #[test]
    fn model_filters_are_validated_and_canonical() {
        assert_eq!(
            DiscoveredModelQuery::new(Some("bad\nkey".into()), None, None),
            Err(DiscoveryQueryError::AdapterInstanceKey)
        );
        assert_eq!(
            DiscoveredModelQuery::new(None, Some("gone"), None),
            Err(DiscoveryQueryError::State)
        );
        let query =
            DiscoveredModelQuery::new(Some("openai-main".into()), Some("missing"), Some(true))
                .unwrap();
        assert_eq!(query.canonical(), r#"["openai-main","missing",true,null]"#);
        assert_ne!(
            query.canonical(),
            DiscoveredModelQuery::default().canonical()
        );
        assert_eq!(
            DiscoveredModelQuery::filtered(None, None, None, Some("maybe")),
            Err(DiscoveryQueryError::Evidence)
        );
        let none = DiscoveredModelQuery::filtered(None, None, None, Some("none")).unwrap();
        let refused = DiscoveredModelQuery::filtered(None, None, None, Some("refused")).unwrap();
        assert_eq!(none.canonical(), r#"[null,null,null,["none"]]"#);
        assert_eq!(refused.canonical(), r#"[null,null,null,["refused"]]"#);
        // A set: order and repeats do not matter.
        let set =
            DiscoveredModelQuery::filtered(None, None, None, Some("none,verified,entitled,none"))
                .unwrap();
        assert_eq!(
            set.canonical(),
            r#"[null,null,null,["entitled","none","verified"]]"#
        );
        for invalid in ["", "verified,", "verified,maybe"] {
            assert_eq!(
                DiscoveredModelQuery::filtered(None, None, None, Some(invalid)),
                Err(DiscoveryQueryError::Evidence),
                "{invalid}"
            );
        }

        assert_eq!(
            DeployableModelQuery::new(None, None, None, Some("generate")),
            Err(DiscoveryQueryError::Operation)
        );
        assert_eq!(
            DeployableModelQuery::new(None, None, Some("bad\nstatus".into()), None),
            Err(DiscoveryQueryError::LifecycleStatus)
        );
        // Lifecycle compares without case, so its token does too.
        let upper =
            DeployableModelQuery::new(None, Some(false), Some("Preview".into()), Some("chat"))
                .unwrap();
        let lower =
            DeployableModelQuery::new(None, Some(false), Some("preview".into()), Some("chat"))
                .unwrap();
        assert_eq!(upper.canonical(), lower.canonical());
        assert_eq!(upper.canonical(), r#"[null,false,"preview","chat"]"#);
    }

    /// The contract's schemas list exactly the fields a response carries.
    #[test]
    fn discovered_models_match_the_contract() {
        use crate::domain::model_evidence::{
            EvidenceReason, EvidenceSource, EvidenceState, EvidenceView,
        };
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let fields = |value: &serde_json::Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort_unstable();
            keys
        };
        let required = |schema: &str| {
            let mut names: Vec<String> = contract["components"]["schemas"][schema]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
            names.sort_unstable();
            names
        };
        let now = Utc::now();
        let evidence = EvidenceView {
            state: EvidenceState::Refused,
            reason: Some(EvidenceReason::AccessDenied),
            source: EvidenceSource::Traffic,
            deployment: Some("d".into()),
            provider_status: Some(403),
            provider_code: Some("AccessDeniedException".into()),
            observed_at: now,
            verified_at: None,
        };
        let model = DiscoveredModelView {
            adapter_instance_key: "k".into(),
            provider_family: "bedrock".into(),
            model: "m".into(),
            display_name: None,
            first_seen_at: now,
            last_seen_at: now,
            missing_since: None,
            deployed: false,
            operations: None,
            availability: ModelAvailability::Listed,
            evidence: Some(evidence.clone()),
            deployment: DeploymentDetails::new(None, None, None, Some("GlobalStandard"), Some(50)),
        };
        assert_eq!(
            fields(&serde_json::to_value(&model).unwrap()),
            required("DiscoveredModel")
        );
        assert_eq!(
            fields(&serde_json::to_value(&evidence).unwrap()),
            required("ModelEvidence")
        );
        let reasons =
            contract["components"]["schemas"]["ModelEvidence"]["properties"]["reason"]["enum"]
                .as_array()
                .unwrap()
                .clone();
        for reason in [EvidenceReason::AccessDenied, EvidenceReason::NotFound] {
            assert!(reasons.contains(&serde_json::json!(reason.as_str())));
        }
        assert_eq!(
            fields(&serde_json::to_value(model.deployment.as_ref().unwrap()).unwrap()),
            required("DiscoveredDeployment")
        );
    }

    #[test]
    fn deployable_models_match_the_contract_and_keep_usable_terms() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let required = contract["components"]["schemas"]["DeployableModel"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let view = DeployableModelView {
            adapter_instance_key: "k".into(),
            provider_family: "azure-ai-foundry".into(),
            format: "Anthropic".into(),
            model: "claude-opus-5-5".into(),
            version: "2".into(),
            lifecycle_status: None,
            default_version: true,
            skus: vec![],
            max_capacity: None,
            retires_at: None,
            operations: None,
            deployments: vec![],
            listed_at: Utc::now(),
        };
        let fields = serde_json::to_value(&view)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(fields, required);

        assert_eq!(DeployableModel::new("OpenAI", "gpt", ""), None);
        assert_eq!(DeployableModel::new(" OpenAI", "gpt", "1"), None);
        let many: Vec<String> = (0..40).map(|i| format!("sku-{i}")).collect();
        let mut skus: Vec<&str> = vec!["GlobalStandard", "GlobalStandard", "bad\u{7}"];
        skus.extend(many.iter().map(String::as_str));
        let model = DeployableModel::new("OpenAI", "gpt", "1")
            .unwrap()
            .with_terms(Some(" Preview "), true, &skus, Some(-5), None);
        assert_eq!(model.lifecycle_status.as_deref(), Some("Preview"));
        assert_eq!(model.skus.len(), MAX_DEPLOYABLE_SKUS);
        assert_eq!(model.skus[..2], ["GlobalStandard", "sku-0"]);
        assert_eq!(model.max_capacity, None);
    }

    #[test]
    fn deployment_details_keep_only_usable_values() {
        assert_eq!(DeploymentDetails::new(None, None, None, None, None), None);
        assert_eq!(
            DeploymentDetails::new(Some(" "), Some("a\nb"), None, None, Some(-1)),
            None
        );
        let long = "x".repeat(MAX_DEPLOYMENT_TEXT_BYTES + 1);
        assert_eq!(
            DeploymentDetails::new(
                Some("OpenAI"),
                Some(&long),
                Some(" 1 "),
                Some("GlobalStandard"),
                Some(i64::from(i32::MAX) + 1)
            ),
            Some(DeploymentDetails {
                format: Some("OpenAI".into()),
                model: None,
                version: Some("1".into()),
                sku: Some("GlobalStandard".into()),
                capacity: None,
                ..DeploymentDetails::default()
            })
        );
    }
}
