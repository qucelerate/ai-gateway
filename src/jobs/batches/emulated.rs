//! Emulated batch jobs (D-021, Phase 7 milestone 5): the lines of a family
//! without a native batch API (or a group too small for one), each sent as
//! an ordinary request by the worker, with bounded concurrency and within
//! the organization's rate limits, at the on-demand price their lines were
//! reserved at.
//!
//! Crash safety: a line is marked sent before its request goes out, and its
//! result is saved before it settles. A line found sent but not ended was
//! in flight when a worker died: it settles from its saved result, or, with
//! none, goes to reconciliation. It is never sent again, so a crash never
//! makes the provider run a line twice.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde_json::{Value, json};

use super::{
    BatchRunError, BatchRunner, Ended, ROUTING_POLICY_DENIED, Step, capped, saved_results,
    zero_usage,
};
use crate::{
    domain::{
        batch::{BatchEndpoint, LineRequest},
        inference::{
            CorrelationId, DeploymentModelReference, InferenceContext,
            ProviderAttemptIdempotencyKey, ProviderError, ProviderErrorCategory, Usage,
        },
        public_request::{ChatBodyResponse, choice_response, embedding_response, usage_response},
    },
    persistence::batches::{JobChange, JobLine, LeasedJob, SendGroup},
};

/// How long one run sends lines before it lets the job go: well inside the
/// step limit, which is inside the lease.
const SEND_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// How long to wait when the rate limits leave no room, or the provider
/// refused for its own limits.
const LIMITED_WAIT_SECONDS: i64 = 15;
/// Saved results read or deleted at once while a job ends.
const OBJECT_CONCURRENCY: usize = 16;

/// One sent line's end, before it is settled.
struct Answered {
    state: &'static str,
    status_code: Option<u16>,
    error_code: Option<String>,
    usage: Option<Usage>,
    request_id: Option<String>,
    body: Option<Value>,
}

impl BatchRunner {
    /// The object a line's saved result is kept in until its job ends.
    fn line_key(job: &LeasedJob, line: i32) -> String {
        format!(
            "batch-results/{}/{}/{}/lines/{line}",
            job.organization_id, job.batch_id, job.id
        )
    }

    pub(super) async fn run_emulated(
        &self,
        job: &LeasedJob,
        endpoint: BatchEndpoint,
        now: DateTime<Utc>,
    ) -> Result<Step, BatchRunError> {
        if job.state == "pending" {
            self.store.claim_job_lines(job, now).await?;
            // Nothing is sent where the routing policy, as it stands now,
            // forbids (D-023); a job some of whose lines were sent is
            // `submitted` and never reaches here.
            let denied_before = job.provider_status.as_deref() == Some(ROUTING_POLICY_DENIED);
            if denied_before || !self.store.job_permitted(job).await? {
                self.end_denied(job, now).await?;
                return Ok(Step::Collected);
            }
            self.update(
                job,
                JobChange {
                    state: Some("submitted"),
                    provider_status: Some("running"),
                    progressed: true,
                    ..JobChange::default()
                },
            )
            .await?;
        }
        // Lines a run that died had in flight.
        self.recover_sent(job, now).await?;
        if job.cancel_requested || now >= job.batch_expires_at {
            let state = if job.cancel_requested {
                "cancelled"
            } else {
                "expired"
            };
            self.end_emulated(job, state, now).await?;
            return Ok(Step::Collected);
        }
        let adapter = self.provider(job).await?;
        let input = self.input(job, endpoint).await?;
        let (_, concurrency) = self.store.emulation(job.organization_id).await?;
        let started = std::time::Instant::now();
        loop {
            if started.elapsed() >= SEND_WINDOW {
                self.wait(job, now, 0).await?;
                return Ok(Step::Polled);
            }
            // The routing policy as it stands before each group (D-023):
            // once it forbids the job, nothing more is sent, and the lines
            // already sent keep their results.
            if !self.store.job_permitted(job).await? {
                self.end_emulated(job, "denied", now).await?;
                return Ok(Step::Collected);
            }
            // Cancellation and expiry as they are now, and the group within
            // the rate limits, read and marked sent in one transaction.
            let sent = match self.store.send_group(job, concurrency).await? {
                SendGroup::Lost => return Err(BatchRunError::LeaseLost),
                SendGroup::Cancelled => {
                    self.end_emulated(job, "cancelled", now).await?;
                    return Ok(Step::Collected);
                }
                SendGroup::Expired => {
                    self.end_emulated(job, "expired", now).await?;
                    return Ok(Step::Collected);
                }
                SendGroup::NoRoom => {
                    self.wait(job, now, LIMITED_WAIT_SECONDS).await?;
                    return Ok(Step::Polled);
                }
                SendGroup::Sent(sent) => sent,
            };
            if sent.is_empty() {
                // Every line was sent and has ended, or ends now.
                self.end_emulated(job, "completed", now).await?;
                return Ok(Step::Collected);
            }
            let lines: HashMap<i32, JobLine> = self
                .store
                .job_lines(job)
                .await?
                .into_iter()
                .filter(|line| sent.contains(&line.line_number))
                .map(|line| (line.line_number, line))
                .collect();
            let calls = sent.iter().map(|number| {
                let adapter = adapter.clone();
                let line = lines.get(number);
                let parsed = u32::try_from(*number)
                    .ok()
                    .and_then(|number| input.get(&number));
                async move {
                    let (Some(line), Some(parsed)) = (line, parsed) else {
                        return (*number, Err(BatchRunError::Corrupt));
                    };
                    let answer = self
                        .send(job, adapter.as_ref(), line, &parsed.request, &parsed.alias)
                        .await;
                    (*number, answer)
                }
            });
            let answers: Vec<(i32, Result<Option<Answered>, BatchRunError>)> =
                futures::stream::iter(calls)
                    .buffer_unordered(concurrency.max(1) as usize)
                    .collect()
                    .await;
            // Every answer is saved and settled before an error is returned,
            // so one bad line does not send its neighbours' results to
            // reconciliation.
            let mut limited = false;
            let mut failed = None;
            for (number, answer) in answers {
                let Some(line) = lines.get(&number) else {
                    failed.get_or_insert(BatchRunError::Corrupt);
                    continue;
                };
                match answer {
                    // Refused before any work (its own rate limit): sent
                    // again later.
                    Ok(None) => {
                        self.store.unsend_line(job, number).await?;
                        limited = true;
                    }
                    Ok(Some(answered)) => self.save_and_settle(job, line, answered, now).await?,
                    Err(error) => {
                        failed.get_or_insert(error);
                    }
                }
            }
            if let Some(error) = failed {
                return Err(error);
            }
            if limited {
                self.wait(job, now, LIMITED_WAIT_SECONDS).await?;
                return Ok(Step::Polled);
            }
        }
    }

    /// Lets the job go, to run again after `seconds`.
    async fn wait(
        &self,
        job: &LeasedJob,
        now: DateTime<Utc>,
        seconds: i64,
    ) -> Result<(), BatchRunError> {
        self.update(
            job,
            JobChange {
                next_run_at: Some(now + chrono::Duration::seconds(seconds)),
                progressed: true,
                ..JobChange::default()
            },
        )
        .await
    }

    /// Sends one line as an ordinary request. `None` when the provider
    /// refused it before doing any work, so it may be sent again.
    async fn send(
        &self,
        job: &LeasedJob,
        adapter: &dyn crate::provider::InferenceProviderAdapter,
        line: &JobLine,
        request: &LineRequest,
        alias: &str,
    ) -> Result<Option<Answered>, BatchRunError> {
        let request = capped(request, line.output_token_cap)?;
        let context = InferenceContext::new(
            CorrelationId::new(),
            ProviderAttemptIdempotencyKey::new(format!(
                "gateway-batch-{}",
                line.reservation_id.simple()
            ))
            .map_err(|_| BatchRunError::Corrupt)?,
            DeploymentModelReference::new(job.deployment.clone(), job.model.clone())
                .map_err(|_| BatchRunError::Corrupt)?,
        )
        .with_adapter_instance_key(job.adapter_instance_key.clone())
        .with_provider_family(job.family.clone());
        let id = format!("batch_req_{}_{}", job.batch_id, line.line_number);
        let outcome = match request {
            LineRequest::Chat(request) => adapter.chat(context, request).await.map(|response| {
                let usage = response.usage().map(|u| u.usage());
                let body = serde_json::to_value(ChatBodyResponse {
                    id,
                    model: alias.to_owned(),
                    choices: response.choices().iter().map(choice_response).collect(),
                    usage: response.usage().map(|u| usage_response(u.usage())),
                });
                (
                    body,
                    usage,
                    response.provider_request_id().map(ToOwned::to_owned),
                )
            }),
            LineRequest::Embeddings(request) => {
                adapter.embeddings(context, request).await.map(|response| {
                    let usage = response.usage().map(|u| u.usage());
                    let request_id = response.provider_request_id().map(ToOwned::to_owned);
                    (
                        serde_json::to_value(embedding_response(response, alias)),
                        usage,
                        request_id,
                    )
                })
            }
        };
        Ok(match outcome {
            Ok((body, usage, request_id)) => {
                let body = body.map_err(|_| BatchRunError::Corrupt)?;
                Some(match usage {
                    Some(usage) => Answered {
                        state: "succeeded",
                        status_code: Some(200),
                        error_code: None,
                        usage: Some(usage),
                        request_id,
                        body: Some(body),
                    },
                    // Served without usage: returned, but never charged a
                    // guess.
                    None => Answered {
                        state: "unknown",
                        status_code: Some(200),
                        error_code: Some("result_unavailable".to_owned()),
                        usage: None,
                        request_id,
                        body: Some(body),
                    },
                })
            }
            // Never reached the provider, or refused for its own limits:
            // sent again later.
            Err(error) if retryable(&error) => None,
            // Refused before any inference, as the adapter documents the
            // refusal: it charges nothing.
            Err(error) if adapter.refused_before_inference(&error) => Some(Answered {
                state: "failed",
                status_code: error.http_status(),
                error_code: Some(format!("provider_{}", error.category().as_str())),
                usage: Some(zero_usage()),
                request_id: error.provider_request_id().map(ToOwned::to_owned),
                body: None,
            }),
            // As an ordinary request's failure: charged what the provider
            // reports it billed, else settled through reconciliation.
            Err(error) => {
                let usage = error.partial_usage();
                Some(Answered {
                    state: if usage.is_some() { "failed" } else { "unknown" },
                    status_code: error.http_status(),
                    error_code: Some(format!("provider_{}", error.category().as_str())),
                    usage,
                    request_id: error.provider_request_id().map(ToOwned::to_owned),
                    body: None,
                })
            }
        })
    }

    /// Saves a line's result, then settles it.
    async fn save_and_settle(
        &self,
        job: &LeasedJob,
        line: &JobLine,
        answered: Answered,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let saved = json!({
            "line": line.line_number,
            "status_code": answered.status_code,
            "request_id": answered.request_id,
            "body": answered.body,
            "error": answered.error_code,
            "state": answered.state,
            "usage": answered.usage,
        });
        self.objects()?
            .put(
                &Self::line_key(job, line.line_number),
                format!("{saved}\n").into_bytes(),
            )
            .await
            .map_err(|_| BatchRunError::Objects)?;
        let ended = [Ended {
            line,
            state: answered.state,
            status_code: answered.status_code,
            error_code: answered.error_code,
            usage: answered.usage,
            request_id: answered.request_id,
        }];
        self.settle_all(job, &ended, now).await
    }

    /// Settles the lines a run that died had sent: from the result each
    /// saved, or, with none, through reconciliation.
    async fn recover_sent(&self, job: &LeasedJob, now: DateTime<Utc>) -> Result<(), BatchRunError> {
        let lines: Vec<JobLine> = self
            .store
            .job_lines(job)
            .await?
            .into_iter()
            .filter(|line| line.state == "pending" && line.sent_at.is_some())
            .collect();
        if lines.is_empty() {
            return Ok(());
        }
        let objects = self.objects()?;
        let saved: Vec<Option<Vec<u8>>> = futures::stream::iter(lines.iter().map(|line| {
            let key = Self::line_key(job, line.line_number);
            async move { objects.get(&key).await }
        }))
        .buffered(OBJECT_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .map_err(|_| BatchRunError::Objects)?;
        let mut ended = Vec::with_capacity(lines.len());
        for (line, bytes) in lines.iter().zip(saved) {
            let result = bytes
                .as_deref()
                .map(saved_results)
                .and_then(|mut results| results.remove(&line.line_number));
            ended.push(match result {
                Some(result) => Ended {
                    line,
                    state: result.state,
                    status_code: result.status_code,
                    error_code: result.error_code,
                    usage: result.usage,
                    request_id: result.request_id,
                },
                None => Ended {
                    line,
                    state: "unknown",
                    status_code: None,
                    error_code: Some("result_unavailable".to_owned()),
                    usage: None,
                    request_id: None,
                },
            });
        }
        self.settle_all(job, &ended, now).await
    }

    /// Ends an emulated job as `state` (`completed`, `cancelled`,
    /// `expired`, or `denied` by the routing policy): lines never sent
    /// settle at zero (none are left unsent when it completed), lines in
    /// flight from a run that died settle from their saved results or go to
    /// reconciliation, and lines already ended keep their results. Then the
    /// job's saved results become its result file, and each line's own is
    /// deleted.
    pub(super) async fn end_emulated(
        &self,
        job: &LeasedJob,
        state: &'static str,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        self.recover_sent(job, now).await?;
        // A line the routing policy refused fails, as a denied job's lines
        // do, charging nothing.
        let (unsent_state, code, status_code) = match state {
            "cancelled" => ("cancelled", "batch_cancelled", None),
            "denied" => ("failed", ROUTING_POLICY_DENIED, Some(403_u16)),
            _ => ("expired", "batch_expired", None),
        };
        let lines = self.store.job_lines(job).await?;
        let objects = self.objects()?;
        // Lines never sent: no request went out, so none ran.
        let unsent: Vec<Ended> = lines
            .iter()
            .filter(|line| line.state == "pending" && line.sent_at.is_none())
            .map(|line| Ended {
                line,
                state: unsent_state,
                status_code,
                error_code: Some(code.to_owned()),
                usage: Some(zero_usage()),
                request_id: None,
            })
            .collect();
        // Recorded first, so a run cut short after this never sends them.
        for end in &unsent {
            let saved = json!({
                "line": end.line.line_number, "status_code": status_code, "request_id": null,
                "body": null, "error": code, "state": unsent_state, "usage": end.usage,
            });
            objects
                .put(
                    &Self::line_key(job, end.line.line_number),
                    format!("{saved}\n").into_bytes(),
                )
                .await
                .map_err(|_| BatchRunError::Objects)?;
            self.store.mark_sent(job, end.line.line_number).await?;
        }
        self.settle_all(job, &unsent, now).await?;
        // One result file of every line's saved result. A job whose file
        // was written already only deletes what is left.
        if job.result_object_key.is_none() {
            let saved: Vec<Option<Vec<u8>>> = futures::stream::iter(lines.iter().map(|line| {
                let key = Self::line_key(job, line.line_number);
                async move { objects.get(&key).await }
            }))
            .buffered(OBJECT_CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .map_err(|_| BatchRunError::Objects)?;
            let mut jsonl = Vec::new();
            for bytes in saved.into_iter().flatten() {
                jsonl.extend_from_slice(&bytes);
            }
            let key = format!(
                "batch-results/{}/{}/{}",
                job.organization_id, job.batch_id, job.id
            );
            objects
                .put(&key, jsonl)
                .await
                .map_err(|_| BatchRunError::Objects)?;
            self.update(
                job,
                JobChange {
                    result_object_key: Some(&key),
                    ..JobChange::default()
                },
            )
            .await?;
        }
        let deleted: Vec<Result<(), _>> = futures::stream::iter(lines.iter().map(|line| {
            let key = Self::line_key(job, line.line_number);
            async move { objects.delete(&key).await }
        }))
        .buffer_unordered(OBJECT_CONCURRENCY)
        .collect()
        .await;
        if deleted.iter().any(Result::is_err) {
            return Err(BatchRunError::Objects);
        }
        let status = if state == "denied" {
            ROUTING_POLICY_DENIED
        } else {
            state
        };
        self.finish(job, Some(status)).await
    }
}

/// Whether sending the request again cannot charge twice: it never left
/// the gateway, or the provider refused it with 429 for its own limits
/// (D-023), never after it reported usage.
fn retryable(error: &ProviderError) -> bool {
    error.partial_usage().is_none()
        && error.category() != ProviderErrorCategory::Cancelled
        && (error.was_not_sent() || error.http_status() == Some(429))
}
