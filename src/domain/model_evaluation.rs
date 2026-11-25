//! An organization's own model evaluations (Phase 8 milestone 4,
//! `docs/model-card-design.md`): a set of items with expected answers, run
//! against several models through the batch API, scored per model.
//!
//! The organization submits a batch whose lines' `custom_id` begin with an
//! item's id (`<item>:<anything>`, so one item can be asked of several
//! aliases in one batch), then records a run from the completed batch. Each
//! output line's answer, the first choice's message text, is matched against
//! the item's expected answer, and only the counts per model are kept.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ITEMS: usize = 1000;
pub const MAX_NAME_CHARS: usize = 120;
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
pub const MAX_EXPECTED_CHARS: usize = 1000;
pub const MAX_ITEM_ID_BYTES: usize = 64;

/// How an answer matches the expected one, after both are trimmed,
/// lowercased and their whitespace runs made single spaces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The whole answer is the expected answer.
    Exact,
    /// The answer contains the expected answer.
    Contains,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationItem {
    /// Letters, digits, `-`, `_` and `.`; unique in the set.
    pub id: String,
    pub expected: String,
    #[serde(rename = "match")]
    pub match_kind: MatchKind,
}

/// Why a set is refused.
pub fn invalid_set(
    name: &str,
    description: Option<&str>,
    items: &[EvaluationItem],
) -> Option<&'static str> {
    let text_ok = |text: &str, max: usize| {
        text.chars().count() <= max && !text.chars().any(|c| c.is_control() && c != '\n')
    };
    if name.trim().is_empty() || !text_ok(name, MAX_NAME_CHARS) || name.contains('\n') {
        return Some("name must be 1 to 120 characters on one line");
    }
    if description.is_some_and(|d| !text_ok(d, MAX_DESCRIPTION_CHARS)) {
        return Some("description must be at most 1000 characters");
    }
    if items.is_empty() || items.len() > MAX_ITEMS {
        return Some("a set has 1 to 1000 items");
    }
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let id_ok = (1..=MAX_ITEM_ID_BYTES).contains(&item.id.len())
            && item
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
        if !id_ok {
            return Some("an item id is 1 to 64 letters, digits, '-', '_' or '.'");
        }
        if !seen.insert(item.id.as_str()) {
            return Some("item ids must be unique");
        }
        if normalized(&item.expected).is_empty() || !text_ok(&item.expected, MAX_EXPECTED_CHARS) {
            return Some("an expected answer is 1 to 1000 characters");
        }
    }
    None
}

fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether `answer` matches `item`.
pub fn matches(item: &EvaluationItem, answer: &str) -> bool {
    let (expected, answer) = (normalized(&item.expected), normalized(answer));
    match item.match_kind {
        MatchKind::Exact => answer == expected,
        MatchKind::Contains => answer.contains(&expected),
    }
}

/// The item a line's `custom_id` names: what comes before the first `:`.
pub fn item_of(custom_id: &str) -> &str {
    custom_id
        .split_once(':')
        .map_or(custom_id, |(item, _)| item)
}

/// A batch output line's answer: the first choice's message text of a
/// successful chat response (`response.body.choices[0].message.content`, a
/// string or a list of text parts). `None` for an error, or a line without
/// text (an embedding).
pub fn answer_of(line: &Value) -> Option<String> {
    if !line.get("error").is_none_or(Value::is_null) {
        return None;
    }
    let response = line.get("response")?;
    if response.get("status_code").and_then(Value::as_u64) != Some(200) {
        return None;
    }
    match response.pointer("/body/choices/0/message/content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text: Vec<&str> = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.concat())
        }
        _ => None,
    }
}

/// One model's score in a run.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelScore {
    pub model: String,
    /// Lines that asked an item of the set.
    pub items: u32,
    /// Of those, the ones with an answer (the rest failed or expired).
    pub answered: u32,
    pub correct: u32,
}

/// Scores each model from the batch's output `lines`, keyed by custom_id,
/// and the model each line ran on, by custom_id. Returns the scores, by
/// model, and how many lines named no item of the set.
pub fn score(
    items: &[EvaluationItem],
    lines: &[Value],
    models: &HashMap<String, String>,
) -> (Vec<ModelScore>, u32) {
    let items: HashMap<&str, &EvaluationItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();
    let mut scores: BTreeMap<String, ModelScore> = BTreeMap::new();
    let mut unmatched = 0_u32;
    for line in lines {
        let Some(custom_id) = line.get("custom_id").and_then(Value::as_str) else {
            unmatched += 1;
            continue;
        };
        let (Some(item), Some(model)) = (items.get(item_of(custom_id)), models.get(custom_id))
        else {
            unmatched += 1;
            continue;
        };
        let score = scores.entry(model.clone()).or_insert_with(|| ModelScore {
            model: model.clone(),
            ..ModelScore::default()
        });
        score.items += 1;
        if let Some(answer) = answer_of(line) {
            score.answered += 1;
            if matches(item, &answer) {
                score.correct += 1;
            }
        }
    }
    (scores.into_values().collect(), unmatched)
}

/// A set, as the API shows it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvaluationSet {
    pub id: uuid::Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub items: Vec<EvaluationItem>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<super::model_card::GovernanceActor>,
    /// The admin resource version, the `ETag` a delete's `If-Match` names.
    /// A set never changes, so it is 1 until the set is deleted. Absent
    /// from responses recorded before it was added, which replay as 1.
    #[serde(default = "first_version")]
    pub version: u64,
}

fn first_version() -> u64 {
    1
}

/// A run, with each model's score.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvaluationRun {
    pub id: uuid::Uuid,
    pub set_id: uuid::Uuid,
    pub batch_id: uuid::Uuid,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub created_by: Option<super::model_card::GovernanceActor>,
    pub unmatched_lines: u32,
    pub results: Vec<ModelScore>,
}

/// A model's score in the latest run of one of the organization's sets,
/// for its card.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrganizationEvaluation {
    pub set_id: uuid::Uuid,
    pub set_name: String,
    pub run_id: uuid::Uuid,
    pub batch_id: uuid::Uuid,
    pub scored_at: chrono::DateTime<chrono::Utc>,
    pub items: u32,
    pub answered: u32,
    pub correct: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, expected: &str, match_kind: MatchKind) -> EvaluationItem {
        EvaluationItem {
            id: id.into(),
            expected: expected.into(),
            match_kind,
        }
    }

    fn answered(custom_id: &str, content: Value) -> Value {
        json!({"custom_id": custom_id, "error": null, "response": {"status_code": 200,
               "body": {"choices": [{"message": {"role": "assistant", "content": content}}]}}})
    }

    #[test]
    fn sets_are_bounded_and_ids_unique() {
        let ok = [item("q1", "Paris", MatchKind::Exact)];
        assert_eq!(invalid_set("Capitals", None, &ok), None);
        assert!(invalid_set("", None, &ok).is_some());
        assert!(invalid_set("a\nb", None, &ok).is_some());
        assert!(invalid_set("n", None, &[]).is_some());
        assert!(invalid_set("n", None, &[item("q 1", "x", MatchKind::Exact)]).is_some());
        assert!(invalid_set("n", None, &[item("q1", "  ", MatchKind::Exact)]).is_some());
        assert!(
            invalid_set(
                "n",
                None,
                &[
                    item("q1", "a", MatchKind::Exact),
                    item("q1", "b", MatchKind::Exact)
                ]
            )
            .is_some()
        );
        assert!(invalid_set("n", Some(&"x".repeat(1001)), &ok).is_some());
    }

    #[test]
    fn answers_match_after_normalizing() {
        let exact = item("q", "Paris", MatchKind::Exact);
        assert!(matches(&exact, "  paris "));
        assert!(!matches(&exact, "Paris, France"));
        let contains = item("q", "New  York", MatchKind::Contains);
        assert!(matches(&contains, "It is new york city."));
        assert_eq!(item_of("q1:claude"), "q1");
        assert_eq!(item_of("q1:a:b"), "q1");
        assert_eq!(item_of("q1"), "q1");
    }

    #[test]
    fn runs_are_scored_per_model() {
        let items = [
            item("q1", "Paris", MatchKind::Exact),
            item("q2", "4", MatchKind::Contains),
        ];
        let lines = vec![
            answered("q1:a", json!("Paris")),
            answered(
                "q2:a",
                json!([{"type": "text", "text": "2+2 = "}, {"type": "text", "text": "4"}]),
            ),
            answered("q1:b", json!("Lyon")),
            json!({"custom_id": "q2:b", "response": null,
                   "error": {"code": "expired", "message": "x"}}),
            answered("q9:b", json!("?")),
            answered("q1:c", json!("Paris")),
        ];
        let models: HashMap<String, String> = [
            ("q1:a", "model-a"),
            ("q2:a", "model-a"),
            ("q1:b", "model-b"),
            ("q2:b", "model-b"),
            ("q9:b", "model-b"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let (scores, unmatched) = score(&items, &lines, &models);
        assert_eq!(
            scores,
            [
                ModelScore {
                    model: "model-a".into(),
                    items: 2,
                    answered: 2,
                    correct: 2
                },
                ModelScore {
                    model: "model-b".into(),
                    items: 2,
                    answered: 1,
                    correct: 0
                },
            ]
        );
        // q9 names no item; q1:c ran on no known line.
        assert_eq!(unmatched, 2);
        assert_eq!(
            answer_of(&json!({"custom_id": "x", "error": null,
            "response": {"status_code": 200, "body": {"data": [{"embedding": [0.1]}]}}})),
            None
        );
    }
}
