//! Automatic model offers: the catalog an opted-in provider account's
//! listing and the published prices make.
//!
//! After each successful listing of an opted-in account, the planner takes
//! the active release and offers every listed model it can price: a
//! deployment named as the admin console names one, its prices, and a route
//! per operation whose alias is the model identifier. Prices come only from
//! a price source: the item the deployment is mapped to, otherwise a price
//! published for that exact model. A model without one is recorded as
//! needing a price and never guessed at. An offered deployment's price
//! then follows its source through automatic price adoption (D-024), which
//! compares it as it was priced (by mapping, model or listing name), so the
//! planner never reprices one itself. Offered deployments are removed when
//! the listing no longer reports the model; deployments an admin added are left alone, and an
//! offered deployment an admin's release dropped is withdrawn rather than
//! offered again.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    catalog::{
        CapabilityManifest, CatalogManifest, CatalogOperation, DeploymentManifest,
        MAX_MANIFEST_ITEMS, PriceManifest, ProviderFamily, RouteManifest, takes_tools,
    },
    discovery::ModelAvailability,
    model_evidence::EvidenceReason,
    pricing::{
        ObservedPrice, PriceDimension, PriceMappingRow, SourceObservations, minor_exponent,
        money_units_per_token, published_for,
    },
    public_prices::{AWS_GLOBAL_SUFFIX, AWS_LONG_CONTEXT_SUFFIX, AWS_PRICE_LIST_SOURCE},
};

/// The organization has no active release.
pub const ERROR_NO_ACTIVE_RELEASE: &str = "no_active_release";
/// The active release does not declare the account with the listed family.
pub const ERROR_ACCOUNT_NOT_DECLARED: &str = "account_not_declared";
/// The planned release failed the catalog's checks.
pub const ERROR_INVALID_RELEASE: &str = "invalid_release";
/// An unrevoked price override cannot be billed by the planned release.
pub const ERROR_INCOMPATIBLE_PRICE_OVERRIDE: &str = "incompatible_price_override";

/// Limits of an offered deployment's capabilities, as the admin console
/// gives a deployment it adds.
const MAX_INPUT_BYTES: u64 = 1_048_576;
const MAX_OUTPUT_TOKENS: u32 = 8192;
const MAX_TOKENS_PER_BYTE: u32 = 1;
const RESERVATION_TTL_SECONDS: u64 = 300;
const MAX_LOCATION_BYTES: usize = 255;
const MAX_SOURCE_TEXT: usize = 128;

/// What the latest run decided for a listed model.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfferStatus {
    /// In the active release, priced by a source.
    Offered,
    /// No source prices it; map its deployment to a published item.
    NeedsPrice,
    /// Not a chat or embedding model the account's adapter can call, or the
    /// listing cannot say the account may call it.
    Unsupported,
    /// An admin's release dropped its offered deployment.
    Withdrawn,
    /// The release has no room for another deployment.
    CatalogFull,
    /// The account's evidence says it cannot call the model (D-018): a call
    /// was refused for it. Not offered, and an offered deployment is removed,
    /// until evidence verifies it again.
    Refused,
    /// A read-only check during discovery found something the tenant must do
    /// before the account can call the model (D-018), such as accept its
    /// agreement. Not offered, and an offered deployment is removed, until a
    /// check or a call shows the account may call it.
    RequiresAction,
    /// The listing could not check whether the account may call the model,
    /// where a first call could subscribe the account (Bedrock's Marketplace
    /// models), and no evidence says it may: not offered until a listing
    /// checks it (D-035). One already offered stays offered.
    Unchecked,
}

impl OfferStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offered => "offered",
            Self::NeedsPrice => "needs_price",
            Self::Unsupported => "unsupported",
            Self::Withdrawn => "withdrawn",
            Self::CatalogFull => "catalog_full",
            Self::Refused => "refused",
            Self::RequiresAction => "requires_action",
            Self::Unchecked => "unchecked",
        }
    }
}

impl FromStr for OfferStatus {
    type Err = UnknownStatus;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "offered" => Ok(Self::Offered),
            "needs_price" => Ok(Self::NeedsPrice),
            "unsupported" => Ok(Self::Unsupported),
            "withdrawn" => Ok(Self::Withdrawn),
            "catalog_full" => Ok(Self::CatalogFull),
            "refused" => Ok(Self::Refused),
            "requires_action" => Ok(Self::RequiresAction),
            "unchecked" => Ok(Self::Unchecked),
            _ => Err(UnknownStatus),
        }
    }
}

#[derive(Debug)]
pub struct UnknownStatus;

impl fmt::Display for UnknownStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown offer status")
    }
}

impl std::error::Error for UnknownStatus {}

/// The body of an opt-in.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AutoOfferInput {
    /// The location offered deployments declare, which also picks regional
    /// prices (the AWS region of a Bedrock account). Without one, the
    /// account's existing deployments' location.
    #[serde(default)]
    pub location: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct InvalidLocation;

impl AutoOfferInput {
    pub fn validate(self) -> Result<Self, InvalidLocation> {
        if let Some(location) = &self.location {
            if location.is_empty()
                || location.len() > MAX_LOCATION_BYTES
                || location.trim() != location
                || location.chars().any(char::is_control)
            {
                return Err(InvalidLocation);
            }
        }
        Ok(self)
    }
}

/// An opt-in and what its latest run decided.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutoOfferView {
    pub adapter_instance_key: String,
    pub location: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The latest run, the release active after it, or why it failed.
    pub applied_at: Option<DateTime<Utc>>,
    pub applied_release_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub version: u64,
    pub models: Vec<OfferedModelView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OfferedModelView {
    pub model: String,
    pub deployment: String,
    pub status: OfferStatus,
    pub price_source: Option<String>,
    pub price_item: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// The deployment a model of account `key` gets, as the admin console names
/// it. Deployment names appear in URL paths, so anything but letters,
/// digits, `.`, `_`, `:` and `-` becomes one dash per run.
pub fn deployment_name(key: &str, model: &str) -> String {
    let mut name = format!("{key}-");
    let mut dashed = false;
    for c in model.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
            name.push(c);
            dashed = false;
        } else if !dashed {
            name.push('-');
            dashed = true;
        }
    }
    truncate(&name, 255).to_owned()
}

/// The name a model gets when its [`deployment_name`] is taken by another
/// model's deployment: the name cut short, then a dash and eight hex digits
/// of the model identifier's digest, so it stays the same on every run.
pub fn distinct_deployment_name(key: &str, model: &str) -> String {
    let digest = Sha256::digest(model.as_bytes());
    let suffix: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{suffix}",
        truncate(&deployment_name(key, model), 255 - 9)
    )
}

/// `text` cut to at most `max` bytes on a character boundary.
fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A model the account's latest listing recorded.
#[derive(Clone, Debug)]
pub struct ListedModel {
    pub model: String,
    /// The name the listing gives the model.
    pub display_name: Option<String>,
    pub operations: Option<Vec<CatalogOperation>>,
    pub availability: ModelAvailability,
    /// The latest listing no longer reports it.
    pub missing: bool,
    /// What the account's evidence says stands between it and the model,
    /// when something does.
    pub blocked: Option<Blocked>,
    /// The latest listing could not check the account's entitlement, where
    /// a call could subscribe it, and no evidence shows the account may call
    /// the model: it is deferred, unless already offered.
    pub unchecked: bool,
}

/// Why the account's evidence keeps a listed model from being offered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Blocked {
    /// A call was refused for it.
    Refused(EvidenceReason),
    /// A check found something the tenant must do first.
    RequiresAction(EvidenceReason),
}

/// A model the previous run decided on.
#[derive(Clone, Debug)]
pub struct PreviousOffer {
    pub model: String,
    pub deployment: String,
    pub status: OfferStatus,
    pub price_source: Option<String>,
    pub price_item: Option<String>,
}

pub struct PlanInput<'a> {
    pub adapter_instance_key: &'a str,
    pub family: ProviderFamily,
    pub location: Option<&'a str>,
    pub active: &'a CatalogManifest,
    pub listed: &'a [ListedModel],
    pub previous: &'a [PreviousOffer],
    /// Another release was activated since the previous run.
    pub catalog_changed: bool,
    /// The current snapshots of the family's sources.
    pub sources: &'a [SourceObservations],
    pub mappings: &'a [PriceMappingRow],
    pub now: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedModel {
    pub model: String,
    pub deployment: String,
    pub status: OfferStatus,
    pub price_source: Option<String>,
    pub price_item: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    /// The next release, when the catalog changes.
    pub manifest: Option<CatalogManifest>,
    pub models: Vec<PlannedModel>,
    pub added: u32,
    /// Offered deployments whose price or location changed.
    pub updated: u32,
    pub removed: u32,
}

/// A source's price for one deployment, per token.
#[derive(Clone, Debug, Eq, PartialEq)]
struct OfferedPrice {
    source: String,
    item: String,
    currency: String,
    input: i64,
    output: i64,
    cached_input: i64,
    reasoning: i64,
    other: i64,
    /// Every amount is the published one; otherwise some were rounded up or
    /// charged at a higher dimension's price.
    exact: bool,
}

/// Plans the next release. `Err` carries [`ERROR_INVALID_RELEASE`] when the
/// planned release fails the catalog's checks.
pub fn plan(input: &PlanInput<'_>) -> Result<Plan, &'static str> {
    let key = input.adapter_instance_key;
    let mut manifest = input.active.clone();
    // The next release has no `chat_stream` (D-020), whatever the active one
    // stored before it had.
    manifest.fold_streaming_into_chat();
    let location = input.location.map(str::to_owned).or_else(|| {
        let mut existing: Vec<&DeploymentManifest> = manifest
            .deployments
            .iter()
            .filter(|d| d.adapter_instance_key == key)
            .collect();
        existing.sort_by(|a, b| a.name.cmp(&b.name));
        existing.into_iter().find_map(|d| d.location.clone())
    });
    let deployed: HashMap<String, String> = manifest
        .deployments
        .iter()
        .map(|d| (d.name.clone(), d.model.clone()))
        .collect();
    // Offered deployments this run manages, by model: those an admin's
    // release has since dropped are withdrawn instead.
    let mut managed: HashMap<String, String> = HashMap::new();
    let mut withdrawn: HashMap<String, String> = HashMap::new();
    for previous in input.previous {
        match previous.status {
            OfferStatus::Offered
                if deployed.contains_key(&previous.deployment) || !input.catalog_changed =>
            {
                managed.insert(previous.model.clone(), previous.deployment.clone());
            }
            OfferStatus::Offered | OfferStatus::Withdrawn => {
                withdrawn.insert(previous.model.clone(), previous.deployment.clone());
            }
            _ => {}
        }
    }
    let mut models = Vec::new();
    let (mut added, mut updated, mut removed) = (0, 0, 0);
    let mut listed: Vec<&ListedModel> = input.listed.iter().collect();
    listed.sort_by(|a, b| a.model.cmp(&b.model));
    let reported: HashSet<&str> = listed
        .iter()
        .filter(|model| !model.missing)
        .map(|model| model.model.as_str())
        .collect();

    // Offered deployments whose model is no longer listed leave first, so
    // their room is free for new ones. A withdrawal outlives the listing,
    // so a model listed again later stays withdrawn.
    for (model, deployment) in &managed {
        if !reported.contains(model.as_str()) {
            remove_deployment(&mut manifest, deployment);
            removed += 1;
        }
    }
    let mut gone: Vec<(&String, &String)> = withdrawn
        .iter()
        .filter(|(model, _)| !reported.contains(model.as_str()))
        .collect();
    gone.sort();
    for (model, deployment) in gone {
        models.push(PlannedModel {
            model: model.clone(),
            deployment: deployment.clone(),
            status: OfferStatus::Withdrawn,
            price_source: None,
            price_item: None,
        });
    }
    // Which model each deployment name belongs to: the release's, those
    // this run decides on, and those the opt-in already records, so two
    // models never share one name.
    // A model keeps the name the opt-in recorded for it, so a mapping saved
    // for a model that needs a price keeps applying.
    let recorded: HashMap<&str, &str> = input
        .previous
        .iter()
        .map(|p| (p.model.as_str(), p.deployment.as_str()))
        .collect();
    let mut claimed: HashMap<String, String> = deployed.clone();
    for previous in input.previous {
        claimed
            .entry(previous.deployment.clone())
            .or_insert_with(|| previous.model.clone());
    }
    for (model, deployment) in managed.iter().chain(withdrawn.iter()) {
        claimed.insert(deployment.clone(), model.clone());
    }

    for listed in listed.into_iter().filter(|model| !model.missing) {
        if let Some(deployment) = withdrawn.get(&listed.model) {
            models.push(PlannedModel {
                model: listed.model.clone(),
                deployment: deployment.clone(),
                status: OfferStatus::Withdrawn,
                price_source: None,
                price_item: None,
            });
            continue;
        }
        let managed_name = managed.get(&listed.model).cloned();
        let is_managed = managed_name.is_some();
        let deployment = match managed_name {
            Some(name) => name,
            None => {
                let name = recorded.get(listed.model.as_str()).map_or_else(
                    || deployment_name(key, &listed.model),
                    |name| (*name).to_owned(),
                );
                match claimed.get(&name) {
                    // A deployment of that name an admin added for this
                    // model is theirs.
                    Some(model) if *model == listed.model && deployed.contains_key(&name) => {
                        continue;
                    }
                    Some(model) if *model != listed.model => {
                        let distinct = distinct_deployment_name(key, &listed.model);
                        if claimed
                            .get(&distinct)
                            .is_some_and(|model| *model != listed.model)
                        {
                            models.push(PlannedModel {
                                model: listed.model.clone(),
                                deployment: distinct,
                                status: OfferStatus::Unsupported,
                                price_source: None,
                                price_item: None,
                            });
                            continue;
                        }
                        if deployed.contains_key(&distinct) {
                            continue;
                        }
                        distinct
                    }
                    _ => name,
                }
            }
        };
        claimed.insert(deployment.clone(), listed.model.clone());
        let record = |status, price: Option<(&str, &str)>| PlannedModel {
            model: listed.model.clone(),
            deployment: deployment.clone(),
            status,
            price_source: price.map(|(source, _)| source.to_owned()),
            price_item: price.map(|(_, item)| item.to_owned()),
        };
        let operations = operations(input.family, listed);
        if listed.availability != ModelAvailability::Listed || operations.is_empty() {
            if is_managed {
                remove_deployment(&mut manifest, &deployment);
                removed += 1;
            }
            models.push(record(OfferStatus::Unsupported, None));
            continue;
        }
        if let Some(blocked) = listed.blocked {
            if is_managed {
                remove_deployment(&mut manifest, &deployment);
                removed += 1;
            }
            models.push(record(
                match blocked {
                    Blocked::Refused(_) => OfferStatus::Refused,
                    Blocked::RequiresAction(_) => OfferStatus::RequiresAction,
                },
                None,
            ));
            continue;
        }
        // Not offered until a listing checks it; one already offered is not
        // removed because one listing's checks ended early.
        if listed.unchecked && !is_managed {
            models.push(record(OfferStatus::Unchecked, None));
            continue;
        }
        let embeddings_only = operations == [CatalogOperation::Embeddings];
        let price = price_for(
            input,
            &deployment,
            &listed.model,
            location.as_deref(),
            embeddings_only,
        );
        if is_managed {
            let Some(price) = price else {
                // A source that stops publishing keeps the price last offered.
                let previous = input.previous.iter().find(|p| p.model == listed.model);
                models.push(PlannedModel {
                    model: listed.model.clone(),
                    deployment: deployment.clone(),
                    status: OfferStatus::Offered,
                    price_source: previous.and_then(|p| p.price_source.clone()),
                    price_item: previous.and_then(|p| p.price_item.clone()),
                });
                if relocate(&mut manifest, &deployment, location.as_deref()) {
                    updated += 1;
                }
                if enable_tools(&mut manifest, &deployment, input.family, &listed.model) {
                    updated += 1;
                }
                continue;
            };
            // Its price follows the source through adoption (D-024).
            if relocate(&mut manifest, &deployment, location.as_deref()) {
                updated += 1;
            }
            if enable_tools(&mut manifest, &deployment, input.family, &listed.model) {
                updated += 1;
            }
            models.push(record(
                OfferStatus::Offered,
                Some((&price.source, &price.item)),
            ));
            continue;
        }
        let Some(price) = price else {
            models.push(record(OfferStatus::NeedsPrice, None));
            continue;
        };
        if manifest.deployments.len() >= MAX_MANIFEST_ITEMS
            || manifest.prices.len() + operations.len() > MAX_MANIFEST_ITEMS
            || manifest.routes.len() + operations.len() > MAX_MANIFEST_ITEMS
        {
            models.push(record(OfferStatus::CatalogFull, None));
            continue;
        }
        manifest.deployments.push(DeploymentManifest {
            name: deployment.clone(),
            model: listed.model.clone(),
            location: location.clone(),
            adapter_instance_key: key.to_owned(),
            capabilities: operations
                .iter()
                .map(|operation| CapabilityManifest {
                    operation: operation.as_str().to_owned(),
                    max_input_bytes: MAX_INPUT_BYTES,
                    max_output_tokens: MAX_OUTPUT_TOKENS,
                    max_tokens_per_byte: MAX_TOKENS_PER_BYTE,
                    tokenizer: "configured".into(),
                    reservation_ttl_seconds: RESERVATION_TTL_SECONDS,
                    chat_input_count_mode: "unsupported".into(),
                    reasoning_allowance_tokens: 0,
                    tools: takes_tool_operation(*operation)
                        && takes_tools(input.family, &listed.model),
                })
                .collect(),
        });
        for operation in &operations {
            manifest
                .prices
                .push(price_manifest(&deployment, *operation, &price));
            // An alias that already routes the operation keeps its route.
            if !manifest
                .routes
                .iter()
                .any(|r| r.alias == listed.model && r.operation == operation.as_str())
            {
                manifest.routes.push(RouteManifest::new(
                    listed.model.clone(),
                    operation.as_str(),
                    deployment.clone(),
                ));
            }
        }
        added += 1;
        models.push(record(
            OfferStatus::Offered,
            Some((&price.source, &price.item)),
        ));
    }

    let manifest = if added + updated + removed > 0 {
        manifest.release.version = truncate(
            &format!("{} auto {key}", input.now.format("%Y-%m-%dT%H:%MZ")),
            128,
        )
        .to_owned();
        manifest.validate().map_err(|_| ERROR_INVALID_RELEASE)?;
        Some(manifest)
    } else {
        None
    };
    Ok(Plan {
        manifest,
        models,
        added,
        updated,
        removed,
    })
}

/// The operations to offer a model: those the listing states, else chat
/// for any model but an embedding one, of those the family's adapter
/// implements. Never a batch operation: an offer has one price, an
/// on-demand one, and a batch-type deployment (Azure's `GlobalBatch`) is
/// added by an admin.
fn operations(family: ProviderFamily, model: &ListedModel) -> Vec<CatalogOperation> {
    let stated = match &model.operations {
        Some(operations) => operations.clone(),
        None if model.model.to_ascii_lowercase().contains("embed") => {
            vec![CatalogOperation::Embeddings]
        }
        None => vec![CatalogOperation::Chat],
    };
    // A listing's `chat_stream` is chat (D-020); offers declare only chat.
    let stated: Vec<_> = stated
        .into_iter()
        .map(|operation| match operation {
            CatalogOperation::ChatStream => CatalogOperation::Chat,
            other => other,
        })
        .collect();
    CatalogOperation::DECLARED
        .into_iter()
        .filter(|operation| {
            !operation.is_batch() && stated.contains(operation) && family.supports(*operation)
        })
        .collect()
}

/// The AWS Price List items that price a Bedrock model, found by the name
/// the account's listing gives its foundation model: the standard tier, and
/// the long-context tier where the model has one. A system inference profile
/// (`us.anthropic.…`, `global.anthropic.…`) names a listed foundation model
/// after its geography; a `global.` profile takes the global price, any
/// other the regional one. Both names come from the provider, so nothing is
/// guessed: a model whose name the price list does not use stays unpriced.
pub fn price_list_items(
    family: ProviderFamily,
    listed: &[ListedModel],
    model: &str,
) -> Option<(String, String)> {
    // An account's own resource (an application profile, provisioned
    // throughput, ...) is named by its ARN, and by whatever name the tenant
    // gave it: that name says nothing about what it calls.
    if family != ProviderFamily::Bedrock || model.starts_with("arn:") {
        return None;
    }
    let named = |id: &str| {
        listed
            .iter()
            .find(|m| m.model == id)
            .and_then(|m| m.display_name.as_deref())
    };
    let (name, global) = match model.split_once('.') {
        Some((geography, foundation)) if named(foundation).is_some() => {
            (named(foundation)?, geography == "global")
        }
        _ => (named(model)?, false),
    };
    let item = if global {
        format!("{name}{AWS_GLOBAL_SUFFIX}")
    } else {
        name.to_owned()
    };
    let long_context = format!("{item}{AWS_LONG_CONTEXT_SUFFIX}");
    Some((item, long_context))
}

/// The price a source states for the deployment: a mapped item first, at
/// any of the family's sources, then a price published for the model, then
/// the AWS Price List item named after it (see [`price_list_items`]). In
/// USD when a source states it, else the first currency by code.
fn price_for(
    input: &PlanInput<'_>,
    deployment: &str,
    model: &str,
    location: Option<&str>,
    embeddings_only: bool,
) -> Option<OfferedPrice> {
    let mut sources: Vec<&SourceObservations> = input
        .sources
        .iter()
        .filter(|source| source.family == input.family)
        .collect();
    sources.sort_by(|a, b| a.source.cmp(&b.source));
    let mapping_for = |source: &SourceObservations| {
        input
            .mappings
            .iter()
            .find(|m| m.deployment == deployment && m.source == source.source)
    };
    let mapped = sources
        .iter()
        .filter_map(|source| Some((*source, Some(mapping_for(source)?))));
    let by_model = sources
        .iter()
        .filter(|source| mapping_for(source).is_none())
        .map(|source| (*source, None));
    let priced = |source: &SourceObservations, mapping: Option<&PriceMappingRow>| {
        let (_, mut candidates) = published_for(source, mapping, model, location, false);
        candidates.sort_by(|a, b| {
            (a.currency != "USD", &a.currency).cmp(&(b.currency != "USD", &b.currency))
        });
        candidates
            .into_iter()
            .find_map(|observed| per_token(input.family, &source.source, observed, embeddings_only))
    };
    let by_name = || {
        let (standard, long_context) = price_list_items(input.family, input.listed, model)?;
        let source = sources.iter().find(|source| {
            source.source == AWS_PRICE_LIST_SOURCE && mapping_for(source).is_none()
        })?;
        let item = |item: String| PriceMappingRow {
            deployment: deployment.to_owned(),
            source: source.source.clone(),
            item,
            region: None,
        };
        let price = priced(source, Some(&item(standard)))?;
        // A flat price must cover the long-context tier too.
        Some(match priced(source, Some(&item(long_context))) {
            Some(long_context) => price.at_least(&long_context),
            None => price,
        })
    };
    mapped
        .chain(by_model)
        .find_map(|(source, mapping)| priced(source, mapping))
        .or_else(by_name)
}

impl OfferedPrice {
    /// This price raised to `other`'s in each dimension where it is higher;
    /// unchanged when `other` is in another currency.
    fn at_least(self, other: &OfferedPrice) -> OfferedPrice {
        if self.currency != other.currency {
            return self;
        }
        let raised = |mine: i64, theirs: i64| mine.max(theirs);
        let price = OfferedPrice {
            input: raised(self.input, other.input),
            output: raised(self.output, other.output),
            cached_input: raised(self.cached_input, other.cached_input),
            reasoning: raised(self.reasoning, other.reasoning),
            other: raised(self.other, other.other),
            ..self.clone()
        };
        OfferedPrice {
            exact: self.exact && price == self,
            ..price
        }
    }
}

/// A published price per token, or `None` when it lacks an input price, or
/// an output price for a chat model.
fn per_token(
    family: ProviderFamily,
    source: &str,
    observed: &ObservedPrice,
    embeddings_only: bool,
) -> Option<OfferedPrice> {
    let exponent = minor_exponent(&observed.currency)?;
    let units = |dimension| {
        observed.prices.get(dimension).map(|amount| {
            ceil_units(&money_units_per_token(
                amount,
                observed.per_tokens,
                exponent,
            ))
        })
    };
    let optional = |dimension: PriceDimension| match units(dimension) {
        Some(units) if dimension.reported_by(family) => units,
        _ => Some((0, true)),
    };
    let (input, input_exact) = units(PriceDimension::Input)??;
    let (output, output_exact) = if embeddings_only {
        (0, true)
    } else {
        units(PriceDimension::Output)??
    };
    // Cached input the source does not price is charged as input, an upper
    // bound; a family that never reports it is charged nothing for it.
    let (cached_input, cached_exact) = if PriceDimension::CachedInput.reported_by(family) {
        match units(PriceDimension::CachedInput) {
            Some(units) => units?,
            None => (input, false),
        }
    } else {
        (0, true)
    };
    // Reasoning and other usage are charged at their published price where
    // the family reports them, and not at all otherwise.
    let (reasoning, reasoning_exact) = optional(PriceDimension::Reasoning)?;
    let (other, other_exact) = optional(PriceDimension::Other)?;
    Some(OfferedPrice {
        source: truncate(source, MAX_SOURCE_TEXT).to_owned(),
        item: observed.item.clone(),
        currency: observed.currency.clone(),
        input,
        output,
        cached_input,
        reasoning,
        other,
        exact: input_exact && output_exact && cached_exact && reasoning_exact && other_exact,
    })
}

/// A decimal amount of money units rounded up to a whole unit, and whether
/// it was whole already.
fn ceil_units(units: &str) -> Option<(i64, bool)> {
    let (whole, fraction) = units.split_once('.').unwrap_or((units, ""));
    let whole: i64 = whole.parse().ok()?;
    let exact = fraction.bytes().all(|b| b == b'0');
    Some((if exact { whole } else { whole.checked_add(1)? }, exact))
}

fn price_manifest(
    deployment: &str,
    operation: CatalogOperation,
    price: &OfferedPrice,
) -> PriceManifest {
    PriceManifest {
        deployment: deployment.to_owned(),
        operation: operation.as_str().to_owned(),
        input_minor_per_token: price.input,
        output_minor_per_token: if operation == CatalogOperation::Embeddings {
            0
        } else {
            price.output
        },
        cached_input_minor_per_token: price.cached_input,
        // Embedding adapters report neither counter.
        reasoning_minor_per_token: if operation == CatalogOperation::Embeddings {
            0
        } else {
            price.reasoning
        },
        other_minor_per_token: if operation == CatalogOperation::Embeddings {
            0
        } else {
            price.other
        },
        currency: price.currency.clone(),
        source: price.source.clone(),
        source_version: truncate(&price.item, MAX_SOURCE_TEXT).to_owned(),
        confidence: if price.exact { "known" } else { "conservative" }.into(),
    }
}

/// Moves an offered deployment to the opt-in's location. Returns whether it
/// moved.
fn relocate(manifest: &mut CatalogManifest, deployment: &str, location: Option<&str>) -> bool {
    let Some(location) = location else {
        return false;
    };
    match manifest
        .deployments
        .iter_mut()
        .find(|d| d.name == deployment)
    {
        Some(found) if found.location.as_deref() != Some(location) => {
            found.location = Some(location.to_owned());
            true
        }
        _ => false,
    }
}

/// Whether a capability of `operation` may take tools: chat, streamed or
/// batched (D-032).
fn takes_tool_operation(operation: CatalogOperation) -> bool {
    matches!(
        operation,
        CatalogOperation::Chat | CatalogOperation::ChatStream | CatalogOperation::ChatBatch
    )
}

/// Sets an offered deployment's chat capabilities (chat, streamed and
/// batched) to take tools where the model certainly does (D-032), and says
/// whether anything changed. Never turns tools off.
fn enable_tools(
    manifest: &mut CatalogManifest,
    deployment: &str,
    family: ProviderFamily,
    model: &str,
) -> bool {
    if !takes_tools(family, model) {
        return false;
    }
    let mut changed = false;
    for found in manifest
        .deployments
        .iter_mut()
        .filter(|d| d.name == deployment)
    {
        for capability in &mut found.capabilities {
            let operation = capability.operation().parse::<CatalogOperation>();
            if operation.is_ok_and(takes_tool_operation) && !capability.tools {
                capability.tools = true;
                changed = true;
            }
        }
    }
    changed
}

fn remove_deployment(manifest: &mut CatalogManifest, deployment: &str) {
    manifest.deployments.retain(|d| d.name != deployment);
    manifest.prices.retain(|p| p.deployment != deployment);
    manifest.routes.retain(|r| r.deployment != deployment);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        catalog::{ProviderManifest, ReleaseManifest},
        pricing::{DimensionPrices, PerTokens, PriceAmount},
    };

    fn now() -> DateTime<Utc> {
        "2026-10-27T12:00:00Z".parse().unwrap()
    }

    fn base() -> CatalogManifest {
        CatalogManifest {
            release: ReleaseManifest {
                version: "base".into(),
            },
            providers: vec![ProviderManifest {
                adapter_instance_key: "bedrock".into(),
                family: "bedrock".into(),
            }],
            deployments: vec![],
            prices: vec![],
            routes: vec![],
        }
    }

    fn listed(model: &str) -> ListedModel {
        ListedModel {
            model: model.into(),
            display_name: None,
            operations: None,
            availability: ModelAvailability::Listed,
            missing: false,
            blocked: None,
            unchecked: false,
        }
    }

    fn observed(
        item: &str,
        model: Option<&str>,
        region: Option<&str>,
        input: &str,
        output: &str,
    ) -> ObservedPrice {
        ObservedPrice {
            item: item.into(),
            model: model.map(str::to_owned),
            region: region.map(str::to_owned),
            currency: "USD".into(),
            per_tokens: PerTokens::Million,
            prices: DimensionPrices {
                input: PriceAmount::parse(input),
                output: PriceAmount::parse(output),
                ..Default::default()
            },
        }
    }

    fn sources(observations: Vec<ObservedPrice>) -> Vec<SourceObservations> {
        vec![SourceObservations {
            source: "aws-price-list:bedrock".into(),
            family: ProviderFamily::Bedrock,
            snapshot_id: Uuid::nil(),
            observations,
        }]
    }

    fn input<'a>(
        active: &'a CatalogManifest,
        listed: &'a [ListedModel],
        previous: &'a [PreviousOffer],
        sources: &'a [SourceObservations],
        mappings: &'a [PriceMappingRow],
    ) -> PlanInput<'a> {
        PlanInput {
            adapter_instance_key: "bedrock",
            family: ProviderFamily::Bedrock,
            location: Some("us-east-1"),
            active,
            listed,
            previous,
            catalog_changed: false,
            sources,
            mappings,
            now: now(),
        }
    }

    const SONNET: &str = "us.anthropic.claude-sonnet-4-6";
    const DEPLOYMENT: &str = "bedrock-us.anthropic.claude-sonnet-4-6";

    fn mapping() -> Vec<PriceMappingRow> {
        vec![PriceMappingRow {
            deployment: DEPLOYMENT.into(),
            source: "aws-price-list:bedrock".into(),
            item: "ClaudeSonnet4.6".into(),
            region: None,
        }]
    }

    fn named(model: &str, name: &str) -> ListedModel {
        ListedModel {
            display_name: Some(name.into()),
            ..listed(model)
        }
    }

    #[test]
    fn a_release_planned_from_one_with_chat_stream_has_none() {
        let listed = [listed(SONNET)];
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let first = plan(&input(&base(), &listed, &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        // As stored before D-020, streaming priced apart from chat.
        let mut active = first.clone();
        let mut capability = active.deployments[0].capabilities[0].clone();
        capability.operation = "chat_stream".into();
        active.deployments[0].capabilities.push(capability);
        let mut price = active.prices[0].clone();
        price.operation = "chat_stream".into();
        price.input_minor_per_token += 1;
        active.prices.push(price);
        let mut route = active.routes[0].clone();
        route.operation = "chat_stream".into();
        active.routes.push(route);
        // A plan that changes nothing leaves the release alone; the next one
        // that does (here moving the offer to another region) has no
        // `chat_stream`.
        let unchanged = plan(&input(&active, &listed, &[], &sources, &mappings)).unwrap();
        assert!(unchanged.manifest.is_none());
        let previous = [offered(SONNET)];
        let next = plan(&PlanInput {
            location: Some("us-west-2"),
            ..input(&active, &listed, &previous, &sources, &mappings)
        })
        .unwrap()
        .manifest
        .expect("the location changed");
        assert!(
            next.deployments
                .iter()
                .flat_map(|d| &d.capabilities)
                .all(|c| c.operation != "chat_stream")
        );
        assert!(next.prices.iter().all(|p| p.operation != "chat_stream"));
        assert!(next.routes.iter().all(|r| r.operation != "chat_stream"));
        assert_eq!(next.prices.len(), 1);
        assert_eq!(next.prices[0].input_minor_per_token, 300);
        assert_eq!(next.deployments[0].location.as_deref(), Some("us-west-2"));
    }

    #[test]
    fn an_offers_price_is_left_for_adoption_to_change() {
        let listed = [listed(SONNET)];
        let mappings = mapping();
        let published = |input: &str, output: &str| {
            sources(vec![observed(
                "ClaudeSonnet4.6",
                None,
                Some("us-east-1"),
                input,
                output,
            )])
        };
        let active = plan(&input(
            &base(),
            &listed,
            &[],
            &published("3", "15"),
            &mappings,
        ))
        .unwrap()
        .manifest
        .unwrap();
        let previous = [offered(SONNET)];
        // The source's price changes: the planner leaves the release alone
        // (automatic price adoption compares it and adopts or holds the
        // change, D-024) and still records where the offer's price comes
        // from.
        let next = plan(&input(
            &active,
            &listed,
            &previous,
            &published("4", "20"),
            &mappings,
        ))
        .unwrap();
        assert!(next.manifest.is_none());
        assert_eq!(next.updated, 0);
        assert_eq!(next.models[0].status, OfferStatus::Offered);
        assert_eq!(
            next.models[0].price_item.as_deref(),
            Some("ClaudeSonnet4.6")
        );
    }

    #[test]
    fn listed_models_are_priced_by_their_bedrock_name() {
        let active = base();
        let listed = [
            named("anthropic.claude-sonnet-4-6", "Claude Sonnet 4.6"),
            named(SONNET, "US Anthropic Claude Sonnet 4.6"),
            named(
                "global.anthropic.claude-sonnet-4-6",
                "Global Anthropic Claude Sonnet 4.6",
            ),
            named("amazon.nova-pro-v1:0", "Nova Pro"),
            listed("unnamed.model"),
        ];
        let priced = |item: &str, input: &str, output: &str| ObservedPrice {
            prices: DimensionPrices {
                cached_input: PriceAmount::parse("0.33"),
                other: PriceAmount::parse("4.125"),
                ..observed(item, None, None, input, output).prices
            },
            ..observed(item, None, Some("us-east-1"), input, output)
        };
        let sources = sources(vec![
            priced("Claude Sonnet 4.6", "3.3", "16.5"),
            // Output above the standard context costs more.
            priced("Claude Sonnet 4.6 · long context", "3.3", "24.75"),
            priced("Claude Sonnet 4.6 · global", "3", "15"),
            // Another region's price is not this location's.
            ObservedPrice {
                region: Some("eu-west-1".into()),
                ..priced("Nova Pro", "1", "1")
            },
        ]);
        let plan = plan(&input(&active, &listed, &[], &sources, &[])).unwrap();
        let manifest = plan.manifest.expect("the catalog changes");
        let chat = |deployment: &str| {
            manifest
                .prices
                .iter()
                .find(|p| p.deployment == deployment && p.operation == "chat")
                .unwrap()
        };
        let regional = chat(DEPLOYMENT);
        assert_eq!(
            (
                regional.input_minor_per_token,
                regional.output_minor_per_token,
                regional.cached_input_minor_per_token,
                regional.other_minor_per_token,
            ),
            (330, 2475, 33, 413)
        );
        assert_eq!(regional.confidence, "conservative");
        let global = chat("bedrock-global.anthropic.claude-sonnet-4-6");
        assert_eq!(
            (global.input_minor_per_token, global.output_minor_per_token),
            (300, 1500)
        );
        // The foundation model itself takes the regional price.
        assert_eq!(
            chat("bedrock-anthropic.claude-sonnet-4-6").output_minor_per_token,
            2475
        );
        let statuses: Vec<_> = plan
            .models
            .iter()
            .map(|m| (m.model.as_str(), m.status, m.price_item.as_deref()))
            .collect();
        assert_eq!(
            statuses,
            [
                ("amazon.nova-pro-v1:0", OfferStatus::NeedsPrice, None),
                (
                    "anthropic.claude-sonnet-4-6",
                    OfferStatus::Offered,
                    Some("Claude Sonnet 4.6")
                ),
                (
                    "global.anthropic.claude-sonnet-4-6",
                    OfferStatus::Offered,
                    Some("Claude Sonnet 4.6 · global")
                ),
                ("unnamed.model", OfferStatus::NeedsPrice, None),
                (SONNET, OfferStatus::Offered, Some("Claude Sonnet 4.6")),
            ]
        );

        // A mapping still decides, and other families never match by name.
        let mappings = mapping();
        let mapped = sources_with(
            &sources,
            observed("ClaudeSonnet4.6", None, Some("us-east-1"), "5", "25"),
        );
        let plan = super::plan(&input(&active, &listed, &[], &mapped, &mappings)).unwrap();
        let manifest = plan.manifest.unwrap();
        let price = manifest
            .prices
            .iter()
            .find(|p| p.deployment == DEPLOYMENT)
            .unwrap();
        assert_eq!(price.input_minor_per_token, 500);
        assert_eq!(
            price_list_items(ProviderFamily::AzureOpenAi, &listed, SONNET),
            None
        );
        // An account's own resource is never priced by the name it was
        // given, even a published model's.
        const PROFILE: &str =
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/claude.sonnet";
        let mut owned = listed.to_vec();
        let mut profile = owned[0].clone();
        profile.model = PROFILE.to_owned();
        profile.display_name = Some("Claude Sonnet 4.6".to_owned());
        owned.push(profile);
        assert_eq!(
            price_list_items(ProviderFamily::Bedrock, &owned, PROFILE),
            None
        );
    }

    fn sources_with(
        sources: &[SourceObservations],
        extra: ObservedPrice,
    ) -> Vec<SourceObservations> {
        let mut sources = sources.to_vec();
        sources[0].observations.push(extra);
        sources
    }

    #[test]
    fn deployment_names_match_the_admin_console() {
        assert_eq!(deployment_name("bedrock", SONNET), DEPLOYMENT);
        assert_eq!(deployment_name("k", "a b/c"), "k-a-b-c");
        assert_eq!(deployment_name("k", "a  //b"), "k-a-b");
        assert_eq!(deployment_name("k", "é/ü"), "k--");
        assert_eq!(deployment_name("k", &"a".repeat(300)).len(), 255);
    }

    #[test]
    fn a_mapped_model_is_offered_under_its_identifier() {
        let active = base();
        let listed = [listed(SONNET), listed("amazon.titan-embed-text-v2:0")];
        let sources = sources(vec![
            observed("ClaudeSonnet4.6", None, Some("us-east-1"), "3", "15"),
            observed("ClaudeSonnet4.6", None, Some("eu-west-1"), "4", "20"),
        ]);
        let mappings = mapping();
        let plan = plan(&input(&active, &listed, &[], &sources, &mappings)).unwrap();
        let manifest = plan.manifest.expect("the catalog changes");
        assert_eq!(plan.added, 1);
        assert_eq!(manifest.deployments.len(), 1);
        assert_eq!(manifest.deployments[0].name, DEPLOYMENT);
        assert_eq!(
            manifest.deployments[0].location.as_deref(),
            Some("us-east-1")
        );
        assert_eq!(manifest.prices.len(), 1);
        let price = &manifest.prices[0];
        assert_eq!(
            (price.input_minor_per_token, price.output_minor_per_token),
            (300, 1500)
        );
        // Unpublished cached input is charged as input: an upper bound.
        assert_eq!(price.cached_input_minor_per_token, 300);
        assert_eq!(price.confidence, "conservative");
        assert_eq!(price.source, "aws-price-list:bedrock");
        assert_eq!(price.source_version, "ClaudeSonnet4.6");
        let routes: Vec<_> = manifest
            .routes
            .iter()
            .map(|r| (r.alias.as_str(), r.operation.as_str()))
            .collect();
        assert_eq!(routes, [(SONNET, "chat")]);
        // Bedrock embeds; no source prices Titan here, so it needs one.
        assert_eq!(
            plan.models,
            [
                PlannedModel {
                    model: "amazon.titan-embed-text-v2:0".into(),
                    deployment: "bedrock-amazon.titan-embed-text-v2:0".into(),
                    status: OfferStatus::NeedsPrice,
                    price_source: None,
                    price_item: None,
                },
                PlannedModel {
                    model: SONNET.into(),
                    deployment: DEPLOYMENT.into(),
                    status: OfferStatus::Offered,
                    price_source: Some("aws-price-list:bedrock".into()),
                    price_item: Some("ClaudeSonnet4.6".into()),
                },
            ]
        );
    }

    #[test]
    fn a_model_without_a_published_price_needs_one() {
        let active = base();
        let listed = [listed(SONNET)];
        // Public price lists name products, not models: unmapped, unpriced.
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let plan = plan(&input(&active, &listed, &[], &sources, &[])).unwrap();
        assert!(plan.manifest.is_none());
        assert_eq!(plan.models[0].status, OfferStatus::NeedsPrice);
    }

    #[test]
    fn a_price_published_for_the_model_prices_it() {
        let active = base();
        let listed = [listed(SONNET)];
        let sources = sources(vec![observed("sonnet", Some(SONNET), None, "3", "15")]);
        let plan = plan(&input(&active, &listed, &[], &sources, &[])).unwrap();
        assert_eq!(plan.added, 1);
        assert_eq!(plan.manifest.unwrap().prices[0].input_minor_per_token, 300);
    }

    fn offered(model: &str) -> PreviousOffer {
        PreviousOffer {
            model: model.into(),
            deployment: deployment_name("bedrock", model),
            status: OfferStatus::Offered,
            price_source: Some("aws-price-list:bedrock".into()),
            price_item: Some("ClaudeSonnet4.6".into()),
        }
    }

    #[test]
    fn an_offered_claude_takes_tools_in_chat_streams_and_batches() {
        let listed = [listed(SONNET)];
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let first = plan(&input(&base(), &listed, &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        assert!(first.deployments[0].capabilities.iter().all(|c| c.tools));
        // An admin batches it (and a release from before D-020 streams it
        // apart), with tools off: the next plan turns them on everywhere.
        let mut active = first.clone();
        let chat = active.deployments[0].capabilities[0].clone();
        for operation in ["chat_batch", "chat_stream"] {
            active.deployments[0].capabilities.push(CapabilityManifest {
                operation: operation.into(),
                ..chat.clone()
            });
        }
        for capability in &mut active.deployments[0].capabilities {
            capability.tools = false;
        }
        let previous = [offered(SONNET)];
        let next = plan(&input(&active, &listed, &previous, &sources, &mappings))
            .unwrap()
            .manifest
            .expect("tools were turned on");
        // The plan drops the stored `chat_stream` (D-020) and keeps the batch.
        let capabilities = &next.deployments[0].capabilities;
        let operations: Vec<_> = capabilities.iter().map(|c| c.operation()).collect();
        assert_eq!(operations, ["chat", "chat_batch"]);
        assert!(capabilities.iter().all(|c| c.tools), "{capabilities:?}");
        // A stored stream capability takes them too, where one is kept.
        let mut kept = active.clone();
        assert!(enable_tools(
            &mut kept,
            DEPLOYMENT,
            ProviderFamily::Bedrock,
            SONNET
        ));
        assert!(kept.deployments[0].capabilities.iter().all(|c| c.tools));
        // A model not certain to take tools is left as it is.
        assert!(!enable_tools(
            &mut active,
            DEPLOYMENT,
            ProviderFamily::Bedrock,
            "amazon.nova-pro-v1:0"
        ));
        assert!(active.deployments[0].capabilities.iter().all(|c| !c.tools));
    }

    #[test]
    fn offered_models_follow_the_source_and_the_listing() {
        let listed_now = [listed(SONNET)];
        let sources_then = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let first = plan(&input(&base(), &listed_now, &[], &sources_then, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        let previous = [offered(SONNET)];

        // Unchanged: no release.
        let same = plan(&input(
            &first,
            &listed_now,
            &previous,
            &sources_then,
            &mappings,
        ))
        .unwrap();
        assert!(same.manifest.is_none());
        assert_eq!(same.models[0].status, OfferStatus::Offered);

        // The source's price changes: left for adoption.
        let sources_now = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "2.5",
            "15",
        )]);
        let next = plan(&input(
            &first,
            &listed_now,
            &previous,
            &sources_now,
            &mappings,
        ))
        .unwrap();
        // Adoption reprices it (D-024), not the planner.
        assert_eq!(next.updated, 0);
        assert!(next.manifest.is_none());

        // The source stops publishing it: the price stays.
        let kept = plan(&input(
            &first,
            &listed_now,
            &previous,
            &sources(vec![]),
            &mappings,
        ))
        .unwrap();
        assert!(kept.manifest.is_none());
        assert_eq!(
            kept.models[0].price_item.as_deref(),
            Some("ClaudeSonnet4.6")
        );

        // The listing drops it: removed with its routes.
        let mut gone = listed(SONNET);
        gone.missing = true;
        let removed = plan(&input(&first, &[gone], &previous, &sources_then, &mappings)).unwrap();
        assert_eq!(removed.removed, 1);
        let manifest = removed.manifest.unwrap();
        assert!(manifest.deployments.is_empty() && manifest.routes.is_empty());
        assert!(removed.models.is_empty());
    }

    #[test]
    fn admin_choices_are_kept() {
        let listed_now = [listed(SONNET)];
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let previous = [offered(SONNET)];
        // An admin's release dropped the offered deployment: withdrawn.
        let empty = base();
        let mut dropped = input(&empty, &listed_now, &previous, &sources, &mappings);
        dropped.catalog_changed = true;
        let plan_dropped = plan(&dropped).unwrap();
        assert!(plan_dropped.manifest.is_none());
        assert_eq!(plan_dropped.models[0].status, OfferStatus::Withdrawn);

        // A deployment of that name an admin added is left as it is.
        let mut theirs = plan(&input(&base(), &listed_now, &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        theirs.prices[0].input_minor_per_token = 1;
        let untouched = plan(&input(&theirs, &listed_now, &[], &sources, &mappings)).unwrap();
        assert!(untouched.manifest.is_none());
        assert!(untouched.models.is_empty());
    }

    #[test]
    fn an_alias_in_use_keeps_its_route() {
        let mut active = plan(&input(
            &base(),
            &[listed(SONNET)],
            &[],
            &sources(vec![observed("s", Some(SONNET), None, "3", "15")]),
            &[],
        ))
        .unwrap()
        .manifest
        .unwrap();
        // The admin renamed the deployment but kept the alias.
        for deployment in &mut active.deployments {
            deployment.name = "mine".into();
        }
        for price in &mut active.prices {
            price.deployment = "mine".into();
        }
        for route in &mut active.routes {
            route.deployment = "mine".into();
        }
        let plan = plan(&input(
            &active,
            &[listed(SONNET)],
            &[],
            &sources(vec![observed("s", Some(SONNET), None, "3", "15")]),
            &[],
        ))
        .unwrap();
        let manifest = plan.manifest.unwrap();
        assert_eq!(manifest.deployments.len(), 2);
        assert!(manifest.routes.iter().all(|r| r.deployment == "mine"));
    }

    #[test]
    fn a_refused_model_is_not_offered_and_an_offered_one_is_removed() {
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let refused = ListedModel {
            blocked: Some(Blocked::Refused(EvidenceReason::AccessDenied)),
            ..listed(SONNET)
        };
        let fresh = plan(&input(
            &base(),
            std::slice::from_ref(&refused),
            &[],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert!(fresh.manifest.is_none());
        assert_eq!(fresh.models[0].status, OfferStatus::Refused);
        assert_eq!(fresh.models[0].price_source, None);

        let first = plan(&input(&base(), &[listed(SONNET)], &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        let previous = [offered(SONNET)];
        let removed = plan(&input(&first, &[refused], &previous, &sources, &mappings)).unwrap();
        assert_eq!(removed.removed, 1);
        assert_eq!(removed.models[0].status, OfferStatus::Refused);
        let manifest = removed.manifest.unwrap();
        assert!(manifest.deployments.is_empty() && manifest.routes.is_empty());

        // Verified again: offered again.
        let back = plan(&input(
            &manifest,
            &[listed(SONNET)],
            &[PreviousOffer {
                status: OfferStatus::Refused,
                price_source: None,
                price_item: None,
                ..offered(SONNET)
            }],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert_eq!(back.added, 1);
        assert_eq!(back.models[0].status, OfferStatus::Offered);
    }

    #[test]
    fn a_model_needing_action_is_not_offered_and_an_offered_one_is_removed() {
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let blocked = ListedModel {
            blocked: Some(Blocked::RequiresAction(EvidenceReason::AgreementRequired)),
            ..listed(SONNET)
        };
        let fresh = plan(&input(
            &base(),
            std::slice::from_ref(&blocked),
            &[],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert!(fresh.manifest.is_none());
        assert_eq!(fresh.models[0].status, OfferStatus::RequiresAction);
        assert_eq!(fresh.models[0].price_source, None);

        let first = plan(&input(&base(), &[listed(SONNET)], &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        let removed = plan(&input(
            &first,
            &[blocked],
            &[offered(SONNET)],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert_eq!(removed.removed, 1);
        assert_eq!(removed.models[0].status, OfferStatus::RequiresAction);
        let manifest = removed.manifest.unwrap();
        assert!(manifest.deployments.is_empty() && manifest.routes.is_empty());

        // The check finds the agreement accepted: offered again.
        let back = plan(&input(
            &manifest,
            &[listed(SONNET)],
            &[PreviousOffer {
                status: OfferStatus::RequiresAction,
                price_source: None,
                price_item: None,
                ..offered(SONNET)
            }],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert_eq!(back.added, 1);
        assert_eq!(back.models[0].status, OfferStatus::Offered);
    }

    #[test]
    fn a_model_whose_entitlement_went_unchecked_is_deferred_but_not_removed() {
        let sources = sources(vec![observed(
            "ClaudeSonnet4.6",
            None,
            Some("us-east-1"),
            "3",
            "15",
        )]);
        let mappings = mapping();
        let unchecked = ListedModel {
            unchecked: true,
            ..listed(SONNET)
        };
        // Never offered: deferred until a listing checks it.
        let fresh = plan(&input(
            &base(),
            std::slice::from_ref(&unchecked),
            &[],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert!(fresh.manifest.is_none());
        assert_eq!(fresh.models[0].status, OfferStatus::Unchecked);
        // Already offered: one listing's checks ending early removes nothing.
        let first = plan(&input(&base(), &[listed(SONNET)], &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        let kept = plan(&input(
            &first,
            &[unchecked],
            &[offered(SONNET)],
            &sources,
            &mappings,
        ))
        .unwrap();
        assert_eq!(kept.removed, 0);
        assert_eq!(kept.models[0].status, OfferStatus::Offered);
        assert!(
            kept.manifest
                .is_none_or(|manifest| !manifest.deployments.is_empty())
        );
    }

    #[test]
    fn unknown_availability_is_not_offered() {
        let mut model = listed(SONNET);
        model.availability = ModelAvailability::Unknown;
        let sources = sources(vec![observed("s", Some(SONNET), None, "3", "15")]);
        let plan = plan(&input(&base(), &[model], &[], &sources, &[])).unwrap();
        assert!(plan.manifest.is_none());
        assert_eq!(plan.models[0].status, OfferStatus::Unsupported);
    }

    #[test]
    fn a_profile_only_model_is_not_offered_but_prices_its_profiles() {
        let listed = [
            ListedModel {
                availability: ModelAvailability::ProfileOnly,
                ..named("anthropic.claude-sonnet-4-6", "Claude Sonnet 4.6")
            },
            named(SONNET, "US Anthropic Claude Sonnet 4.6"),
        ];
        let sources = sources(vec![observed(
            "Claude Sonnet 4.6",
            None,
            Some("us-east-1"),
            "3.3",
            "16.5",
        )]);
        let plan = plan(&input(&base(), &listed, &[], &sources, &[])).unwrap();
        let statuses: Vec<_> = plan
            .models
            .iter()
            .map(|m| (m.model.as_str(), m.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("anthropic.claude-sonnet-4-6", OfferStatus::Unsupported),
                (SONNET, OfferStatus::Offered),
            ]
        );
        let manifest = plan.manifest.expect("the profile is offered");
        assert!(
            manifest
                .deployments
                .iter()
                .all(|d| d.name != "bedrock-anthropic.claude-sonnet-4-6")
        );
    }

    #[test]
    fn units_round_up() {
        assert_eq!(ceil_units("300"), Some((300, true)));
        assert_eq!(ceil_units("7.5"), Some((8, false)));
        assert_eq!(ceil_units("0.001"), Some((1, false)));
        assert_eq!(ceil_units("x"), None);
    }

    #[test]
    fn locations_are_bounded() {
        assert!(
            AutoOfferInput {
                location: Some("us-east-1".into())
            }
            .validate()
            .is_ok()
        );
        assert!(AutoOfferInput { location: None }.validate().is_ok());
        for bad in ["", " us", "a\nb"] {
            assert!(
                AutoOfferInput {
                    location: Some(bad.into())
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn models_whose_names_collide_get_distinct_deployments() {
        let listed = [listed("a/b"), listed("a-b")];
        let sources = sources(vec![
            observed("ab1", Some("a/b"), None, "1", "2"),
            observed("ab2", Some("a-b"), None, "1", "2"),
        ]);
        let plan = plan(&input(&base(), &listed, &[], &sources, &[])).unwrap();
        assert_eq!(plan.added, 2);
        let mut names: Vec<_> = plan.models.iter().map(|m| m.deployment.clone()).collect();
        names.sort();
        assert_eq!(names[0], "bedrock-a-b");
        assert_eq!(names[1], distinct_deployment_name("bedrock", "a/b"));
        assert!(names[1].starts_with("bedrock-a-b-") && names[1].len() == "bedrock-a-b-".len() + 8);
        // Long identifiers that differ only past the cut stay distinct too.
        let long = format!("{}x", "m".repeat(300));
        let longer = format!("{}y", "m".repeat(300));
        assert_ne!(
            distinct_deployment_name("k", &long),
            distinct_deployment_name("k", &longer)
        );
        assert!(distinct_deployment_name("k", &long).len() <= 255);
    }

    #[test]
    fn a_withdrawal_outlives_a_gap_in_the_listing() {
        let previous = [PreviousOffer {
            status: OfferStatus::Withdrawn,
            price_source: None,
            price_item: None,
            ..offered(SONNET)
        }];
        let mut gone = listed(SONNET);
        gone.missing = true;
        let sources = sources(vec![]);
        let plan = plan(&input(&base(), &[gone], &previous, &sources, &[])).unwrap();
        assert_eq!(plan.models.len(), 1);
        assert_eq!(plan.models[0].status, OfferStatus::Withdrawn);
    }

    #[test]
    fn a_new_location_moves_offered_deployments() {
        let listed_now = [listed(SONNET)];
        let sources = sources(vec![
            observed("ClaudeSonnet4.6", None, Some("us-east-1"), "3", "15"),
            observed("ClaudeSonnet4.6", None, Some("us-west-2"), "3", "15"),
        ]);
        let mappings = mapping();
        let first = plan(&input(&base(), &listed_now, &[], &sources, &mappings))
            .unwrap()
            .manifest
            .unwrap();
        let previous = [offered(SONNET)];
        let mut moved = input(&first, &listed_now, &previous, &sources, &mappings);
        moved.location = Some("us-west-2");
        let plan = plan(&moved).unwrap();
        // The same price in both regions: still a new release.
        assert_eq!(plan.updated, 1);
        assert_eq!(
            plan.manifest.unwrap().deployments[0].location.as_deref(),
            Some("us-west-2")
        );
    }

    #[test]
    fn reported_reasoning_and_other_usage_are_charged() {
        let mut sheet = observed("o", Some("gpt"), None, "1", "4");
        sheet.prices.reasoning = PriceAmount::parse("4");
        sheet.prices.other = PriceAmount::parse("2");
        let price = per_token(ProviderFamily::OpenAi, "price-sheet:openai", &sheet, false).unwrap();
        assert_eq!((price.reasoning, price.other), (400, 200));
        // Bedrock never reports reasoning, so it is not charged.
        let price = per_token(
            ProviderFamily::Bedrock,
            "price-sheet:bedrock",
            &sheet,
            false,
        )
        .unwrap();
        assert_eq!((price.reasoning, price.other), (0, 200));
    }

    #[test]
    fn a_model_waiting_for_a_price_keeps_its_name() {
        // `a/b` was given the distinct name while `a-b` was listed too.
        let distinct = distinct_deployment_name("bedrock", "a/b");
        let previous = [PreviousOffer {
            model: "a/b".into(),
            deployment: distinct.clone(),
            status: OfferStatus::NeedsPrice,
            price_source: None,
            price_item: None,
        }];
        let mappings = [PriceMappingRow {
            deployment: distinct.clone(),
            source: "aws-price-list:bedrock".into(),
            item: "AB".into(),
            region: None,
        }];
        let sources = sources(vec![observed("AB", None, Some("us-east-1"), "1", "2")]);
        // `a-b` is no longer listed; `a/b` keeps its name and its mapping.
        let plan = plan(&input(
            &base(),
            &[listed("a/b")],
            &previous,
            &sources,
            &mappings,
        ))
        .unwrap();
        assert_eq!(plan.added, 1);
        assert_eq!(plan.models[0].deployment, distinct);
        assert_eq!(plan.models[0].status, OfferStatus::Offered);
    }

    #[test]
    fn embeddings_are_not_charged_for_reasoning_or_other_usage() {
        let mut sheet = observed("e", Some("text-embedding-3"), None, "1", "0");
        sheet.prices.reasoning = PriceAmount::parse("4");
        sheet.prices.other = PriceAmount::parse("2");
        let price = per_token(ProviderFamily::OpenAi, "price-sheet:openai", &sheet, true).unwrap();
        let manifest = price_manifest("d", CatalogOperation::Embeddings, &price);
        assert_eq!(
            (
                manifest.reasoning_minor_per_token,
                manifest.other_minor_per_token
            ),
            (0, 0)
        );
        let chat = price_manifest("d", CatalogOperation::Chat, &price);
        assert_eq!(chat.reasoning_minor_per_token, 400);
    }
}
