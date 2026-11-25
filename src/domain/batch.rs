//! Batches (D-021, docs/batch-design.md): an OpenAI-shaped input file of
//! single requests, validated in full before anything is reserved or sent.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::catalog::CatalogOperation;
use super::inference::{ChatRequest, EmbeddingRequest};
use super::public_request::{self, ChatBody, EmbeddingBody};

/// The most lines one batch takes. The strictest native limit the gateway
/// serves today is OpenAI's 50,000; a batch is reserved in one transaction,
/// so the gateway starts lower.
pub const MAX_BATCH_LINES: usize = 10_000;
/// The largest file the files endpoint accepts.
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
/// The largest provider result file the worker reads in one piece.
pub const MAX_RESULT_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// A job's lines are grouped so the most their results can come to stays
/// under this, well inside [`MAX_RESULT_FILE_BYTES`]: a job whose results
/// could outgrow what the worker reads would never be collected.
pub const MAX_JOB_RESULT_BYTES: u64 = 192 * 1024 * 1024;
/// Validation reports at most this many line errors.
pub const MAX_LINE_ERRORS: usize = 100;
pub const MAX_CUSTOM_ID_CHARS: usize = 512;
pub const MAX_METADATA_ENTRIES: usize = 16;
pub const MAX_METADATA_KEY_CHARS: usize = 64;
pub const MAX_METADATA_VALUE_CHARS: usize = 512;
/// What a batch's public id starts with: `batch_<uuid>`.
pub const BATCH_ID_PREFIX: &str = "batch_";

/// A batch's id as the batches APIs, usage records and traces give it.
pub fn public_batch_id(id: uuid::Uuid) -> String {
    format!("{BATCH_ID_PREFIX}{id}")
}

/// A batch named as the batches APIs name it (`batch_<uuid>`), or by its
/// bare UUID, as usage filters and evaluation runs take it.
pub fn parse_batch_ref(value: &str) -> Option<uuid::Uuid> {
    let value = value.trim();
    value
        .strip_prefix(BATCH_ID_PREFIX)
        .unwrap_or(value)
        .parse::<uuid::Uuid>()
        .ok()
        .filter(|_| !value.is_empty())
}

/// A batch's line reservations last its completion window plus this grace,
/// so results that land at the end of the window still settle.
pub const RESERVATION_GRACE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
pub const COMPLETION_WINDOW: &str = "24h";
pub const COMPLETION_WINDOW_DURATION: std::time::Duration =
    std::time::Duration::from_secs(24 * 3600);

/// How a provider job runs (D-021).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BatchMode {
    /// Through the provider's own batch API, at the batch price.
    Native,
    /// By the gateway, each line an ordinary request at the on-demand price,
    /// where the organization allows it.
    Emulated,
}

impl BatchMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Emulated => "emulated",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "native" => Some(Self::Native),
            "emulated" => Some(Self::Emulated),
            _ => None,
        }
    }

    /// Which price the mode's lines are reserved and charged at.
    pub const fn price_source(self) -> &'static str {
        match self {
            Self::Native => "batch",
            Self::Emulated => "on_demand",
        }
    }
}

/// The storage a staged provider job keeps its input and output in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagingKind {
    /// Amazon S3, for Bedrock.
    S3,
    /// Google Cloud Storage, for Gemini on Agent Platform.
    Gcs,
}

impl StagingKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gcs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "s3" => Some(Self::S3),
            "gcs" => Some(Self::Gcs),
            _ => None,
        }
    }
}

/// The most lines of one emulated job in flight at once.
pub const MAX_EMULATED_CONCURRENCY: u32 = 32;
pub const DEFAULT_EMULATED_CONCURRENCY: u32 = 4;

/// An organization's batch settings, as the admin API takes them.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchSettingsInput {
    pub emulated_allowed: bool,
    #[serde(default)]
    pub emulated_concurrency: Option<u32>,
}

impl BatchSettingsInput {
    pub fn validate(self) -> Result<(bool, u32), &'static str> {
        let concurrency = self
            .emulated_concurrency
            .unwrap_or(DEFAULT_EMULATED_CONCURRENCY);
        if !(1..=MAX_EMULATED_CONCURRENCY).contains(&concurrency) {
            return Err("emulated_concurrency must be between 1 and 32");
        }
        Ok((self.emulated_allowed, concurrency))
    }
}

/// A connection's batch staging, as the admin API takes it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchStagingInput {
    pub bucket: String,
    #[serde(default)]
    pub prefix: Option<String>,
    /// The bucket's region; S3 only.
    #[serde(default)]
    pub region: Option<String>,
    /// The service role a Bedrock batch job assumes; S3 only.
    #[serde(default)]
    pub role_arn: Option<String>,
}

/// A checked staging: bucket, prefix (empty or ending in `/`), region and
/// role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidStaging {
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    pub role_arn: Option<String>,
}

impl BatchStagingInput {
    /// Checks the staging for `kind`, as the migration's checks do, so a
    /// refusal names the field rather than failing in the database.
    pub fn validate(&self, kind: StagingKind) -> Result<ValidStaging, &'static str> {
        let bucket = self.bucket.trim();
        let bucket_ok = (3..=222).contains(&bucket.len())
            && bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && bucket
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && bucket
                .bytes()
                .last()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !bucket_ok {
            return Err(
                "bucket must be a bucket name of 3 to 222 lowercase letters, digits, dots, dashes and underscores",
            );
        }
        let prefix = self.prefix.as_deref().unwrap_or("").trim();
        let prefix = if prefix.is_empty() || prefix.ends_with('/') {
            prefix.to_owned()
        } else {
            format!("{prefix}/")
        };
        let prefix_ok = prefix.is_empty()
            || (prefix.len() <= 256
                && !prefix.starts_with('/')
                && !prefix.contains("//")
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!_.*'()/-".contains(&b)));
        if !prefix_ok {
            return Err("prefix may hold letters, digits and !_.*'()/- only, at most 256 of them");
        }
        let region = self
            .region
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let role_arn = self
            .role_arn
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        match kind {
            StagingKind::S3 => {
                let Some(region) = region.filter(|r| aws_region(r)) else {
                    return Err("region must be the bucket's AWS region");
                };
                let Some(role_arn) = role_arn.filter(|r| iam_role_arn(r)) else {
                    return Err("role_arn must be the IAM role the Bedrock batch job assumes");
                };
                Ok(ValidStaging {
                    bucket: bucket.to_owned(),
                    prefix,
                    region: Some(region.to_owned()),
                    role_arn: Some(role_arn.to_owned()),
                })
            }
            StagingKind::Gcs => {
                if region.is_some() || role_arn.is_some() {
                    return Err("a Cloud Storage staging takes no region or role_arn");
                }
                Ok(ValidStaging {
                    bucket: bucket.to_owned(),
                    prefix,
                    region: None,
                    role_arn: None,
                })
            }
        }
    }
}

fn aws_region(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() >= 3
        && parts[0].len() == 2
        && parts[0].bytes().all(|b| b.is_ascii_lowercase())
        && parts[1..].iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
        && parts
            .last()
            .is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()))
}

fn iam_role_arn(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("arn:") else {
        return false;
    };
    let mut fields = rest.splitn(5, ':');
    let partition = fields.next().unwrap_or_default();
    let service = fields.next().unwrap_or_default();
    let region = fields.next().unwrap_or("x");
    let account = fields.next().unwrap_or_default();
    let resource = fields.next().unwrap_or_default();
    let name = resource.strip_prefix("role/").unwrap_or_default();
    (partition == "aws" || partition.starts_with("aws-"))
        && service == "iam"
        && region.is_empty()
        && account.len() == 12
        && account.bytes().all(|b| b.is_ascii_digit())
        && (1..=512).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+=,.@_/-".contains(&b))
}

/// The single-request endpoint every line of a batch calls.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BatchEndpoint {
    ChatCompletions,
    Embeddings,
}

impl BatchEndpoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/v1/chat/completions",
            Self::Embeddings => "/v1/embeddings",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "/v1/chat/completions" => Some(Self::ChatCompletions),
            "/v1/embeddings" => Some(Self::Embeddings),
            _ => None,
        }
    }

    /// The catalog operation a line of this endpoint is routed and priced
    /// by.
    pub const fn operation(self) -> CatalogOperation {
        match self {
            Self::ChatCompletions => CatalogOperation::ChatBatch,
            Self::Embeddings => CatalogOperation::EmbeddingsBatch,
        }
    }
}

/// A batch's status, as OpenAI names them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchStatus {
    Validating,
    InProgress,
    Finalizing,
    Completed,
    Failed,
    Expired,
    Cancelling,
    Cancelled,
}

impl BatchStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Validating => "validating",
            Self::InProgress => "in_progress",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Cancelling => "cancelling",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "validating" => Self::Validating,
            "in_progress" => Self::InProgress,
            "finalizing" => Self::Finalizing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            "cancelling" => Self::Cancelling,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Expired | Self::Cancelled
        )
    }
}

/// One validated line's request.
#[derive(Clone, Debug)]
pub enum LineRequest {
    Chat(ChatRequest),
    Embeddings(EmbeddingRequest),
}

/// One validated input line.
#[derive(Clone, Debug)]
pub struct ParsedLine {
    /// 1-based, counting every line of the file, blank or not.
    pub line_number: u32,
    pub custom_id: String,
    pub alias: String,
    pub request: LineRequest,
}

/// Bytes a result line can hold beyond its answer: ids, usage, the
/// wrapper, and the provider's own fields.
const RESULT_LINE_OVERHEAD: u64 = 4 * 1024;
/// The most JSON one output token can take: a few bytes of text, each of
/// which may be escaped as `\uXXXX`.
const RESULT_BYTES_PER_TOKEN: u64 = 64;
/// The widest embedding the gateway assumes a model returns, and the most
/// JSON one of its values takes (`-1.2345678e-05,` and some).
const MAX_EMBEDDING_DIMENSIONS: u64 = 4096;
const RESULT_BYTES_PER_VALUE: u64 = 20;

/// The most bytes a line's result can take in the provider's result file,
/// given the output its reservation covers.
pub fn result_bound(request: &LineRequest, output_cap: Option<u32>) -> u64 {
    match request {
        LineRequest::Chat(_) => RESULT_LINE_OVERHEAD.saturating_add(
            u64::from(output_cap.unwrap_or(0)).saturating_mul(RESULT_BYTES_PER_TOKEN),
        ),
        LineRequest::Embeddings(request) => RESULT_LINE_OVERHEAD.saturating_add(
            (request.input.len() as u64)
                .saturating_mul(256 + MAX_EMBEDDING_DIMENSIONS * RESULT_BYTES_PER_VALUE),
        ),
    }
}

/// Why a line was refused, reported as OpenAI's `errors.data[]`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LineError {
    pub code: &'static str,
    pub message: String,
    /// 1-based; `None` for an error of the file as a whole.
    pub line: Option<u32>,
}

impl LineError {
    pub fn line(line: u32, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            line: Some(line),
        }
    }

    pub fn file(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            line: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputLine {
    custom_id: String,
    method: String,
    url: String,
    body: Value,
}

/// Parses and validates a whole input file for `endpoint`. Every line must
/// be a request the single endpoint would take (without `stream`), with a
/// unique `custom_id`; any refusal rejects the whole file, reporting up to
/// [`MAX_LINE_ERRORS`] of them.
pub fn parse_input(
    bytes: &[u8],
    endpoint: BatchEndpoint,
) -> Result<Vec<ParsedLine>, Vec<LineError>> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(vec![LineError::file(
            "invalid_file_encoding",
            "the input file is not UTF-8",
        )]);
    };
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        if lines.len() + errors.len() >= MAX_BATCH_LINES {
            errors.push(LineError::file(
                "too_many_lines",
                format!("a batch takes at most {MAX_BATCH_LINES} lines"),
            ));
            break;
        }
        match parse_line(raw, number, endpoint) {
            Ok(line) => {
                if !seen.insert(line.custom_id.clone()) {
                    errors.push(LineError::line(
                        number,
                        "duplicate_custom_id",
                        "custom_id is used by an earlier line",
                    ));
                } else {
                    lines.push(line);
                }
            }
            Err(error) => errors.push(error),
        }
        if errors.len() >= MAX_LINE_ERRORS {
            break;
        }
    }
    if errors.is_empty() && lines.is_empty() {
        errors.push(LineError::file("empty_file", "the input file has no lines"));
    }
    if errors.is_empty() {
        Ok(lines)
    } else {
        Err(errors)
    }
}

fn parse_line(raw: &str, number: u32, endpoint: BatchEndpoint) -> Result<ParsedLine, LineError> {
    let line: InputLine = serde_json::from_str(raw).map_err(|_| {
        LineError::line(
            number,
            "invalid_json_line",
            "the line is not an object of custom_id, method, url and body",
        )
    })?;
    if line.custom_id.is_empty() || line.custom_id.chars().count() > MAX_CUSTOM_ID_CHARS {
        return Err(LineError::line(
            number,
            "invalid_custom_id",
            format!("custom_id must have 1 to {MAX_CUSTOM_ID_CHARS} characters"),
        ));
    }
    if line.method != "POST" {
        return Err(LineError::line(
            number,
            "invalid_method",
            "method must be POST",
        ));
    }
    if line.url != endpoint.as_str() {
        return Err(LineError::line(
            number,
            "mismatched_url",
            format!("url must be the batch's endpoint, {}", endpoint.as_str()),
        ));
    }
    let (alias, request) = match endpoint {
        BatchEndpoint::ChatCompletions => {
            let body: ChatBody = serde_json::from_value(line.body).map_err(|_| {
                LineError::line(
                    number,
                    "invalid_body",
                    "body is not a chat completion request",
                )
            })?;
            let (alias, request) = public_request::chat_request(body).map_err(|error| {
                let message = match error {
                    public_request::RequestBoundError::ChatToolsInvalid(detail) => detail,
                    _ => "body is out of bounds",
                };
                LineError::line(number, error.code(), message)
            })?;
            (alias, LineRequest::Chat(request))
        }
        BatchEndpoint::Embeddings => {
            let body: EmbeddingBody = serde_json::from_value(line.body).map_err(|_| {
                LineError::line(number, "invalid_body", "body is not an embeddings request")
            })?;
            let (alias, request) = public_request::embedding_request(body)
                .map_err(|error| LineError::line(number, error.code(), "body is out of bounds"))?;
            (alias, LineRequest::Embeddings(request))
        }
    };
    Ok(ParsedLine {
        line_number: number,
        custom_id: line.custom_id,
        alias,
        request,
    })
}

/// Checks a batch's `metadata`: at most [`MAX_METADATA_ENTRIES`] string
/// values, as OpenAI takes.
pub fn valid_metadata(metadata: &serde_json::Map<String, Value>) -> bool {
    metadata.len() <= MAX_METADATA_ENTRIES
        && metadata.iter().all(|(key, value)| {
            !key.is_empty()
                && key.chars().count() <= MAX_METADATA_KEY_CHARS
                && value
                    .as_str()
                    .is_some_and(|value| value.chars().count() <= MAX_METADATA_VALUE_CHARS)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_named_by_its_public_id_or_its_uuid() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(parse_batch_ref(&public_batch_id(id)), Some(id));
        assert_eq!(parse_batch_ref(&format!(" {id} ")), Some(id));
        for refused in ["", "batch_", "batch_x", "file-1", "batch_batch_1"] {
            assert_eq!(parse_batch_ref(refused), None, "{refused}");
        }
    }

    #[test]
    fn a_line_result_bound_covers_its_whole_answer() {
        let chat = LineRequest::Chat(ChatRequest {
            messages: Vec::new(),
            temperature: None,
            max_tokens: None,
            ..Default::default()
        });
        assert_eq!(result_bound(&chat, Some(1000)), 4 * 1024 + 64_000);
        let embeddings = |inputs: usize| {
            LineRequest::Embeddings(EmbeddingRequest {
                input: vec!["x".to_owned(); inputs],
            })
        };
        // 300 lines of 128 inputs each, as wide as 1,536 values or more, no
        // longer fit one job: creation splits them.
        let line = result_bound(&embeddings(128), None);
        assert!(300 * line > MAX_JOB_RESULT_BYTES);
        assert!(line < MAX_JOB_RESULT_BYTES);
        assert!(128 * 1536 * 16 < line - 4 * 1024);
        // A full embeddings line always fits a job; a chat line allowed a
        // vast output does not, and is refused.
        assert!(
            result_bound(&embeddings(public_request::MAX_EMBEDDINGS), None) < MAX_JOB_RESULT_BYTES
        );
        assert!(result_bound(&chat, Some(4_000_000)) > MAX_JOB_RESULT_BYTES);
    }

    fn chat_line(id: &str, alias: &str) -> String {
        serde_json::json!({
            "custom_id": id,
            "method": "POST",
            "url": "/v1/chat/completions",
            "body": {"model": alias, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 16}
        })
        .to_string()
    }

    #[test]
    fn valid_files_parse_with_their_line_numbers() {
        let file = format!("{}\n\n{}\n", chat_line("a", "fast"), chat_line("b", "slow"));
        let lines = parse_input(file.as_bytes(), BatchEndpoint::ChatCompletions).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line_number, 1);
        assert_eq!(lines[1].line_number, 3);
        assert_eq!(lines[1].alias, "slow");
        assert!(
            matches!(&lines[0].request, LineRequest::Chat(request) if request.max_tokens == Some(16))
        );
    }

    #[test]
    fn every_refused_line_is_reported_by_number() {
        let streamed = serde_json::json!({
            "custom_id": "s", "method": "POST", "url": "/v1/chat/completions",
            "body": {"model": "fast", "messages": [{"role": "user", "content": "hi"}], "stream": true}
        });
        let file = [
            chat_line("a", "fast"),
            chat_line("a", "fast"),
            "not json".to_owned(),
            streamed.to_string(),
            chat_line("e", "fast").replace("/v1/chat/completions", "/v1/embeddings"),
            chat_line("f", "fast").replace("POST", "GET"),
            chat_line("", "fast"),
        ]
        .join("\n");
        let errors = parse_input(file.as_bytes(), BatchEndpoint::ChatCompletions).unwrap_err();
        let codes: Vec<_> = errors.iter().map(|e| (e.line, e.code)).collect();
        assert_eq!(
            codes,
            vec![
                (Some(2), "duplicate_custom_id"),
                (Some(3), "invalid_json_line"),
                (Some(4), "invalid_body"),
                (Some(5), "mismatched_url"),
                (Some(6), "invalid_method"),
                (Some(7), "invalid_custom_id"),
            ]
        );
    }

    #[test]
    fn bounds_are_the_single_endpoints_own() {
        let line = serde_json::json!({
            "custom_id": "a", "method": "POST", "url": "/v1/embeddings",
            "body": {"model": "embed", "input": []}
        });
        let errors =
            parse_input(line.to_string().as_bytes(), BatchEndpoint::Embeddings).unwrap_err();
        assert_eq!(errors[0].code, "embedding_request_out_of_bounds");
        assert_eq!(
            parse_input(b"\n\n", BatchEndpoint::Embeddings).unwrap_err()[0].code,
            "empty_file"
        );
        assert_eq!(
            parse_input(&[0xff, 0xfe], BatchEndpoint::Embeddings).unwrap_err()[0].code,
            "invalid_file_encoding"
        );
    }

    #[test]
    fn metadata_is_a_small_map_of_strings() {
        let ok = serde_json::json!({"team": "search"});
        assert!(valid_metadata(ok.as_object().unwrap()));
        let number = serde_json::json!({"n": 1});
        assert!(!valid_metadata(number.as_object().unwrap()));
        let many: serde_json::Map<_, _> = (0..17)
            .map(|i| (format!("k{i}"), Value::String("v".into())))
            .collect();
        assert!(!valid_metadata(&many));
    }

    #[test]
    fn staging_is_checked_for_its_cloud() {
        let input =
            |bucket: &str, prefix: Option<&str>, region: Option<&str>, role: Option<&str>| {
                BatchStagingInput {
                    bucket: bucket.into(),
                    prefix: prefix.map(Into::into),
                    region: region.map(Into::into),
                    role_arn: role.map(Into::into),
                }
            };
        let role = "arn:aws:iam::123456789012:role/bedrock-batch";
        let s3 = input("tenant-batches", Some("gw"), Some("us-east-1"), Some(role))
            .validate(StagingKind::S3)
            .unwrap();
        assert_eq!(s3.prefix, "gw/");
        assert_eq!(s3.role_arn.as_deref(), Some(role));
        // S3 needs its region and the job's role; Cloud Storage takes neither.
        assert!(
            input("tenant-batches", None, None, Some(role))
                .validate(StagingKind::S3)
                .is_err()
        );
        assert!(
            input("tenant-batches", None, Some("us-east-1"), None)
                .validate(StagingKind::S3)
                .is_err()
        );
        assert!(
            input(
                "tenant-batches",
                None,
                Some("us-east-1"),
                Some("arn:aws:iam::12:role/x")
            )
            .validate(StagingKind::S3)
            .is_err()
        );
        assert!(
            input("tenant-batches", None, Some("us-east-1"), None)
                .validate(StagingKind::Gcs)
                .is_err()
        );
        let gcs = input("tenant-batches", Some("a/b/"), None, None)
            .validate(StagingKind::Gcs)
            .unwrap();
        assert_eq!(gcs.prefix, "a/b/");
        for bad in ["UPPER", "a", "-start", "has space"] {
            assert!(
                input(bad, None, None, None)
                    .validate(StagingKind::Gcs)
                    .is_err(),
                "{bad}"
            );
        }
        for bad in ["/abs", "a//b", "a?b"] {
            assert!(
                input("ok-bucket", Some(bad), None, None)
                    .validate(StagingKind::Gcs)
                    .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn settings_bound_the_emulated_concurrency() {
        let input = |c: Option<u32>| BatchSettingsInput {
            emulated_allowed: true,
            emulated_concurrency: c,
        };
        assert_eq!(
            input(None).validate(),
            Ok((true, DEFAULT_EMULATED_CONCURRENCY))
        );
        assert_eq!(input(Some(32)).validate(), Ok((true, 32)));
        assert!(input(Some(0)).validate().is_err());
        assert!(input(Some(33)).validate().is_err());
        assert_eq!(BatchMode::Emulated.price_source(), "on_demand");
        assert_eq!(BatchMode::parse("native"), Some(BatchMode::Native));
    }

    #[test]
    fn statuses_round_trip_and_know_when_they_end() {
        for status in [
            BatchStatus::Validating,
            BatchStatus::InProgress,
            BatchStatus::Finalizing,
            BatchStatus::Completed,
            BatchStatus::Failed,
            BatchStatus::Expired,
            BatchStatus::Cancelling,
            BatchStatus::Cancelled,
        ] {
            assert_eq!(BatchStatus::parse(status.as_str()), Some(status));
        }
        assert!(BatchStatus::Cancelled.is_terminal());
        assert!(!BatchStatus::Cancelling.is_terminal());
        assert_eq!(
            BatchEndpoint::Embeddings.operation(),
            CatalogOperation::EmbeddingsBatch
        );
    }
}
