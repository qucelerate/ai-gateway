//! The native batch contract (D-021): what the batch worker needs from a
//! provider family that runs batch jobs. One provider job is the lines of
//! one batch that one deployment serves; it is tagged with the gateway's
//! batch and job ids, so a worker that crashed after submitting finds it
//! again instead of submitting it twice.

use std::collections::HashMap;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::{
    batch::{BatchEndpoint, LineRequest},
    inference::{ChatResponse, EmbeddingResponse, ProviderError},
};

/// What tags a provider job as the gateway's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchTag {
    pub batch_id: Uuid,
    pub job_id: Uuid,
}

/// A provider job's state as the provider reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderBatch {
    pub id: String,
    pub status: ProviderBatchStatus,
    /// The provider's own status word, recorded as it was.
    pub raw_status: String,
    pub output_file_id: Option<String>,
    pub error_file_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderBatchStatus {
    /// Accepted, validating or running.
    Running,
    /// Asked to stop and not stopped yet.
    Cancelling,
    Completed,
    /// Stopped by the window: lines that did not finish never ran.
    Expired,
    Cancelled,
    /// Refused as a whole (its input failed the provider's validation):
    /// no line ran.
    Failed,
}

impl ProviderBatchStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Cancelling)
    }
}

/// A line's response, as the provider served it.
#[derive(Clone, Debug)]
pub enum LineResponse {
    Chat(ChatResponse),
    Embeddings(EmbeddingResponse),
}

/// How one line of a provider job ended.
#[derive(Clone, Debug)]
pub enum LineOutcome {
    Succeeded {
        response: Box<LineResponse>,
        request_id: Option<String>,
    },
    /// The provider answered the line with an error and did not bill it.
    Failed {
        status_code: Option<u16>,
        code: Option<String>,
        request_id: Option<String>,
    },
    /// The job stopped (its window ended, or it was cancelled) before the
    /// line ran.
    NotRun,
    /// The provider reported the line, but not in a form the gateway can
    /// read: its usage is unknown.
    Unreadable,
}

#[async_trait]
pub trait BatchProvider: Send + Sync {
    /// The provider job carrying `tag`, created from the input file
    /// `input_file_id`, if one was submitted. `None` only when the search
    /// proved there is none; a search cut short is an error.
    async fn find_batch(
        &self,
        tag: BatchTag,
        input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError>;

    /// Uploads a job's input; returns the provider's file id. A provider
    /// that takes the lines in the create call itself ([`Self::inline_input`])
    /// uploads nothing and returns a reference the worker records before
    /// creating the job, which [`Self::find_batch`] then searches from.
    async fn upload_batch_input(
        &self,
        tag: BatchTag,
        jsonl: Vec<u8>,
    ) -> Result<String, ProviderError>;

    /// Whether [`Self::create_batch`] needs the job's input lines, not only
    /// the uploaded file (Anthropic's Message Batches take the requests in
    /// the create call).
    fn inline_input(&self) -> bool {
        false
    }

    /// Creates the job from `input_file_id`; `input` carries the lines when
    /// [`Self::inline_input`]. `model` is the deployment's, which every line
    /// names; providers whose job names its model take it from here.
    async fn create_batch(
        &self,
        input_file_id: &str,
        input: Option<&[u8]>,
        endpoint: BatchEndpoint,
        tag: BatchTag,
        model: &str,
    ) -> Result<ProviderBatch, ProviderError>;

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError>;

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError>;

    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError>;

    /// One input line for the provider: line `line` of job `tag`, served by
    /// `model`.
    fn input_line(
        &self,
        tag: BatchTag,
        line: u32,
        endpoint: BatchEndpoint,
        model: &str,
        request: &LineRequest,
    ) -> Result<String, ProviderError>;

    /// The outcome of every line the provider's output and error files
    /// report, by line number. A line in neither is absent.
    fn parse_results(
        &self,
        endpoint: BatchEndpoint,
        output: &[u8],
        errors: &[u8],
    ) -> HashMap<u32, LineOutcome>;
}

/// Where a staged provider job (Bedrock, Gemini on Agent Platform) keeps its
/// input and output: a bucket in the tenant's own cloud, written and read
/// with the connection's own credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchStaging {
    pub bucket: String,
    /// Empty, or ending in `/`.
    pub prefix: String,
    /// The bucket's region (S3 only).
    pub region: Option<String>,
    /// The service role a Bedrock batch job assumes.
    pub role_arn: Option<String>,
}

impl BatchStaging {
    /// The object name under which `tag`'s job keeps `name`.
    pub fn object(&self, tag: BatchTag, name: &str) -> String {
        format!(
            "{}gateway-batches/{}/{}/{name}",
            self.prefix, tag.batch_id, tag.job_id
        )
    }
}

/// The storage a family's native batch jobs stage through, if any.
pub(crate) fn staging_kind(
    family: crate::domain::catalog::ProviderFamily,
) -> Option<crate::domain::batch::StagingKind> {
    use crate::domain::{batch::StagingKind, catalog::ProviderFamily};
    match family {
        ProviderFamily::Bedrock => Some(StagingKind::S3),
        ProviderFamily::Gemini => Some(StagingKind::Gcs),
        _ => None,
    }
}

/// Bedrock's default quota for the fewest records a batch inference job
/// takes; a smaller group runs emulated or is refused.
pub(crate) const BEDROCK_MIN_JOB_LINES: usize = 100;

/// The fewest lines one native job of `family` takes.
pub(crate) fn min_job_lines(family: crate::domain::catalog::ProviderFamily) -> usize {
    match family {
        crate::domain::catalog::ProviderFamily::Bedrock => BEDROCK_MIN_JOB_LINES,
        _ => 1,
    }
}

/// Whether `family`'s batch API takes `request` as a line. Checked when a
/// batch is created, so a line no job could send refuses the batch rather
/// than holding up its job: Anthropic takes text conversations only, as its
/// single endpoint does.
pub(crate) fn line_supported(
    family: crate::domain::catalog::ProviderFamily,
    request: &LineRequest,
) -> bool {
    use crate::domain::catalog::ProviderFamily;
    match (family, request) {
        (ProviderFamily::Anthropic, LineRequest::Chat(request)) => {
            super::anthropic::validate_text_request(request).is_ok()
        }
        (ProviderFamily::Anthropic, LineRequest::Embeddings(_)) => false,
        // Bedrock's batch jobs run Converse, which takes text turns only;
        // its embedding models take one text per record, not a line's list.
        (ProviderFamily::Bedrock, LineRequest::Chat(request)) => {
            super::bedrock::validate_batch_line(request).is_ok()
        }
        (ProviderFamily::Gemini, LineRequest::Chat(request)) => {
            super::gemini::validate_batch_line(request).is_ok()
        }
        (ProviderFamily::Bedrock | ProviderFamily::Gemini, LineRequest::Embeddings(_)) => false,
        _ => true,
    }
}

/// The largest provider batch file the worker reads; batch creation groups
/// lines into jobs whose results stay under it.
pub(super) const MAX_BATCH_FILE_BYTES: usize = crate::domain::batch::MAX_RESULT_FILE_BYTES as usize;
/// Pages of the provider's batch listing searched for a tagged job.
pub(super) const MAX_FIND_PAGES: usize = 10;

/// What one page of the batch listing says about a tagged job.
#[derive(Debug, PartialEq)]
pub(super) enum PageSearch<'a> {
    Found(&'a serde_json::Value),
    /// The listing ended, or reached jobs older than the job's input.
    Absent,
    /// Search on after this id.
    Next(String),
}

/// OpenAI's Batch API shapes, which Azure OpenAI's Global and Data Zone
/// Batch share: jobs, their listing and tags, input lines and result files.
pub(super) mod openai_format {
    use std::collections::HashMap;

    use serde_json::{Value, json};

    use super::{
        BatchTag, LineOutcome, LineResponse, PageSearch, ProviderBatch, ProviderBatchStatus,
    };
    use crate::domain::{
        batch::BatchEndpoint,
        inference::{ChatResponse, EmbeddingResponse, ProviderError, ProviderErrorCategory},
    };

    const TAG_BATCH: &str = "gateway_batch";
    const TAG_JOB: &str = "gateway_job";

    fn protocol_error() -> ProviderError {
        ProviderError::new(ProviderErrorCategory::Protocol)
    }

    /// The `metadata` that tags a job as the gateway's.
    pub(in crate::provider) fn metadata(tag: BatchTag) -> Value {
        json!({
            TAG_BATCH: tag.batch_id.to_string(),
            TAG_JOB: tag.job_id.to_string(),
        })
    }

    /// One input line: the line number is its `custom_id`.
    pub(in crate::provider) fn input_line(
        line: u32,
        url: &str,
        body: Value,
    ) -> Result<String, ProviderError> {
        serde_json::to_string(&json!({
            "custom_id": line.to_string(),
            "method": "POST",
            "url": url,
            "body": body,
        }))
        .map_err(|_| protocol_error())
    }

    pub(in crate::provider) fn provider_batch(
        value: &Value,
    ) -> Result<ProviderBatch, ProviderError> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?
            .to_owned();
        let raw_status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?
            .to_owned();
        let status = match raw_status.as_str() {
            "validating" | "in_progress" | "finalizing" => ProviderBatchStatus::Running,
            "cancelling" => ProviderBatchStatus::Cancelling,
            "completed" => ProviderBatchStatus::Completed,
            "expired" => ProviderBatchStatus::Expired,
            "cancelled" => ProviderBatchStatus::Cancelled,
            "failed" => ProviderBatchStatus::Failed,
            _ => return Err(protocol_error()),
        };
        let file = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(ToOwned::to_owned)
        };
        Ok(ProviderBatch {
            id,
            status,
            raw_status,
            output_file_id: file("output_file_id"),
            error_file_id: file("error_file_id"),
        })
    }

    pub(in crate::provider) fn search_page(
        page: &Value,
        tag: BatchTag,
        input_created: Option<i64>,
    ) -> Result<PageSearch<'_>, ProviderError> {
        let data = page
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(protocol_error)?;
        if let Some(found) = data.iter().find(|batch| tagged(batch, tag)) {
            return Ok(PageSearch::Found(found));
        }
        let older = |batch: &Value| {
            matches!(
                (batch.get("created_at").and_then(Value::as_i64), input_created),
                (Some(created), Some(input)) if created < input
            )
        };
        if data.iter().any(older) || page.get("has_more").and_then(Value::as_bool) != Some(true) {
            return Ok(PageSearch::Absent);
        }
        page.get("last_id")
            .and_then(Value::as_str)
            .map(|last| PageSearch::Next(last.to_owned()))
            .ok_or_else(protocol_error)
    }

    fn tagged(value: &Value, tag: BatchTag) -> bool {
        let metadata = value.get("metadata");
        let field = |key: &str| metadata.and_then(|m| m.get(key)).and_then(Value::as_str);
        field(TAG_BATCH) == Some(tag.batch_id.to_string().as_str())
            && field(TAG_JOB) == Some(tag.job_id.to_string().as_str())
    }

    type ParseChat = fn(Value, Option<String>) -> Result<ChatResponse, ProviderError>;
    type ParseEmbeddings = fn(Value, Option<String>) -> Result<EmbeddingResponse, ProviderError>;

    fn line_outcome(
        endpoint: BatchEndpoint,
        value: &Value,
        chat: ParseChat,
        embeddings: ParseEmbeddings,
    ) -> LineOutcome {
        let error_code = |error: &Value| {
            error
                .get("code")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            let code = error_code(error);
            if code.as_deref() == Some("batch_expired")
                || code.as_deref() == Some("batch_cancelled")
            {
                return LineOutcome::NotRun;
            }
            return LineOutcome::Failed {
                status_code: None,
                code,
                request_id: None,
            };
        }
        let Some(response) = value.get("response").filter(|r| r.is_object()) else {
            return LineOutcome::Unreadable;
        };
        let request_id = response
            .get("request_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let Some(status) = response
            .get("status_code")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
        else {
            return LineOutcome::Unreadable;
        };
        let body = response.get("body").cloned().unwrap_or(Value::Null);
        if status != 200 {
            return LineOutcome::Failed {
                status_code: Some(status),
                code: body.get("error").and_then(error_code),
                request_id,
            };
        }
        let parsed = match endpoint {
            BatchEndpoint::ChatCompletions => {
                chat(body, request_id.clone()).map(LineResponse::Chat)
            }
            BatchEndpoint::Embeddings => {
                embeddings(body, request_id.clone()).map(LineResponse::Embeddings)
            }
        };
        match parsed {
            Ok(response) => LineOutcome::Succeeded {
                response: Box::new(response),
                request_id,
            },
            Err(_) => LineOutcome::Unreadable,
        }
    }

    /// Every line the output and error files report, by the line number in
    /// its `custom_id`.
    pub(in crate::provider) fn parse_results(
        endpoint: BatchEndpoint,
        output: &[u8],
        errors: &[u8],
        chat: ParseChat,
        embeddings: ParseEmbeddings,
    ) -> HashMap<u32, LineOutcome> {
        let mut outcomes = HashMap::new();
        for bytes in [output, errors] {
            for raw in String::from_utf8_lossy(bytes).lines() {
                if raw.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(raw) else {
                    continue;
                };
                let Some(line) = value
                    .get("custom_id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.parse::<u32>().ok())
                else {
                    continue;
                };
                outcomes.insert(line, line_outcome(endpoint, &value, chat, embeddings));
            }
        }
        outcomes
    }
}
