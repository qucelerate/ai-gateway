//! Automatic price adoption (D-024): catalog prices follow their price
//! sources.
//!
//! After price sources are fetched, the planner compares an organization's
//! active release with every source (the drift report) and adopts each
//! published price that differs, in a new release: the dimensions that
//! differ take the published price, rounded up to a whole money unit, and
//! the rest of the price stays. A price billed through an override, and a
//! price no source publishes, are left alone. A change that looks wrong is
//! held for an admin instead of adopted: one that sets a billed dimension to
//! zero, cuts one by more than half or more than doubles one, and a price
//! two sources disagree about (or one source publishes as it is and another
//! differently). A proposal an admin dismissed is not held again until its
//! source publishes something else.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::catalog::{CatalogManifest, PriceManifest};
use super::pricing::{
    CatalogPriceSource, ComparisonStatus, DimensionDrift, DimensionStatus, PriceDimension,
    PriceDrift, PriceDriftEntry, billable_units,
};

/// A change is held when a dimension becomes more than this many times
/// what it was.
pub const HOLD_INCREASE_FACTOR: i64 = 2;
/// ... or less than one in this many of what it was.
pub const HOLD_DECREASE_FACTOR: i64 = 2;

const CHAT: &str = "chat";
const CHAT_STREAM: &str = "chat_stream";

/// Whether an organization's prices follow their sources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PricePolicyView {
    pub auto_adopt: bool,
    /// 0 until the policy is first set.
    pub version: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

impl PricePolicyView {
    /// The policy of an organization that never set one: prices follow.
    pub fn unset() -> Self {
        Self {
            auto_adopt: true,
            version: 0,
            updated_at: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PricePolicyInput {
    pub auto_adopt: bool,
}

/// Why a published price was held rather than adopted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    LargeIncrease,
    LargeDecrease,
    ToZero,
    SourcesDisagree,
}

impl HoldReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LargeIncrease => "large_increase",
            Self::LargeDecrease => "large_decrease",
            Self::ToZero => "to_zero",
            Self::SourcesDisagree => "sources_disagree",
        }
    }
}

impl FromStr for HoldReason {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "large_increase" => Ok(Self::LargeIncrease),
            "large_decrease" => Ok(Self::LargeDecrease),
            "to_zero" => Ok(Self::ToZero),
            "sources_disagree" => Ok(Self::SourcesDisagree),
            _ => Err(()),
        }
    }
}

impl fmt::Display for HoldReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldStatus {
    Held,
    /// The catalog now bills what was held (an admin adopted it).
    Adopted,
    Dismissed,
    /// The catalog or the source moved on.
    Superseded,
}

impl HoldStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Adopted => "adopted",
            Self::Dismissed => "dismissed",
            Self::Superseded => "superseded",
        }
    }
}

impl FromStr for HoldStatus {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "held" => Ok(Self::Held),
            "adopted" => Ok(Self::Adopted),
            "dismissed" => Ok(Self::Dismissed),
            "superseded" => Ok(Self::Superseded),
            _ => Err(()),
        }
    }
}

/// One dimension of a proposed price, in money units per token.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DimensionChange {
    pub dimension: PriceDimension,
    /// What the catalog bills now.
    pub catalog: i64,
    /// What the source publishes, as an exact decimal.
    pub observed: String,
    /// What adopting it would bill: `observed` rounded up to a whole unit.
    pub proposed: i64,
}

/// A source's price for one priced deployment operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposedPrice {
    pub deployment: String,
    pub operation: String,
    pub model: String,
    pub currency: String,
    pub source: String,
    pub snapshot_id: Uuid,
    pub item: Option<String>,
    pub changes: Vec<DimensionChange>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeldPrice {
    pub proposal: ProposedPrice,
    pub reason: HoldReason,
    /// Identifies the proposal, so a dismissed one is not held again.
    pub fingerprint: String,
}

/// A held price, as the API shows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceHoldView {
    pub id: Uuid,
    pub deployment: String,
    pub operation: String,
    pub model: String,
    pub currency: String,
    pub source: String,
    pub reason: HoldReason,
    pub changes: Vec<DimensionChange>,
    pub status: HoldStatus,
    pub release_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// 1 while held; dismissing it needs this version.
    pub version: u64,
}

/// What one adoption run would do.
#[derive(Clone, Debug, Default)]
pub struct AdoptionPlan {
    /// The release to create and activate; `None` when nothing is adopted.
    pub manifest: Option<CatalogManifest>,
    pub adopted: Vec<ProposedPrice>,
    pub held: Vec<HeldPrice>,
}

/// Plans the adoption of every published price that differs from `active`,
/// whose drift report is `drift`. `dismissed` holds the fingerprints of
/// proposals an admin dismissed.
pub fn plan(
    active: &CatalogManifest,
    drift: &PriceDrift,
    dismissed: &HashSet<String>,
    now: DateTime<Utc>,
) -> AdoptionPlan {
    let mut plan = AdoptionPlan::default();
    // Streaming is a mode of chat (D-020): a deployment's chat entry stands
    // for its `chat_stream` one, which is adopted only when it is alone.
    let chat: HashSet<&str> = drift
        .prices
        .iter()
        .filter(|entry| entry.operation == CHAT)
        .map(|entry| entry.deployment.as_str())
        .collect();
    let mut manifest = active.clone();
    for entry in &drift.prices {
        if entry.operation == CHAT_STREAM && chat.contains(entry.deployment.as_str()) {
            continue;
        }
        let Some(decision) = decide(entry) else {
            continue;
        };
        match decision {
            Decision::Adopt(proposal) => {
                if apply(&mut manifest, &proposal) {
                    plan.adopted.push(proposal);
                }
            }
            Decision::Hold(held) => {
                if !dismissed.contains(&held.fingerprint) {
                    plan.held.push(held);
                }
            }
        }
    }
    if !plan.adopted.is_empty() {
        let label = match plan.adopted.as_slice() {
            [one] => format!("price {}", one.deployment),
            many => format!("{} prices", many.len()),
        };
        manifest.release.version = truncate(
            &format!("{} auto {label}", now.format("%Y-%m-%dT%H:%MZ")),
            128,
        );
        plan.manifest = Some(manifest);
    }
    plan
}

enum Decision {
    Adopt(ProposedPrice),
    Hold(HeldPrice),
}

fn decide(entry: &PriceDriftEntry) -> Option<Decision> {
    // An override is what is billed; change the override instead.
    if entry.price_source == CatalogPriceSource::Override {
        return None;
    }
    let mut proposals = Vec::new();
    let mut agrees = false;
    for comparison in &entry.comparisons {
        match comparison.status {
            ComparisonStatus::Matches => agrees = true,
            ComparisonStatus::Differs => {
                let mut changes = Vec::new();
                for dimension in &comparison.dimensions {
                    if dimension.status != DimensionStatus::Differs {
                        continue;
                    }
                    let observed = dimension.observed.as_deref()?;
                    changes.push(DimensionChange {
                        dimension: dimension.dimension,
                        catalog: dimension.catalog,
                        observed: observed.to_owned(),
                        proposed: billable_units(observed)?,
                    });
                }
                if changes.is_empty() {
                    continue;
                }
                proposals.push(ProposedPrice {
                    deployment: entry.deployment.clone(),
                    operation: entry.operation.clone(),
                    model: entry.model.clone(),
                    currency: entry.currency.clone(),
                    source: comparison.source.clone(),
                    snapshot_id: comparison.snapshot_id,
                    item: comparison.observed_item.clone(),
                    changes,
                });
            }
            _ => {}
        }
    }
    // Each proposal as it would be applied: a stand-in cached-input price
    // follows input (from its own comparison). Sources agree only if these
    // agree, so one source publishing cached input is never overridden by
    // another's inferred one. Fingerprints cover only what the sources
    // published, so a proposal dismissed before cached input followed input
    // keeps its fingerprint.
    let followed: Vec<ProposedPrice> = proposals
        .iter()
        .map(|proposal| {
            let mut proposal = proposal.clone();
            if let Some(comparison) = entry
                .comparisons
                .iter()
                .find(|c| c.source == proposal.source && c.snapshot_id == proposal.snapshot_id)
            {
                follow_input(&comparison.dimensions, &mut proposal.changes);
            }
            proposal
        })
        .collect();
    let first_published = proposals.first()?.clone();
    let first = followed.first()?.clone();
    let proposed = |proposal: &ProposedPrice| -> Vec<(PriceDimension, i64)> {
        let mut changes: Vec<(PriceDimension, i64)> = proposal
            .changes
            .iter()
            .map(|change| (change.dimension, change.proposed))
            .collect();
        changes.sort_by_key(|(dimension, _)| format!("{dimension:?}"));
        changes
    };
    let disagree = agrees
        || followed
            .iter()
            .skip(1)
            .any(|other| proposed(other) != proposed(&first));
    if disagree {
        return Some(Decision::Hold(HeldPrice {
            fingerprint: fingerprint(&proposals),
            proposal: first,
            reason: HoldReason::SourcesDisagree,
        }));
    }
    if let Some(reason) = suspicious(&first.changes) {
        return Some(Decision::Hold(HeldPrice {
            fingerprint: fingerprint(std::slice::from_ref(&first_published)),
            proposal: first,
            reason,
        }));
    }
    Some(Decision::Adopt(first))
}

/// A cached-input price the source does not publish stands in at the input
/// price (as automatic offers charge it, an upper bound), so it moves with
/// input: when input changes and the catalog's cached-input price equals
/// its input price, cached input takes the new input price too.
fn follow_input(dimensions: &[DimensionDrift], changes: &mut Vec<DimensionChange>) {
    let find = |dimension| dimensions.iter().find(|d| d.dimension == dimension);
    let (Some(input), Some(cached)) = (
        find(PriceDimension::Input),
        find(PriceDimension::CachedInput),
    ) else {
        return;
    };
    if cached.status != DimensionStatus::Unpublished || cached.catalog != input.catalog {
        return;
    }
    let Some(change) = changes
        .iter()
        .find(|change| change.dimension == PriceDimension::Input)
        .cloned()
    else {
        return;
    };
    changes.push(DimensionChange {
        dimension: PriceDimension::CachedInput,
        catalog: cached.catalog,
        ..change
    });
}

/// Why a change should be held, if it should.
fn suspicious(changes: &[DimensionChange]) -> Option<HoldReason> {
    let pairs: Vec<(i64, i64)> = changes
        .iter()
        .map(|change| (change.catalog, change.proposed))
        .collect();
    suspicious_prices(&pairs)
}

/// Why repricing each `(before, after)` dimension pair (whole money units
/// per token) should wait for an admin, if it should.
pub fn suspicious_prices(pairs: &[(i64, i64)]) -> Option<HoldReason> {
    let reason = |&(before, after): &(i64, i64)| {
        if before > 0 && after == 0 {
            Some(HoldReason::ToZero)
        } else if before > 0 && after.saturating_mul(HOLD_DECREASE_FACTOR) < before {
            Some(HoldReason::LargeDecrease)
        } else if before > 0 && after > before.saturating_mul(HOLD_INCREASE_FACTOR) {
            Some(HoldReason::LargeIncrease)
        } else {
            None
        }
    };
    // The most consequential reason first: charging nothing, then too little.
    [
        HoldReason::ToZero,
        HoldReason::LargeDecrease,
        HoldReason::LargeIncrease,
    ]
    .into_iter()
    .find(|wanted| pairs.iter().any(|pair| reason(pair) == Some(*wanted)))
}

/// A stable identity for what is proposed, so a dismissed proposal stays
/// dismissed until a source publishes something else.
pub fn fingerprint(proposals: &[ProposedPrice]) -> String {
    let mut canonical: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for proposal in proposals {
        let mut changes: Vec<String> = proposal
            .changes
            .iter()
            .map(|change| format!("{:?}={}", change.dimension, change.proposed))
            .collect();
        changes.sort();
        canonical.insert(proposal.source.clone(), changes);
    }
    let head = proposals.first();
    let mut hasher = Sha256::new();
    if let Some(head) = head {
        hasher.update(head.deployment.as_bytes());
        hasher.update([0]);
        hasher.update(head.operation.as_bytes());
        hasher.update([0]);
    }
    for (source, changes) in canonical {
        hasher.update(source.as_bytes());
        hasher.update([0]);
        hasher.update(changes.join(";").as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Sets the proposal's dimensions on the deployment's price, and on its
/// streaming twin so the two never contradict each other. `false` when the
/// release no longer prices it in that currency.
fn apply(manifest: &mut CatalogManifest, proposal: &ProposedPrice) -> bool {
    let chat_like = proposal.operation == CHAT || proposal.operation == CHAT_STREAM;
    let mut applied = false;
    for price in manifest.prices.iter_mut() {
        let same = price.deployment == proposal.deployment
            && (price.operation == proposal.operation
                || (chat_like && (price.operation == CHAT || price.operation == CHAT_STREAM)));
        if !same || price.currency != proposal.currency {
            continue;
        }
        set(price, proposal);
        applied = true;
    }
    applied
}

fn set(price: &mut PriceManifest, proposal: &ProposedPrice) {
    let mut exact = true;
    for change in &proposal.changes {
        exact &= billable_units(&change.observed)
            .is_some_and(|units| units.to_string() == change.observed);
        let field = match change.dimension {
            PriceDimension::Input => &mut price.input_minor_per_token,
            PriceDimension::Output => &mut price.output_minor_per_token,
            PriceDimension::CachedInput => &mut price.cached_input_minor_per_token,
            PriceDimension::Reasoning => &mut price.reasoning_minor_per_token,
            PriceDimension::Other => &mut price.other_minor_per_token,
        };
        *field = change.proposed;
    }
    price.source = truncate(&proposal.source, 128);
    price.source_version = truncate(
        proposal
            .item
            .as_deref()
            .unwrap_or(&proposal.snapshot_id.to_string()),
        128,
    );
    // Known only if it was and what changed is exact: a dimension the
    // source does not publish keeps whatever made the price an estimate.
    price.confidence = if exact && price.confidence == "known" {
        "known".into()
    } else {
        "conservative".into()
    };
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::catalog::{ProviderManifest, ReleaseManifest};
    use crate::domain::pricing::{DimensionDrift, MatchedBy, PerTokens, PriceComparison};

    fn price(deployment: &str, operation: &str, input: i64, output: i64) -> PriceManifest {
        PriceManifest {
            deployment: deployment.into(),
            operation: operation.into(),
            input_minor_per_token: input,
            output_minor_per_token: output,
            cached_input_minor_per_token: 0,
            reasoning_minor_per_token: 0,
            other_minor_per_token: 0,
            currency: "USD".into(),
            source: "operator".into(),
            source_version: "v".into(),
            confidence: "known".into(),
        }
    }

    fn manifest(prices: Vec<PriceManifest>) -> CatalogManifest {
        CatalogManifest {
            release: ReleaseManifest {
                version: "before".into(),
            },
            providers: vec![ProviderManifest {
                adapter_instance_key: "k".into(),
                family: "openai".into(),
            }],
            deployments: Vec::new(),
            prices,
            routes: Vec::new(),
        }
    }

    fn comparison(source: &str, dims: &[(PriceDimension, i64, &str)]) -> PriceComparison {
        let differs = dims
            .iter()
            .any(|(_, catalog, observed)| billable_units(observed) != Some(*catalog));
        PriceComparison {
            source: source.into(),
            snapshot_id: Uuid::nil(),
            status: if differs {
                ComparisonStatus::Differs
            } else {
                ComparisonStatus::Matches
            },
            matched_by: MatchedBy::Model,
            observed_item: Some(format!("{source}-item")),
            observed_region: None,
            observed_currencies: Vec::new(),
            per_tokens: Some(PerTokens::Million),
            dimensions: dims
                .iter()
                .map(|(dimension, catalog, observed)| DimensionDrift {
                    dimension: *dimension,
                    catalog: *catalog,
                    observed: Some((*observed).into()),
                    published: None,
                    status: if billable_units(observed) == Some(*catalog) {
                        DimensionStatus::Matches
                    } else {
                        DimensionStatus::Differs
                    },
                })
                .collect(),
        }
    }

    fn entry(
        deployment: &str,
        operation: &str,
        source: CatalogPriceSource,
        comparisons: Vec<PriceComparison>,
    ) -> PriceDriftEntry {
        PriceDriftEntry {
            deployment: deployment.into(),
            operation: operation.into(),
            model: "m".into(),
            location: None,
            provider_family: "openai".into(),
            currency: "USD".into(),
            price_source: source,
            comparisons,
        }
    }

    fn drift(prices: Vec<PriceDriftEntry>) -> PriceDrift {
        PriceDrift {
            release_id: Uuid::nil(),
            sources: Vec::new(),
            prices,
        }
    }

    const IN: PriceDimension = PriceDimension::Input;
    const OUT: PriceDimension = PriceDimension::Output;

    #[test]
    fn a_differing_price_is_adopted_rounded_up_and_the_rest_stays() {
        let active = manifest(vec![price("d", "chat", 250, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![comparison("s", &[(IN, 250, "300.25"), (OUT, 1000, "1000")])],
        )]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        let next = planned.manifest.expect("a release");
        assert_eq!(next.prices[0].input_minor_per_token, 301);
        assert_eq!(next.prices[0].output_minor_per_token, 1000);
        assert_eq!(next.prices[0].source, "s");
        assert_eq!(next.prices[0].source_version, "s-item");
        // Rounded up, so no longer exact.
        assert_eq!(next.prices[0].confidence, "conservative");
        assert!(next.release.version.contains("auto price d"));
        assert_eq!(planned.adopted.len(), 1);
        assert!(planned.held.is_empty());
    }

    #[test]
    fn a_cached_input_price_standing_in_at_input_moves_with_it() {
        let unpublished = |catalog| DimensionDrift {
            dimension: PriceDimension::CachedInput,
            catalog,
            observed: None,
            published: None,
            status: DimensionStatus::Unpublished,
        };
        let run = |cached: i64| {
            let mut stand_in = price("d", "chat", 80, 320);
            stand_in.cached_input_minor_per_token = cached;
            let active = manifest(vec![stand_in]);
            let mut compared = comparison("s", &[(IN, 80, "75"), (OUT, 320, "320")]);
            compared.dimensions.push(unpublished(cached));
            let report = drift(vec![entry(
                "d",
                "chat",
                CatalogPriceSource::Release,
                vec![compared],
            )]);
            plan(&active, &report, &HashSet::new(), Utc::now())
                .manifest
                .expect("a release")
                .prices[0]
                .clone()
        };
        // Equal to input: it stood in for it, and follows it.
        let next = run(80);
        assert_eq!(next.input_minor_per_token, 75);
        assert_eq!(next.cached_input_minor_per_token, 75);
        // Set apart by an admin: left alone.
        let next = run(20);
        assert_eq!(next.input_minor_per_token, 75);
        assert_eq!(next.cached_input_minor_per_token, 20);

        // A held change's fingerprint covers only what the source
        // published, so a proposal dismissed before cached input followed
        // input is still recognised.
        let mut stand_in = price("d", "chat", 80, 320);
        stand_in.cached_input_minor_per_token = 80;
        let active = manifest(vec![stand_in]);
        let mut compared = comparison("s", &[(IN, 80, "20"), (OUT, 320, "320")]);
        compared.dimensions.push(unpublished(80));
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![compared.clone()],
        )]);
        let held = plan(&active, &report, &HashSet::new(), Utc::now()).held;
        assert_eq!(held.len(), 1);
        let published_only = ProposedPrice {
            deployment: "d".into(),
            operation: "chat".into(),
            model: "m".into(),
            currency: "USD".into(),
            source: "s".into(),
            snapshot_id: Uuid::nil(),
            item: Some("s-item".into()),
            changes: vec![DimensionChange {
                dimension: IN,
                catalog: 80,
                observed: "20".into(),
                proposed: 20,
            }],
        };
        assert_eq!(held[0].fingerprint, fingerprint(&[published_only]));
        // The held proposal still carries cached input, for adopting it.
        assert_eq!(held[0].proposal.changes.len(), 2);
        let dismissed = HashSet::from([held[0].fingerprint.clone()]);
        assert!(
            plan(&active, &report, &dismissed, Utc::now())
                .held
                .is_empty()
        );
    }

    #[test]
    fn an_inferred_cached_input_price_disagreeing_with_a_published_one_is_held() {
        // The catalog bills cached input at input (100). AWS publishes input
        // 110 without cached input; a price sheet publishes input 110 and
        // cached input 100. Inferring cached 110 from AWS would contradict
        // the sheet, so the sources disagree.
        let mut stand_in = price("d", "chat", 100, 500);
        stand_in.cached_input_minor_per_token = 100;
        let active = manifest(vec![stand_in]);
        let mut aws = comparison(
            "aws-price-list:bedrock",
            &[(IN, 100, "110"), (OUT, 500, "500")],
        );
        aws.dimensions.push(DimensionDrift {
            dimension: PriceDimension::CachedInput,
            catalog: 100,
            observed: None,
            published: None,
            status: DimensionStatus::Unpublished,
        });
        let sheet = comparison(
            "price-sheet:bedrock",
            &[
                (IN, 100, "110"),
                (OUT, 500, "500"),
                (PriceDimension::CachedInput, 100, "100"),
            ],
        );
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![aws, sheet],
        )]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        assert!(planned.manifest.is_none());
        assert_eq!(planned.held.len(), 1);
        assert_eq!(planned.held[0].reason, HoldReason::SourcesDisagree);
    }

    #[test]
    fn nothing_differing_plans_no_release() {
        let active = manifest(vec![price("d", "chat", 250, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![comparison("s", &[(IN, 250, "250"), (OUT, 1000, "999.5")])],
        )]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        assert!(planned.manifest.is_none());
        assert!(planned.adopted.is_empty() && planned.held.is_empty());
    }

    #[test]
    fn overridden_prices_are_left_alone() {
        let active = manifest(vec![price("d", "chat", 250, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Override,
            vec![comparison("s", &[(IN, 250, "300")])],
        )]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        assert!(planned.manifest.is_none() && planned.held.is_empty());
    }

    #[test]
    fn implausible_changes_are_held_most_consequential_first() {
        for (observed, reason) in [
            ("0", HoldReason::ToZero),
            ("124", HoldReason::LargeDecrease),
            ("501", HoldReason::LargeIncrease),
        ] {
            let active = manifest(vec![price("d", "chat", 250, 1000)]);
            let report = drift(vec![entry(
                "d",
                "chat",
                CatalogPriceSource::Release,
                vec![comparison("s", &[(IN, 250, observed)])],
            )]);
            let planned = plan(&active, &report, &HashSet::new(), Utc::now());
            assert!(planned.manifest.is_none(), "{observed}");
            assert_eq!(planned.held.len(), 1);
            assert_eq!(planned.held[0].reason, reason, "{observed}");
        }
        // Exactly double, or exactly half, is adopted.
        for observed in ["500", "125"] {
            let active = manifest(vec![price("d", "chat", 250, 1000)]);
            let report = drift(vec![entry(
                "d",
                "chat",
                CatalogPriceSource::Release,
                vec![comparison("s", &[(IN, 250, observed)])],
            )]);
            assert!(
                plan(&active, &report, &HashSet::new(), Utc::now())
                    .manifest
                    .is_some(),
                "{observed}"
            );
        }
        // A dimension priced for the first time raises no hold.
        let active = manifest(vec![price("d", "chat", 0, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![comparison("s", &[(IN, 0, "250")])],
        )]);
        assert!(
            plan(&active, &report, &HashSet::new(), Utc::now())
                .manifest
                .is_some()
        );
    }

    #[test]
    fn disagreeing_sources_are_held_and_a_dismissal_sticks_until_the_source_moves() {
        let active = manifest(vec![price("d", "chat", 250, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![
                comparison("a", &[(IN, 250, "300")]),
                comparison("b", &[(IN, 250, "310")]),
            ],
        )]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        assert!(planned.manifest.is_none());
        assert_eq!(planned.held[0].reason, HoldReason::SourcesDisagree);
        // One source agreeing with the catalog is a disagreement too.
        let agreeing = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![
                comparison("a", &[(IN, 250, "300")]),
                comparison("b", &[(IN, 250, "250")]),
            ],
        )]);
        assert_eq!(
            plan(&active, &agreeing, &HashSet::new(), Utc::now()).held[0].reason,
            HoldReason::SourcesDisagree
        );
        // Two sources agreeing on the new price adopt it.
        let both = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![
                comparison("a", &[(IN, 250, "300")]),
                comparison("b", &[(IN, 250, "299.5")]),
            ],
        )]);
        assert!(
            plan(&active, &both, &HashSet::new(), Utc::now())
                .manifest
                .is_some()
        );

        let dismissed: HashSet<String> = [planned.held[0].fingerprint.clone()].into();
        let again = plan(&active, &report, &dismissed, Utc::now());
        assert!(again.held.is_empty() && again.manifest.is_none());
        let moved = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![
                comparison("a", &[(IN, 250, "320")]),
                comparison("b", &[(IN, 250, "310")]),
            ],
        )]);
        assert_eq!(plan(&active, &moved, &dismissed, Utc::now()).held.len(), 1);
    }

    #[test]
    fn chat_stands_for_streaming_and_both_are_kept_equal() {
        let active = manifest(vec![
            price("d", "chat", 250, 1000),
            price("d", "chat_stream", 250, 1000),
        ]);
        let differs = || comparison("s", &[(IN, 250, "300")]);
        let report = drift(vec![
            entry("d", "chat", CatalogPriceSource::Release, vec![differs()]),
            entry(
                "d",
                "chat_stream",
                CatalogPriceSource::Release,
                vec![differs()],
            ),
        ]);
        let planned = plan(&active, &report, &HashSet::new(), Utc::now());
        assert_eq!(planned.adopted.len(), 1);
        let next = planned.manifest.unwrap();
        assert!(
            next.prices
                .iter()
                .all(|price| price.input_minor_per_token == 300)
        );
        // A lone streaming price is adopted as itself.
        let lone = manifest(vec![price("e", "chat_stream", 250, 1000)]);
        let report = drift(vec![entry(
            "e",
            "chat_stream",
            CatalogPriceSource::Release,
            vec![differs()],
        )]);
        let next = plan(&lone, &report, &HashSet::new(), Utc::now())
            .manifest
            .unwrap();
        assert_eq!(next.prices[0].input_minor_per_token, 300);
    }

    #[test]
    fn exact_prices_stay_known() {
        let active = manifest(vec![price("d", "chat", 250, 1000)]);
        let report = drift(vec![entry(
            "d",
            "chat",
            CatalogPriceSource::Release,
            vec![comparison("s", &[(IN, 250, "300")])],
        )]);
        let next = plan(&active, &report, &HashSet::new(), Utc::now())
            .manifest
            .unwrap();
        assert_eq!(next.prices[0].confidence, "known");
    }

    #[test]
    fn reasons_and_statuses_round_trip() {
        for reason in [
            HoldReason::LargeIncrease,
            HoldReason::LargeDecrease,
            HoldReason::ToZero,
            HoldReason::SourcesDisagree,
        ] {
            assert_eq!(reason.as_str().parse::<HoldReason>(), Ok(reason));
        }
        for status in [
            HoldStatus::Held,
            HoldStatus::Adopted,
            HoldStatus::Dismissed,
            HoldStatus::Superseded,
        ] {
            assert_eq!(status.as_str().parse::<HoldStatus>(), Ok(status));
        }
    }

    /// Stored dismissals compare against this lowercase hex SHA-256.
    #[test]
    fn the_fingerprint_is_lowercase_hex_sha256() {
        assert_eq!(
            fingerprint(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
