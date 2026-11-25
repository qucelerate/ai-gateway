//! Curated model card files (Phase 8 milestone 2, `docs/model-card-design.md`).
//!
//! Card files describe a model's public facts (overview, evaluations,
//! safety), each with where it came from, when it was read and who reported
//! it. Data-handling files describe what a provider family does with
//! requests (training use, retention, residency). Both ship in the
//! repository's `model-cards/` directory, compiled into the gateway so a
//! self-hosted deployment needs no outbound calls, and an operator may add or
//! replace them with `GATEWAY_MODEL_CARDS_DIR`.
//!
//! A card attaches to a catalog model only through an exact identifier its
//! `matches` lists for the deployment's provider family, as price mappings
//! do (D-015). Every fact must carry an `https` source and a retrieval date;
//! a file that does not is refused, so an unsourced number is never shown.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub use super::model_master::Modality;

/// The card file schema.
pub const CARD_FILE_SCHEMA: &str = "qucelerate.model-card.v1";
/// The data-handling file schema.
pub const DATA_HANDLING_SCHEMA: &str = "qucelerate.data-handling.v1";
/// Gemini's services, whose terms differ: the Gemini API (with a key) and
/// Gemini Enterprise Agent Platform (with a Google Cloud identity).
pub const SERVICE_GEMINI_API: &str = "gemini-api";
pub const SERVICE_AGENT_PLATFORM: &str = "agent-platform";

/// The largest card or data-handling file read.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// The most files read from an operator directory.
pub const MAX_OPERATOR_FILES: usize = 2_000;

/// The overview facts a card may give.
pub const OVERVIEW_KEYS: &[&str] = &[
    "release_date",
    "version",
    "family",
    "modalities_in",
    "modalities_out",
    "context_window_tokens",
    "max_output_tokens",
    "knowledge_cutoff",
    "license",
    "open_weight",
    "parameters",
    "retirement",
];
/// The safety facts a card may give.
pub const SAFETY_KEYS: &[&str] = &[
    "model_card_url",
    "safety_evaluations_url",
    "acceptable_use_url",
    "intended_uses",
    "prohibited_uses",
    "known_limitations",
];
/// The facts a data-handling file may give.
pub const DATA_HANDLING_KEYS: &[&str] = &[
    "training_use",
    "shared_with_model_provider",
    "provider_access",
    "retention",
    "abuse_monitoring",
    "zero_data_retention",
    "residency",
    "encryption",
    "terms_url",
];

/// Who reported a fact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedBy {
    /// The model's provider, about its own model.
    Provider,
    /// Someone else: an evaluator, or a hub computing it from the weights.
    Independent,
}

/// One public fact with its source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub value: serde_json::Value,
    pub source_url: String,
    pub retrieved_at: NaiveDate,
    pub reported_by: ReportedBy,
    /// The source's title, for a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A benchmark result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evaluation {
    pub name: String,
    pub score: serde_json::Value,
    /// Shots, tools, thinking budget, and the like.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<String>,
    /// Who ran it.
    pub evaluator: String,
    pub source_url: String,
    pub retrieved_at: NaiveDate,
    pub reported_by: ReportedBy,
}

/// A card file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CardFile {
    pub schema: String,
    /// `<vendor>/<model>`, as the file's path.
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// What the model does: `chat`, the default, or `embeddings`. A
    /// deployment links to the card only in its modality (D-033).
    #[serde(default, skip_serializing_if = "Modality::is_chat")]
    pub modality: Modality,
    /// Exact catalog model identifiers, per provider family.
    pub matches: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub overview: BTreeMap<String, Fact>,
    #[serde(default)]
    pub evaluations: Vec<Evaluation>,
    #[serde(default)]
    pub safety: BTreeMap<String, Fact>,
}

/// A data-handling file: one per provider family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataHandlingFile {
    pub schema: String,
    pub provider_family: String,
    /// Which of the family's services the terms cover, when they differ by
    /// service: `gemini-api` or `agent-platform` for Gemini, a deployment's
    /// model format (`anthropic`) for Azure AI Foundry. Absent: the family's
    /// default terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub name: String,
    /// Which of the family's models the terms cover, when not all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<String>,
    pub facts: BTreeMap<String, Fact>,
}

/// Where a file came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOrigin {
    /// Shipped with the gateway.
    Builtin,
    /// The operator's `GATEWAY_MODEL_CARDS_DIR`.
    Operator,
    /// Imported by the organization from a Hugging Face Hub model card,
    /// for a model no file covers.
    Imported,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CardFileError {
    #[error("{0}: {1}")]
    Invalid(String, String),
    #[error("{path}: model {model} on {family} is already matched by {other}")]
    DuplicateMatch {
        path: String,
        family: String,
        model: String,
        other: String,
    },
    #[error("{0}: cannot be read")]
    Unreadable(String),
    #[error("GATEWAY_MODEL_CARDS_DIR has more than {MAX_OPERATOR_FILES} files")]
    TooManyFiles,
}

fn invalid(path: &str, reason: impl Into<String>) -> CardFileError {
    CardFileError::Invalid(path.to_owned(), reason.into())
}

/// A fact's value is a string, a number, a boolean or a list of strings;
/// the console shows nothing else.
fn shown_value(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => true,
        Value::Array(items) => items.iter().all(Value::is_string),
        Value::Null | Value::Object(_) => false,
    }
}

fn check_fact(path: &str, at: &str, fact: &Fact) -> Result<(), CardFileError> {
    check_source(path, at, &fact.source_url)?;
    if !shown_value(&fact.value) {
        return Err(invalid(
            path,
            format!("{at}: value must be a string, number, boolean or list of strings"),
        ));
    }
    Ok(())
}

fn check_source(path: &str, at: &str, source: &str) -> Result<(), CardFileError> {
    match url::Url::parse(source) {
        Ok(url) if url.scheme() == "https" && url.host_str().is_some() => Ok(()),
        _ => Err(invalid(
            path,
            format!("{at}: source_url must be an https URL"),
        )),
    }
}

fn check_keys(
    path: &str,
    section: &str,
    facts: &BTreeMap<String, Fact>,
    allowed: &[&str],
) -> Result<(), CardFileError> {
    for (key, fact) in facts {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid(
                path,
                format!("{section}.{key} is not a known fact"),
            ));
        }
        check_fact(path, &format!("{section}.{key}"), fact)?;
    }
    Ok(())
}

/// Parses and checks a card file; `path` names it in errors.
pub fn parse_card(path: &str, text: &str) -> Result<CardFile, CardFileError> {
    let card: CardFile =
        serde_json::from_str(text).map_err(|error| invalid(path, error.to_string()))?;
    if card.schema != CARD_FILE_SCHEMA {
        return Err(invalid(path, format!("schema must be {CARD_FILE_SCHEMA}")));
    }
    let id_ok = card.id.split_once('/').is_some_and(|(vendor, model)| {
        let part = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._".contains(&b))
        };
        part(vendor) && part(model)
    });
    if !id_ok {
        return Err(invalid(
            path,
            "id must be <vendor>/<model> in lowercase letters, digits, '-', '.' and '_'",
        ));
    }
    // `org/<slug>` names an organization's own master records (D-033).
    if card.id.starts_with(super::model_master::ORG_PREFIX) {
        return Err(invalid(
            path,
            "the vendor `org` is reserved for organization records",
        ));
    }
    if card.name.trim().is_empty() || card.vendor.trim().is_empty() {
        return Err(invalid(path, "name and vendor are required"));
    }
    if card.matches.values().all(Vec::is_empty) {
        return Err(invalid(path, "matches lists no model identifier"));
    }
    for (family, models) in &card.matches {
        // As `model_card_match` holds them (migration 0141): a family of 1
        // to 64 bytes, a model of 1 to 255.
        if !valid_match_family(family) || models.iter().any(|m| !super::model_card::valid_model(m))
        {
            return Err(invalid(
                path,
                format!("matches.{family} has an invalid entry"),
            ));
        }
    }
    check_keys(path, "overview", &card.overview, OVERVIEW_KEYS)?;
    check_keys(path, "safety", &card.safety, SAFETY_KEYS)?;
    for (index, evaluation) in card.evaluations.iter().enumerate() {
        let at = format!("evaluations[{index}]");
        if evaluation.name.trim().is_empty() || evaluation.evaluator.trim().is_empty() {
            return Err(invalid(
                path,
                format!("{at}: name and evaluator are required"),
            ));
        }
        if !(evaluation.score.is_string() || evaluation.score.is_number()) {
            return Err(invalid(
                path,
                format!("{at}: score must be a string or number"),
            ));
        }
        check_source(path, &at, &evaluation.source_url)?;
    }
    Ok(card)
}

/// Parses and checks a data-handling file.
pub fn parse_data_handling(path: &str, text: &str) -> Result<DataHandlingFile, CardFileError> {
    let file: DataHandlingFile =
        serde_json::from_str(text).map_err(|error| invalid(path, error.to_string()))?;
    if file.schema != DATA_HANDLING_SCHEMA {
        return Err(invalid(
            path,
            format!("schema must be {DATA_HANDLING_SCHEMA}"),
        ));
    }
    if file.provider_family.is_empty() || file.name.trim().is_empty() {
        return Err(invalid(path, "provider_family and name are required"));
    }
    if file.facts.is_empty() {
        return Err(invalid(path, "facts is empty"));
    }
    let service_ok = |service: &str| {
        (1..=64).contains(&service.len())
            && service
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if file
        .service
        .as_deref()
        .is_some_and(|service| !service_ok(service))
    {
        return Err(invalid(
            path,
            "service must be 1-64 lowercase letters, digits and '-'",
        ));
    }
    check_keys(path, "facts", &file.facts, DATA_HANDLING_KEYS)?;
    Ok(file)
}

/// The card files shipped with the gateway: `(path, contents)` under
/// `model-cards/`. A test checks this list against the directory.
pub const BUILTIN_FILES: &[(&str, &str)] = &[
    (
        "amazon/nova-2-lite.json",
        include_str!("../../model-cards/amazon/nova-2-lite.json"),
    ),
    (
        "amazon/nova-micro.json",
        include_str!("../../model-cards/amazon/nova-micro.json"),
    ),
    (
        "amazon/nova-pro.json",
        include_str!("../../model-cards/amazon/nova-pro.json"),
    ),
    (
        "amazon/titan-embed-text-v2.json",
        include_str!("../../model-cards/amazon/titan-embed-text-v2.json"),
    ),
    (
        "anthropic/claude-haiku-4-5.json",
        include_str!("../../model-cards/anthropic/claude-haiku-4-5.json"),
    ),
    (
        "anthropic/claude-haiku-5-5.json",
        include_str!("../../model-cards/anthropic/claude-haiku-5-5.json"),
    ),
    (
        "anthropic/claude-opus-4-6.json",
        include_str!("../../model-cards/anthropic/claude-opus-4-6.json"),
    ),
    (
        "anthropic/claude-opus-5-5.json",
        include_str!("../../model-cards/anthropic/claude-opus-5-5.json"),
    ),
    (
        "anthropic/claude-sonnet-4-6.json",
        include_str!("../../model-cards/anthropic/claude-sonnet-4-6.json"),
    ),
    (
        "anthropic/claude-sonnet-5-5.json",
        include_str!("../../model-cards/anthropic/claude-sonnet-5-5.json"),
    ),
    (
        "cohere/embed-v4.json",
        include_str!("../../model-cards/cohere/embed-v4.json"),
    ),
    (
        "data-handling/azure-ai-foundry-anthropic.json",
        include_str!("../../model-cards/data-handling/azure-ai-foundry-anthropic.json"),
    ),
    (
        "data-handling/azure-ai-foundry.json",
        include_str!("../../model-cards/data-handling/azure-ai-foundry.json"),
    ),
    (
        "data-handling/bedrock.json",
        include_str!("../../model-cards/data-handling/bedrock.json"),
    ),
    (
        "data-handling/gemini-agent-platform.json",
        include_str!("../../model-cards/data-handling/gemini-agent-platform.json"),
    ),
    (
        "data-handling/gemini-api.json",
        include_str!("../../model-cards/data-handling/gemini-api.json"),
    ),
    (
        "google/gemini-4-argon.json",
        include_str!("../../model-cards/google/gemini-4-argon.json"),
    ),
    (
        "google/gemini-embedding-2.json",
        include_str!("../../model-cards/google/gemini-embedding-2.json"),
    ),
    (
        "openai/gpt-5-mini.json",
        include_str!("../../model-cards/openai/gpt-5-mini.json"),
    ),
    (
        "openai/gpt-5-nano.json",
        include_str!("../../model-cards/openai/gpt-5-nano.json"),
    ),
    (
        "openai/gpt-6-astra.json",
        include_str!("../../model-cards/openai/gpt-6-astra.json"),
    ),
    (
        "openai/gpt-6-luna.json",
        include_str!("../../model-cards/openai/gpt-6-luna.json"),
    ),
    (
        "openai/gpt-6.1-sol.json",
        include_str!("../../model-cards/openai/gpt-6.1-sol.json"),
    ),
    (
        "openai/gpt-oss-120b.json",
        include_str!("../../model-cards/openai/gpt-oss-120b.json"),
    ),
    (
        "openai/gpt-oss-20b.json",
        include_str!("../../model-cards/openai/gpt-oss-20b.json"),
    ),
    (
        "qwen/qwen3-235b-a22b-instruct-2507.json",
        include_str!("../../model-cards/qwen/qwen3-235b-a22b-instruct-2507.json"),
    ),
    (
        "xai/grok-4.7.json",
        include_str!("../../model-cards/xai/grok-4.7.json"),
    ),
];

/// The directory data-handling files live in, under the cards' root.
const DATA_HANDLING_DIR: &str = "data-handling/";

/// The cards and data-handling files in force.
#[derive(Clone, Debug, Default)]
pub struct CardLibrary {
    cards: Vec<(CardFile, FileOrigin)>,
    /// `(family, model)` → index into `cards`.
    index: HashMap<(String, String), usize>,
    /// By `(family, service)`; `service` empty for the family's default.
    data_handling: HashMap<(String, String), (DataHandlingFile, FileOrigin)>,
    /// The service each of the process's own adapters calls, by adapter
    /// instance key, where the family has several.
    process_services: HashMap<String, &'static str>,
}

impl CardLibrary {
    /// The shipped files only.
    pub fn builtin() -> Result<Self, CardFileError> {
        Self::build(
            BUILTIN_FILES
                .iter()
                .map(|(path, text)| ((*path).to_owned(), (*text).to_owned(), FileOrigin::Builtin)),
        )
    }

    /// The shipped files, then the operator's: an operator card with a
    /// shipped card's `id`, or a data-handling file for the same family,
    /// replaces it.
    pub fn load(operator_dir: Option<&Path>) -> Result<Self, CardFileError> {
        let mut files: Vec<(String, String, FileOrigin)> = BUILTIN_FILES
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned(), FileOrigin::Builtin))
            .collect();
        if let Some(dir) = operator_dir {
            files.extend(
                read_dir(dir)?
                    .into_iter()
                    .map(|(path, text)| (path, text, FileOrigin::Operator)),
            );
        }
        Self::build(files.into_iter())
    }

    fn build(
        files: impl Iterator<Item = (String, String, FileOrigin)>,
    ) -> Result<Self, CardFileError> {
        let mut cards: BTreeMap<String, (CardFile, FileOrigin, String)> = BTreeMap::new();
        let mut data_handling = HashMap::new();
        for (path, text, origin) in files {
            if path.starts_with(DATA_HANDLING_DIR) {
                let file = parse_data_handling(&path, &text)?;
                let key = (
                    file.provider_family.clone(),
                    file.service.clone().unwrap_or_default(),
                );
                data_handling.insert(key, (file, origin));
            } else {
                let card = parse_card(&path, &text)?;
                cards.insert(card.id.clone(), (card, origin, path));
            }
        }
        let mut library = Self {
            data_handling,
            ..Self::default()
        };
        for (_, (card, origin, path)) in cards {
            let position = library.cards.len();
            for (family, models) in &card.matches {
                for model in models {
                    let key = (family.clone(), model.clone());
                    if let Some(other) = library.index.get(&key) {
                        return Err(CardFileError::DuplicateMatch {
                            path,
                            family: family.clone(),
                            model: model.clone(),
                            other: library.cards[*other].0.id.clone(),
                        });
                    }
                    library.index.insert(key, position);
                }
            }
            library.cards.push((card, origin));
        }
        Ok(library)
    }

    /// The card matching a deployment's model exactly.
    pub fn card(&self, family: &str, model: &str) -> Option<(&CardFile, FileOrigin)> {
        self.index
            .get(&(family.to_owned(), model.to_owned()))
            .map(|&i| (&self.cards[i].0, self.cards[i].1))
    }

    /// A provider family's default data-handling terms.
    pub fn data_handling(&self, family: &str) -> Option<(&DataHandlingFile, FileOrigin)> {
        self.data_handling
            .get(&(family.to_owned(), String::new()))
            .map(|(file, origin)| (file, *origin))
    }

    /// The terms that cover a deployment of `family` on `service`: that
    /// service's file, else the family's default. With the service unknown,
    /// every file of the family, since any of them may apply.
    pub fn data_handling_for(
        &self,
        family: &str,
        service: Option<&str>,
    ) -> Vec<(&DataHandlingFile, FileOrigin)> {
        let mut found: Vec<(&DataHandlingFile, FileOrigin)> = match service {
            Some(service) => self
                .data_handling
                .get(&(family.to_owned(), service.to_owned()))
                .or_else(|| self.data_handling.get(&(family.to_owned(), String::new())))
                .map(|(file, origin)| (file, *origin))
                .into_iter()
                .collect(),
            None => self
                .data_handling
                .iter()
                .filter(|((f, _), _)| f == family)
                .map(|(_, (file, origin))| (file, *origin))
                .collect(),
        };
        found.sort_by(|a, b| a.0.service.cmp(&b.0.service));
        found
    }

    /// Records the service the process's own adapter `key` calls.
    pub fn set_process_service(&mut self, key: String, service: &'static str) {
        self.process_services.insert(key, service);
    }

    /// The service the process's own adapter `key` calls, if recorded.
    pub fn process_service(&self, key: &str) -> Option<&'static str> {
        self.process_services.get(key).copied()
    }

    /// Whether `family` has terms that differ by service.
    pub fn has_services(&self, family: &str) -> bool {
        self.data_handling
            .keys()
            .any(|(f, service)| f == family && !service.is_empty())
    }

    pub fn card_count(&self) -> usize {
        self.cards.len()
    }

    /// Every card, by id.
    pub fn cards(&self) -> impl Iterator<Item = (&CardFile, FileOrigin)> {
        self.cards.iter().map(|(card, origin)| (card, *origin))
    }

    /// The card with `id`.
    pub fn card_by_id(&self, id: &str) -> Option<(&CardFile, FileOrigin)> {
        self.cards
            .iter()
            .find(|(card, _)| card.id == id)
            .map(|(card, origin)| (card, *origin))
    }
}

/// How deep an operator directory is read.
const MAX_DEPTH: usize = 8;

/// Every `.json` file under `dir`, as `(path relative to dir, contents)`,
/// with `/` separators. Links are followed and entries whose names start
/// with `.` are skipped, so a projected ConfigMap or Secret is read through
/// its visible links, never its hidden `..data` and timestamped backing
/// directories.
fn read_dir(dir: &Path) -> Result<Vec<(String, String)>, CardFileError> {
    let mut out = Vec::new();
    let mut pending: Vec<(PathBuf, Vec<String>)> = vec![(dir.to_path_buf(), Vec::new())];
    let name = |path: &Path| path.display().to_string();
    while let Some((current, relative)) = pending.pop() {
        let entries =
            std::fs::read_dir(&current).map_err(|_| CardFileError::Unreadable(name(&current)))?;
        for entry in entries {
            let entry = entry.map_err(|_| CardFileError::Unreadable(name(&current)))?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            // Follows links: a ConfigMap's visible entries are links.
            let metadata =
                std::fs::metadata(&path).map_err(|_| CardFileError::Unreadable(name(&path)))?;
            let mut at = relative.clone();
            at.push(file_name);
            if metadata.is_dir() {
                if at.len() > MAX_DEPTH {
                    return Err(invalid(&name(&path), "nested too deeply"));
                }
                pending.push((path, at));
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if out.len() >= MAX_OPERATOR_FILES {
                return Err(CardFileError::TooManyFiles);
            }
            if metadata.len() > MAX_FILE_BYTES {
                return Err(invalid(&name(&path), "larger than 256 KiB"));
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|_| CardFileError::Unreadable(name(&path)))?;
            out.push((at.join("/"), text));
        }
    }
    out.sort();
    Ok(out)
}

/// A `matches` key: a provider family name of 1 to 64 bytes, no controls.
fn valid_match_family(family: &str) -> bool {
    (1..=64).contains(&family.len()) && !family.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_5_5_cards_list_bedrocks_inference_profiles() {
        // SYSTEM_DEFINED inference profiles, ListInferenceProfiles on
        // 2026-11-19 (ca. in ca-central-1): no jp. profile of Sonnet 5.5,
        // no ca. one of Opus 5.5.
        let library = CardLibrary::builtin().unwrap();
        for (model, prefixes) in [
            (
                "haiku-5-5",
                &["", "us.", "global.", "eu.", "au.", "jp.", "ca."][..],
            ),
            ("opus-5-5", &["", "us.", "global.", "eu.", "au.", "jp."][..]),
            (
                "sonnet-5-5",
                &["", "us.", "global.", "eu.", "au.", "ca."][..],
            ),
        ] {
            for prefix in prefixes {
                let id = format!("{prefix}anthropic.claude-{model}");
                assert_eq!(
                    library
                        .card("bedrock", &id)
                        .map(|(card, _)| card.id.as_str()),
                    Some(format!("anthropic/claude-{model}").as_str()),
                    "{id}"
                );
            }
        }
        assert!(
            library
                .card("bedrock", "jp.anthropic.claude-sonnet-5-5")
                .is_none()
        );
        assert!(
            library
                .card("bedrock", "ca.anthropic.claude-opus-5-5")
                .is_none()
        );
    }

    #[test]
    fn shipped_files_parse_and_are_all_listed() {
        let library = CardLibrary::builtin().unwrap();
        assert!(library.card_count() >= 3);
        assert!(library.card("bedrock", "openai.gpt-oss-120b-1:0").is_some());
        assert!(library.data_handling("bedrock").is_some());
        // Every file in model-cards/ is compiled in.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("model-cards");
        let on_disk: Vec<String> = read_dir(&root)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let mut listed: Vec<String> = BUILTIN_FILES.iter().map(|(p, _)| (*p).to_owned()).collect();
        listed.sort();
        assert_eq!(
            on_disk, listed,
            "BUILTIN_FILES must list every model-cards/*.json"
        );
        // A card's path is its id.
        for (path, text) in BUILTIN_FILES {
            if !path.starts_with(DATA_HANDLING_DIR) {
                let card = parse_card(path, text).unwrap();
                assert_eq!(format!("{}.json", card.id), *path);
            }
        }
    }

    fn card_json(id: &str, model: &str, source: &str) -> String {
        serde_json::json!({
            "schema": CARD_FILE_SCHEMA,
            "id": id,
            "name": "M",
            "vendor": "V",
            "matches": {"bedrock": [model]},
            "overview": {"license": {
                "value": "mit", "source_url": source,
                "retrieved_at": "2026-11-14", "reported_by": "provider"}},
        })
        .to_string()
    }

    #[test]
    fn match_entries_fit_the_stored_matches() {
        let with = |family: &str, model: &str| {
            let mut card: serde_json::Value =
                serde_json::from_str(&card_json("v/m", "x", "https://example.com/m")).unwrap();
            card["matches"] = serde_json::json!({ family: [model] });
            card.to_string()
        };
        assert!(parse_card("a", &with(&"f".repeat(64), "x")).is_ok());
        assert!(parse_card("a", &with(&"f".repeat(65), "x")).is_err());
        assert!(parse_card("a", &with("", "x")).is_err());
        assert!(parse_card("a", &with("bedrock", &"m".repeat(255))).is_ok());
        assert!(parse_card("a", &with("bedrock", &"m".repeat(256))).is_err());
    }

    #[test]
    fn unsourced_or_unknown_facts_are_refused() {
        assert!(parse_card("a", &card_json("v/m", "x", "https://example.com/m")).is_ok());
        assert!(matches!(
            parse_card("a", &card_json("v/m", "x", "http://example.com/m")),
            Err(CardFileError::Invalid(_, _))
        ));
        assert!(parse_card("a", &card_json("v/m", "x", "")).is_err());
        let no_date =
            card_json("v/m", "x", "https://e.com").replace("\"retrieved_at\":\"2026-11-14\",", "");
        assert!(parse_card("a", &no_date).is_err());
        let unknown = card_json("v/m", "x", "https://e.com").replace("\"license\"", "\"vibes\"");
        assert!(parse_card("a", &unknown).is_err());
        assert!(parse_card("a", &card_json("Vendor/M", "x", "https://e.com")).is_err());
        let extra =
            card_json("v/m", "x", "https://e.com").replace("\"name\"", "\"extra\":1,\"name\"");
        assert!(parse_card("a", &extra).is_err());
    }

    #[test]
    fn operator_files_replace_by_id_and_family_and_cannot_double_match() {
        let dir = std::env::temp_dir().join(format!("cards-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("openai")).unwrap();
        std::fs::create_dir_all(dir.join("data-handling")).unwrap();
        std::fs::write(
            dir.join("openai/gpt-oss-120b.json"),
            card_json(
                "openai/gpt-oss-120b",
                "private-120b",
                "https://example.com/c",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("data-handling/bedrock.json"),
            serde_json::json!({
                "schema": DATA_HANDLING_SCHEMA, "provider_family": "bedrock", "name": "Ours",
                "facts": {"retention": {"value": "none", "source_url": "https://example.com/t",
                    "retrieved_at": "2026-11-14", "reported_by": "provider"}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let library = CardLibrary::load(Some(&dir)).unwrap();
        // The shipped card's match is gone; the operator's replaces it.
        assert!(library.card("bedrock", "openai.gpt-oss-120b-1:0").is_none());
        let (card, origin) = library.card("bedrock", "private-120b").unwrap();
        assert_eq!(card.id, "openai/gpt-oss-120b");
        assert_eq!(origin, FileOrigin::Operator);
        assert_eq!(library.data_handling("bedrock").unwrap().0.name, "Ours");
        // A second card claiming a matched model is refused.
        std::fs::write(
            dir.join("openai/other.json"),
            card_json(
                "openai/other",
                "openai.gpt-oss-20b-1:0",
                "https://example.com/c",
            ),
        )
        .unwrap();
        assert!(matches!(
            CardLibrary::load(Some(&dir)),
            Err(CardFileError::DuplicateMatch { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(
            CardLibrary::load(Some(&dir)),
            Err(CardFileError::Unreadable(_))
        ));
    }

    /// A projected ConfigMap: visible links into a hidden, timestamped
    /// directory that `..data` points at.
    #[cfg(unix)]
    #[test]
    fn a_projected_config_map_is_read_through_its_visible_links() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("cards-cm-{}", uuid::Uuid::new_v4()));
        let backing = dir.join("..2026_11_14_10_00_00.1");
        std::fs::create_dir_all(backing.join("data-handling")).unwrap();
        std::fs::write(
            backing.join("data-handling/bedrock.json"),
            serde_json::json!({
                "schema": DATA_HANDLING_SCHEMA, "provider_family": "bedrock", "name": "Ours",
                "facts": {"retention": {"value": "none", "source_url": "https://example.com/t",
                    "retrieved_at": "2026-11-14", "reported_by": "provider"}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            backing.join("private.json"),
            card_json("acme/private", "acme-private", "https://example.com/c"),
        )
        .unwrap();
        symlink("..2026_11_14_10_00_00.1", dir.join("..data")).unwrap();
        symlink("..data/data-handling", dir.join("data-handling")).unwrap();
        symlink("..data/private.json", dir.join("private.json")).unwrap();
        let files: Vec<String> = read_dir(&dir)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(files, ["data-handling/bedrock.json", "private.json"]);
        let library = CardLibrary::load(Some(&dir)).unwrap();
        assert_eq!(library.data_handling("bedrock").unwrap().0.name, "Ours");
        assert!(library.card("bedrock", "acme-private").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
