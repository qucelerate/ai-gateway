//! Files and batches (D-021, docs/batch-design.md): a client uploads a
//! JSONL file of single requests and asks for it to run as a batch. The
//! whole file is validated and routed, and every line reserved, before the
//! batch is accepted; a worker then runs it through providers' native batch
//! APIs.

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    application::{
        AccountingServiceError,
        inference::{InferenceFacade, InferenceFacadeError, PublicModelAlias, PublicOperation},
        trace_objects::{TraceObjectError, TraceObjects},
    },
    domain::{
        accounting::QuotaScope,
        batch::{
            self, BatchEndpoint, BatchMode, COMPLETION_WINDOW, COMPLETION_WINDOW_DURATION,
            LineError, MAX_FILE_BYTES, MAX_LINE_ERRORS, RESERVATION_GRACE,
        },
        catalog::{CatalogOperation, ProviderFamily},
        identity::VerifiedIdentity,
    },
    persistence::batches::{
        BatchFileRow, BatchRow, BatchStore, BatchStoreError, CreateOutcome, NewBatch, NewJob,
        NewLine,
    },
};

pub const FILE_PURPOSE_BATCH: &str = "batch";
pub const FILE_PURPOSE_OUTPUT: &str = "batch_output";

#[derive(Debug, Error)]
pub enum BatchError {
    #[error("the batch file store is not configured")]
    StoreUnavailable,
    #[error("file not found")]
    FileNotFound,
    #[error("batch not found")]
    BatchNotFound,
    #[error("file purpose must be batch")]
    InvalidPurpose,
    #[error("file is too large")]
    FileTooLarge,
    #[error("invalid batch request: {0}")]
    InvalidRequest(&'static str),
    /// The input file was refused, line by line.
    #[error("the input file failed validation")]
    Invalid(Vec<LineError>),
    #[error("idempotency key reused with a different batch")]
    IdempotencyConflict,
    #[error(transparent)]
    Store(BatchStoreError),
    #[error(transparent)]
    Objects(#[from] TraceObjectError),
    #[error(transparent)]
    Facade(#[from] InferenceFacadeError),
}

impl From<BatchStoreError> for BatchError {
    fn from(error: BatchStoreError) -> Self {
        match error {
            BatchStoreError::IdempotencyConflict => Self::IdempotencyConflict,
            BatchStoreError::FileGone => Self::FileNotFound,
            other => Self::Store(other),
        }
    }
}

/// What a client asks for in `POST /batches`.
#[derive(Clone, Debug)]
pub struct BatchSpec {
    pub input_file_id: Uuid,
    pub endpoint: String,
    pub completion_window: String,
    pub metadata: serde_json::Map<String, Value>,
}

#[derive(Clone)]
pub struct BatchService {
    store: BatchStore,
    staging: crate::persistence::batch_settings::BatchSettingsStore,
    inference: InferenceFacade,
    objects: Option<Arc<dyn TraceObjects>>,
}

/// The object name of a batch file's content.
pub(crate) fn object_key(organization_id: Uuid, file_id: Uuid) -> String {
    format!("batch-files/{organization_id}/{file_id}")
}

impl BatchService {
    pub fn new(
        store: BatchStore,
        inference: InferenceFacade,
        objects: Option<Arc<dyn TraceObjects>>,
    ) -> Self {
        Self {
            staging: crate::persistence::batch_settings::BatchSettingsStore::new(
                store.pool().clone(),
            ),
            store,
            inference,
            objects,
        }
    }

    pub fn store(&self) -> &BatchStore {
        &self.store
    }

    fn objects(&self) -> Result<&Arc<dyn TraceObjects>, BatchError> {
        self.objects.as_ref().ok_or(BatchError::StoreUnavailable)
    }

    /// Stores an input file for the service account.
    pub async fn upload(
        &self,
        identity: &VerifiedIdentity,
        purpose: &str,
        filename: &str,
        bytes: Vec<u8>,
        now: DateTime<Utc>,
    ) -> Result<BatchFileRow, BatchError> {
        if purpose != FILE_PURPOSE_BATCH {
            return Err(BatchError::InvalidPurpose);
        }
        if bytes.len() > MAX_FILE_BYTES {
            return Err(BatchError::FileTooLarge);
        }
        let filename = sanitize_filename(filename);
        let objects = self.objects()?;
        let id = Uuid::new_v4();
        let organization_id = identity.organization_id.0;
        let key = object_key(organization_id, id);
        let sha256 = Sha256::digest(&bytes).to_vec();
        let size = i64::try_from(bytes.len()).map_err(|_| BatchError::FileTooLarge)?;
        let row = BatchFileRow {
            id,
            organization_id,
            service_account_id: identity.service_account_id.0,
            purpose: purpose.to_owned(),
            filename,
            bytes: size,
            sha256,
            object_key: key,
            created_at: now,
            content_purged_at: None,
        };
        // Recorded before the object is written, so an object always has a
        // record the expiry sweep finds, whatever fails after.
        self.store.insert_file(&row).await?;
        if let Err(error) = objects.put(&row.object_key, bytes).await {
            if let Err(abandon) = self.store.abandon_file(organization_id, id).await {
                tracing::warn!(error = %abandon, "a failed batch upload could not be marked");
            }
            return Err(error.into());
        }
        Ok(row)
    }

    pub async fn file(
        &self,
        identity: &VerifiedIdentity,
        id: Uuid,
    ) -> Result<BatchFileRow, BatchError> {
        self.store
            .file(
                identity.organization_id.0,
                identity.service_account_id.0,
                id,
            )
            .await?
            .ok_or(BatchError::FileNotFound)
    }

    /// A file and its content; a file whose content was purged is gone.
    pub async fn file_content(
        &self,
        identity: &VerifiedIdentity,
        id: Uuid,
    ) -> Result<(BatchFileRow, Vec<u8>), BatchError> {
        let file = self.file(identity, id).await?;
        if file.content_purged_at.is_some() {
            return Err(BatchError::FileNotFound);
        }
        let bytes = self
            .objects()?
            .get(&file.object_key)
            .await?
            .ok_or(BatchError::FileNotFound)?;
        Ok((file, bytes))
    }

    /// Validates, routes and reserves a whole batch, or refuses it with
    /// nothing held. A repeated `idempotency_key` returns the batch it
    /// created.
    pub async fn create(
        &self,
        identity: &VerifiedIdentity,
        idempotency_key: &str,
        spec: BatchSpec,
        now: DateTime<Utc>,
    ) -> Result<BatchRow, BatchError> {
        let endpoint =
            BatchEndpoint::parse(&spec.endpoint).ok_or(BatchError::InvalidRequest("endpoint"))?;
        if spec.completion_window != COMPLETION_WINDOW {
            return Err(BatchError::InvalidRequest("completion_window"));
        }
        if !batch::valid_metadata(&spec.metadata) {
            return Err(BatchError::InvalidRequest("metadata"));
        }
        let metadata = Value::Object(spec.metadata.clone());
        let fingerprint = fingerprint(&spec, endpoint, identity.end_user.as_deref());
        let organization_id = identity.organization_id.0;
        let service_account_id = identity.service_account_id.0;
        if let Some(existing) = self
            .store
            .batch_by_key(organization_id, service_account_id, idempotency_key)
            .await?
        {
            return if existing.fingerprint == fingerprint {
                Ok(existing)
            } else {
                Err(BatchError::IdempotencyConflict)
            };
        }
        let (file, bytes) = self.file_content(identity, spec.input_file_id).await?;
        if file.purpose != FILE_PURPOSE_BATCH {
            return Err(BatchError::InvalidPurpose);
        }
        let lines = batch::parse_input(&bytes, endpoint).map_err(BatchError::Invalid)?;
        let operation = match endpoint.operation() {
            CatalogOperation::ChatBatch => PublicOperation::ChatBatch,
            _ => PublicOperation::EmbeddingsBatch,
        };
        let id = Uuid::new_v4();
        let ttl = COMPLETION_WINDOW_DURATION + RESERVATION_GRACE;
        let (emulated_allowed, _) = self.store.emulation(organization_id).await?;
        let unbatched = match operation {
            PublicOperation::ChatBatch => PublicOperation::Chat,
            _ => PublicOperation::Embeddings,
        };
        let mut planner = Planner::new(identity, &self.inference, id, ttl);
        let mut native_routes: HashMap<String, Result<Option<PublicModelAlias>, &'static str>> =
            HashMap::new();
        let mut staged: HashMap<String, bool> = HashMap::new();
        // Lines no native job takes, run emulated where the organization
        // allows it.
        let mut emulated = Vec::new();
        let mut native = Vec::new();
        for line in lines {
            if planner.full() {
                break;
            }
            if !native_routes.contains_key(&line.alias) {
                let route = match self
                    .inference
                    .resolve_batch(identity, &line.alias, operation)
                    .await
                {
                    Ok(route) if runs_batches(&route) => Ok(Some(route)),
                    Ok(_) | Err(InferenceFacadeError::UnknownModel) => Ok(None),
                    Err(InferenceFacadeError::InsufficientScope) => Err("insufficient_scope"),
                    Err(InferenceFacadeError::RoutingPolicyDenied) => Err("routing_policy_denied"),
                    Err(other) => return Err(other.into()),
                };
                native_routes.insert(line.alias.clone(), route);
            }
            let route = match &native_routes[&line.alias] {
                Ok(route) => route.clone(),
                Err(code) => {
                    planner.refuse(line.line_number, code, route_message(code, &line.alias));
                    continue;
                }
            };
            let Some(route) = route else {
                if emulated_allowed {
                    emulated.push(line);
                } else {
                    planner.refuse(
                        line.line_number,
                        "model_unavailable",
                        format!(
                            "model {} has no batch route, and the organization does not allow emulated batches",
                            line.alias
                        ),
                    );
                }
                continue;
            };
            if let Err(InferenceFacadeError::ToolsUnsupported(detail)) =
                tools_check(&route, &line.request)
            {
                planner.refuse(line.line_number, "tools_unsupported", detail.to_owned());
                continue;
            }
            let family = route
                .provider_family
                .as_deref()
                .and_then(|family| family.parse::<ProviderFamily>().ok());
            let supported = family.is_some_and(|family| {
                crate::provider::batch::line_supported(family, &line.request)
            });
            if !supported {
                planner.refuse(
                    line.line_number,
                    "invalid_body",
                    format!("model {}'s provider does not take this request", line.alias),
                );
                continue;
            }
            // Bedrock and Gemini stage their jobs in the tenant's bucket: a
            // connection without one runs no native job.
            if family
                .and_then(crate::provider::batch::staging_kind)
                .is_some()
            {
                let key = route.adapter_instance_key.clone().unwrap_or_default();
                let ready = match staged.get(&key) {
                    Some(ready) => *ready,
                    None => {
                        let ready = self
                            .staging
                            .staging_for(organization_id, &key)
                            .await
                            .map_err(|error| BatchError::Store(BatchStoreError::Database(error)))?
                            .is_some();
                        staged.insert(key, ready);
                        ready
                    }
                };
                if !ready {
                    if emulated_allowed {
                        emulated.push(line);
                    } else {
                        planner.refuse(
                            line.line_number,
                            "batch_staging_missing",
                            format!(
                                "model {}'s provider connection has no batch staging bucket",
                                line.alias
                            ),
                        );
                    }
                    continue;
                }
            }
            native.push((line, route));
        }
        // A provider's smallest job: a group below it runs emulated where
        // allowed, else is refused, before anything is reserved for it.
        let mut sizes: HashMap<(String, String, String), usize> = HashMap::new();
        for (_, route) in &native {
            *sizes.entry(group_key(route)).or_default() += 1;
        }
        for (line, route) in native {
            let minimum = route
                .provider_family
                .as_deref()
                .and_then(|family| family.parse::<ProviderFamily>().ok())
                .map_or(1, crate::provider::batch::min_job_lines);
            if sizes[&group_key(&route)] >= minimum {
                planner.plan(line, &route, BatchMode::Native);
            } else if emulated_allowed {
                emulated.push(line);
            } else {
                planner.refuse(
                    line.line_number,
                    "batch_too_small",
                    format!(
                        "model {}'s provider runs batch jobs of at least {minimum} lines",
                        line.alias
                    ),
                );
            }
        }
        // A native job the result bound split below its provider's minimum
        // (a long tail of a large group) runs emulated, or is refused.
        for line in planner.undersized(emulated_allowed) {
            emulated.push(line);
        }
        let mut unary: HashMap<String, Result<PublicModelAlias, &'static str>> = HashMap::new();
        let mut checks: HashMap<String, Option<crate::application::inference::BatchLineCheck>> =
            HashMap::new();
        for line in emulated {
            if planner.full() {
                break;
            }
            if !unary.contains_key(&line.alias) {
                let route = match self
                    .inference
                    .resolve_batch(identity, &line.alias, unbatched)
                    .await
                {
                    Ok(route) if route.adapter_instance_key.is_some() => Ok(route),
                    Ok(_) | Err(InferenceFacadeError::UnknownModel) => Err("model_unavailable"),
                    Err(InferenceFacadeError::InsufficientScope) => Err("insufficient_scope"),
                    Err(InferenceFacadeError::RoutingPolicyDenied) => Err("routing_policy_denied"),
                    Err(other) => return Err(other.into()),
                };
                unary.insert(line.alias.clone(), route);
            }
            let route = match unary[&line.alias].clone() {
                Ok(route) => route,
                Err(code) => {
                    planner.refuse(line.line_number, code, route_message(code, &line.alias));
                    continue;
                }
            };
            if let Err(InferenceFacadeError::ToolsUnsupported(detail)) =
                tools_check(&route, &line.request)
            {
                planner.refuse(line.line_number, "tools_unsupported", detail.to_owned());
                continue;
            }
            // Each line goes out as a single request: refused now what the
            // adapter would refuse then, before anything is held for it.
            // Whether the provider is reachable is no matter for creation:
            // a connection that cannot be opened now (its endpoint does not
            // resolve) checks nothing here, and its lines are checked, and
            // retried, as the worker sends them.
            if !checks.contains_key(&line.alias) {
                let check = match self.inference.batch_line_check(identity, &route).await {
                    Ok(check) => Some(check),
                    Err(InferenceFacadeError::Accounting(AccountingServiceError::Provider(
                        error,
                    ))) => {
                        tracing::warn!(
                            organization_id = %organization_id,
                            batch_id = %id,
                            alias = %line.alias,
                            category = error.category().as_str(),
                            "batch line provider unavailable at creation; lines checked when sent"
                        );
                        None
                    }
                    Err(error) => return Err(error.into()),
                };
                checks.insert(line.alias.clone(), check);
            }
            if checks[&line.alias]
                .as_ref()
                .is_some_and(|check| check.check(&line.request).is_err())
            {
                planner.refuse(
                    line.line_number,
                    "invalid_body",
                    format!("model {}'s provider does not take this request", line.alias),
                );
                continue;
            }
            planner.plan(line, &route, BatchMode::Emulated);
        }
        let (jobs, planned, mut errors) = planner.finish()?;
        errors.sort_by_key(|error| error.line);
        errors.truncate(MAX_LINE_ERRORS);
        if !errors.is_empty() {
            return Err(BatchError::Invalid(errors));
        }
        let expires_at = now
            + chrono::Duration::from_std(COMPLETION_WINDOW_DURATION)
                .unwrap_or_else(|_| chrono::Duration::hours(24));
        let scope = QuotaScope::from_identity(identity)
            .map_err(|error| BatchError::Store(BatchStoreError::Accounting(error.into())))?;
        let outcome = self
            .store
            .create(
                NewBatch {
                    id,
                    scope,
                    idempotency_key: idempotency_key.to_owned(),
                    fingerprint,
                    endpoint: endpoint.as_str(),
                    operation: endpoint.operation().as_str(),
                    input_file_id: file.id,
                    metadata,
                    expires_at,
                    jobs,
                    lines: planned,
                },
                now,
            )
            .await?;
        Ok(match outcome {
            CreateOutcome::Created(row) | CreateOutcome::Existing(row) => row,
        })
    }

    pub async fn batch(
        &self,
        identity: &VerifiedIdentity,
        id: Uuid,
    ) -> Result<BatchRow, BatchError> {
        self.store
            .batch(
                identity.organization_id.0,
                identity.service_account_id.0,
                id,
            )
            .await?
            .ok_or(BatchError::BatchNotFound)
    }

    pub async fn list(
        &self,
        identity: &VerifiedIdentity,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<BatchRow>, BatchError> {
        Ok(self
            .store
            .list(
                identity.organization_id.0,
                identity.service_account_id.0,
                after,
                limit,
            )
            .await?)
    }

    pub async fn cancel(
        &self,
        identity: &VerifiedIdentity,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<BatchRow, BatchError> {
        self.store
            .cancel(
                identity.organization_id.0,
                identity.service_account_id.0,
                id,
                now,
            )
            .await?
            .ok_or(BatchError::BatchNotFound)
    }
}

/// Why a line's route was refused, in words.
fn route_message(code: &str, alias: &str) -> String {
    match code {
        "routing_policy_denied" => {
            format!("the organization's routing policy permits none of model {alias}'s deployments")
        }
        "insufficient_scope" => format!("the API key may not call model {alias}"),
        _ => format!("model {alias} has no batch route"),
    }
}

/// The lines one native job may take: one deployment's.
fn group_key(route: &PublicModelAlias) -> (String, String, String) {
    (
        route.deployment.clone(),
        route.model.clone(),
        route.adapter_instance_key.clone().unwrap_or_default(),
    )
}

/// Reserves a batch's lines and groups them into provider jobs: one per
/// deployment and mode, split further so a job's results always fit what
/// the worker reads.
struct Planner<'a> {
    identity: &'a VerifiedIdentity,
    inference: &'a InferenceFacade,
    batch_id: Uuid,
    ttl: std::time::Duration,
    jobs: Vec<NewJob>,
    /// The most each job's results can come to.
    job_bytes: Vec<u64>,
    lines: Vec<(NewLine, batch::ParsedLine)>,
    errors: Vec<LineError>,
    failure: Option<BatchError>,
}

impl<'a> Planner<'a> {
    fn new(
        identity: &'a VerifiedIdentity,
        inference: &'a InferenceFacade,
        batch_id: Uuid,
        ttl: std::time::Duration,
    ) -> Self {
        Self {
            identity,
            inference,
            batch_id,
            ttl,
            jobs: Vec::new(),
            job_bytes: Vec::new(),
            lines: Vec::new(),
            errors: Vec::new(),
            failure: None,
        }
    }

    /// Enough refusals to report, or a failure that ends the planning.
    fn full(&self) -> bool {
        self.errors.len() >= MAX_LINE_ERRORS || self.failure.is_some()
    }

    fn refuse(&mut self, line: u32, code: &'static str, message: impl Into<String>) {
        self.errors.push(LineError::line(line, code, message));
    }

    /// Reserves `line` on `route` and puts it in a job of `mode`.
    fn plan(&mut self, line: batch::ParsedLine, route: &PublicModelAlias, mode: BatchMode) {
        if self.full() {
            return;
        }
        let key = format!("batch:{}:{}", self.batch_id, line.line_number);
        let reservation = match self.inference.plan_batch_line(
            self.identity,
            route,
            &line.request,
            &key,
            self.ttl,
        ) {
            Ok(reservation) => reservation,
            Err(InferenceFacadeError::Accounting(
                AccountingServiceError::InputBoundExceeded
                | AccountingServiceError::OutputCapExceeded,
            )) => {
                self.refuse(
                    line.line_number,
                    "accounting_bound_exceeded",
                    "the request exceeds the model's bounds",
                );
                return;
            }
            Err(other) => {
                self.failure = Some(other.into());
                return;
            }
        };
        // Lines go to a job whose results, at most, still fit what the
        // worker reads; a line that could not fit any job is refused.
        let bound = batch::result_bound(
            &line.request,
            reservation.output_token_cap.map(|cap| cap.get()),
        );
        if bound > batch::MAX_JOB_RESULT_BYTES {
            self.refuse(
                line.line_number,
                "batch_line_too_large",
                "the line's result could be larger than a batch job can return",
            );
            return;
        }
        let adapter = route.adapter_instance_key.clone().unwrap_or_default();
        let index = self
            .jobs
            .iter()
            .zip(&self.job_bytes)
            .position(|(job, bytes)| {
                job.deployment == route.deployment
                    && job.model == route.model
                    && job.adapter_instance_key == adapter
                    && job.mode == mode
                    && bytes + bound <= batch::MAX_JOB_RESULT_BYTES
            });
        let job_id = match index {
            Some(index) => {
                self.job_bytes[index] += bound;
                self.jobs[index].id
            }
            None => {
                let job = NewJob {
                    id: Uuid::new_v4(),
                    ordinal: 0,
                    mode,
                    deployment: route.deployment.clone(),
                    model: route.model.clone(),
                    adapter_instance_key: adapter,
                    family: route.provider_family.clone().unwrap_or_default(),
                };
                let id = job.id;
                self.jobs.push(job);
                self.job_bytes.push(bound);
                id
            }
        };
        self.lines.push((
            NewLine {
                line_number: line.line_number,
                custom_id: line.custom_id.clone(),
                job_id,
                reservation,
            },
            line,
        ));
    }

    /// Takes out the native jobs smaller than their provider's minimum and
    /// their lines: returned to run emulated when `emulated_allowed`, else
    /// refused.
    fn undersized(&mut self, emulated_allowed: bool) -> Vec<batch::ParsedLine> {
        let small: Vec<(Uuid, usize)> = self
            .jobs
            .iter()
            .filter(|job| job.mode == BatchMode::Native)
            .filter_map(|job| {
                let minimum = job
                    .family
                    .parse::<ProviderFamily>()
                    .map_or(1, crate::provider::batch::min_job_lines);
                let count = self
                    .lines
                    .iter()
                    .filter(|(l, _)| l.job_id == job.id)
                    .count();
                (count < minimum).then_some((job.id, minimum))
            })
            .collect();
        if small.is_empty() {
            return Vec::new();
        }
        let mut moved = Vec::new();
        let lines = std::mem::take(&mut self.lines);
        for (planned, parsed) in lines {
            match small.iter().find(|(id, _)| *id == planned.job_id) {
                None => self.lines.push((planned, parsed)),
                Some(_) if emulated_allowed => moved.push(parsed),
                Some((_, minimum)) => self.refuse(
                    parsed.line_number,
                    "batch_too_small",
                    format!(
                        "model {}'s provider runs batch jobs of at least {minimum} lines",
                        parsed.alias
                    ),
                ),
            }
        }
        let keep: Vec<bool> = self
            .jobs
            .iter()
            .map(|job| !small.iter().any(|(id, _)| *id == job.id))
            .collect();
        let mut index = 0;
        self.jobs.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });
        let mut index = 0;
        self.job_bytes.retain(|_| {
            let kept = keep[index];
            index += 1;
            kept
        });
        moved
    }

    /// The jobs, numbered, and lines to write, and the refusals.
    fn finish(self) -> Result<(Vec<NewJob>, Vec<NewLine>, Vec<LineError>), BatchError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        let mut jobs = self.jobs;
        for (ordinal, job) in jobs.iter_mut().enumerate() {
            job.ordinal = i32::try_from(ordinal).unwrap_or(i32::MAX);
        }
        let mut lines: Vec<NewLine> = self.lines.into_iter().map(|(line, _)| line).collect();
        lines.sort_by_key(|line| line.line_number);
        Ok((jobs, lines, self.errors))
    }
}

/// Whether the route's family runs native batches. Families the catalog
/// accepts batch routes for all do; a route from before that check, or one
/// the test catalog names without a family, does not.
fn runs_batches(route: &PublicModelAlias) -> bool {
    route
        .provider_family
        .as_deref()
        .and_then(|family| family.parse::<ProviderFamily>().ok())
        .is_some_and(ProviderFamily::supports_batch)
}

fn fingerprint(spec: &BatchSpec, endpoint: BatchEndpoint, end_user: Option<&str>) -> [u8; 32] {
    let mut digest = Sha256::new();
    let mut frame = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    };
    frame(b"gateway-batch-v1");
    frame(spec.input_file_id.as_bytes());
    frame(endpoint.as_str().as_bytes());
    frame(spec.completion_window.as_bytes());
    // serde_json's maps are ordered by key, so equal metadata encodes the
    // same.
    frame(Value::Object(spec.metadata.clone()).to_string().as_bytes());
    // The end user the batch is charged to (D-028); framed only when named,
    // so batches without one keep their fingerprint.
    if let Some(end_user) = end_user {
        frame(b"end-user");
        frame(end_user.as_bytes());
    }
    digest.finalize().into()
}

/// A filename as stored and returned: no path, no control characters, at
/// most 255 characters, never empty.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).take(255).collect();
    if clean.trim().is_empty() {
        "input.jsonl".to_owned()
    } else {
        clean
    }
}

/// A chat line's tools checked against its route as a single request's are
/// (D-032).
fn tools_check(
    route: &PublicModelAlias,
    request: &crate::domain::batch::LineRequest,
) -> Result<(), InferenceFacadeError> {
    match request {
        crate::domain::batch::LineRequest::Chat(request) => {
            crate::application::inference::check_tools(route, request)
        }
        crate::domain::batch::LineRequest::Embeddings(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_lose_paths_and_control_characters() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\data\\in.jsonl"), "in.jsonl");
        assert_eq!(sanitize_filename("a\nb.jsonl"), "ab.jsonl");
        assert_eq!(sanitize_filename(""), "input.jsonl");
        assert_eq!(sanitize_filename(&"x".repeat(300)).len(), 255);
    }

    #[test]
    fn equal_requests_share_a_fingerprint() {
        let spec = |metadata: Value| BatchSpec {
            input_file_id: Uuid::nil(),
            endpoint: "/v1/embeddings".into(),
            completion_window: "24h".into(),
            metadata: metadata.as_object().unwrap().clone(),
        };
        let a = fingerprint(
            &spec(serde_json::json!({"a": "1", "b": "2"})),
            BatchEndpoint::Embeddings,
            None,
        );
        let b = fingerprint(
            &spec(serde_json::json!({"b": "2", "a": "1"})),
            BatchEndpoint::Embeddings,
            None,
        );
        let c = fingerprint(
            &spec(serde_json::json!({"a": "1"})),
            BatchEndpoint::Embeddings,
            None,
        );
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Another end user is another batch.
        let alice = fingerprint(
            &spec(serde_json::json!({"a": "1"})),
            BatchEndpoint::Embeddings,
            Some("alice"),
        );
        assert_ne!(alice, c);
        assert_ne!(
            alice,
            fingerprint(
                &spec(serde_json::json!({"a": "1"})),
                BatchEndpoint::Embeddings,
                Some("bob"),
            )
        );
    }
}
