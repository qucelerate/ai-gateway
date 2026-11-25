//! Gemini batch prediction jobs on Agent Platform (D-021, Phase 7
//! milestone 4), staged through the tenant's own Cloud Storage bucket.
//!
//! A job's input is written to the connection's staging bucket with the
//! connection's own token (the service account the gateway impersonates for
//! inference); the project's Agent Platform service agent reads it and
//! writes the predictions beside it. Each line's request carries its line
//! number as the label `gateway_line` (`line-<n>`: Vertex reads a label value
//! that looks like a number as one and refuses the request), which the
//! output echoes, and each job
//! carries the labels `gateway_batch` and `gateway_job`, by which a worker
//! that crashed after creating it finds it again.

use std::collections::HashMap;

use async_trait::async_trait;
use reqwest::{StatusCode, Url, header};
use serde_json::{Value, json};

use super::{
    GeminiAdapter, bounded_bytes, internal_error, parse_response, protocol_error, send_with_timeout,
};
use crate::{
    domain::{
        batch::{BatchEndpoint, LineRequest},
        inference::{ProviderError, ProviderErrorCategory},
    },
    provider::batch::{
        BatchProvider, BatchStaging, BatchTag, LineOutcome, LineResponse, MAX_BATCH_FILE_BYTES,
        MAX_FIND_PAGES, ProviderBatch, ProviderBatchStatus,
    },
};

const INPUT_OBJECT: &str = "input.jsonl";
const OUTPUT_FOLDER: &str = "output";
const LINE_LABEL: &str = "gateway_line";
/// What a line label's value starts with.
const LINE_PREFIX: &str = "line-";
/// Prediction files one job's output is read from.
const MAX_OUTPUT_FILES: usize = 1000;

/// The adapter with a connection's staging, on Agent Platform only.
pub(super) struct StagedGemini<'a> {
    adapter: &'a GeminiAdapter,
    staging: BatchStaging,
    /// `.../v1/projects/<project>/locations/<location>/`.
    api: Url,
}

impl<'a> StagedGemini<'a> {
    /// `None` for the Gemini API (a key), which has no batch prediction
    /// jobs, or a publisher URL not under a project's location.
    pub(super) fn new(adapter: &'a GeminiAdapter, staging: &BatchStaging) -> Option<Self> {
        adapter.agent_platform.as_ref()?;
        let base = adapter.base_url.as_str();
        let api = base.strip_suffix("publishers/google/")?;
        Some(Self {
            adapter,
            staging: staging.clone(),
            api: Url::parse(api).ok()?,
        })
    }

    fn uri(&self, name: &str) -> String {
        format!("gs://{}/{name}", self.staging.bucket)
    }

    /// The object a `gs://` URI names, which must be in the staging bucket.
    fn name_of<'u>(&self, uri: &'u str) -> Result<&'u str, ProviderError> {
        uri.strip_prefix("gs://")
            .and_then(|rest| rest.split_once('/'))
            .filter(|(bucket, name)| *bucket == self.staging.bucket && !name.is_empty())
            .map(|(_, name)| name)
            .ok_or_else(internal_error)
    }

    fn storage(&self, path: &str) -> Result<Url, ProviderError> {
        self.adapter
            .storage_url
            .join(path)
            .map_err(|_| internal_error())
    }

    fn jobs_url(&self, suffix: &str) -> Result<Url, ProviderError> {
        self.api
            .join(&format!("batchPredictionJobs{suffix}"))
            .map_err(|_| internal_error())
    }

    /// A job's URL from its resource name, which must be in this
    /// location. Google names the project by number there, not by ID, so
    /// only the location and job id are taken from it.
    fn job_url(&self, name: &str, suffix: &str) -> Result<Url, ProviderError> {
        let location = self
            .adapter
            .agent_platform
            .as_ref()
            .map(|platform| platform.location.as_str())
            .ok_or_else(internal_error)?;
        let parts: Vec<&str> = name.split('/').collect();
        let id = match parts.as_slice() {
            [
                "projects",
                project,
                "locations",
                at,
                "batchPredictionJobs",
                id,
            ] if !project.is_empty() && *at == location => *id,
            _ => return Err(protocol_error()),
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(protocol_error());
        }
        self.jobs_url(&format!("/{id}{suffix}"))
    }

    async fn object(&self, name: &str) -> Result<Option<Vec<u8>>, ProviderError> {
        let mut url = self.storage(&format!(
            "storage/v1/b/{}/o/{}",
            self.staging.bucket,
            percent_encode(name)
        ))?;
        url.set_query(Some("alt=media"));
        let response = send_with_timeout(
            self.adapter.authorize(self.adapter.client.get(url)).await?,
            self.adapter.request_timeout,
        )
        .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(storage_error(response.status()));
        }
        bounded_bytes(
            response,
            MAX_BATCH_FILE_BYTES,
            self.adapter.idle_read_timeout,
        )
        .await
        .map(Some)
    }

    fn provider_batch(&self, value: &Value) -> Result<ProviderBatch, ProviderError> {
        let id = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(protocol_error)?
            .to_owned();
        let raw_status = value
            .get("state")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?
            .to_owned();
        let status = match raw_status.as_str() {
            "JOB_STATE_QUEUED" | "JOB_STATE_PENDING" | "JOB_STATE_RUNNING"
            | "JOB_STATE_UPDATING" | "JOB_STATE_PAUSED" => ProviderBatchStatus::Running,
            "JOB_STATE_CANCELLING" => ProviderBatchStatus::Cancelling,
            // A line missing from a partly successful job's output is
            // unknown, never assumed free.
            "JOB_STATE_SUCCEEDED" | "JOB_STATE_PARTIALLY_SUCCEEDED" => {
                ProviderBatchStatus::Completed
            }
            "JOB_STATE_EXPIRED" => ProviderBatchStatus::Expired,
            "JOB_STATE_CANCELLED" => ProviderBatchStatus::Cancelled,
            "JOB_STATE_FAILED" => ProviderBatchStatus::Failed,
            _ => return Err(protocol_error()),
        };
        let output_file_id = value
            .get("outputInfo")
            .and_then(|info| info.get("gcsOutputDirectory"))
            .and_then(Value::as_str)
            .filter(|_| status.is_terminal())
            .map(ToOwned::to_owned);
        if let Some(directory) = &output_file_id {
            // Only ever the staging bucket's.
            self.name_of(directory).map_err(|_| protocol_error())?;
        }
        Ok(ProviderBatch {
            id,
            status,
            raw_status,
            output_file_id,
            error_file_id: None,
        })
    }
}

/// A Cloud Storage object name as one path segment.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A Cloud Storage refusal: never its body.
fn storage_error(status: StatusCode) -> ProviderError {
    let category = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorCategory::RateLimited,
        status if status.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    };
    ProviderError::from_parts(
        category,
        Some(super::PROVIDER.to_owned()),
        None,
        Some(status.as_u16()),
        Some("gcs_staging".to_owned()),
        None,
        None,
    )
    .unwrap_or_else(|_| protocol_error())
}

#[async_trait]
impl BatchProvider for StagedGemini<'_> {
    async fn find_batch(
        &self,
        tag: BatchTag,
        _input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError> {
        let filter = format!("labels.gateway_job=\"{}\"", tag.job_id);
        let mut next: Option<String> = None;
        for _ in 0..MAX_FIND_PAGES {
            let mut url = self.jobs_url("")?;
            {
                let mut query = url.query_pairs_mut();
                query
                    .append_pair("filter", &filter)
                    .append_pair("pageSize", "100");
                if let Some(token) = next.as_deref() {
                    query.append_pair("pageToken", token);
                }
            }
            let page = self.adapter.get_json(url).await?;
            let jobs = page
                .get("batchPredictionJobs")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // The filter is the provider's; the labels are checked here too.
            if let Some(found) = jobs.iter().find(|job| {
                let label = |key: &str| {
                    job.get("labels")
                        .and_then(|labels| labels.get(key))
                        .and_then(Value::as_str)
                };
                label("gateway_job") == Some(tag.job_id.to_string().as_str())
                    && label("gateway_batch") == Some(tag.batch_id.to_string().as_str())
            }) {
                return self.provider_batch(found).map(Some);
            }
            match page.get("nextPageToken").and_then(Value::as_str) {
                Some(token) if !token.is_empty() => next = Some(token.to_owned()),
                _ => return Ok(None),
            }
        }
        Err(protocol_error())
    }

    async fn upload_batch_input(
        &self,
        tag: BatchTag,
        jsonl: Vec<u8>,
    ) -> Result<String, ProviderError> {
        let name = self.staging.object(tag, INPUT_OBJECT);
        let mut url = self.storage(&format!("upload/storage/v1/b/{}/o", self.staging.bucket))?;
        url.query_pairs_mut()
            .append_pair("uploadType", "media")
            .append_pair("name", &name);
        let response = send_with_timeout(
            self.adapter
                .authorize(
                    self.adapter
                        .client
                        .post(url)
                        .header(header::CONTENT_TYPE, "application/jsonl")
                        .body(jsonl),
                )
                .await?,
            self.adapter.request_timeout,
        )
        .await?;
        if !response.status().is_success() {
            return Err(storage_error(response.status()));
        }
        Ok(self.uri(&name))
    }

    async fn create_batch(
        &self,
        input_file_id: &str,
        _input: Option<&[u8]>,
        endpoint: BatchEndpoint,
        tag: BatchTag,
        model: &str,
    ) -> Result<ProviderBatch, ProviderError> {
        if endpoint != BatchEndpoint::ChatCompletions {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        }
        if model.is_empty() || model.contains(['/', '?', '#', ' ']) {
            return Err(internal_error());
        }
        self.name_of(input_file_id)?;
        let output = self.uri(&self.staging.object(tag, OUTPUT_FOLDER));
        let body = json!({
            "displayName": format!("gateway-{}", tag.job_id.simple()),
            "model": format!("publishers/google/models/{model}"),
            "inputConfig": {"instancesFormat": "jsonl", "gcsSource": {"uris": [input_file_id]}},
            "outputConfig": {"predictionsFormat": "jsonl",
                             "gcsDestination": {"outputUriPrefix": output}},
            "labels": {
                "gateway_batch": tag.batch_id.to_string(),
                "gateway_job": tag.job_id.to_string(),
            },
        });
        let created = self.adapter.post_json(self.jobs_url("")?, &body).await?;
        self.provider_batch(&created)
    }

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let value = self.adapter.get_json(self.job_url(id, "")?).await?;
        self.provider_batch(&value)
    }

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        match self
            .adapter
            .post_json(self.job_url(id, ":cancel")?, &json!({}))
            .await
        {
            Ok(_) => {}
            // A job that ended meanwhile cannot be cancelled.
            Err(error)
                if matches!(error.http_status(), Some(400 | 409))
                    || error.category() == ProviderErrorCategory::InvalidRequest => {}
            Err(error) => return Err(error),
        }
        self.batch(id).await
    }

    /// Every prediction file in the job's output directory, one after the
    /// other; an output never written reads as empty.
    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError> {
        let directory = self.name_of(id)?.trim_end_matches('/');
        let prefix = format!("{directory}/");
        let mut names = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let mut url = self.storage(&format!("storage/v1/b/{}/o", self.staging.bucket))?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("prefix", &prefix);
                if let Some(token) = next.as_deref() {
                    query.append_pair("pageToken", token);
                }
            }
            let page = self.adapter.get_json(url).await?;
            for item in page
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    if name.ends_with(".jsonl") {
                        names.push(name.to_owned());
                    }
                }
            }
            if names.len() > MAX_OUTPUT_FILES {
                return Err(protocol_error());
            }
            match page.get("nextPageToken").and_then(Value::as_str) {
                Some(token) if !token.is_empty() => next = Some(token.to_owned()),
                _ => break,
            }
        }
        names.sort();
        let mut content = Vec::new();
        for name in names {
            let Some(bytes) = self.object(&name).await? else {
                continue;
            };
            if content.len() + bytes.len() + 1 > MAX_BATCH_FILE_BYTES {
                return Err(protocol_error());
            }
            content.extend_from_slice(&bytes);
            if !content.ends_with(b"\n") {
                content.push(b'\n');
            }
        }
        Ok(content)
    }

    fn input_line(
        &self,
        _tag: BatchTag,
        line: u32,
        _endpoint: BatchEndpoint,
        _model: &str,
        request: &LineRequest,
    ) -> Result<String, ProviderError> {
        let LineRequest::Chat(request) = request else {
            return Err(ProviderError::new(
                ProviderErrorCategory::UnsupportedFeature,
            ));
        };
        let mut body = self.adapter.request_body(request)?;
        body["labels"] = json!({ LINE_LABEL: format!("{LINE_PREFIX}{line}") });
        serde_json::to_string(&json!({ "request": body })).map_err(|_| internal_error())
    }

    fn parse_results(
        &self,
        endpoint: BatchEndpoint,
        output: &[u8],
        _errors: &[u8],
    ) -> HashMap<u32, LineOutcome> {
        let mut outcomes = HashMap::new();
        if endpoint != BatchEndpoint::ChatCompletions {
            return outcomes;
        }
        for raw in String::from_utf8_lossy(output).lines() {
            let Ok(value) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            let Some(line) = line_number(&value) else {
                continue;
            };
            outcomes.insert(line, prediction_outcome(&value));
        }
        outcomes
    }
}

/// The line a prediction answers, from its echoed request's label:
/// `line-<n>`, or the bare number of a job sent before labels had the
/// prefix. A request Vertex could not parse may be echoed as its JSON text.
fn line_number(prediction: &Value) -> Option<u32> {
    let request = prediction.get("request")?;
    let parsed;
    let request = match request.as_str() {
        Some(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            &parsed
        }
        None => request,
    };
    let label = request.get("labels")?.get(LINE_LABEL)?;
    let text = match label {
        Value::String(text) => text.as_str(),
        Value::Number(number) => return number.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => return None,
    };
    text.strip_prefix(LINE_PREFIX)
        .unwrap_or(text)
        .parse::<u32>()
        .ok()
}

fn prediction_outcome(value: &Value) -> LineOutcome {
    let status = value.get("status").and_then(Value::as_str).unwrap_or("");
    let response = value.get("response").filter(|response| {
        response
            .as_object()
            .is_some_and(|object| !object.is_empty())
    });
    match (status.is_empty(), response) {
        (true, Some(response)) => match parse_response(response.clone(), "batch".to_owned()) {
            Ok(response) => LineOutcome::Succeeded {
                response: Box::new(LineResponse::Chat(response)),
                request_id: value
                    .get("response")
                    .and_then(|r| r.get("responseId"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            },
            Err(_) => LineOutcome::Unreadable,
        },
        // The status is Google's message, which may echo the request: the
        // line is reported by a fixed code. An error beside a response may
        // have been billed, and is left unreadable for reconciliation.
        (false, None) => LineOutcome::Failed {
            status_code: None,
            code: Some("invalid_request".to_owned()),
            request_id: None,
        },
        _ => LineOutcome::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predictions_are_read_by_their_line_label() {
        let ok = json!({
            "status": "",
            "processed_time": "2026-11-17T00:00:00Z",
            "request": {"contents": [], "labels": {"gateway_line": "line-7"}},
            "response": {
                "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]},
                                "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 7,
                                  "totalTokenCount": 12},
                "responseId": "resp-7"
            }
        });
        let failed = json!({
            "status": "Bad request: the prompt said something",
            "request": {"labels": {"gateway_line": "line-8"}},
            "response": {}
        });
        // As Vertex reports a request it could not parse: the request as
        // text, an error status, no response.
        let unparsed = json!({
            "status": "Failed to parse JSON into proto",
            "request": "{\"labels\":{\"gateway_line\":\"line-9\"}}"
        });
        // A job sent before the prefix.
        let old = json!({"status": "x", "request": {"labels": {"gateway_line": "10"}}});
        let output = format!("{ok}\n{failed}\n{unparsed}\n{old}\nnot json\n");
        let mut lines = HashMap::new();
        for raw in output.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(raw) {
                lines.insert(line_number(&value).unwrap(), prediction_outcome(&value));
            }
        }
        assert_eq!(lines.len(), 4);
        for line in [9, 10] {
            assert!(
                matches!(&lines[&line], LineOutcome::Failed { .. }),
                "{line}"
            );
        }
        assert!(matches!(
            &lines[&7],
            LineOutcome::Succeeded { request_id, .. } if request_id.as_deref() == Some("resp-7")
        ));
        assert!(matches!(
            &lines[&8],
            LineOutcome::Failed { code, .. } if code.as_deref() == Some("invalid_request")
        ));
    }
}
