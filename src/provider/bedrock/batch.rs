//! Bedrock batch inference jobs (D-021, Phase 7 milestone 3), staged through
//! the tenant's own S3 bucket.
//!
//! A job's input is written to the connection's staging bucket with the
//! connection's own credentials (the role the gateway assumes for
//! inference), and `CreateModelInvocationJob` runs it as `Converse` calls
//! under the staging's service role, which Bedrock assumes to read the input
//! and write the output. Every record's `recordId` is its line number.
//!
//! A job is named for the gateway's job id and created with that id as its
//! `clientRequestToken`, so a worker that crashed after the create finds the
//! job by name, and a create repeated with the same token is ignored by
//! Bedrock rather than run twice.

use std::collections::HashMap;

use async_trait::async_trait;
use reqwest::{StatusCode, Url, header};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    BedrockAdapter, bounded_bytes, internal_error, invalid_request, parse_response,
    percent_encode_path_segment, protocol_error, send_with_timeout,
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

/// The input object's name inside a job's staging folder. Bedrock names the
/// output after it: `<output prefix><job id>/input.jsonl.out`.
const INPUT_OBJECT: &str = "input.jsonl";
const OUTPUT_FOLDER: &str = "output/";
/// The shortest window Bedrock takes, which is the batch's own.
const TIMEOUT_HOURS: u32 = 24;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The adapter with a connection's staging.
pub(super) struct StagedBedrock<'a> {
    adapter: &'a BedrockAdapter,
    bucket: String,
    prefix: String,
    region: String,
    role_arn: String,
}

impl<'a> StagedBedrock<'a> {
    /// `None` unless the staging names its region and role, as an S3
    /// staging always does.
    pub(super) fn new(adapter: &'a BedrockAdapter, staging: &BatchStaging) -> Option<Self> {
        Some(Self {
            adapter,
            bucket: staging.bucket.clone(),
            prefix: staging.prefix.clone(),
            region: staging.region.clone()?,
            role_arn: staging.role_arn.clone()?,
        })
    }

    fn staging(&self) -> BatchStaging {
        BatchStaging {
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            region: Some(self.region.clone()),
            role_arn: Some(self.role_arn.clone()),
        }
    }

    fn uri(&self, key: &str) -> String {
        format!("s3://{}/{key}", self.bucket)
    }

    /// The object an `s3://` URI names, which must be in the staging
    /// bucket: the worker reads nothing else.
    fn key_of<'u>(&self, uri: &'u str) -> Result<&'u str, ProviderError> {
        uri.strip_prefix("s3://")
            .and_then(|rest| rest.split_once('/'))
            .filter(|(bucket, key)| *bucket == self.bucket && !key.is_empty())
            .map(|(_, key)| key)
            .ok_or_else(internal_error)
    }

    /// Path-style through the configured endpoint, else virtual-hosted,
    /// except for a bucket with dots, which no wildcard certificate covers.
    /// Object names hold only characters no service escapes.
    fn object_url(&self, key: &str) -> Result<Url, ProviderError> {
        if !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!_.*'()/-".contains(&b))
        {
            return Err(internal_error());
        }
        let url = match &self.adapter.s3_base_url {
            Some(base) => base.join(&format!("{}/{key}", self.bucket)),
            None => {
                let domain = if self.region.starts_with("cn-") {
                    "amazonaws.com.cn"
                } else {
                    "amazonaws.com"
                };
                if self.bucket.contains('.') {
                    Url::parse(&format!(
                        "https://s3.{}.{domain}/{}/{key}",
                        self.region, self.bucket
                    ))
                } else {
                    Url::parse(&format!(
                        "https://{}.s3.{}.{domain}/{key}",
                        self.bucket, self.region
                    ))
                }
            }
        };
        url.map_err(|_| internal_error())
    }

    async fn s3(
        &self,
        method: reqwest::Method,
        key: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ProviderError> {
        let credentials = self.adapter.credentials().await?;
        let url = self.object_url(key)?;
        let digest = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            hex(&Sha256::digest(&body))
        };
        let content_type = "application/jsonl";
        let mut headers = vec![("x-amz-content-sha256", digest.as_str())];
        if method == reqwest::Method::PUT {
            headers.push(("content-type", content_type));
        }
        let signed = crate::aws::sign(
            &credentials,
            &self.region,
            "s3",
            &crate::aws::SignedRequest {
                method: method.as_str(),
                url: &url,
                headers: &headers,
                body: &body,
            },
            &crate::aws::amz_date_now(),
        )
        .map_err(|_| internal_error())?;
        let mut request = self
            .adapter
            .client
            .request(method.clone(), url)
            .header("x-amz-content-sha256", digest.as_str());
        if method == reqwest::Method::PUT {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        for (name, value) in signed {
            request = request.header(name, value);
        }
        send_with_timeout(request.body(body), self.adapter.request_timeout).await
    }

    /// One control-plane call: `GET` without a body, else `POST` with one.
    async fn control(&self, path: &str, body: Option<&Value>) -> Result<Value, ProviderError> {
        let url = self
            .adapter
            .control_base_url
            .join(path)
            .map_err(|_| internal_error())?;
        let request = match body {
            Some(body) => {
                let bytes = serde_json::to_vec(body).map_err(|_| internal_error())?;
                self.adapter
                    .signed_request(url, bytes, "application/json")
                    .await?
            }
            None => self.adapter.signed_get(url).await?,
        };
        let response = send_with_timeout(request, self.adapter.request_timeout).await?;
        if !response.status().is_success() {
            return Err(self.adapter.response_error(response).await);
        }
        let bytes = bounded_bytes(
            response,
            self.adapter.max_response_bytes,
            self.adapter.idle_read_timeout,
        )
        .await?;
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    /// A job's path: its whole ARN as one segment. Bedrock refuses the
    /// 12-character id alone ("The provided ARN is invalid"), and only an
    /// ARN of a job is taken, so nothing else can be addressed.
    fn job_path(id: &str) -> Result<String, ProviderError> {
        let (prefix, short) = id
            .rsplit_once(":model-invocation-job/")
            .ok_or_else(protocol_error)?;
        let mut parts = prefix.split(':');
        let well_formed = matches!(parts.next(), Some("arn"))
            && matches!(parts.next(), Some("aws" | "aws-cn" | "aws-us-gov"))
            && matches!(parts.next(), Some("bedrock"))
            && parts.next().is_some_and(|region| {
                !region.is_empty()
                    && region.len() <= 20
                    && region
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
            && parts.next().is_some_and(|account| {
                account.len() == 12 && account.bytes().all(|b| b.is_ascii_digit())
            })
            && parts.next().is_none()
            && short.len() == 12
            && short
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if !well_formed {
            return Err(protocol_error());
        }
        Ok(format!(
            "model-invocation-job/{}",
            percent_encode_path_segment(id)
        ))
    }

    fn provider_batch(&self, value: &Value) -> Result<ProviderBatch, ProviderError> {
        let id = value
            .get("jobArn")
            .and_then(Value::as_str)
            .filter(|arn| !arn.is_empty())
            .ok_or_else(protocol_error)?
            .to_owned();
        let raw_status = value
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?
            .to_owned();
        let status = match raw_status.as_str() {
            "Submitted" | "Validating" | "Scheduled" | "InProgress" => ProviderBatchStatus::Running,
            "Stopping" => ProviderBatchStatus::Cancelling,
            "Completed" => ProviderBatchStatus::Completed,
            // Not every record ran in time: those absent from the output
            // never ran, as when the window ends.
            "PartiallyCompleted" | "Expired" => ProviderBatchStatus::Expired,
            "Stopped" => ProviderBatchStatus::Cancelled,
            "Failed" => ProviderBatchStatus::Failed,
            _ => return Err(protocol_error()),
        };
        // The output is written under the job's id; known for a job that
        // ended, which is when the worker reads it.
        let output_file_id = match status {
            ProviderBatchStatus::Running | ProviderBatchStatus::Cancelling => None,
            _ => {
                let short = id.rsplit('/').next().unwrap_or_default();
                let output = value
                    .get("outputDataConfig")
                    .and_then(|config| config.get("s3OutputDataConfig"))
                    .and_then(|config| config.get("s3Uri"))
                    .and_then(Value::as_str)
                    .ok_or_else(protocol_error)?;
                let folder = if output.ends_with('/') {
                    output.to_owned()
                } else {
                    format!("{output}/")
                };
                // Only ever the staging bucket's.
                self.key_of(&folder).map_err(|_| protocol_error())?;
                Some(format!("{folder}{short}/{INPUT_OBJECT}.out"))
            }
        };
        Ok(ProviderBatch {
            id,
            status,
            raw_status,
            output_file_id,
            error_file_id: None,
        })
    }
}

/// The job name that tags a job as the gateway's.
fn job_name(tag: BatchTag) -> String {
    format!("gateway-{}", tag.job_id.simple())
}

/// A record's id: its line number.
fn record_id(line: u32) -> String {
    format!("L{line:010}")
}

fn line_of(record: &str) -> Option<u32> {
    record.strip_prefix('L')?.parse().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[async_trait]
impl BatchProvider for StagedBedrock<'_> {
    async fn find_batch(
        &self,
        tag: BatchTag,
        _input_file_id: &str,
    ) -> Result<Option<ProviderBatch>, ProviderError> {
        let name = job_name(tag);
        let mut next: Option<String> = None;
        for _ in 0..MAX_FIND_PAGES {
            let mut query = vec![("maxResults", "100"), ("nameContains", name.as_str())];
            if let Some(token) = next.as_deref() {
                query.push(("nextToken", token));
            }
            let page = self
                .adapter
                .control_page("model-invocation-jobs", &query)
                .await?;
            let jobs = page
                .get("invocationJobSummaries")
                .and_then(Value::as_array)
                .ok_or_else(protocol_error)?;
            if let Some(found) = jobs
                .iter()
                .find(|job| job.get("jobName").and_then(Value::as_str) == Some(name.as_str()))
            {
                return self.provider_batch(found).map(Some);
            }
            match page.get("nextToken").and_then(Value::as_str) {
                Some(token) if !token.is_empty() => next = Some(token.to_owned()),
                _ => return Ok(None),
            }
        }
        // A search cut short proves nothing.
        Err(protocol_error())
    }

    async fn upload_batch_input(
        &self,
        tag: BatchTag,
        jsonl: Vec<u8>,
    ) -> Result<String, ProviderError> {
        let key = self.staging().object(tag, INPUT_OBJECT);
        let response = self.s3(reqwest::Method::PUT, &key, jsonl).await?;
        if !response.status().is_success() {
            return Err(storage_error(response.status()));
        }
        Ok(self.uri(&key))
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
        self.key_of(input_file_id)?;
        let output = self.uri(&self.staging().object(tag, OUTPUT_FOLDER));
        let body = json!({
            "clientRequestToken": tag.job_id.simple().to_string(),
            "jobName": job_name(tag),
            "modelId": model,
            "modelInvocationType": "Converse",
            "roleArn": self.role_arn,
            "inputDataConfig": {"s3InputDataConfig": {"s3Uri": input_file_id}},
            "outputDataConfig": {"s3OutputDataConfig": {"s3Uri": output}},
            "timeoutDurationInHours": TIMEOUT_HOURS,
            "tags": [
                {"key": "gateway_batch", "value": tag.batch_id.to_string()},
                {"key": "gateway_job", "value": tag.job_id.to_string()},
            ],
        });
        let created = self.control("model-invocation-job", Some(&body)).await?;
        let arn = created
            .get("jobArn")
            .and_then(Value::as_str)
            .ok_or_else(protocol_error)?;
        self.batch(arn).await
    }

    async fn batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let value = self.control(&Self::job_path(id)?, None).await?;
        self.provider_batch(&value)
    }

    async fn cancel_batch(&self, id: &str) -> Result<ProviderBatch, ProviderError> {
        let path = format!("{}/stop", Self::job_path(id)?);
        match self.control(&path, Some(&json!({}))).await {
            Ok(_) => {}
            // A job that ended meanwhile cannot be stopped; it is read as
            // it is.
            Err(error) if error.http_status() == Some(400) => {}
            Err(error) => return Err(error),
        }
        self.batch(id).await
    }

    /// The object's content; an output Bedrock never wrote (a job that
    /// failed or stopped before any record) reads as empty.
    async fn file_content(&self, id: &str) -> Result<Vec<u8>, ProviderError> {
        let key = self.key_of(id)?;
        let response = self.s3(reqwest::Method::GET, key, Vec::new()).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
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
        let body = self.adapter.request_body(request)?;
        serde_json::to_string(&json!({
            "recordId": record_id(line),
            "modelInput": body,
        }))
        .map_err(|_| internal_error())
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
            let Some(line) = value
                .get("recordId")
                .and_then(Value::as_str)
                .and_then(line_of)
            else {
                continue;
            };
            outcomes.insert(line, record_outcome(&value));
        }
        outcomes
    }
}

fn record_outcome(value: &Value) -> LineOutcome {
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        let status_code = error
            .get("errorCode")
            .and_then(Value::as_u64)
            .and_then(|code| u16::try_from(code).ok())
            .filter(|code| (100..=599).contains(code));
        let code = match status_code {
            Some(429) => "rate_limited",
            Some(code) if code >= 500 => "provider_error",
            _ => "invalid_request",
        };
        return LineOutcome::Failed {
            status_code,
            code: Some(code.to_owned()),
            request_id: None,
        };
    }
    let Some(output) = value.get("modelOutput").filter(|output| output.is_object()) else {
        return LineOutcome::Unreadable;
    };
    match parse_response(without_batch_nulls(output), "batch".to_owned(), None) {
        Ok(response) => LineOutcome::Succeeded {
            response: Box::new(LineResponse::Chat(response)),
            request_id: None,
        },
        Err(_) => LineOutcome::Unreadable,
    }
}

/// A batch record's `modelOutput` read as a Converse answer. Bedrock's batch
/// output writes every field of its response types, `null` where Converse
/// leaves one out: each content block names every block type
/// (`"toolUse": null, "text": "40", …`), and usage carries `"cacheDetails":
/// null` and `"serverToolUsage": {"webSearchRequests": null}`. Those nulls are
/// dropped where Bedrock pads (content blocks, usage, and usage's nested
/// counts), so the answer parses as Converse's would. A tool call's input is
/// the model's own JSON and keeps its nulls.
fn without_batch_nulls(output: &Value) -> Value {
    fn drop_nulls(object: &mut serde_json::Map<String, Value>) {
        object.retain(|_, value| !value.is_null());
    }
    let mut output = output.clone();
    if let Some(blocks) = output
        .pointer_mut("/output/message/content")
        .and_then(Value::as_array_mut)
    {
        for block in blocks {
            if let Some(block) = block.as_object_mut() {
                drop_nulls(block);
            }
        }
    }
    if let Some(usage) = output.get_mut("usage").and_then(Value::as_object_mut) {
        drop_nulls(usage);
        for counts in usage.values_mut() {
            if let Some(counts) = counts.as_object_mut() {
                drop_nulls(counts);
            }
        }
    }
    output
}

/// An S3 refusal: never the body, which may name the bucket's contents.
fn storage_error(status: StatusCode) -> ProviderError {
    let category = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            ProviderErrorCategory::RateLimited
        }
        status if status.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    };
    ProviderError::from_parts(
        category,
        Some(super::PROVIDER.to_owned()),
        None,
        Some(status.as_u16()),
        Some("s3_staging".to_owned()),
        None,
        None,
    )
    .unwrap_or_else(|_| invalid_request())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_read_by_their_line() {
        assert_eq!(record_id(7), "L0000000007");
        assert_eq!(line_of(&record_id(4_000_000_000)), Some(4_000_000_000));
        assert_eq!(line_of("7"), None);
        let ok = json!({"recordId": "L0000000001", "modelOutput": {
            "output": {"message": {"role": "assistant", "content": [{"text": "hi"}]}},
            "stopReason": "end_turn",
            "usage": {"inputTokens": 5, "outputTokens": 7, "totalTokens": 12}}});
        assert!(matches!(record_outcome(&ok), LineOutcome::Succeeded { .. }));
        let throttled = json!({"recordId": "L0000000002",
                               "error": {"errorCode": 429, "errorMessage": "slow down"}});
        let LineOutcome::Failed {
            status_code, code, ..
        } = record_outcome(&throttled)
        else {
            panic!("a record error is a failure");
        };
        assert_eq!(
            (status_code, code.as_deref()),
            (Some(429), Some("rate_limited"))
        );
        // Neither an answer nor an error: its usage is unknown.
        assert!(matches!(
            record_outcome(&json!({"recordId": "L0000000003"})),
            LineOutcome::Unreadable
        ));
    }

    #[test]
    fn job_names_fit_bedrock_and_job_paths_take_only_its_ids() {
        let tag = BatchTag {
            batch_id: uuid::Uuid::new_v4(),
            job_id: uuid::Uuid::new_v4(),
        };
        assert!(job_name(tag).len() <= 63);
        assert_eq!(
            StagedBedrock::job_path(
                "arn:aws:bedrock:us-east-1:123456789012:model-invocation-job/abcdef123456"
            )
            .unwrap(),
            "model-invocation-job/arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Amodel-invocation-job%2Fabcdef123456"
        );
        // Bedrock refuses the id alone, and nothing but a job's ARN is
        // taken.
        for id in [
            "abcdef123456",
            "../../other",
            "ABCDEF123456",
            "arn:aws:bedrock:us-east-1:123456789012:model-invocation-job/ABCDEF123456",
            "arn:aws:bedrock:us-east-1:12345678901:model-invocation-job/abcdef123456",
            "arn:aws:s3:us-east-1:123456789012:model-invocation-job/abcdef123456",
            "arn:aws:bedrock:us-east-1:123456789012:x:model-invocation-job/abcdef123456",
            "arn:aws:bedrock:us-east-1:123456789012:model-invocation-job/abcdef123456/stop",
        ] {
            assert!(StagedBedrock::job_path(id).is_err(), "{id}");
        }
    }

    /// Records exactly as Bedrock's batch output wrote them on dev
    /// (2026-11-19), one from Claude Sonnet 4.6 and one from Nova Pro:
    /// every content block names every block type, with nulls, and usage
    /// carries null counts.
    #[test]
    fn records_as_bedrock_writes_them_parse_with_their_usage() {
        let records: Vec<Value> = include_str!("batch_output_records.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected = [
            ("L0000000041", "40", 16, 5),
            ("L0000000101", "I understand", 8, 16),
        ];
        for (record, (id, text, input, output)) in records.iter().zip(expected) {
            assert_eq!(record["recordId"], id);
            let LineOutcome::Succeeded { response, .. } = record_outcome(record) else {
                panic!("{id} does not parse");
            };
            let LineResponse::Chat(response) = *response else {
                panic!("{id} is not a chat answer");
            };
            let json = serde_json::to_value(&response).unwrap();
            assert!(
                json["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap()
                    .starts_with(text),
                "{id}: {json}"
            );
            assert_eq!(
                json["usage"]["usage"]["input_tokens"]["value"], input,
                "{id}: {json}"
            );
            assert_eq!(
                json["usage"]["usage"]["output_tokens"]["value"], output,
                "{id}: {json}"
            );
        }
    }

    #[test]
    fn a_tool_calls_input_keeps_its_nulls() {
        let output = json!({
            "output": {"message": {"role": "assistant", "content": [
                {"text": null, "toolUse": {"toolUseId": "t1", "name": "f", "input": {"city": null}}}
            ]}},
            "usage": {"inputTokens": 1, "outputTokens": 1, "cacheDetails": null}
        });
        let cleaned = without_batch_nulls(&output);
        assert_eq!(
            cleaned["output"]["message"]["content"][0],
            json!({"toolUse": {"toolUseId": "t1", "name": "f", "input": {"city": null}}})
        );
        assert_eq!(
            cleaned["usage"],
            json!({"inputTokens": 1, "outputTokens": 1})
        );
    }
}
