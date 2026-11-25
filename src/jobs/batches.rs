//! Runs batches' provider jobs (D-021): submits each job to its provider's
//! native batch API, polls it, collects and normalizes its results, settles
//! every line through the ordinary accounting, and writes each finished
//! batch's output files.
//!
//! Crash safety: every step is recorded before the next one starts, and
//! every step can be repeated.
//! - A job's lines are claimed before anything is sent, so accounting
//!   recovery never releases a hold whose lines may be running.
//! - A job that has no provider id is first looked for by its tag, so a
//!   worker that died after the provider accepted the job never submits it
//!   twice.
//! - Results are read again from the provider until the job is recorded
//!   finished, and each line settles under a finalization id derived from
//!   its reservation, so a settlement repeated after a crash replays.

mod emulated;

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    application::{batches::object_key, trace_objects::TraceObjects},
    domain::{
        accounting::{ReservationId, TraceId},
        batch::{BatchEndpoint, LineRequest, ParsedLine, parse_input},
        catalog::ProviderFamily,
        identity::OrganizationId,
        inference::{ProviderError, ProviderErrorCategory, TokenCounter, Usage},
        public_request::{ChatBodyResponse, choice_response, embedding_response, usage_response},
    },
    jobs::discovery::ProviderAccounts,
    persistence::{
        AccountingStore, AccountingStoreError, Settlement,
        batches::{
            BatchFileRow, BatchFinal, BatchRow, BatchStore, BatchStoreError, JobChange, JobLine,
            LeasedJob,
        },
    },
    provider::batch::{
        BatchProvider, BatchTag, LineOutcome, LineResponse, ProviderBatch, ProviderBatchStatus,
    },
};

/// The job and line code of work the routing policy refused (D-023).
const ROUTING_POLICY_DENIED: &str = "routing_policy_denied";
/// A job the provider refused to stage or create for the connection's
/// permissions.
const PERMISSION_DENIED: &str = "provider_permission_denied";

/// A provider refusal no retry changes: the connection's identity may not
/// make the call.
fn permission_denied(error: &ProviderError) -> bool {
    matches!(
        error.category(),
        ProviderErrorCategory::Authentication | ProviderErrorCategory::Authorization
    )
}

/// What a family's connection needs to run batch jobs, for the log of a
/// refusal (docs/operations.md, batch staging).
fn permissions_needed(family: &str) -> &'static str {
    match family {
        "bedrock" => {
            "bedrock:CreateModelInvocationJob and bedrock:TagResource on model-invocation-job/*, \
             iam:PassRole on the batch service role, s3:PutObject on the staging bucket"
        }
        "gemini" => {
            "roles/aiplatform.user on the project, roles/storage.objectCreator on the bucket"
        }
        _ => "the provider's batch and file permissions",
    }
}
/// How long a worker holds a job or a batch before another may take it.
const LEASE_MINUTES: i64 = 10;
/// Jobs and batches one run takes.
const RUN_LIMIT: i64 = 10;
/// A job its provider has not finished this long after the batch's window
/// ended is given up: its lines go to reconciliation before their holds
/// (window plus six hours) would expire.
const GIVE_UP_AFTER_HOURS: i64 = 5;
const MAX_BACKOFF_SECONDS: i64 = 15 * 60;
/// Ended jobs' saved results one run deletes.
const CLEAN_LIMIT: i64 = 100;
/// The longest one job's step may take: well inside its lease, so no other
/// worker takes the job over while this one still works on it.
const STEP_LIMIT: std::time::Duration = std::time::Duration::from_secs(7 * 60);

#[derive(Debug, thiserror::Error)]
pub enum BatchRunError {
    #[error(transparent)]
    Store(#[from] BatchStoreError),
    #[error(transparent)]
    Accounting(#[from] AccountingStoreError),
    #[error("provider batch call failed: {}", .0.category().as_str())]
    Provider(ProviderError),
    #[error("the provider has no credential or no batch API for the job")]
    ProviderUnavailable,
    #[error("the file store failed")]
    Objects,
    #[error("the batch's records are inconsistent")]
    Corrupt,
    #[error("another worker took over the job")]
    LeaseLost,
    #[error("the job's run outlasted its lease")]
    TimedOut,
    #[error("the provider connection runs no batch jobs of this kind")]
    Unsupported,
    #[error("the provider connection has no batch staging")]
    StagingMissing,
}

impl BatchRunError {
    fn code(&self) -> &'static str {
        match self {
            Self::Store(_) | Self::Accounting(_) => "database",
            Self::Provider(_) => "provider_error",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::Objects => "file_store",
            Self::Corrupt => "corrupt",
            Self::LeaseLost => "lease_lost",
            Self::TimedOut => "timed_out",
            Self::Unsupported => "batch_unsupported",
            Self::StagingMissing => "batch_staging_missing",
        }
    }
}

impl From<ProviderError> for BatchRunError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

/// What one run did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchReport {
    pub jobs: u64,
    pub submitted: u64,
    pub collected: u64,
    pub retried: u64,
    pub finalized: u64,
    /// Ended jobs' saved results deleted.
    pub cleaned: u64,
}

pub struct BatchRunner {
    store: BatchStore,
    staging: crate::persistence::batch_settings::BatchSettingsStore,
    accounting: AccountingStore,
    objects: Option<Arc<dyn TraceObjects>>,
    accounts: Arc<dyn ProviderAccounts>,
    poll: chrono::Duration,
}

/// One line's end, before it is settled.
struct Ended<'a> {
    line: &'a JobLine,
    state: &'static str,
    status_code: Option<u16>,
    error_code: Option<String>,
    usage: Option<Usage>,
    request_id: Option<String>,
}

/// A line that charges nothing: every counter reported as zero, so a price
/// with cached, reasoning or other rates still prices it (at zero) rather
/// than finding usage it cannot price.
fn zero_usage() -> Usage {
    Usage::complete(
        TokenCounter::exact(0),
        TokenCounter::exact(0),
        Some(TokenCounter::exact(0)),
        Some(TokenCounter::exact(0)),
        Some(TokenCounter::exact(0)),
    )
    .expect("zero usage is complete")
}

/// The same id for every settlement attempt of one line.
fn finalization_id(reservation: Uuid) -> Uuid {
    derived_id(b"gateway-batch-settlement", reservation)
}

fn derived_id(label: &[u8], id: Uuid) -> Uuid {
    let digest = Sha256::new()
        .chain_update(label)
        .chain_update(id.as_bytes())
        .finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

/// Fixed phrases: a provider's own message may echo the request.
fn error_message(code: &str) -> &'static str {
    match code {
        "batch_expired" => "the batch's completion window ended before the line ran",
        "batch_cancelled" => "the batch was cancelled before the line ran",
        "provider_job_failed" => "the provider refused the batch job",
        "result_unavailable" => "the provider's result for the line could not be read",
        "batch_unsupported" => "the provider connection runs no batch jobs of this kind",
        "batch_staging_missing" => "the provider connection has no batch staging bucket",
        "routing_policy_denied" => "the organization's routing policy forbids the deployment",
        "provider_permission_denied" => {
            "the provider connection is not permitted to create the batch job; check its batch permissions"
        }
        _ => "the provider refused the request",
    }
}

impl BatchRunner {
    pub fn new(
        store: BatchStore,
        accounting: AccountingStore,
        objects: Option<Arc<dyn TraceObjects>>,
        accounts: Arc<dyn ProviderAccounts>,
    ) -> Self {
        Self {
            staging: crate::persistence::batch_settings::BatchSettingsStore::new(
                store.pool().clone(),
            ),
            store,
            accounting,
            objects,
            accounts,
            poll: chrono::Duration::seconds(60),
        }
    }

    /// The file store batches are kept in; for tests, which share one with
    /// the API.
    #[doc(hidden)]
    pub fn with_objects(mut self, objects: Option<Arc<dyn TraceObjects>>) -> Self {
        self.objects = objects;
        self
    }

    /// How long to wait between polls of a running provider job.
    pub fn with_poll(mut self, poll: chrono::Duration) -> Self {
        self.poll = poll;
        self
    }

    fn objects(&self) -> Result<&Arc<dyn TraceObjects>, BatchRunError> {
        self.objects.as_ref().ok_or(BatchRunError::Objects)
    }

    /// Runs the jobs that are due, then writes the output of every batch
    /// whose jobs have all finished.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<BatchReport, BatchStoreError> {
        let mut report = BatchReport::default();
        // Without a file store no batch can exist.
        if self.objects.is_none() {
            return Ok(report);
        }
        let lease = chrono::Duration::minutes(LEASE_MINUTES);
        // One job leased at a time, as it starts: a job is never held
        // while others run.
        for _ in 0..RUN_LIMIT {
            let Some(job) = self.store.lease_jobs(now, lease, 1).await?.pop() else {
                break;
            };
            report.jobs += 1;
            let step = tokio::time::timeout(STEP_LIMIT, self.run_job(&job, now))
                .await
                .unwrap_or(Err(BatchRunError::TimedOut));
            match step {
                Ok(Step::Submitted) => report.submitted += 1,
                Ok(Step::Collected) => report.collected += 1,
                Ok(Step::Polled) => {}
                // The worker that took the job over runs it.
                Err(BatchRunError::LeaseLost) => {}
                Err(error) => {
                    tracing::warn!(
                        organization_id = %job.organization_id,
                        batch_id = %job.batch_id,
                        job_id = %job.id,
                        error = %error,
                        "batch job run failed"
                    );
                    if self.give_up(&job, now).await {
                        report.collected += 1;
                        continue;
                    }
                    report.retried += 1;
                    let backoff = (30i64 << job.attempts.clamp(0, 10)).min(MAX_BACKOFF_SECONDS);
                    self.store
                        .update_job(
                            &job,
                            JobChange {
                                error_code: Some(error.code()),
                                next_run_at: Some(now + chrono::Duration::seconds(backoff)),
                                ..JobChange::default()
                            },
                        )
                        .await?;
                }
            }
        }
        for _ in 0..RUN_LIMIT {
            let Some((batch, token)) = self
                .store
                .lease_finished_batches(now, lease, 1)
                .await?
                .pop()
            else {
                break;
            };
            let finalized = tokio::time::timeout(STEP_LIMIT, self.finalize(&batch, token, now))
                .await
                .unwrap_or(Err(BatchRunError::TimedOut));
            match finalized {
                Ok(true) => report.finalized += 1,
                Ok(false) => {}
                Err(error) => tracing::warn!(
                    organization_id = %batch.organization_id,
                    batch_id = %batch.id,
                    error = %error,
                    "batch finalization failed; retrying"
                ),
            }
        }
        // After finalizing, so a batch ended this run is cleaned this run.
        match self.clean_results().await {
            Ok(cleaned) => report.cleaned += cleaned,
            Err(error) => tracing::warn!(error = %error, "batch job results not cleaned"),
        }
        Ok(report)
    }

    /// Ends a job whose run keeps failing once the batch's window is five
    /// hours past, before its holds would expire: a job nothing was sent
    /// for settles at zero, any other goes to reconciliation. Returns
    /// whether the job ended.
    async fn give_up(&self, job: &LeasedJob, now: DateTime<Utc>) -> bool {
        if now < job.batch_expires_at + chrono::Duration::hours(GIVE_UP_AFTER_HOURS) {
            return false;
        }
        let ended = if job.mode == "emulated" {
            self.end_emulated(job, "expired", now).await
        } else if job.result_object_key.is_some() {
            self.resume_saved(job, now).await
        } else if job.provider_input_file_id.is_none() && job.provider_batch_id.is_none() {
            self.end_unsent(job, "expired", now).await
        } else {
            self.end_unconfirmed(job, now).await
        };
        match ended {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(
                    organization_id = %job.organization_id,
                    batch_id = %job.batch_id,
                    job_id = %job.id,
                    error = %error,
                    "batch job not given up; retrying"
                );
                false
            }
        }
    }

    /// Records a change to the job, while this worker still holds it.
    async fn update(&self, job: &LeasedJob, change: JobChange<'_>) -> Result<(), BatchRunError> {
        if self.store.update_job(job, change).await? {
            Ok(())
        } else {
            Err(BatchRunError::LeaseLost)
        }
    }

    async fn provider(
        &self,
        job: &LeasedJob,
    ) -> Result<Arc<dyn crate::provider::InferenceProviderAdapter>, BatchRunError> {
        let family: ProviderFamily = job.family.parse().map_err(|_| BatchRunError::Corrupt)?;
        let adapter = self
            .accounts
            .adapter(
                OrganizationId(job.organization_id),
                &job.adapter_instance_key,
                family,
            )
            .await?
            .ok_or(BatchRunError::ProviderUnavailable)?;
        Ok(adapter)
    }

    /// The job's batch API on `adapter`: staged for Bedrock and Gemini,
    /// else the adapter's own. A staged job keeps the staging it was first
    /// given, before anything is uploaded, and is created and collected
    /// through that copy whatever the connection's staging becomes.
    async fn batch_api<'a>(
        &self,
        job: &LeasedJob,
        adapter: &'a Arc<dyn crate::provider::InferenceProviderAdapter>,
    ) -> Result<BatchApi<'a>, BatchRunError> {
        let family: ProviderFamily = job.family.parse().map_err(|_| BatchRunError::Corrupt)?;
        if crate::provider::batch::staging_kind(family).is_none() {
            return adapter
                .batches()
                .map(BatchApi::Plain)
                .ok_or(BatchRunError::Unsupported);
        }
        let staging = match job.staging() {
            Some(kept) => kept,
            None => {
                // A job sent before jobs kept their staging (migration
                // 0106) uses the connection's, even one deleted since.
                let sent = job.provider_input_file_id.is_some() || job.provider_batch_id.is_some();
                let current = self
                    .staging
                    .staging_for_job(job.organization_id, &job.adapter_instance_key, sent)
                    .await
                    .map_err(|error| BatchRunError::Store(BatchStoreError::Database(error)))?
                    .ok_or(BatchRunError::StagingMissing)?;
                if sent {
                    current
                } else {
                    self.store
                        .keep_staging(job, &current)
                        .await?
                        .ok_or(BatchRunError::LeaseLost)?
                }
            }
        };
        adapter
            .staged_batches(&staging)
            .map(BatchApi::Staged)
            .ok_or(BatchRunError::Unsupported)
    }

    /// The batch's input, line by line.
    async fn input(
        &self,
        job: &LeasedJob,
        endpoint: BatchEndpoint,
    ) -> Result<HashMap<u32, ParsedLine>, BatchRunError> {
        let bytes = self
            .objects()?
            .get(&job.input_object_key)
            .await
            .map_err(|_| BatchRunError::Objects)?
            .ok_or(BatchRunError::Objects)?;
        let lines = parse_input(&bytes, endpoint).map_err(|_| BatchRunError::Corrupt)?;
        Ok(lines
            .into_iter()
            .map(|line| (line.line_number, line))
            .collect())
    }

    async fn run_job(&self, job: &LeasedJob, now: DateTime<Utc>) -> Result<Step, BatchRunError> {
        let endpoint = BatchEndpoint::parse(&job.endpoint).ok_or(BatchRunError::Corrupt)?;
        if job.mode == "emulated" {
            return self.run_emulated(job, endpoint, now).await;
        }
        let tag = BatchTag {
            batch_id: job.batch_id,
            job_id: job.id,
        };
        let past_window = now >= job.batch_expires_at;
        let give_up = now >= job.batch_expires_at + chrono::Duration::hours(GIVE_UP_AFTER_HOURS);
        if job.state == "pending" {
            self.store.claim_job_lines(job, now).await?;
            // Nothing has gone out unless an upload was recorded: a job is
            // created only after its input is.
            let never_sent = job.provider_input_file_id.is_none();
            if (job.cancel_requested || past_window) && never_sent {
                let state = if job.cancel_requested {
                    "cancelled"
                } else {
                    "expired"
                };
                self.end_unsent(job, state, now).await?;
                return Ok(Step::Collected);
            }
            // Nothing is sent where the routing policy, as it stands now,
            // forbids (D-023): a job not yet uploaded ends unsent.
            let denied_before = job.provider_status.as_deref() == Some(ROUTING_POLICY_DENIED);
            if never_sent && (denied_before || !self.store.job_permitted(job).await?) {
                self.end_denied(job, now).await?;
                return Ok(Step::Collected);
            }
            let adapter = match self.provider(job).await {
                Ok(adapter) => adapter,
                Err(error) if give_up => {
                    // Possibly submitted, never confirmed: unknown, not free.
                    let _ = error;
                    self.end_unconfirmed(job, now).await?;
                    return Ok(Step::Collected);
                }
                Err(error) => return Err(error),
            };
            let api = match self.batch_api(job, &adapter).await {
                Ok(api) => api,
                // Nothing went out: the lines end at zero, with why.
                Err(error @ (BatchRunError::Unsupported | BatchRunError::StagingMissing))
                    if never_sent =>
                {
                    self.end_refused(job, error.code(), now).await?;
                    return Ok(Step::Collected);
                }
                Err(error) => return Err(error),
            };
            let batches = api.get();
            // A provider job is created from an uploaded input, and the
            // upload is recorded before the job is created: without one
            // there is nothing to look for.
            if let Some(input_file) = &job.provider_input_file_id {
                if let Some(found) = batches.find_batch(tag, input_file).await? {
                    self.submitted(job, &found, now).await?;
                    return Ok(Step::Submitted);
                }
                if job.cancel_requested || past_window {
                    // Uploaded, but the provider has no job: it never ran.
                    let state = if job.cancel_requested {
                        "cancelled"
                    } else {
                        "expired"
                    };
                    self.end_unsent(job, state, now).await?;
                    return Ok(Step::Collected);
                }
            }
            let created = async {
                let mut jsonl = None;
                let input_file = match &job.provider_input_file_id {
                    Some(id) => id.clone(),
                    None => {
                        let built = self.provider_input(job, endpoint, batches).await?;
                        let id = batches.upload_batch_input(tag, built.clone()).await?;
                        jsonl = Some(built);
                        self.update(
                            job,
                            JobChange {
                                input_file_id: Some(&id),
                                ..JobChange::default()
                            },
                        )
                        .await?;
                        id
                    }
                };
                // A provider that takes the lines in the create call needs
                // them again after a crash between recording the upload and
                // creating.
                if batches.inline_input() && jsonl.is_none() {
                    jsonl = Some(self.provider_input(job, endpoint, batches).await?);
                }
                let inline = jsonl.as_deref().filter(|_| batches.inline_input());
                Ok::<_, BatchRunError>(
                    batches
                        .create_batch(&input_file, inline, endpoint, tag, &job.model)
                        .await?,
                )
            }
            .await;
            match created {
                Ok(created) => {
                    self.submitted(job, &created, now).await?;
                    return Ok(Step::Submitted);
                }
                // The connection may not stage or create the job: that is
                // the tenant's permissions, which a retry does not change.
                // The provider took nothing (a job it had created was looked
                // for above), so the lines end at zero, with why.
                Err(BatchRunError::Provider(error)) if permission_denied(&error) => {
                    tracing::warn!(
                        organization_id = %job.organization_id,
                        batch_id = %job.batch_id,
                        job_id = %job.id,
                        provider_family = %job.family,
                        needs = permissions_needed(&job.family),
                        "batch job refused by the provider for permission; lines failed"
                    );
                    self.update(
                        job,
                        JobChange {
                            error_code: Some(PERMISSION_DENIED),
                            ..JobChange::default()
                        },
                    )
                    .await?;
                    self.end_refused(job, PERMISSION_DENIED, now).await?;
                    return Ok(Step::Collected);
                }
                Err(error) => return Err(error),
            }
        }
        let id = job
            .provider_batch_id
            .as_deref()
            .ok_or(BatchRunError::Corrupt)?;
        let adapter = match self.provider(job).await {
            Ok(adapter) => adapter,
            Err(_) if give_up => {
                self.end_unconfirmed(job, now).await?;
                return Ok(Step::Collected);
            }
            Err(error) => return Err(error),
        };
        let api = match self.batch_api(job, &adapter).await {
            Ok(api) => api,
            Err(_) if give_up => {
                self.end_unconfirmed(job, now).await?;
                return Ok(Step::Collected);
            }
            Err(error) => return Err(error),
        };
        let batches = api.get();
        let mut current = batches.batch(id).await?;
        if job.cancel_requested && current.status == ProviderBatchStatus::Running {
            current = batches.cancel_batch(id).await?;
        }
        if !current.status.is_terminal() {
            if give_up {
                self.end_unconfirmed(job, now).await?;
                return Ok(Step::Collected);
            }
            self.update(
                job,
                JobChange {
                    provider_status: Some(&current.raw_status),
                    next_run_at: Some(now + self.poll),
                    progressed: true,
                    ..JobChange::default()
                },
            )
            .await?;
            return Ok(Step::Polled);
        }
        self.collect(job, endpoint, batches, &current, now).await?;
        Ok(Step::Collected)
    }

    /// The job's lines as the provider takes them, one per line, each held
    /// to the output its reservation covers.
    async fn provider_input(
        &self,
        job: &LeasedJob,
        endpoint: BatchEndpoint,
        batches: &dyn BatchProvider,
    ) -> Result<Vec<u8>, BatchRunError> {
        let tag = BatchTag {
            batch_id: job.batch_id,
            job_id: job.id,
        };
        let input = self.input(job, endpoint).await?;
        let mut jsonl = String::new();
        for line in self.store.job_lines(job).await? {
            let number = u32::try_from(line.line_number).map_err(|_| BatchRunError::Corrupt)?;
            let parsed = input.get(&number).ok_or(BatchRunError::Corrupt)?;
            let request = capped(&parsed.request, line.output_token_cap)?;
            jsonl.push_str(&batches.input_line(tag, number, endpoint, &job.model, &request)?);
            jsonl.push('\n');
        }
        Ok(jsonl.into_bytes())
    }

    async fn submitted(
        &self,
        job: &LeasedJob,
        batch: &ProviderBatch,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        self.update(
            job,
            JobChange {
                provider_batch_id: Some(&batch.id),
                provider_status: Some(&batch.raw_status),
                state: Some("submitted"),
                next_run_at: Some(now + self.poll),
                progressed: true,
                ..JobChange::default()
            },
        )
        .await?;
        Ok(())
    }

    /// Ends a job whose results were collected and saved but not all
    /// settled: the rest settle from the saved results, which stay as they
    /// are. A line the file does not cover goes to reconciliation.
    async fn resume_saved(&self, job: &LeasedJob, now: DateTime<Utc>) -> Result<(), BatchRunError> {
        let key = job
            .result_object_key
            .as_deref()
            .ok_or(BatchRunError::Corrupt)?;
        let bytes = self
            .objects()?
            .get(key)
            .await
            .map_err(|_| BatchRunError::Objects)?;
        let Some(bytes) = bytes else {
            // Nothing saved survives to keep.
            return self.end_unconfirmed(job, now).await;
        };
        let saved = saved_results(&bytes);
        let lines = self.store.job_lines(job).await?;
        let ended: Vec<Ended> = lines
            .iter()
            .map(|line| match saved.get(&line.line_number) {
                Some(result) => Ended {
                    line,
                    state: result.state,
                    status_code: result.status_code,
                    error_code: result.error_code.clone(),
                    usage: result.usage,
                    request_id: result.request_id.clone(),
                },
                None => Ended {
                    line,
                    state: "unknown",
                    status_code: None,
                    error_code: Some("result_unavailable".to_owned()),
                    usage: None,
                    request_id: None,
                },
            })
            .collect();
        self.settle_all(job, &ended, now).await?;
        // Recorded when the results were collected.
        let status = match job.provider_status.as_deref() {
            Some("expired") => "expired",
            Some("cancelled") => "cancelled",
            Some("failed") => "failed",
            _ => "completed",
        };
        self.finish(job, Some(status)).await
    }

    /// Ends a job the routing policy forbids before anything was sent:
    /// every line fails with `routing_policy_denied` and charges nothing.
    async fn end_denied(&self, job: &LeasedJob, now: DateTime<Utc>) -> Result<(), BatchRunError> {
        // Recorded before anything is settled, so a run cut short at any
        // point after this resumes the denial instead of sending the lines.
        self.update(
            job,
            JobChange {
                provider_status: Some(ROUTING_POLICY_DENIED),
                ..JobChange::default()
            },
        )
        .await?;
        let lines = self.store.job_lines(job).await?;
        let ended: Vec<Ended> = lines
            .iter()
            .map(|line| Ended {
                line,
                state: "failed",
                status_code: Some(403),
                error_code: Some(ROUTING_POLICY_DENIED.to_owned()),
                usage: Some(zero_usage()),
                request_id: None,
            })
            .collect();
        self.write_results(job, &ended, &HashMap::new()).await?;
        self.settle_all(job, &ended, now).await?;
        self.finish(job, Some(ROUTING_POLICY_DENIED)).await
    }

    /// Ends a job its connection cannot run (no batch API of the kind, or
    /// no staging) before anything was sent: every line fails with `code`
    /// and charges nothing.
    async fn end_refused(
        &self,
        job: &LeasedJob,
        code: &'static str,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let lines = self.store.job_lines(job).await?;
        let ended: Vec<Ended> = lines
            .iter()
            .map(|line| Ended {
                line,
                state: "failed",
                status_code: None,
                error_code: Some(code.to_owned()),
                usage: Some(zero_usage()),
                request_id: None,
            })
            .collect();
        self.write_results(job, &ended, &HashMap::new()).await?;
        self.settle_all(job, &ended, now).await?;
        self.finish(job, Some("failed")).await
    }

    /// Ends a job none of whose lines reached the provider: each settles at
    /// zero.
    async fn end_unsent(
        &self,
        job: &LeasedJob,
        state: &'static str,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let code = if state == "cancelled" {
            "batch_cancelled"
        } else {
            "batch_expired"
        };
        let lines = self.store.job_lines(job).await?;
        let ended: Vec<Ended> = lines
            .iter()
            .map(|line| Ended {
                line,
                state,
                status_code: None,
                error_code: Some(code.to_owned()),
                usage: Some(zero_usage()),
                request_id: None,
            })
            .collect();
        self.write_results(job, &ended, &HashMap::new()).await?;
        self.settle_all(job, &ended, now).await?;
        self.finish(job, Some(state)).await
    }

    /// Ends a job the provider may have run but never reported: its lines
    /// go to reconciliation.
    async fn end_unconfirmed(
        &self,
        job: &LeasedJob,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let lines = self.store.job_lines(job).await?;
        let ended: Vec<Ended> = lines
            .iter()
            .map(|line| Ended {
                line,
                state: "unknown",
                status_code: None,
                error_code: Some("result_unavailable".to_owned()),
                usage: None,
                request_id: None,
            })
            .collect();
        self.write_results(job, &ended, &HashMap::new()).await?;
        self.settle_all(job, &ended, now).await?;
        self.finish(job, Some("expired")).await
    }

    async fn collect(
        &self,
        job: &LeasedJob,
        endpoint: BatchEndpoint,
        batches: &dyn BatchProvider,
        current: &ProviderBatch,
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let download = |id: &Option<String>| {
            let id = id.clone();
            async move {
                match id {
                    Some(id) => batches.file_content(&id).await,
                    None => Ok(Vec::new()),
                }
            }
        };
        let output = download(&current.output_file_id).await?;
        let errors = download(&current.error_file_id).await?;
        let outcomes = batches.parse_results(endpoint, &output, &errors);
        let input = self.input(job, endpoint).await?;
        let lines = self.store.job_lines(job).await?;
        let mut bodies = HashMap::new();
        let mut ended = Vec::with_capacity(lines.len());
        for line in &lines {
            let number = u32::try_from(line.line_number).map_err(|_| BatchRunError::Corrupt)?;
            let alias = input
                .get(&number)
                .map_or("", |parsed| parsed.alias.as_str());
            let (state, status_code, error_code, usage, request_id) = match outcomes.get(&number) {
                Some(LineOutcome::Succeeded {
                    response,
                    request_id,
                }) => {
                    let id = format!("batch_req_{}_{number}", job.batch_id);
                    let (body, usage) = match response.as_ref() {
                        LineResponse::Chat(response) => (
                            serde_json::to_value(ChatBodyResponse {
                                id,
                                model: alias.to_owned(),
                                choices: response.choices().iter().map(choice_response).collect(),
                                usage: response.usage().map(|u| usage_response(u.usage())),
                            }),
                            response.usage().map(|u| u.usage()),
                        ),
                        LineResponse::Embeddings(response) => {
                            let usage = response.usage().map(|u| u.usage());
                            (
                                serde_json::to_value(embedding_response(response.clone(), alias)),
                                usage,
                            )
                        }
                    };
                    bodies.insert(line.line_number, body.map_err(|_| BatchRunError::Corrupt)?);
                    ("succeeded", Some(200), None, usage, request_id.clone())
                }
                Some(LineOutcome::Failed {
                    status_code,
                    code,
                    request_id,
                }) => (
                    "failed",
                    *status_code,
                    Some(
                        code.clone()
                            .unwrap_or_else(|| "provider_failure".to_owned()),
                    ),
                    Some(zero_usage()),
                    request_id.clone(),
                ),
                Some(LineOutcome::NotRun) | None
                    if current.status == ProviderBatchStatus::Expired =>
                {
                    (
                        "expired",
                        None,
                        Some("batch_expired".to_owned()),
                        Some(zero_usage()),
                        None,
                    )
                }
                Some(LineOutcome::NotRun) | None
                    if current.status == ProviderBatchStatus::Cancelled =>
                {
                    (
                        "cancelled",
                        None,
                        Some("batch_cancelled".to_owned()),
                        Some(zero_usage()),
                        None,
                    )
                }
                None if current.status == ProviderBatchStatus::Failed => (
                    "failed",
                    None,
                    Some("provider_job_failed".to_owned()),
                    Some(zero_usage()),
                    None,
                ),
                // Completed without the line, or with a result the gateway
                // cannot read: never charged a guess.
                _ => (
                    "unknown",
                    None,
                    Some("result_unavailable".to_owned()),
                    None,
                    None,
                ),
            };
            ended.push(Ended {
                line,
                state,
                status_code,
                error_code,
                usage,
                request_id,
            });
        }
        self.update(
            job,
            JobChange {
                provider_status: Some(&current.raw_status),
                output_file_id: current.output_file_id.as_deref(),
                error_file_id: current.error_file_id.as_deref(),
                ..JobChange::default()
            },
        )
        .await?;
        self.write_results(job, &ended, &bodies).await?;
        self.settle_all(job, &ended, now).await?;
        let state = match current.status {
            ProviderBatchStatus::Expired => "expired",
            ProviderBatchStatus::Cancelled => "cancelled",
            ProviderBatchStatus::Failed => "failed",
            _ => "completed",
        };
        self.finish(job, Some(state)).await
    }

    /// The job's normalized results, for the batch's output files: written
    /// before any line settles, and the same every time.
    async fn write_results(
        &self,
        job: &LeasedJob,
        ended: &[Ended<'_>],
        bodies: &HashMap<i32, Value>,
    ) -> Result<(), BatchRunError> {
        let mut jsonl = String::new();
        for end in ended {
            let value = json!({
                "line": end.line.line_number,
                "status_code": end.status_code,
                "request_id": end.request_id,
                "body": bodies.get(&end.line.line_number),
                "error": end.error_code,
                // What its settlement needs, should it have to resume from
                // this file alone.
                "state": end.state,
                "usage": end.usage,
            });
            jsonl.push_str(&value.to_string());
            jsonl.push('\n');
        }
        let key = format!(
            "batch-results/{}/{}/{}",
            job.organization_id, job.batch_id, job.id
        );
        self.objects()?
            .put(&key, jsonl.into_bytes())
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
        Ok(())
    }

    /// Settles every line still pending, each in its own transaction.
    async fn settle_all(
        &self,
        job: &LeasedJob,
        ended: &[Ended<'_>],
        now: DateTime<Utc>,
    ) -> Result<(), BatchRunError> {
        let organization_id = OrganizationId(job.organization_id);
        for end in ended.iter().filter(|end| end.line.state == "pending") {
            let provider_error = end.error_code.as_ref().and_then(|code| {
                // A line the routing policy refused never reached a provider.
                (end.state == "failed" && code != ROUTING_POLICY_DENIED).then(|| {
                    ProviderError::from_parts(
                        ProviderErrorCategory::InvalidRequest,
                        Some(job.family.clone()),
                        end.request_id.clone(),
                        end.status_code,
                        Some(code.clone()),
                        None,
                        None,
                    )
                    .ok()
                })?
            });
            let settlement = Settlement {
                finalization_id: finalization_id(end.line.reservation_id),
                usage: end.usage.clone(),
                trace_id: TraceId(end.line.trace_id),
                provider_request_id: end.request_id.clone(),
                provider_error,
                timings: Default::default(),
            };
            let mut state = end.state;
            match self
                .accounting
                .settle(
                    organization_id,
                    ReservationId(end.line.reservation_id),
                    settlement,
                    now,
                )
                .await
            {
                // A breach is recorded by the settlement itself.
                Ok(_) | Err(AccountingStoreError::BoundExceeded) => {}
                // A hold recovery released (its claim came too late): the
                // line never ran.
                Err(AccountingStoreError::NotSettleable) => state = "expired",
                // Usage its price cannot charge: the settlement kept the
                // hold and opened a reconciliation item, and replays the same
                // way. The line ends unknown rather than stopping the job's
                // other lines on every retry.
                Err(AccountingStoreError::Domain(error)) => {
                    tracing::warn!(
                        organization_id = %job.organization_id,
                        batch_id = %job.batch_id,
                        job_id = %job.id,
                        line = end.line.line_number,
                        error = %error,
                        "batch line usage could not be priced; sent to reconciliation"
                    );
                    state = "unknown";
                }
                Err(error) => return Err(error.into()),
            }
            self.store
                .finish_line(
                    job.organization_id,
                    job.batch_id,
                    end.line.line_number,
                    state,
                    end.status_code.map(i32::from),
                    end.error_code
                        .as_deref()
                        .map(|code| &code[..code.len().min(64)]),
                )
                .await?;
        }
        Ok(())
    }

    async fn finish(&self, job: &LeasedJob, status: Option<&str>) -> Result<(), BatchRunError> {
        self.update(
            job,
            JobChange {
                provider_status: status,
                state: Some("finished"),
                progressed: true,
                ..JobChange::default()
            },
        )
        .await?;
        Ok(())
    }

    /// Writes a batch's output and error files from its jobs' results and
    /// ends it. Repeated after a crash, it writes the same files.
    async fn finalize(
        &self,
        batch: &BatchRow,
        token: Uuid,
        now: DateTime<Utc>,
    ) -> Result<bool, BatchRunError> {
        let objects = self.objects()?;
        let lines = self.store.batch_lines(batch).await?;
        let jobs = self.store.batch_jobs(batch).await?;
        let mut results: HashMap<i32, Value> = HashMap::new();
        for (_, key, _) in &jobs {
            let Some(key) = key else { continue };
            let Some(bytes) = objects.get(key).await.map_err(|_| BatchRunError::Objects)? else {
                continue;
            };
            for raw in String::from_utf8_lossy(&bytes).lines() {
                if let Ok(value) = serde_json::from_str::<Value>(raw) {
                    if let Some(line) = value
                        .get("line")
                        .and_then(Value::as_i64)
                        .and_then(|l| i32::try_from(l).ok())
                    {
                        results.insert(line, value);
                    }
                }
            }
        }
        let mut output = String::new();
        let mut errors = String::new();
        let mut completed = 0;
        let mut failed = 0;
        for line in &lines {
            let result = results.get(&line.line_number);
            let field = |name: &str| {
                result
                    .and_then(|r| r.get(name))
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            let id = format!("batch_req_{}_{}", batch.id, line.line_number);
            // A success is reported only with its answer. Should the saved
            // result be lost, the line is reported as an error rather than
            // as a success with no body.
            let answered = result.is_some_and(|r| r.get("body").is_some_and(|b| !b.is_null()));
            if line.state == "succeeded" && answered {
                completed += 1;
                output.push_str(
                    &json!({
                        "id": id,
                        "custom_id": line.custom_id,
                        "response": {
                            "status_code": 200,
                            "request_id": field("request_id"),
                            "body": field("body"),
                        },
                        "error": null,
                    })
                    .to_string(),
                );
                output.push('\n');
            } else {
                failed += 1;
                let code = field("error")
                    .as_str()
                    .filter(|_| line.state != "succeeded")
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| "result_unavailable".to_owned());
                let error = json!({"code": code, "message": error_message(&code)});
                let response = field("status_code")
                    .as_u64()
                    .filter(|_| line.state != "succeeded")
                    .map(|status| {
                        json!({
                            "status_code": status,
                            "request_id": field("request_id"),
                            "body": {"error": error},
                        })
                    });
                errors.push_str(
                    &json!({
                        "id": id,
                        "custom_id": line.custom_id,
                        "response": response,
                        "error": error,
                    })
                    .to_string(),
                );
                errors.push('\n');
            }
        }
        let output = self
            .output_file(batch, b"gateway-batch-output", "output", output, now)
            .await?;
        let errors = self
            .output_file(batch, b"gateway-batch-errors", "errors", errors, now)
            .await?;
        let statuses: Vec<&str> = jobs.iter().filter_map(|(_, _, s)| s.as_deref()).collect();
        let status = if batch.status == "cancelling" {
            "cancelled"
        } else if statuses.contains(&"expired") {
            "expired"
        } else if !statuses.is_empty() && statuses.iter().all(|s| *s == "failed") {
            "failed"
        } else {
            "completed"
        };
        let ended = self
            .store
            .finalize(
                batch,
                token,
                BatchFinal {
                    status,
                    completed,
                    failed,
                    output,
                    errors,
                },
                now,
            )
            .await?;
        // The jobs' intermediate results hold content the output files now
        // carry: the run's cleanup deletes them, and retries any it missed.
        Ok(ended)
    }

    /// Deletes the saved results of jobs whose batch has ended, and records
    /// each deletion, so one missed by a crash or a refused delete is
    /// retried by the next run.
    async fn clean_results(&self) -> Result<u64, BatchRunError> {
        let objects = self.objects()?;
        let mut cleaned = 0;
        for (organization_id, job_id, key) in self.store.ended_job_results(CLEAN_LIMIT).await? {
            if let Err(error) = objects.delete(&key).await {
                tracing::warn!(%organization_id, %job_id, error = %error, "batch job results not deleted");
                continue;
            }
            self.store
                .clear_job_result(organization_id, job_id, &key)
                .await?;
            cleaned += 1;
        }
        Ok(cleaned)
    }

    async fn output_file(
        &self,
        batch: &BatchRow,
        label: &[u8],
        name: &str,
        content: String,
        now: DateTime<Utc>,
    ) -> Result<Option<BatchFileRow>, BatchRunError> {
        if content.is_empty() {
            return Ok(None);
        }
        let id = derived_id(label, batch.id);
        let key = object_key(batch.organization_id, id);
        let bytes = content.into_bytes();
        let row = BatchFileRow {
            id,
            organization_id: batch.organization_id,
            service_account_id: batch.service_account_id,
            purpose: crate::application::batches::FILE_PURPOSE_OUTPUT.to_owned(),
            filename: format!("batch_{}_{name}.jsonl", batch.id),
            bytes: i64::try_from(bytes.len()).map_err(|_| BatchRunError::Corrupt)?,
            sha256: Sha256::digest(&bytes).to_vec(),
            object_key: key.clone(),
            created_at: now,
            content_purged_at: None,
        };
        self.objects()?
            .put(&key, bytes)
            .await
            .map_err(|_| BatchRunError::Objects)?;
        Ok(Some(row))
    }
}

/// One line of a job's saved results.
struct SavedResult {
    state: &'static str,
    status_code: Option<u16>,
    error_code: Option<String>,
    usage: Option<Usage>,
    request_id: Option<String>,
}

/// A job's saved results by line. A line saved without its usage settles
/// through reconciliation, never as free.
fn saved_results(bytes: &[u8]) -> HashMap<i32, SavedResult> {
    let mut saved = HashMap::new();
    for raw in String::from_utf8_lossy(bytes).lines() {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let Some(line) = value
            .get("line")
            .and_then(Value::as_i64)
            .and_then(|l| i32::try_from(l).ok())
        else {
            continue;
        };
        let usage = value
            .get("usage")
            .filter(|usage| !usage.is_null())
            .and_then(|usage| serde_json::from_value::<Usage>(usage.clone()).ok());
        let state = match value.get("state").and_then(Value::as_str) {
            Some("succeeded") => "succeeded",
            Some("failed") => "failed",
            Some("expired") => "expired",
            Some("cancelled") => "cancelled",
            _ => "unknown",
        };
        let text = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        saved.insert(
            line,
            SavedResult {
                state,
                status_code: value
                    .get("status_code")
                    .and_then(Value::as_u64)
                    .and_then(|code| u16::try_from(code).ok()),
                error_code: text("error"),
                // Without usage the line goes to reconciliation.
                usage: usage.filter(|_| state != "unknown"),
                request_id: text("request_id"),
            },
        );
    }
    saved
}

/// A line's request held to the output its reservation covers, as the
/// ordinary chat path holds a request: the client's `max_tokens` may ask for
/// more than the route allows.
fn capped(request: &LineRequest, cap: Option<i64>) -> Result<LineRequest, BatchRunError> {
    let mut request = request.clone();
    if let LineRequest::Chat(chat) = &mut request {
        let cap = cap
            .and_then(|cap| u32::try_from(cap).ok())
            .filter(|cap| *cap > 0)
            .ok_or(BatchRunError::Corrupt)?;
        chat.max_tokens = Some(cap);
    }
    Ok(request)
}

enum Step {
    Submitted,
    Polled,
    Collected,
}

/// A job's batch API: the adapter's own, or one staged through the
/// connection's bucket.
enum BatchApi<'a> {
    Plain(&'a dyn BatchProvider),
    Staged(Box<dyn BatchProvider + 'a>),
}

impl BatchApi<'_> {
    fn get(&self) -> &dyn BatchProvider {
        match self {
            Self::Plain(api) => *api,
            Self::Staged(api) => api.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_ids_are_the_same_for_every_attempt_of_a_line() {
        let reservation = Uuid::new_v4();
        assert_eq!(finalization_id(reservation), finalization_id(reservation));
        assert_ne!(
            finalization_id(reservation),
            finalization_id(Uuid::new_v4())
        );
        assert_ne!(
            derived_id(b"gateway-batch-output", reservation),
            derived_id(b"gateway-batch-errors", reservation)
        );
    }
}
