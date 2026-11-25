//! One record per model across providers (Phase 13, D-033).
//!
//! A master record names one model, whoever serves it: a model card's id
//! (`anthropic/claude-sonnet-4-6`), or one of the organization's own records
//! (`org/<slug>`) for a model no card covers. Each deployment has at most one
//! master, resolved in this order:
//!
//! 1. the organization's link for the deployment's provider family and model
//!    identifier, when one is set (a link may be to none);
//! 2. otherwise, the card whose `matches` lists that family and identifier
//!    exactly;
//! 3. otherwise, none.
//!
//! Nothing is matched by similarity of names (D-015). Links are catalog
//! metadata: routing reads routes, and a route proposal built from links
//! reaches routing only through a release ([`propose`]).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::catalog::{CatalogOperation, MAX_ROUTE_DEPLOYMENTS, ToolCallIds, tool_call_ids};
use super::model_card::GovernanceActor;

/// The prefix of an organization's own master records.
pub const ORG_PREFIX: &str = "org/";
/// The longest record name.
pub const MAX_NAME_CHARS: usize = 120;
/// The longest slug.
pub const MAX_SLUG_LEN: usize = 64;

/// What a model does. A deployment links only to a master of a modality it
/// serves: a chat deployment never to an embedding model's record.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    #[default]
    Chat,
    Embeddings,
}

impl Modality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embeddings => "embeddings",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "embeddings" => Some(Self::Embeddings),
            _ => None,
        }
    }

    /// The modality a catalog operation serves; `None` for an unknown one.
    pub fn of_operation(operation: &str) -> Option<Self> {
        match operation.parse::<CatalogOperation>().ok()? {
            CatalogOperation::Chat | CatalogOperation::ChatStream | CatalogOperation::ChatBatch => {
                Some(Self::Chat)
            }
            CatalogOperation::Embeddings | CatalogOperation::EmbeddingsBatch => {
                Some(Self::Embeddings)
            }
        }
    }

    pub fn is_chat(&self) -> bool {
        *self == Self::Chat
    }
}

/// Whether `slug` names an organization record: 1 to 64 lowercase letters,
/// digits, `-`, `.` and `_`, starting with a letter or digit.
pub fn valid_slug(slug: &str) -> bool {
    (1..=MAX_SLUG_LEN).contains(&slug.len())
        && slug
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b))
}

/// The slug of an organization record's id (`org/<slug>`), if it is one.
pub fn org_slug(id: &str) -> Option<&str> {
    id.strip_prefix(ORG_PREFIX).filter(|slug| valid_slug(slug))
}

/// Whether `id` has a master id's shape: `<vendor>/<model>` in lowercase
/// letters, digits, `-`, `.` and `_` (a card's id or `org/<slug>`).
pub fn valid_master_id(id: &str) -> bool {
    let part = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b))
    };
    id.len() <= 255
        && id
            .split_once('/')
            .is_some_and(|(vendor, model)| part(vendor) && part(model))
}

/// A record name, trimmed: 1 to 120 characters without control characters.
pub fn normalized_name(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

/// The alias a route proposal defaults to: the master id's model part
/// (`claude-sonnet-4-6` for `anthropic/claude-sonnet-4-6`).
pub fn default_alias(master: &str) -> &str {
    master.split_once('/').map_or(master, |(_, model)| model)
}

/// One of the organization's own master records.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrgMaster {
    /// `org/<slug>`.
    pub id: String,
    pub name: String,
    pub modality: Modality,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Who last wrote it; `null` once that account is gone.
    pub updated_by: Option<GovernanceActor>,
    /// Deleted (kept, and created again at `version`, `If-Match:
    /// "v<version>"`); only the list's `deleted_records` hold these.
    #[serde(default)]
    pub deleted: bool,
}

/// The organization's link for one provider family and model identifier.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MasterLink {
    pub provider_family: String,
    pub model: String,
    /// The master it links to; `null` links to none (or, cleared, to
    /// nothing).
    pub master: Option<String>,
    pub version: u64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<GovernanceActor>,
    /// Cleared: no link is in use, so the card decides. It is kept, and set
    /// again at `version` (`If-Match: "v<version>"`).
    #[serde(default)]
    pub cleared: bool,
}

/// How a deployment's master was decided.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    /// The organization's link (which may be to none).
    Link,
    /// The card that lists the deployment's family and model.
    Card,
    /// Neither: the deployment has no master.
    None,
}

/// A deployment's master: the link when one is set (`Some(None)` is a link
/// to none), else the matching card, else none.
pub fn resolve<'a>(
    link: Option<Option<&'a str>>,
    card: Option<&'a str>,
) -> (Option<&'a str>, ResolvedBy) {
    match (link, card) {
        (Some(master), _) => (master, ResolvedBy::Link),
        (None, Some(card)) => (Some(card), ResolvedBy::Card),
        (None, None) => (None, ResolvedBy::None),
    }
}

/// Why a route proposal leaves a deployment out of an operation's route.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// The active release prices no such operation on it; a route needs a
    /// price.
    NoPrice,
    /// The organization's routing policy does not permit its provider
    /// family.
    ProviderNotPermitted,
    /// The routing policy restricts locations and its location is not one
    /// of them (or it has none).
    LocationNotPermitted,
    /// Its availability evidence is `refused`.
    AvailabilityRefused,
    /// Its availability evidence is `requires_action`.
    AvailabilityRequiresAction,
    /// Tool calling was asked for and it is not set to take tools.
    NoTools,
    /// Tool calling was asked for and its tool call ids differ from the
    /// route's (Gemini's, or an Agent Platform endpoint's, with others).
    MixedToolCallIds,
    /// Priced in another currency than the route; budgets never convert
    /// currencies.
    Currency,
    /// The route already has the most deployments one operation may have.
    TooManyDeployments,
}

/// One deployment of a master, as a proposal sees it.
#[derive(Clone, Debug)]
pub struct ProposalDeployment {
    pub deployment: String,
    pub provider_family: String,
    pub model: String,
    pub location: Option<String>,
    /// The latest availability evidence's state, if any.
    pub availability: Option<String>,
    pub operations: Vec<ProposalOperation>,
}

/// One operation a deployment serves.
#[derive(Clone, Debug)]
pub struct ProposalOperation {
    pub operation: String,
    pub tools: bool,
    /// The currency the release prices the operation in; `None` when the
    /// release prices no such operation. A route's one currency is checked
    /// on release prices, as the manifest's validation does.
    pub currency: Option<String>,
    /// The currency it is billed in now: an effective price override's
    /// when one is in effect, else the release's. A deployment whose
    /// override is in another currency than the route's is left out of the
    /// route while it is in effect (as configuration versions do).
    pub billed_currency: Option<String>,
}

/// A linked deployment an alias routing others of its master leaves out.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteSuggestion {
    pub alias: String,
    pub operation: String,
    pub deployment: String,
}

/// A deployment a proposal routes to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProposedRoute {
    pub operation: String,
    pub deployment: String,
    pub weight: u32,
    pub priority: u32,
}

/// A deployment a proposal leaves out of one operation's route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Exclusion {
    pub operation: String,
    pub deployment: String,
    pub reason: ExclusionReason,
    /// The reason in words.
    pub detail: String,
}

/// The routing policy a proposal respects (`None` for any).
#[derive(Clone, Copy, Debug, Default)]
pub struct PolicyLists<'a> {
    pub providers: Option<&'a [String]>,
    pub locations: Option<&'a [String]>,
}

/// Whether `operation` takes tools (D-032): only chat does.
fn takes_tools(operation: &str) -> bool {
    matches!(operation, "chat" | "chat_batch")
}

/// The routes a proposal makes for each of `operations` over `deployments`
/// (the master's linked deployments in the active release), at weight and
/// priority 1, and every deployment serving an operation that it leaves
/// out, with why. A route takes every deployment that serves the operation,
/// is priced, permitted by the routing policy and not refused or awaiting
/// action; with `tools`, also set to take tools and of one tool call id
/// dialect; then those of the route's currency (the one most of them are
/// priced in; on a tie, the first by code), and at most
/// [`MAX_ROUTE_DEPLOYMENTS`], by name.
///
/// The rules are the manifest's and configuration versions' own: a route is
/// priced in one currency ([`super::catalog::CatalogManifest::validate`]),
/// and an alias takes tools only where every deployment does and their call
/// ids agree ([`tool_call_ids`]), so the routes publish as they are.
pub fn propose(
    operations: &[String],
    tools: bool,
    deployments: &[ProposalDeployment],
    policy: PolicyLists<'_>,
) -> (Vec<ProposedRoute>, Vec<Exclusion>) {
    let mut deployments: Vec<&ProposalDeployment> = deployments.iter().collect();
    deployments.sort_by(|a, b| a.deployment.cmp(&b.deployment));
    let mut routes = Vec::new();
    let mut excluded = Vec::new();
    for operation in operations {
        let exclude = |excluded: &mut Vec<Exclusion>,
                       deployment: &ProposalDeployment,
                       reason: ExclusionReason,
                       detail: String| {
            excluded.push(Exclusion {
                operation: operation.clone(),
                deployment: deployment.deployment.clone(),
                reason,
                detail,
            });
        };
        let with_tools = tools && takes_tools(operation);
        // Each deployment's own checks.
        let mut kept: Vec<(&ProposalDeployment, String, Option<String>)> = Vec::new();
        for deployment in &deployments {
            let Some(served) = deployment
                .operations
                .iter()
                .find(|served| &served.operation == operation)
            else {
                continue;
            };
            let Some(currency) = served.currency.clone() else {
                exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::NoPrice,
                    format!("the active release prices no {operation} on it"),
                );
                continue;
            };
            if policy
                .providers
                .is_some_and(|providers| !providers.contains(&deployment.provider_family))
            {
                exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::ProviderNotPermitted,
                    format!(
                        "the routing policy does not permit provider family {}",
                        deployment.provider_family
                    ),
                );
                continue;
            }
            if !super::routing_policy::permits(
                None,
                policy.locations,
                &deployment.provider_family,
                deployment.location.as_deref(),
            ) {
                let detail = match &deployment.location {
                    Some(location) => {
                        format!("the routing policy does not permit location {location}")
                    }
                    None => "the routing policy restricts locations and it has none".to_owned(),
                };
                exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::LocationNotPermitted,
                    detail,
                );
                continue;
            }
            match deployment.availability.as_deref() {
                Some("refused") => {
                    exclude(
                        &mut excluded,
                        deployment,
                        ExclusionReason::AvailabilityRefused,
                        "the provider refused it".to_owned(),
                    );
                    continue;
                }
                Some("requires_action") => {
                    exclude(
                        &mut excluded,
                        deployment,
                        ExclusionReason::AvailabilityRequiresAction,
                        "the provider needs an action on the account before it serves it"
                            .to_owned(),
                    );
                    continue;
                }
                _ => {}
            }
            if with_tools && !served.tools {
                exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::NoTools,
                    "it is not set to take tools".to_owned(),
                );
                continue;
            }
            kept.push((deployment, currency, served.billed_currency.clone()));
        }

        // One currency: the one most deployments are priced in.
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, currency, _) in &kept {
            *counts.entry(currency.as_str()).or_default() += 1;
        }
        let route_currency = counts
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(currency, _)| (*currency).to_owned());
        let mut priced = Vec::new();
        for (deployment, currency, billed) in kept {
            match (&route_currency, billed) {
                (Some(route), _) if *route != currency => exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::Currency,
                    format!("priced in {currency}; the route is priced in {route}"),
                ),
                (Some(route), Some(billed)) if *route != billed => exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::Currency,
                    format!(
                        "a price override in {billed} is in effect; the route is priced in {route}"
                    ),
                ),
                _ => priced.push(deployment),
            }
        }

        // One tool call id dialect, as configuration versions require of a
        // route of several deployments: an unknown dialect (an endpoint's, or
        // an unknown family's) may differ from any other.
        let mut chosen = priced;
        if with_tools && chosen.len() > 1 {
            let dialects: Vec<Option<ToolCallIds>> = chosen
                .iter()
                .map(|d| tool_call_ids(&d.provider_family, &d.model))
                .collect();
            let count = |wanted: ToolCallIds| {
                dialects
                    .iter()
                    .filter(|dialect| **dialect == Some(wanted))
                    .count()
            };
            let (plain, gemini) = (count(ToolCallIds::Plain), count(ToolCallIds::Gemini));
            let keep = if plain == 0 && gemini == 0 {
                // Only unknown dialects: one deployment on its own.
                None
            } else if gemini > plain {
                Some(ToolCallIds::Gemini)
            } else {
                Some(ToolCallIds::Plain)
            };
            let mut first_unknown_kept = false;
            let mut next = Vec::new();
            for (deployment, dialect) in chosen.into_iter().zip(dialects) {
                let fits = match keep {
                    Some(keep) => dialect == Some(keep),
                    None if !first_unknown_kept => {
                        first_unknown_kept = true;
                        true
                    }
                    None => false,
                };
                if fits {
                    next.push(deployment);
                } else {
                    exclude(
                        &mut excluded,
                        deployment,
                        ExclusionReason::MixedToolCallIds,
                        match keep {
                            Some(ToolCallIds::Gemini) => "its tool call ids differ from Gemini's, which the route takes",
                            Some(_) => "its tool call ids differ from the route's other deployments' (Gemini's, or an Agent Platform endpoint's, are not replayed elsewhere)",
                            None => "an Agent Platform endpoint's tool call ids are known only when it is called, so it cannot share a tool-calling route",
                        }
                        .to_owned(),
                    );
                }
            }
            chosen = next;
        }

        for (index, deployment) in chosen.into_iter().enumerate() {
            if index >= MAX_ROUTE_DEPLOYMENTS {
                exclude(
                    &mut excluded,
                    deployment,
                    ExclusionReason::TooManyDeployments,
                    format!(
                        "a route has at most {MAX_ROUTE_DEPLOYMENTS} deployments per operation"
                    ),
                );
                continue;
            }
            routes.push(ProposedRoute {
                operation: operation.clone(),
                deployment: deployment.deployment.clone(),
                weight: 1,
                priority: 1,
            });
        }
    }
    (routes, excluded)
}

/// How a proposal changes an existing alias's route for one deployment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteChangeKind {
    Added,
    Removed,
    Changed,
    Unchanged,
}

/// Weight and priority, before or after a change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteWeight {
    pub weight: u32,
    pub priority: u32,
}

/// One deployment of one operation, compared with the alias's route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RouteChange {
    pub operation: String,
    pub deployment: String,
    pub change: RouteChangeKind,
    pub before: Option<RouteWeight>,
    pub after: Option<RouteWeight>,
}

/// The proposal compared with the alias's routes of the same operations
/// (`existing`), by operation and deployment.
pub fn diff(existing: &[ProposedRoute], proposed: &[ProposedRoute]) -> Vec<RouteChange> {
    let key = |route: &ProposedRoute| (route.operation.clone(), route.deployment.clone());
    let weight = |route: &ProposedRoute| RouteWeight {
        weight: route.weight,
        priority: route.priority,
    };
    let mut changes: BTreeMap<(String, String), (Option<RouteWeight>, Option<RouteWeight>)> =
        BTreeMap::new();
    for route in existing {
        changes.entry(key(route)).or_default().0 = Some(weight(route));
    }
    for route in proposed {
        changes.entry(key(route)).or_default().1 = Some(weight(route));
    }
    changes
        .into_iter()
        .map(|((operation, deployment), (before, after))| RouteChange {
            change: match (before, after) {
                (None, _) => RouteChangeKind::Added,
                (_, None) => RouteChangeKind::Removed,
                (Some(b), Some(a)) if a == b => RouteChangeKind::Unchanged,
                _ => RouteChangeKind::Changed,
            },
            operation,
            deployment,
            before,
            after,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(
        name: &str,
        family: &str,
        model: &str,
        location: Option<&str>,
        currency: Option<&str>,
        tools: bool,
    ) -> ProposalDeployment {
        ProposalDeployment {
            deployment: name.into(),
            provider_family: family.into(),
            model: model.into(),
            location: location.map(str::to_owned),
            availability: None,
            operations: vec![ProposalOperation {
                operation: "chat".into(),
                tools,
                currency: currency.map(str::to_owned),
                billed_currency: currency.map(str::to_owned),
            }],
        }
    }

    fn reasons(excluded: &[Exclusion]) -> Vec<(&str, ExclusionReason)> {
        excluded
            .iter()
            .map(|e| (e.deployment.as_str(), e.reason))
            .collect()
    }

    #[test]
    fn resolution_prefers_the_link_then_the_card() {
        assert_eq!(
            resolve(Some(Some("org/x")), Some("a/b")),
            (Some("org/x"), ResolvedBy::Link)
        );
        assert_eq!(resolve(Some(None), Some("a/b")), (None, ResolvedBy::Link));
        assert_eq!(resolve(None, Some("a/b")), (Some("a/b"), ResolvedBy::Card));
        assert_eq!(resolve(None, None), (None, ResolvedBy::None));
    }

    #[test]
    fn ids_slugs_names_and_aliases() {
        assert!(valid_slug("qwen-3.8_local"));
        assert!(!valid_slug("-qwen"));
        assert!(!valid_slug("Qwen"));
        assert!(!valid_slug(""));
        assert!(!valid_slug(&"a".repeat(65)));
        assert_eq!(org_slug("org/qwen"), Some("qwen"));
        assert_eq!(org_slug("org/Q"), None);
        assert_eq!(org_slug("anthropic/claude"), None);
        assert!(valid_master_id("anthropic/claude-sonnet-4-6"));
        assert!(!valid_master_id("anthropic"));
        assert!(!valid_master_id("a/b/c"));
        assert_eq!(normalized_name("  Qwen  ").as_deref(), Some("Qwen"));
        assert_eq!(normalized_name(" "), None);
        assert_eq!(normalized_name("a\u{7}"), None);
        assert_eq!(
            default_alias("anthropic/claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(Modality::of_operation("chat_batch"), Some(Modality::Chat));
        assert_eq!(
            Modality::of_operation("embeddings"),
            Some(Modality::Embeddings)
        );
        assert_eq!(Modality::of_operation("nope"), None);
    }

    #[test]
    fn a_proposal_routes_every_usable_deployment_and_says_why_it_leaves_others_out() {
        let mut refused = deployment(
            "e-refused",
            "bedrock",
            "m",
            Some("us-east-1"),
            Some("USD"),
            true,
        );
        refused.availability = Some("refused".into());
        let mut waiting = deployment(
            "f-waiting",
            "bedrock",
            "m",
            Some("us-east-1"),
            Some("USD"),
            true,
        );
        waiting.availability = Some("requires_action".into());
        let deployments = vec![
            deployment(
                "a-anthropic",
                "anthropic",
                "m",
                Some("us"),
                Some("USD"),
                true,
            ),
            deployment(
                "b-bedrock",
                "bedrock",
                "m",
                Some("us-east-1"),
                Some("USD"),
                true,
            ),
            deployment("c-unpriced", "bedrock", "m", Some("us-east-1"), None, true),
            deployment(
                "d-euro",
                "azure-ai-foundry",
                "m",
                Some("us"),
                Some("EUR"),
                true,
            ),
            refused,
            waiting,
            deployment(
                "g-elsewhere",
                "bedrock",
                "m",
                Some("eu-west-1"),
                Some("USD"),
                true,
            ),
            deployment("h-nowhere", "bedrock", "m", None, Some("USD"), true),
            deployment("i-forbidden", "openai", "m", Some("us"), Some("USD"), true),
            deployment(
                "j-no-tools",
                "bedrock",
                "m",
                Some("us-east-1"),
                Some("USD"),
                false,
            ),
            deployment(
                "k-gemini",
                "gemini",
                "gemini-x",
                Some("us"),
                Some("USD"),
                true,
            ),
        ];
        let providers = vec![
            "anthropic".to_owned(),
            "bedrock".to_owned(),
            "azure-ai-foundry".to_owned(),
            "gemini".to_owned(),
        ];
        let locations = vec!["us".to_owned(), "us-east-1".to_owned()];
        let policy = PolicyLists {
            providers: Some(&providers),
            locations: Some(&locations),
        };
        let (routes, excluded) = propose(&["chat".into()], true, &deployments, policy);
        let names: Vec<&str> = routes.iter().map(|r| r.deployment.as_str()).collect();
        assert_eq!(names, ["a-anthropic", "b-bedrock"]);
        assert!(routes.iter().all(|r| r.weight == 1 && r.priority == 1));
        assert_eq!(
            reasons(&excluded),
            [
                ("c-unpriced", ExclusionReason::NoPrice),
                ("e-refused", ExclusionReason::AvailabilityRefused),
                ("f-waiting", ExclusionReason::AvailabilityRequiresAction),
                ("g-elsewhere", ExclusionReason::LocationNotPermitted),
                ("h-nowhere", ExclusionReason::LocationNotPermitted),
                ("i-forbidden", ExclusionReason::ProviderNotPermitted),
                ("j-no-tools", ExclusionReason::NoTools),
                ("d-euro", ExclusionReason::Currency),
                ("k-gemini", ExclusionReason::MixedToolCallIds),
            ]
        );

        // Without tools, the tools rules do not apply.
        let (routes, excluded) = propose(&["chat".into()], false, &deployments, policy);
        let names: Vec<&str> = routes.iter().map(|r| r.deployment.as_str()).collect();
        assert_eq!(
            names,
            ["a-anthropic", "b-bedrock", "j-no-tools", "k-gemini"]
        );
        assert!(!excluded.iter().any(|e| matches!(
            e.reason,
            ExclusionReason::NoTools | ExclusionReason::MixedToolCallIds
        )));
        // An operation nobody serves proposes nothing and excludes nothing.
        let (routes, excluded) = propose(&["embeddings".into()], false, &deployments, policy);
        assert!(routes.is_empty() && excluded.is_empty());
    }

    #[test]
    fn the_route_takes_the_most_common_currency_and_dialect() {
        let deployments = vec![
            deployment("a", "bedrock", "m", None, Some("EUR"), true),
            deployment("b", "bedrock", "m", None, Some("USD"), true),
            deployment("c", "bedrock", "m", None, Some("USD"), true),
        ];
        let (routes, excluded) = propose(
            &["chat".into()],
            false,
            &deployments,
            PolicyLists::default(),
        );
        assert_eq!(routes.len(), 2);
        assert_eq!(reasons(&excluded), [("a", ExclusionReason::Currency)]);
        // A tie goes to the first currency by code.
        let (routes, _) = propose(
            &["chat".into()],
            false,
            &deployments[..2],
            PolicyLists::default(),
        );
        assert_eq!(routes[0].deployment, "a");

        let deployments = vec![
            deployment("a", "gemini", "gemini-x", None, Some("USD"), true),
            deployment("b", "gemini", "gemini-y", None, Some("USD"), true),
            deployment("c", "bedrock", "m", None, Some("USD"), true),
            deployment("d", "gemini", "endpoints/123", None, Some("USD"), true),
        ];
        let (routes, excluded) =
            propose(&["chat".into()], true, &deployments, PolicyLists::default());
        let names: Vec<&str> = routes.iter().map(|r| r.deployment.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(
            reasons(&excluded),
            [
                ("c", ExclusionReason::MixedToolCallIds),
                ("d", ExclusionReason::MixedToolCallIds)
            ]
        );
        // One endpoint on its own is a route of one.
        let (routes, excluded) = propose(
            &["chat".into()],
            true,
            &deployments[3..],
            PolicyLists::default(),
        );
        assert_eq!(routes.len(), 1);
        assert!(excluded.is_empty());
    }

    #[test]
    fn the_routes_currency_is_the_releases_and_an_override_in_another_leaves_it() {
        let overridden = |name: &str, release: &str, billed: &str| {
            let mut d = deployment(name, "bedrock", "m", None, Some(release), false);
            d.operations[0].billed_currency = Some(billed.to_owned());
            d
        };
        // Released in USD, one overridden in EUR: the route is USD (the
        // release's tie would otherwise go to EUR), and the overridden one
        // is out while its override is in effect.
        let deployments = vec![overridden("a", "USD", "USD"), overridden("b", "USD", "EUR")];
        let (routes, excluded) = propose(
            &["chat".into()],
            false,
            &deployments,
            PolicyLists::default(),
        );
        let names: Vec<&str> = routes.iter().map(|r| r.deployment.as_str()).collect();
        assert_eq!(names, ["a"]);
        assert_eq!(reasons(&excluded), [("b", ExclusionReason::Currency)]);
        assert!(excluded[0].detail.contains("override in EUR"));
        // Released in USD and EUR, the EUR one overridden in USD: the
        // release's currencies decide, as the manifest's validation does.
        let deployments = vec![
            overridden("a", "USD", "USD"),
            overridden("b", "USD", "USD"),
            overridden("c", "EUR", "USD"),
        ];
        let (routes, excluded) = propose(
            &["chat".into()],
            false,
            &deployments,
            PolicyLists::default(),
        );
        assert_eq!(routes.len(), 2);
        assert_eq!(reasons(&excluded), [("c", ExclusionReason::Currency)]);
    }

    #[test]
    fn a_route_has_at_most_the_manifests_number_of_deployments() {
        let deployments: Vec<_> = (0..MAX_ROUTE_DEPLOYMENTS + 2)
            .map(|i| {
                deployment(
                    &format!("d{i:02}"),
                    "bedrock",
                    "m",
                    None,
                    Some("USD"),
                    false,
                )
            })
            .collect();
        let (routes, excluded) = propose(
            &["chat".into()],
            false,
            &deployments,
            PolicyLists::default(),
        );
        assert_eq!(routes.len(), MAX_ROUTE_DEPLOYMENTS);
        assert_eq!(excluded.len(), 2);
        assert!(
            excluded
                .iter()
                .all(|e| e.reason == ExclusionReason::TooManyDeployments)
        );
    }

    #[test]
    fn a_diff_names_each_deployments_change() {
        let route = |deployment: &str, weight: u32| ProposedRoute {
            operation: "chat".into(),
            deployment: deployment.into(),
            weight,
            priority: 1,
        };
        let changes = diff(
            &[route("a", 1), route("b", 2), route("c", 1)],
            &[route("a", 1), route("b", 1), route("d", 1)],
        );
        let kinds: Vec<(&str, RouteChangeKind)> = changes
            .iter()
            .map(|c| (c.deployment.as_str(), c.change))
            .collect();
        assert_eq!(
            kinds,
            [
                ("a", RouteChangeKind::Unchanged),
                ("b", RouteChangeKind::Changed),
                ("c", RouteChangeKind::Removed),
                ("d", RouteChangeKind::Added),
            ]
        );
    }
}
