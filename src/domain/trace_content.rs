//! What a traced request keeps of its content (D-017): the request as the
//! client sent it, and the response as it came back (a streamed one as
//! assembled), in one JSON document, version 1.
//!
//! An embeddings request keeps its input texts, and its response each
//! vector's index and dimensions with its first [`EMBEDDING_PREVIEW`]
//! values: whole vectors are large and say little to a person reading a
//! trace.
//!
//! Each side is capped at the deployment's limit
//! (`GATEWAY_TRACE_CONTENT_MAX_BYTES`, 5 MiB by default). A side over it is
//! kept with its structure and its text (message content, refusals, tool
//! arguments, embedding inputs) cut, in document order, until it fits, and
//! is marked `truncated`; roles, names and finish reasons are kept whole.
//! When even that is over the cap, trailing messages, inputs or choices are
//! dropped. A stream
//! stops collecting once it holds more than the cap, so a long response
//! never grows a process's memory past it.
//!
//! The document is encrypted under a key of its own, which is sealed by the
//! credential keyring and bound to the organization and trace, so neither
//! the object store nor the database alone yields content.

use std::collections::BTreeMap;

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::domain::{
    identity::OrganizationId,
    inference::{
        ChatMessage, ChatRequest, ChatResponse, ChatRole, ChatStreamEvent, EmbeddingRequest,
        EmbeddingResponse,
    },
};

pub const FORMAT: u32 = 1;
pub const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MIN_MAX_BYTES: usize = 1024;
pub const MAX_MAX_BYTES: usize = 64 * 1024 * 1024;
/// The leading values of each embedding a trace keeps.
pub const EMBEDDING_PREVIEW: usize = 8;
const NONCE_BYTES: usize = 12;
const CONTENT_AAD_CONTEXT: &str = "gateway-trace-content-v1";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Truncated {
    pub request: bool,
    pub response: bool,
}

/// The stored document.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TraceDocument {
    pub format: u32,
    pub request: Value,
    /// Null when the provider returned nothing usable.
    pub response: Value,
    pub truncated: Truncated,
}

impl TraceDocument {
    /// The document of `request` and `response`, each side cut to
    /// `max_bytes`. `response_truncated` says the response was already cut
    /// while it streamed.
    pub fn new(
        request: &ChatRequest,
        response: Option<TracedResponse>,
        response_truncated: bool,
        max_bytes: usize,
    ) -> Self {
        Self::of(
            serde_json::to_value(TracedRequest::from(request)).unwrap_or(Value::Null),
            response.and_then(|response| serde_json::to_value(response).ok()),
            response_truncated,
            max_bytes,
        )
    }

    /// The document of a request and response already in their traced
    /// forms ([`TracedRequest`], [`TracedEmbeddingRequest`] and their
    /// responses), each side cut to `max_bytes`.
    pub fn of(
        mut request: Value,
        response: Option<Value>,
        response_truncated: bool,
        max_bytes: usize,
    ) -> Self {
        let mut response = response.unwrap_or(Value::Null);
        let truncated = Truncated {
            request: fit(&mut request, max_bytes),
            response: fit(&mut response, max_bytes) || response_truncated,
        };
        Self {
            format: FORMAT,
            request,
            response,
            truncated,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracedMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// An assistant message's tool calls, content like the rest (D-032).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TracedToolCall>,
}

impl From<&ChatMessage> for TracedMessage {
    fn from(message: &ChatMessage) -> Self {
        Self {
            role: role_name(message.role).to_owned(),
            content: message.content.clone(),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id.clone(),
            tool_calls: message
                .tool_calls
                .iter()
                .map(|call| TracedToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        }
    }
}

/// A tool the request defined, as the client sent it (D-032).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TracedTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TracedRequest {
    pub messages: Vec<TracedMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// What the client asked for, before the gateway's cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The tools, tool choice and parallel calls the client sent (D-032).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<TracedTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<crate::domain::inference::ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl From<&ChatRequest> for TracedRequest {
    fn from(request: &ChatRequest) -> Self {
        Self {
            messages: request.messages.iter().map(Into::into).collect(),
            temperature: request.temperature,
            max_tokens: request.max_tokens,
            tools: request
                .tools
                .iter()
                .map(|tool| TracedTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                })
                .collect(),
            tool_choice: request.tool_choice.clone(),
            parallel_tool_calls: request.parallel_tool_calls,
        }
    }
}

/// An embeddings request as the client sent it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracedEmbeddingRequest {
    pub input: Vec<String>,
}

impl From<&EmbeddingRequest> for TracedEmbeddingRequest {
    fn from(request: &EmbeddingRequest) -> Self {
        Self {
            input: request.input.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TracedEmbedding {
    pub index: u32,
    pub dimensions: usize,
    /// The vector's first [`EMBEDDING_PREVIEW`] values.
    pub preview: Vec<f32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TracedEmbeddingResponse {
    pub model: String,
    pub embeddings: Vec<TracedEmbedding>,
}

impl From<&EmbeddingResponse> for TracedEmbeddingResponse {
    fn from(response: &EmbeddingResponse) -> Self {
        Self {
            model: response.model().to_owned(),
            embeddings: response
                .data()
                .iter()
                .map(|embedding| TracedEmbedding {
                    index: embedding.index(),
                    dimensions: embedding.vector().len(),
                    preview: embedding
                        .vector()
                        .iter()
                        .take(EMBEDDING_PREVIEW)
                        .copied()
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracedToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub arguments: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracedChoice {
    pub index: u32,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TracedToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TracedResponse {
    pub model: String,
    pub choices: Vec<TracedChoice>,
}

impl From<&ChatResponse> for TracedResponse {
    fn from(response: &ChatResponse) -> Self {
        Self {
            model: response.model().to_owned(),
            choices: response
                .choices()
                .iter()
                .map(|choice| TracedChoice {
                    index: choice.index,
                    content: choice.message.content.clone(),
                    refusal: choice.refusal.clone(),
                    tool_calls: choice
                        .tool_calls
                        .iter()
                        .map(|call| TracedToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect(),
                    finish_reason: choice.finish_reason.as_ref().map(|reason| reason.0.clone()),
                })
                .collect(),
        }
    }
}

/// Builds a streamed response from its events, keeping at most `max_bytes`
/// of text.
#[derive(Debug)]
pub struct StreamAssembler {
    max_bytes: usize,
    kept: usize,
    truncated: bool,
    model: String,
    choices: BTreeMap<u32, TracedChoice>,
    /// Each choice's tool calls by their index in the stream.
    tool_calls: BTreeMap<(u32, u32), TracedToolCall>,
}

impl StreamAssembler {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            kept: 0,
            truncated: false,
            model: String::new(),
            choices: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
        }
    }

    /// Takes `cost` bytes from what is left; false, marking the response
    /// truncated, when it does not fit.
    fn charge(&mut self, cost: usize) -> bool {
        if self.kept.saturating_add(cost) > self.max_bytes {
            self.truncated = true;
            return false;
        }
        self.kept += cost;
        true
    }

    /// Appends what fits of `text` to `into`.
    fn append(&mut self, into: Field, text: &str) {
        let room = self.max_bytes.saturating_sub(self.kept);
        let take = floor_char_boundary(text, room);
        if take < text.len() {
            self.truncated = true;
        }
        if take == 0 {
            return;
        }
        self.kept += take;
        let text = &text[..take];
        match into {
            Field::Content(choice) => self.choice(choice).content.push_str(text),
            Field::Refusal(choice) => self
                .choice(choice)
                .refusal
                .get_or_insert_with(String::new)
                .push_str(text),
            Field::Arguments(key) => self
                .tool_calls
                .entry(key)
                .or_default()
                .arguments
                .push_str(text),
        }
    }

    fn choice(&mut self, index: u32) -> &mut TracedChoice {
        self.choices.entry(index).or_insert_with(|| TracedChoice {
            index,
            ..TracedChoice::default()
        })
    }

    /// Whether choice `index` is kept, adding it while there is room.
    fn has_choice(&mut self, index: u32) -> bool {
        if self.choices.contains_key(&index) {
            return true;
        }
        if !self.charge(ENTRY_COST) {
            return false;
        }
        self.choice(index);
        true
    }

    pub fn push(&mut self, event: &ChatStreamEvent) {
        match event {
            ChatStreamEvent::ResponseStarted(started) => self.model = started.model().to_owned(),
            ChatStreamEvent::ChoiceDelta(delta) => {
                if !self.has_choice(delta.choice_index) {
                    return;
                }
                if let Some(text) = &delta.content_delta {
                    self.append(Field::Content(delta.choice_index), text);
                }
                if let Some(text) = &delta.refusal_delta {
                    self.append(Field::Refusal(delta.choice_index), text);
                }
            }
            ChatStreamEvent::ToolCallDelta(delta) => {
                if !self.has_choice(delta.choice_index) {
                    return;
                }
                let key = (delta.choice_index, delta.tool_call_index);
                if !self.tool_calls.contains_key(&key) {
                    if !self.charge(ENTRY_COST) {
                        return;
                    }
                    self.tool_calls.insert(key, TracedToolCall::default());
                }
                // An id or name is kept whole or not at all; one repeated
                // across chunks is charged only for what it adds.
                for (value, is_name) in [(&delta.id, false), (&delta.name, true)] {
                    let Some(value) = value else { continue };
                    let call = self.tool_calls.entry(key).or_default();
                    let kept = if is_name { &call.name } else { &call.id };
                    if kept.as_deref() == Some(value.as_str()) {
                        continue;
                    }
                    let grows = value
                        .len()
                        .saturating_sub(kept.as_ref().map_or(0, String::len));
                    if !self.charge(grows) {
                        continue;
                    }
                    let call = self.tool_calls.entry(key).or_default();
                    if is_name {
                        call.name = Some(value.clone());
                    } else {
                        call.id = Some(value.clone());
                    }
                }
                self.append(Field::Arguments(key), &delta.arguments_delta);
            }
            ChatStreamEvent::ChoiceFinished(finished) => {
                // Kept whole on a kept choice: its entry's charge covers it.
                let index = finished.choice_index();
                if self.has_choice(index) {
                    self.choice(index).finish_reason = Some(finished.finish_reason().0.clone());
                }
            }
            ChatStreamEvent::ResponseCompleted(completion) => {
                if self.model.is_empty() {
                    self.model = completion.model().to_owned();
                }
            }
            ChatStreamEvent::SafetyAnnotation(_) | ChatStreamEvent::Usage(_) => {}
        }
    }

    /// The response so far, and whether text was dropped.
    pub fn finish(mut self) -> (TracedResponse, bool) {
        for ((choice, _), call) in std::mem::take(&mut self.tool_calls) {
            self.choice(choice).tool_calls.push(call);
        }
        (
            TracedResponse {
                model: self.model,
                choices: self.choices.into_values().collect(),
            },
            self.truncated,
        )
    }
}

/// What a choice or tool call entry is charged beyond its text, id and
/// name: its keys, punctuation and finish reason, roughly.
const ENTRY_COST: usize = 64;

/// The fields `fit` cuts; every other string is kept whole.
const TEXT_FIELDS: [&str; 5] = ["content", "refusal", "arguments", "input", "description"];

#[derive(Clone, Copy)]
enum Field {
    Content(u32),
    Refusal(u32),
    Arguments((u32, u32)),
}

const fn role_name(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::Developer => "developer",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
}

/// The largest index at most `index` that is a char boundary of `text`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    (0..=index)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

/// Cuts the text fields ([`TEXT_FIELDS`]) of `value`, in document order,
/// until it serializes to at most `max_bytes`. Returns whether anything was
/// cut. When the rest (keys, roles, names, numbers) alone is over the cap,
/// trailing entries of its arrays are dropped first; a value that still
/// does not fit becomes null.
pub fn fit(value: &mut Value, max_bytes: usize) -> bool {
    if serialized_len(value) <= max_bytes {
        return false;
    }
    let mut skeleton = value.clone();
    cut(&mut skeleton, &mut 0, false);
    if serialized_len(&skeleton) > max_bytes {
        drop_entries(value, &mut skeleton, max_bytes);
        if serialized_len(&skeleton) > max_bytes {
            *value = Value::Null;
            return true;
        }
    }
    let mut left = max_bytes.saturating_sub(serialized_len(&skeleton));
    cut(value, &mut left, false);
    true
}

/// Shortens the top-level arrays of `value` (an object), in key order, to
/// the leading entries whose skeletons fit in `max_bytes`, keeping
/// `skeleton` (`value` with its text cut) in step.
fn drop_entries(value: &mut Value, skeleton: &mut Value, max_bytes: usize) {
    let (Value::Object(values), Value::Object(skeletons)) = (value, skeleton) else {
        return;
    };
    let keys: Vec<String> = skeletons.keys().cloned().collect();
    for key in keys {
        let total = serialized_len(&Value::Object(skeletons.clone()));
        if total <= max_bytes {
            return;
        }
        let Some(Value::Array(items)) = skeletons.get_mut(&key) else {
            continue;
        };
        // The object with this array empty, then each entry and its comma.
        let mut used = total - serialized_len(&Value::Array(items.clone())) + 2;
        let mut keep = 0;
        for item in items.iter() {
            let cost = serialized_len(item) + usize::from(keep > 0);
            if used + cost > max_bytes {
                break;
            }
            used += cost;
            keep += 1;
        }
        items.truncate(keep);
        if let Some(Value::Array(items)) = values.get_mut(&key) {
            items.truncate(keep);
        }
    }
}

fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// Keeps of each text field what its serialized form fits in `left`, and
/// takes that from `left`. `text` says `value` is a text field.
fn cut(value: &mut Value, left: &mut usize, text: bool) {
    match value {
        Value::String(string) if text => {
            let mut keep = 0;
            for (index, c) in string.char_indices() {
                let cost = escaped_len(c);
                if cost > *left {
                    break;
                }
                *left -= cost;
                keep = index + c.len_utf8();
            }
            string.truncate(keep);
        }
        Value::Array(items) => items.iter_mut().for_each(|item| cut(item, left, text)),
        Value::Object(map) => map
            .iter_mut()
            .for_each(|(key, item)| cut(item, left, TEXT_FIELDS.contains(&key.as_str()))),
        _ => {}
    }
}

/// The bytes `c` takes in a JSON string as `serde_json` writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// What binds a content key, and the content it seals, to its trace.
pub fn content_aad(organization_id: OrganizationId, trace_id: uuid::Uuid) -> Vec<u8> {
    [
        CONTENT_AAD_CONTEXT,
        &organization_id.0.to_string(),
        &trace_id.to_string(),
    ]
    .join("\0")
    .into_bytes()
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ContentCipherError {
    #[error("trace content could not be encrypted")]
    Encryption,
    #[error("trace content could not be decrypted")]
    Decryption,
}

/// A new random content key.
pub fn new_content_key() -> Zeroizing<Vec<u8>> {
    Zeroizing::new(Aes256Gcm::generate_key(&mut OsRng).to_vec())
}

/// `nonce || ciphertext` of `plaintext` under `key`.
pub fn seal_content(
    key: &[u8],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, ContentCipherError> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| ContentCipherError::Encryption)?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| ContentCipherError::Encryption)?;
    Ok([nonce.as_slice(), ciphertext.as_slice()].concat())
}

pub fn open_content(
    key: &[u8],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, ContentCipherError> {
    if sealed.len() <= NONCE_BYTES {
        return Err(ContentCipherError::Decryption);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| ContentCipherError::Decryption)?;
    cipher
        .decrypt(
            Nonce::from_slice(&sealed[..NONCE_BYTES]),
            Payload {
                msg: &sealed[NONCE_BYTES..],
                aad,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| ContentCipherError::Decryption)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{
        ChoiceFinished, FinishReason, ResponseStarted, SafeMetadata, StreamChoiceDelta,
        ToolCallDelta,
    };

    fn message(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_owned(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    #[test]
    fn a_document_within_the_cap_is_kept_whole() {
        let request = ChatRequest {
            messages: vec![
                message(ChatRole::System, "be brief"),
                message(ChatRole::User, "hello"),
            ],
            temperature: Some(0.5),
            max_tokens: Some(64),
            ..Default::default()
        };
        let document = TraceDocument::new(&request, None, false, DEFAULT_MAX_BYTES);
        assert_eq!(
            document.request,
            serde_json::json!({
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "user", "content": "hello"},
                ],
                "temperature": 0.5,
                "max_tokens": 64,
            })
        );
        assert_eq!(document.response, Value::Null);
        assert_eq!(document.truncated, Truncated::default());
    }

    #[test]
    fn an_oversized_side_keeps_its_structure_and_is_marked() {
        let request = ChatRequest {
            messages: vec![
                message(ChatRole::User, &"a\"é".repeat(2000)),
                message(ChatRole::User, &"b".repeat(2000)),
            ],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        };
        let document = TraceDocument::new(&request, None, false, 2048);
        assert!(document.truncated.request);
        assert!(!document.truncated.response);
        assert!(serde_json::to_vec(&document.request).unwrap().len() <= 2048);
        let messages = document.request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        // Text goes first to the first message, cut on a char boundary.
        assert!(messages[0]["content"].as_str().unwrap().starts_with("a\"é"));
        assert_eq!(messages[1]["content"], "");
    }

    #[test]
    fn an_embeddings_document_keeps_inputs_and_each_vectors_start() {
        let request = EmbeddingRequest {
            input: vec!["a".repeat(2000), "b".repeat(2000)],
        };
        let vector: Vec<f32> = (0..1536).map(|value| value as f32).collect();
        let response = EmbeddingResponse::new(
            None,
            "embed-a".into(),
            vec![
                crate::domain::inference::Embedding::new(0, vector.clone()).unwrap(),
                crate::domain::inference::Embedding::new(1, vector).unwrap(),
            ],
            None,
        )
        .unwrap();
        let document = TraceDocument::of(
            serde_json::to_value(TracedEmbeddingRequest::from(&request)).unwrap(),
            serde_json::to_value(TracedEmbeddingResponse::from(&response)).ok(),
            false,
            2048,
        );
        // Inputs are text, cut in order like message content.
        assert!(document.truncated.request);
        let input = document.request["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert!(serde_json::to_vec(&document.request).unwrap().len() <= 2048);
        assert_eq!(input[0], "a".repeat(2000));
        let second = input[1].as_str().unwrap();
        assert!(!second.is_empty() && second.len() < 100 && second.starts_with('b'));
        assert!(!document.truncated.response);
        assert_eq!(
            document.response,
            serde_json::json!({
                "model": "embed-a",
                "embeddings": [
                    {"index": 0, "dimensions": 1536, "preview": [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]},
                    {"index": 1, "dimensions": 1536, "preview": [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]},
                ],
            })
        );
    }

    #[test]
    fn a_stream_is_assembled_and_stops_collecting_at_the_cap() {
        // A choice, "hello", a tool call with its id and name, and 3 bytes
        // of arguments.
        let mut assembler = StreamAssembler::new(ENTRY_COST * 2 + 5 + 6 + 6 + 3);
        let events = [
            ChatStreamEvent::ResponseStarted(
                ResponseStarted::new(None, "model-a".into(), SafeMetadata::default()).unwrap(),
            ),
            ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                choice_index: 0,
                content_delta: Some("hello".into()),
                refusal_delta: None,
                annotations: Vec::new(),
            }),
            ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                choice_index: 0,
                tool_call_index: 0,
                id: Some("call-1".into()),
                name: Some("lookup".into()),
                arguments_delta: "{\"q\":".into(),
            }),
            ChatStreamEvent::ChoiceDelta(StreamChoiceDelta {
                choice_index: 0,
                content_delta: Some(" world".into()),
                refusal_delta: None,
                annotations: Vec::new(),
            }),
            ChatStreamEvent::ChoiceFinished(
                ChoiceFinished::new(0, FinishReason("tool_calls".into())).unwrap(),
            ),
        ];
        for event in &events {
            assembler.push(event);
        }
        let (response, truncated) = assembler.finish();
        assert!(truncated);
        assert_eq!(response.model, "model-a");
        assert_eq!(response.choices.len(), 1);
        let choice = &response.choices[0];
        assert_eq!(choice.content, "hello");
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            choice.tool_calls,
            [TracedToolCall {
                id: Some("call-1".into()),
                name: Some("lookup".into()),
                arguments: "{\"q".into(),
            }]
        );
    }

    #[test]
    fn stream_entries_count_against_the_cap() {
        let mut assembler = StreamAssembler::new(1024);
        for index in 0..100 {
            assembler.push(&ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                choice_index: 0,
                tool_call_index: index,
                id: Some(format!("call-{index}")),
                name: Some("a_long_tool_name".repeat(4)),
                arguments_delta: String::new(),
            }));
        }
        let (response, truncated) = assembler.finish();
        assert!(truncated);
        let calls = &response.choices[0].tool_calls;
        assert!(!calls.is_empty() && calls.len() < 10, "{}", calls.len());
        assert!(serde_json::to_vec(&response).unwrap().len() <= 1024);
    }

    #[test]
    fn a_repeated_tool_call_id_is_charged_once() {
        let mut assembler = StreamAssembler::new(1024);
        for _ in 0..30 {
            assembler.push(&ChatStreamEvent::ToolCallDelta(ToolCallDelta {
                choice_index: 0,
                tool_call_index: 0,
                id: Some("c".repeat(40)),
                name: Some("lookup".into()),
                arguments_delta: "x".into(),
            }));
        }
        let (response, truncated) = assembler.finish();
        assert!(!truncated);
        assert_eq!(response.choices[0].tool_calls[0].arguments, "x".repeat(30));
    }

    #[test]
    fn truncation_keeps_who_said_what() {
        let request = ChatRequest {
            messages: vec![
                message(ChatRole::User, &"a".repeat(4000)),
                message(ChatRole::Assistant, &"b".repeat(4000)),
            ],
            temperature: None,
            max_tokens: None,
            ..Default::default()
        };
        let document = TraceDocument::new(&request, None, false, 1024);
        let messages = document.request["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "");

        let mut response = serde_json::to_value(TracedResponse {
            model: "model-a".into(),
            choices: vec![TracedChoice {
                index: 0,
                content: "c".repeat(4000),
                finish_reason: Some("length".into()),
                ..TracedChoice::default()
            }],
        })
        .unwrap();
        assert!(fit(&mut response, 1024));
        assert_eq!(response["choices"][0]["finish_reason"], "length");
        assert_eq!(response["model"], "model-a");
    }

    #[test]
    fn a_side_whose_structure_is_over_the_cap_drops_trailing_entries() {
        let request = ChatRequest {
            messages: (0..100).map(|_| message(ChatRole::User, "hi")).collect(),
            temperature: None,
            max_tokens: None,
            ..Default::default()
        };
        let document = TraceDocument::new(&request, None, false, MIN_MAX_BYTES);
        assert!(document.truncated.request);
        assert!(serde_json::to_vec(&document.request).unwrap().len() <= MIN_MAX_BYTES);
        let messages = document.request["messages"].as_array().unwrap();
        assert!(!messages.is_empty() && messages.len() < 100);
        assert!(messages.iter().all(|message| message["role"] == "user"));

        // Nothing to drop: the side goes.
        let mut value = serde_json::json!({ "model": "m".repeat(2000) });
        assert!(fit(&mut value, MIN_MAX_BYTES));
        assert_eq!(value, Value::Null);
    }

    #[test]
    fn content_opens_only_with_its_key_and_binding() {
        let key = new_content_key();
        let org = OrganizationId::new();
        let trace = uuid::Uuid::new_v4();
        let sealed = seal_content(&key, &content_aad(org, trace), b"secret").unwrap();
        assert_eq!(
            open_content(&key, &content_aad(org, trace), &sealed)
                .unwrap()
                .as_slice(),
            b"secret"
        );
        assert_eq!(
            open_content(&key, &content_aad(org, uuid::Uuid::new_v4()), &sealed),
            Err(ContentCipherError::Decryption)
        );
        assert_eq!(
            open_content(&new_content_key(), &content_aad(org, trace), &sealed),
            Err(ContentCipherError::Decryption)
        );
    }
}
