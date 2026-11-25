//! Importing a card from the Hugging Face Hub (Phase 8 milestone 3,
//! `docs/model-card-design.md`): the structured metadata of a repository's
//! model card (`GET {hub}/api/models/{repository}`) read into the card file
//! schema, for a model no curated card covers.
//!
//! Only structured fields are read: the license, the parameter count the Hub
//! computes from the weights, and the evaluation results the card declares in
//! `model-index`. Free-text sections (intended uses, limitations) are not
//! parsed; the card links the repository's page instead. Each fact names the
//! repository page as its source and the import's date; who reported it is
//! the provider when the caller states the repository is the model
//! provider's own, otherwise independent. The parameter count is computed by
//! the Hub, so it is always independent.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde_json::{Value, json};

use super::model_card_files::{CARD_FILE_SCHEMA, CardFile, parse_card};

/// The most evaluation results kept from one import.
pub const MAX_IMPORTED_EVALUATIONS: usize = 50;
/// The largest Hub response read.
pub const MAX_HUB_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TEXT: usize = 200;

/// Whether `repository` is a Hub model repository id: `owner/name`, each
/// part 1 to 96 of letters, digits, `-`, `_` and `.`, not starting with `-`
/// or `.` and without `..`.
pub fn valid_repository(repository: &str) -> bool {
    let Some((owner, name)) = repository.split_once('/') else {
        return false;
    };
    let part = |part: &str| {
        (1..=96).contains(&part.len())
            && !part.starts_with(['-', '.'])
            && !part.contains("..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    part(owner) && part(name)
}

#[derive(Debug, Eq, PartialEq)]
pub enum ImportError {
    /// The response is not a model card the schema can hold.
    Unusable(String),
}

/// The card an import of `repository` makes from the Hub's response `body`.
pub fn card_from_hub(
    repository: &str,
    model: &str,
    body: &Value,
    provider_repository: bool,
    retrieved_at: NaiveDate,
) -> Result<CardFile, ImportError> {
    let page = format!("https://huggingface.co/{repository}");
    let reported_by = if provider_repository {
        "provider"
    } else {
        "independent"
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| {
                !value.is_empty() && value.len() <= MAX_TEXT && !value.chars().any(char::is_control)
            })
            .map(ToOwned::to_owned)
    };
    let fact = |value: Value, reported_by: &str| {
        json!({"value": value, "source_url": page, "retrieved_at": retrieved_at,
               "reported_by": reported_by, "title": "Hugging Face model card"})
    };
    let card_data = body.get("cardData");
    let mut overview = serde_json::Map::new();
    let license = card_data.and_then(|data| data.get("license"));
    let license = match license {
        Some(Value::Array(items)) => {
            let items: Vec<String> = items
                .iter()
                .filter_map(|item| text(Some(item)))
                .take(8)
                .collect();
            (!items.is_empty()).then(|| items.join(", "))
        }
        other => text(other),
    }
    .map(|license| {
        // `other` names its license apart.
        match card_data.and_then(|data| text(data.get("license_name"))) {
            Some(name) if license == "other" => name,
            _ => license,
        }
    });
    if let Some(license) = license {
        overview.insert("license".into(), fact(json!(license), reported_by));
    }
    if let Some(total) = body
        .pointer("/safetensors/total")
        .and_then(Value::as_u64)
        .filter(|total| *total > 0)
    {
        overview.insert(
            "parameters".into(),
            fact(json!(total.to_string()), "independent"),
        );
    }
    let mut evaluations = Vec::new();
    let entries = card_data
        .and_then(|data| data.get("model-index"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    'entries: for entry in entries {
        let results = entry
            .get("results")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for result in results {
            let dataset = text(result.pointer("/dataset/name"))
                .or_else(|| text(result.pointer("/dataset/type")));
            let Some(dataset) = dataset else { continue };
            // A result naming its own https source (a leaderboard) cites it.
            let source = text(result.pointer("/source/url"))
                .filter(|url| url.starts_with("https://") && url::Url::parse(url).is_ok());
            let evaluator = text(result.pointer("/source/name"))
                .or_else(|| text(body.get("author")))
                .unwrap_or_else(|| "Unknown".into());
            let metrics = result
                .get("metrics")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for metric in metrics {
                let score = match metric.get("value") {
                    Some(Value::Number(number)) => Value::Number(number.clone()),
                    other => match text(other) {
                        Some(score) => Value::String(score),
                        None => continue,
                    },
                };
                let metric_name = text(metric.get("name")).or_else(|| text(metric.get("type")));
                let name = match metric_name {
                    Some(metric) => format!("{dataset} ({metric})"),
                    None => dataset.clone(),
                };
                let mut evaluation = json!({
                    "name": name, "score": score, "evaluator": evaluator,
                    "source_url": source.clone().unwrap_or_else(|| page.clone()),
                    "retrieved_at": retrieved_at, "reported_by": reported_by,
                });
                if let Some(task) = text(result.pointer("/task/name"))
                    .or_else(|| text(result.pointer("/task/type")))
                {
                    evaluation["setting"] = json!(task);
                }
                evaluations.push(evaluation);
                if evaluations.len() == MAX_IMPORTED_EVALUATIONS {
                    break 'entries;
                }
            }
        }
    }
    let (owner, name) = repository.split_once('/').unwrap_or(("", repository));
    let id_part = |part: &str| -> String {
        part.chars()
            .map(|c| match c.to_ascii_lowercase() {
                c @ ('a'..='z' | '0'..='9' | '-' | '.' | '_') => c,
                _ => '-',
            })
            .collect()
    };
    let vendor = text(body.get("author")).unwrap_or_else(|| owner.to_owned());
    let mut matches = BTreeMap::new();
    matches.insert("imported".to_owned(), vec![model.to_owned()]);
    let card = json!({
        "schema": CARD_FILE_SCHEMA,
        "id": format!("huggingface/{}--{}", id_part(owner), id_part(name)),
        "name": name,
        "vendor": vendor,
        "matches": matches,
        "overview": overview,
        "evaluations": evaluations,
        "safety": {"model_card_url": fact(json!(page), reported_by)},
    });
    parse_card("huggingface import", &card.to_string())
        .map_err(|error| ImportError::Unusable(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 17).unwrap()
    }

    #[test]
    fn repositories_are_owner_and_name() {
        for ok in ["openai/gpt-oss-120b", "Qwen/Qwen3.5-7B", "a/b_c.d"] {
            assert!(valid_repository(ok), "{ok}");
        }
        for bad in [
            "", "gpt", "a/b/c", "../x", "a/..", "a/.x", "-a/b", "a/b c", "a/b?x=1",
        ] {
            assert!(!valid_repository(bad), "{bad}");
        }
        assert!(!valid_repository(&format!("a/{}", "x".repeat(97))));
    }

    #[test]
    fn structured_metadata_becomes_sourced_facts() {
        let body = json!({
            "id": "openai/gpt-oss-120b", "author": "openai",
            "safetensors": {"total": 120_412_337_984_u64},
            "cardData": {
                "license": "apache-2.0",
                "model-index": [{"name": "gpt-oss-120b", "results": [
                    {"task": {"type": "text-generation", "name": "Text Generation"},
                     "dataset": {"name": "MMLU", "type": "cais/mmlu"},
                     "metrics": [{"type": "acc", "value": 90.0, "name": "accuracy"},
                                 {"type": "f1", "value": {"bad": 1}}]},
                    {"dataset": {"name": "Arena"},
                     "source": {"name": "LMArena", "url": "https://lmarena.ai/leaderboard"},
                     "metrics": [{"type": "elo", "value": "1350"}]},
                    {"metrics": [{"type": "acc", "value": 1}]}
                ]}]
            }
        });
        let card =
            card_from_hub("openai/gpt-oss-120b", "gpt-oss-120b", &body, true, day()).unwrap();
        assert_eq!(card.id, "huggingface/openai--gpt-oss-120b");
        assert_eq!(card.vendor, "openai");
        let license = &card.overview["license"];
        assert_eq!(license.value, json!("apache-2.0"));
        assert_eq!(
            license.source_url,
            "https://huggingface.co/openai/gpt-oss-120b"
        );
        assert_eq!(license.retrieved_at, day());
        assert_eq!(
            serde_json::to_value(license.reported_by).unwrap(),
            json!("provider")
        );
        // The Hub counts parameters itself.
        assert_eq!(
            serde_json::to_value(card.overview["parameters"].reported_by).unwrap(),
            json!("independent")
        );
        assert_eq!(card.evaluations.len(), 2);
        assert_eq!(card.evaluations[0].name, "MMLU (accuracy)");
        assert_eq!(
            card.evaluations[0].setting.as_deref(),
            Some("Text Generation")
        );
        assert_eq!(card.evaluations[0].evaluator, "openai");
        assert_eq!(card.evaluations[1].evaluator, "LMArena");
        assert_eq!(
            card.evaluations[1].source_url,
            "https://lmarena.ai/leaderboard"
        );
        assert_eq!(card.evaluations[1].score, json!("1350"));
        assert_eq!(
            card.safety["model_card_url"].value,
            json!("https://huggingface.co/openai/gpt-oss-120b")
        );
    }

    #[test]
    fn another_party_repository_is_independent_and_other_licenses_are_named() {
        let body = json!({"author": "someone",
            "cardData": {"license": "other", "license_name": "llama4"}});
        let card = card_from_hub("someone/Llama-Fork", "m", &body, false, day()).unwrap();
        assert_eq!(card.overview["license"].value, json!("llama4"));
        assert_eq!(
            serde_json::to_value(card.overview["license"].reported_by).unwrap(),
            json!("independent")
        );
        assert_eq!(card.id, "huggingface/someone--llama-fork");
        // Nothing usable still links the repository.
        let bare = card_from_hub("a/b", "m", &json!({}), false, day()).unwrap();
        assert!(bare.overview.is_empty() && bare.evaluations.is_empty());
        assert_eq!(bare.vendor, "a");
    }

    #[test]
    fn evaluations_are_bounded() {
        let metrics: Vec<Value> = (0..80)
            .map(|i| json!({"type": format!("m{i}"), "value": i}))
            .collect();
        let body = json!({"cardData": {"model-index": [{"results": [
            {"dataset": {"name": "D"}, "metrics": metrics}]}]}});
        let card = card_from_hub("a/b", "m", &body, false, day()).unwrap();
        assert_eq!(card.evaluations.len(), MAX_IMPORTED_EVALUATIONS);
    }
}
