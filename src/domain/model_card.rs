//! Model cards (Phase 8, D-022, `docs/model-card-design.md`).
//!
//! A card describes one catalog model (the model identifier its deployments
//! name) with what the gateway already knows about it: the deployments that
//! serve it and their prices, availability evidence, this organization's
//! usage, how its requests went, and the catalog changes that touched it.
//! Curated public facts (overview, evaluations, safety, data handling) come
//! from card files (`model_card_files`); a model no card file matches says
//! that no public card is on file.

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use std::collections::BTreeMap;

use super::model_card_files::{CardLibrary, Evaluation, Fact, FileOrigin};
use super::reporting::UsageAggregate;

/// The card document's schema.
pub const MODEL_CARD_SCHEMA: &str = "qucelerate.model-card.v1";
/// The usage windows a card reports, in days, ending now.
pub const USAGE_WINDOW_DAYS: [i64; 3] = [7, 30, 90];
/// The window request outcomes and timings cover, in days.
pub const REQUEST_WINDOW_DAYS: i64 = 30;
/// Most governance entries a card lists, newest first.
pub const MAX_GOVERNANCE_ENTRIES: usize = 25;
/// Most releases read for a card's governance history.
pub const MAX_GOVERNANCE_RELEASES: i64 = 500;
/// The longest model identifier a card is looked up by (as the catalog
/// allows).
pub const MAX_MODEL_LEN: usize = 255;

/// How one request went, kept on its always-kept request record. No content
/// (D-017).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RequestTimings {
    /// From sending the request to the provider to its last event or failure.
    pub provider_latency_ms: Option<u32>,
    /// Streams only: from sending the request to the first content event.
    pub first_event_ms: Option<u32>,
    /// Why the first choice finished, as [`finish_reason_code`] records it.
    pub finish_reason: Option<String>,
}

impl RequestTimings {
    pub fn is_empty(&self) -> bool {
        self.provider_latency_ms.is_none()
            && self.first_event_ms.is_none()
            && self.finish_reason.is_none()
    }
}

/// Milliseconds in `elapsed`, saturating.
pub fn millis(elapsed: std::time::Duration) -> u32 {
    u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX)
}

/// A provider's finish reason as recorded: lowercase, and `other` unless it
/// is a short code (`stop`, `content_filter`, `end_turn`), so nothing a
/// provider puts there that could carry content is kept.
pub fn finish_reason_code(reason: &str) -> String {
    let code = reason.trim().to_ascii_lowercase();
    let short = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.:-".contains(&b));
    if short { code } else { "other".to_owned() }
}

/// Finish reasons that mean a safety system stopped or refused the
/// response, across providers.
pub fn is_safety_finish_reason(code: &str) -> bool {
    matches!(
        code,
        "content_filter"
            | "content_filtered"
            | "refusal"
            | "safety"
            | "blocklist"
            | "prohibited_content"
            | "spii"
            | "recitation"
            | "guardrail_intervened"
            | "image_safety"
    )
}

/// A model identifier as a card is looked up by: the catalog's own, exactly.
pub fn valid_model(model: &str) -> bool {
    !model.is_empty() && model.len() <= MAX_MODEL_LEN && !model.chars().any(char::is_control)
}

/// The card document.
#[derive(Clone, Debug, Serialize)]
pub struct ModelCard {
    pub schema: &'static str,
    pub model: String,
    pub generated_at: DateTime<Utc>,
    /// The active release the catalog sections are read from.
    pub release_id: Uuid,
    pub overview: CardOverview,
    /// Curated public facts (overview, evaluations, safety, data handling).
    pub public_card: PublicCard,
    /// The gateway's own handling; absent without `tracing_read`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_data_handling: Option<GatewayDataHandling>,
    /// *In this organization*: each deployment that serves the model.
    pub deployments: Vec<CardDeployment>,
    /// *Usage here*; absent when the caller cannot read usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<CardUsage>,
    /// Absent when the caller cannot read the audit log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub governance: Option<CardGovernance>,
    /// The organization's own note on the model; `null` when none is set.
    pub organization_note: Option<CardNote>,
    /// Each of the organization's evaluation sets that scored the model,
    /// with its latest run's score: the organization's own, not a
    /// benchmark.
    pub organization_evaluations: Vec<crate::domain::model_evaluation::OrganizationEvaluation>,
    /// Sections left out and why.
    pub omitted: Vec<OmittedSection>,
}

/// The organization's note on a model (Governance), visible to every role
/// that can read the catalog.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct CardNote {
    pub note: String,
    pub version: u64,
    pub updated_at: DateTime<Utc>,
    /// Who last set it; `null` once that account is gone.
    pub updated_by: Option<GovernanceActor>,
}

/// The most characters a note may hold.
impl CardImport {
    /// The card as a card section, matched to `deployments` (every one
    /// serving the model: an import is for the model, not a family).
    pub fn public_card(&self, deployments: Vec<String>) -> PublicCardFile {
        PublicCardFile {
            id: self.card.id.clone(),
            name: self.card.name.clone(),
            vendor: self.card.vendor.clone(),
            origin: crate::domain::model_card_files::FileOrigin::Imported,
            matched_deployments: deployments,
            overview: self.card.overview.clone(),
            evaluations: self.card.evaluations.clone(),
            safety: self.card.safety.clone(),
        }
    }
}

/// A card the organization imported from the Hugging Face Hub, shown for
/// its model when no card file matches.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct CardImport {
    pub model: String,
    /// The Hub repository (`owner/name`).
    pub repository: String,
    /// Whether the importer stated the repository is the model provider's
    /// own, so its facts are self-reported.
    pub provider_repository: bool,
    pub card: crate::domain::model_card_files::CardFile,
    pub version: u64,
    pub imported_at: DateTime<Utc>,
    pub imported_by: Option<GovernanceActor>,
}

pub const MAX_NOTE_CHARS: usize = 4000;

/// A note as an admin sets it: trimmed, without control characters other
/// than newlines and tabs, at most [`MAX_NOTE_CHARS`]. Empty clears it.
pub fn normalized_note(note: &str) -> Option<String> {
    let note = note.trim();
    let ok = note.chars().count() <= MAX_NOTE_CHARS
        && !note
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r');
    ok.then(|| note.replace("\r\n", "\n"))
}

/// One catalog model as the Models table lists it, with the facts its
/// columns show: from the matching card file, and the earliest retirement
/// date a provider listing reports for its deployments.
#[derive(Clone, Debug, Serialize)]
pub struct ModelCardSummary {
    pub model: String,
    pub provider_families: Vec<String>,
    /// The matching card file; `null` when no card is on file.
    pub card: Option<CardRef>,
    pub context_window_tokens: Option<Fact>,
    pub max_output_tokens: Option<Fact>,
    /// The provider's published retirement commitment, from the card file.
    pub retirement: Option<Fact>,
    /// The earliest date a provider listing says inference on one of its
    /// deployments retires (Azure's model catalog).
    pub retires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardRef {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub origin: FileOrigin,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CardOverview {
    /// The name a provider listing gives the model, when one does.
    pub display_name: Option<String>,
    pub provider_families: Vec<String>,
    pub operations: Vec<String>,
    pub locations: Vec<String>,
    /// The largest request any deployment accepts.
    pub max_input_bytes: Option<i64>,
    /// The most output tokens any deployment allows.
    pub max_output_tokens: Option<i64>,
}

/// Curated public facts from card files (`model_card_files`), and the
/// data-handling terms of each provider family serving the model.
#[derive(Clone, Debug, Serialize)]
pub struct PublicCard {
    pub status: PublicCardStatus,
    /// The matching card; absent when `not_on_file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub card: Option<PublicCardFile>,
    /// One per provider family serving the model that has a data-handling
    /// file, by family.
    pub data_handling: Vec<PublicDataHandling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicCardStatus {
    OnFile,
    NotOnFile,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicCardFile {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub origin: FileOrigin,
    /// The deployments it matched, by name.
    pub matched_deployments: Vec<String>,
    pub overview: BTreeMap<String, Fact>,
    pub evaluations: Vec<Evaluation>,
    pub safety: BTreeMap<String, Fact>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PublicDataHandling {
    pub provider_family: String,
    /// The family's service the terms cover; absent for its default terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub name: String,
    /// The deployments serving the model that these terms cover. A
    /// deployment whose service the gateway cannot tell is listed under
    /// each of its family's terms.
    pub deployments: Vec<String>,
    pub origin: FileOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<String>,
    pub facts: BTreeMap<String, Fact>,
}

/// The gateway's own handling of this organization's requests; needs
/// `tracing_read`.
#[derive(Clone, Debug, Serialize)]
pub struct GatewayDataHandling {
    /// `off`, `organization` or `selected`.
    pub trace_mode: &'static str,
    /// Traced requests keep their content.
    pub trace_content: bool,
    pub trace_content_retention_days: u32,
}

/// What a deployment is joined to card files by.
#[derive(Clone, Debug, Default)]
pub struct CardJoin {
    pub deployment: String,
    pub family: String,
    /// Model identifiers to match, in order: the catalog's, then the model
    /// a provider listing says the deployment serves (an Azure deployment
    /// named apart from its model).
    pub models: Vec<String>,
    /// The family's service the deployment runs on, when the gateway can
    /// tell (see [`CardLibrary::data_handling_for`]).
    pub service: Option<String>,
}

impl PublicCard {
    /// The card and data-handling terms `library` has for the deployments.
    /// When the deployments match different cards, the one matching most of
    /// them wins (ties by id).
    pub fn from_library(library: &CardLibrary, deployments: &[CardJoin]) -> Self {
        let mut matched: BTreeMap<
            &str,
            (
                &crate::domain::model_card_files::CardFile,
                FileOrigin,
                Vec<String>,
            ),
        > = BTreeMap::new();
        for join in deployments {
            if let Some((card, origin)) = join
                .models
                .iter()
                .find_map(|model| library.card(&join.family, model))
            {
                matched
                    .entry(card.id.as_str())
                    .or_insert((card, origin, Vec::new()))
                    .2
                    .push(join.deployment.clone());
            }
        }
        let best = matched
            .into_values()
            .max_by(|a, b| a.2.len().cmp(&b.2.len()).then_with(|| b.0.id.cmp(&a.0.id)));
        let card = best.map(|(card, origin, mut names)| {
            names.sort();
            PublicCardFile {
                id: card.id.clone(),
                name: card.name.clone(),
                vendor: card.vendor.clone(),
                origin,
                matched_deployments: names,
                overview: card.overview.clone(),
                evaluations: card.evaluations.clone(),
                safety: card.safety.clone(),
            }
        });
        // Each file of terms, with the deployments it covers.
        let mut terms: BTreeMap<(String, String), PublicDataHandling> = BTreeMap::new();
        for join in deployments {
            for (file, origin) in library.data_handling_for(&join.family, join.service.as_deref()) {
                terms
                    .entry((
                        file.provider_family.clone(),
                        file.service.clone().unwrap_or_default(),
                    ))
                    .or_insert_with(|| PublicDataHandling {
                        provider_family: file.provider_family.clone(),
                        service: file.service.clone(),
                        name: file.name.clone(),
                        deployments: Vec::new(),
                        origin,
                        applies_to: file.applies_to.clone(),
                        facts: file.facts.clone(),
                    })
                    .deployments
                    .push(join.deployment.clone());
            }
        }
        let data_handling = terms
            .into_values()
            .map(|mut terms| {
                terms.deployments.sort();
                terms.deployments.dedup();
                terms
            })
            .collect();
        Self {
            status: if card.is_some() {
                PublicCardStatus::OnFile
            } else {
                PublicCardStatus::NotOnFile
            },
            card,
            data_handling,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CardDeployment {
    pub deployment: String,
    pub adapter_instance_key: String,
    pub provider_family: String,
    pub location: Option<String>,
    /// Aliases routed to it, by operation.
    pub routes: Vec<CardRoute>,
    pub operations: Vec<CardOperation>,
    /// What the gateway last learned about calling the model on this
    /// account (D-018); `null` when nothing yet.
    pub availability: Option<CardAvailability>,
    /// What the provider's own listing says of the deployment (Azure lists
    /// an account's deployments and its model catalog); `null` for
    /// listings of models, which say none of this.
    pub provider_listing: Option<CardListing>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CardListing {
    /// The model format, which says which API serves it (`OpenAI`,
    /// `Anthropic`, `xAI`, ...).
    pub format: Option<String>,
    /// The model the deployment serves, which can differ from its name.
    pub model: Option<String>,
    pub version: Option<String>,
    /// The deployment type (`GlobalStandard`, `DataZoneBatch`, ...).
    pub sku: Option<String>,
    /// The model version's lifecycle status in the account's model catalog
    /// (`GenerallyAvailable`, `Deprecating`, ...).
    pub lifecycle_status: Option<String>,
    /// When inference on that model version retires, per the catalog.
    pub retires_at: Option<DateTime<Utc>>,
    /// When the listing was last read.
    pub listed_at: DateTime<Utc>,
    /// The content filter policy the deployment applies, as the cloud
    /// reports it (Azure's `raiPolicyName`).
    pub content_filter: Option<String>,
    /// That policy's settings, when the account's policy list was read.
    pub content_filter_policy: Option<crate::domain::discovery::ContentFilterPolicy>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CardRoute {
    pub alias: String,
    pub operation: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardOperation {
    pub operation: String,
    pub max_input_bytes: i64,
    pub max_output_tokens: i64,
    /// Whether it takes function tools (D-032).
    pub tools: bool,
    /// The price billed now: the release's, or an effective price override's.
    /// `None` when the release prices no such operation, so no route uses it.
    pub price: Option<CardPrice>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardPrice {
    pub currency: String,
    /// The release price's currency (`currency` is an override's when one
    /// is in effect); not shown, as `currency` is what is billed.
    #[serde(skip)]
    pub release_currency: Option<String>,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    /// `release` or `override`.
    pub billed_by: &'static str,
    /// The release price's source and version (an override's version).
    pub source: Option<String>,
    pub source_version: Option<String>,
    pub confidence: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardAvailability {
    pub state: String,
    pub reason: Option<String>,
    pub source: String,
    pub observed_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardUsage {
    /// Usage over each window, as the usage report gives it for the model.
    pub windows: Vec<UsageWindow>,
    pub requests: RequestStats,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageWindow {
    pub days: i64,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Per `(currency, confidence)`.
    pub totals: Vec<UsageAggregate>,
}

/// How the model's requests went over [`REQUEST_WINDOW_DAYS`].
#[derive(Clone, Debug, Default, Serialize)]
pub struct RequestStats {
    pub days: i64,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub request_count: i64,
    /// Requests the provider refused or failed, by error category.
    pub failed_count: i64,
    pub errors: Vec<CountBy>,
    /// From when timings and finish reasons are recorded for this model;
    /// `null` when no request in the window has them.
    pub timed_since: Option<DateTime<Utc>>,
    pub timed_request_count: i64,
    pub latency_p50_ms: Option<i64>,
    pub latency_p95_ms: Option<i64>,
    pub first_event_p50_ms: Option<i64>,
    pub first_event_p95_ms: Option<i64>,
    pub finish_reasons: Vec<FinishReasonCount>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CountBy {
    pub key: String,
    pub count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FinishReasonCount {
    pub reason: String,
    pub count: i64,
    /// A safety system stopped or refused the response.
    pub safety: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardGovernance {
    /// The oldest release that served the model, of those read.
    pub first_release: Option<GovernanceEntry>,
    /// Releases that changed the model's deployments or prices, newest first.
    pub history: Vec<GovernanceEntry>,
    /// Older changes exist than those listed.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct GovernanceEntry {
    pub release_id: Uuid,
    pub release_version: String,
    pub created_at: DateTime<Utc>,
    pub actor: Option<GovernanceActor>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Deployments whose operations, limits or prices changed.
    pub changed: Vec<String>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct GovernanceActor {
    /// `user`, `service_account` or `system`.
    pub actor_type: String,
    pub id: Option<Uuid>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OmittedSection {
    pub section: &'static str,
    /// The permission that would show it.
    pub requires: &'static str,
}

/// What changed for the model between two releases' views of it: each a
/// deployment's name and a fingerprint of its operations, limits and prices.
pub fn release_changes(
    before: &[(String, String)],
    after: &[(String, String)],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let find = |set: &[(String, String)], name: &str| {
        set.iter()
            .find(|(deployment, _)| deployment == name)
            .map(|(_, f)| f.clone())
    };
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for (name, fingerprint) in after {
        match find(before, name) {
            None => added.push(name.clone()),
            Some(previous) if previous != *fingerprint => changed.push(name.clone()),
            Some(_) => {}
        }
    }
    let removed = before
        .iter()
        .filter(|(name, _)| find(after, name).is_none())
        .map(|(name, _)| name.clone())
        .collect();
    (added, removed, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_reasons_are_short_codes_or_other() {
        assert_eq!(finish_reason_code("STOP"), "stop");
        assert_eq!(finish_reason_code(" content_filter "), "content_filter");
        assert_eq!(finish_reason_code("end_turn"), "end_turn");
        assert_eq!(finish_reason_code(""), "other");
        assert_eq!(finish_reason_code("the user asked about x"), "other");
        assert_eq!(finish_reason_code(&"a".repeat(65)), "other");
        assert!(is_safety_finish_reason("content_filter"));
        assert!(is_safety_finish_reason("refusal"));
        assert!(!is_safety_finish_reason("stop"));
    }

    #[test]
    fn release_changes_name_added_removed_and_changed_deployments() {
        let entry = |name: &str, f: &str| (name.to_owned(), f.to_owned());
        let before = [entry("a", "1"), entry("b", "1"), entry("c", "1")];
        let after = [entry("a", "1"), entry("b", "2"), entry("d", "1")];
        let (added, removed, changed) = release_changes(&before, &after);
        assert_eq!(added, ["d"]);
        assert_eq!(removed, ["c"]);
        assert_eq!(changed, ["b"]);
    }

    #[test]
    fn public_cards_join_exactly_and_prefer_the_card_most_deployments_match() {
        let library = CardLibrary::builtin().unwrap();
        let d = |name: &str, family: &str, model: &str| CardJoin {
            deployment: name.to_owned(),
            family: family.to_owned(),
            models: vec![model.to_owned()],
            service: None,
        };
        let card = PublicCard::from_library(
            &library,
            &[
                d("b-120", "bedrock", "openai.gpt-oss-120b-1:0"),
                d("c-120", "openai-compatible", "gpt-oss-120b"),
            ],
        );
        assert_eq!(card.status, PublicCardStatus::OnFile);
        let file = card.card.unwrap();
        assert_eq!(file.id, "openai/gpt-oss-120b");
        assert_eq!(file.matched_deployments, ["b-120"]);
        assert!(
            file.overview
                .values()
                .all(|f| f.source_url.starts_with("https://"))
        );
        assert_eq!(card.data_handling.len(), 1);
        assert_eq!(card.data_handling[0].provider_family, "bedrock");
        assert_eq!(card.data_handling[0].deployments, ["b-120"]);

        let none = PublicCard::from_library(
            &library,
            &[d("x", "openai-compatible", "openai.gpt-oss-120b-1:0")],
        );
        assert_eq!(none.status, PublicCardStatus::NotOnFile);
        assert!(none.card.is_none());
        assert!(none.data_handling.is_empty());

        // An Azure deployment named apart from its model matches through
        // the model its listing names.
        let mut named = d("azure-x", "azure-ai-foundry", "my-claude");
        named.models.push("claude-opus-5-5".into());
        let card = PublicCard::from_library(&library, &[named]);
        assert_eq!(card.card.unwrap().id, "anthropic/claude-opus-5-5");
    }

    #[test]
    fn data_handling_follows_the_service_and_lists_every_option_when_unknown() {
        let library = CardLibrary::builtin().unwrap();
        let join = |name: &str, service: Option<&str>| CardJoin {
            deployment: name.to_owned(),
            family: "gemini".to_owned(),
            models: vec!["gemini-4-argon".to_owned()],
            service: service.map(str::to_owned),
        };
        let card = PublicCard::from_library(&library, &[join("g-ap", Some("agent-platform"))]);
        let services: Vec<_> = card
            .data_handling
            .iter()
            .map(|t| t.service.clone())
            .collect();
        assert_eq!(services, [Some("agent-platform".to_owned())]);
        let card = PublicCard::from_library(
            &library,
            &[join("g-ap", Some("agent-platform")), join("g-x", None)],
        );
        let listed: Vec<_> = card
            .data_handling
            .iter()
            .map(|t| (t.service.clone().unwrap(), t.deployments.clone()))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "agent-platform".to_owned(),
                    vec!["g-ap".to_owned(), "g-x".to_owned()]
                ),
                ("gemini-api".to_owned(), vec!["g-x".to_owned()]),
            ]
        );
        // Claude on Foundry has its own terms; other formats the default.
        let foundry = |name: &str, service: Option<&str>| CardJoin {
            deployment: name.to_owned(),
            family: "azure-ai-foundry".to_owned(),
            models: vec![name.to_owned()],
            service: service.map(str::to_owned),
        };
        let card = PublicCard::from_library(
            &library,
            &[
                foundry("claude-opus-5-5", Some("anthropic")),
                foundry("gpt-6-luna", Some("openai")),
            ],
        );
        let listed: Vec<_> = card
            .data_handling
            .iter()
            .map(|t| (t.service.clone(), t.deployments.clone()))
            .collect();
        assert_eq!(
            listed,
            [
                (None, vec!["gpt-6-luna".to_owned()]),
                (
                    Some("anthropic".to_owned()),
                    vec!["claude-opus-5-5".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn notes_are_trimmed_bounded_and_free_of_control_characters() {
        assert_eq!(
            normalized_note("  ok\r\nline  ").as_deref(),
            Some("ok\nline")
        );
        assert_eq!(normalized_note("").as_deref(), Some(""));
        assert!(normalized_note("a\u{7}b").is_none());
        assert!(normalized_note(&"x".repeat(MAX_NOTE_CHARS)).is_some());
        assert!(normalized_note(&"x".repeat(MAX_NOTE_CHARS + 1)).is_none());
    }

    #[test]
    fn models_are_looked_up_exactly() {
        assert!(valid_model("us.amazon.nova-2-lite-v1:0"));
        assert!(valid_model("publishers/google/models/gemini-4"));
        assert!(!valid_model(""));
        assert!(!valid_model("a\nb"));
        assert!(!valid_model(&"m".repeat(256)));
    }
}
