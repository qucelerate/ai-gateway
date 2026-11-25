//! Immutable, typed job specifications. Accounting recovery, outbox dispatch,
//! webhook delivery, usage export, model discovery and price sync execute in
//! `run_worker`;
//! queueing and persistence concerns remain external.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

use crate::domain::{identity::OrganizationId, inference::CorrelationId};
use crate::metrics::{
    DISCOVERY_JOBS, OUTBOX_EVENTS, OUTBOX_OLDEST, OUTBOX_PENDING, PRICE_FETCHES,
    PRICE_SOURCES_FAILING, PRICES_ADOPTED, RECONCILIATION_OPEN, RESERVATIONS_OVERDUE,
    RESERVATIONS_RECOVERED, SWEPT_ROWS, USAGE_EXPORT_OBJECTS, USAGE_EXPORT_QUEUED,
    USAGE_EXPORT_RECORDS, USAGE_EXPORTS_FAILING, WEBHOOK_ATTEMPTS, WEBHOOKS_PENDING, WORKER_RUNS,
    global as metrics,
};
use crate::persistence::{
    AccountingStore, AccountingStoreError, OutboxStore,
    analytics_sink::AnalyticsSink,
    backlog::BacklogStore,
    outbox::Sinks,
    retention::RetentionStore,
    usage_export::UsageExportSink,
    webhooks::{WebhookFanout, WebhookStore},
};
use chrono::Utc;
use discovery::DiscoveryRunner;
use pricing::PricingRunner;
use std::time::Duration;
use usage_export::UsageExportRunner;
use webhooks::WebhookDeliverer;

pub mod agreements;
pub mod analytics_sink;
pub mod batches;
pub mod discovery;
pub mod egress;
pub mod pricing;
pub mod retention;
pub mod trace_content;
pub mod usage_export;
pub mod webhooks;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct JobId(pub Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

/// Kind is derived from `JobPayload`; callers cannot create a conflicting kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum JobKind {
    CatalogRefresh,
    PricingRefresh,
    Reconciliation,
    OutboxDispatch,
    RetentionSweep,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CatalogRefresh => "catalog.refresh",
            Self::PricingRefresh => "pricing.refresh",
            Self::Reconciliation => "reconciliation",
            Self::OutboxDispatch => "outbox.dispatch",
            Self::RetentionSweep => "retention.sweep",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct IdempotencyKey(String);
impl IdempotencyKey {
    pub fn new(value: impl Into<String>) -> Result<Self, JobError> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(JobError::EmptyIdempotencyKey)
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for IdempotencyKey {
    type Error = JobError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(try_from = "u16")]
pub struct PayloadSchemaVersion(u16);
impl PayloadSchemaVersion {
    pub fn new(value: u16) -> Result<Self, JobError> {
        if value == 0 {
            Err(JobError::InvalidSchemaVersion)
        } else {
            Ok(Self(value))
        }
    }
    pub fn value(self) -> u16 {
        self.0
    }
}
impl TryFrom<u16> for PayloadSchemaVersion {
    type Error = JobError;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CredentialId(pub Uuid);
impl CredentialId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
impl Default for CredentialId {
    fn default() -> Self {
        Self::new()
    }
}

/// A reference contains only an identifier. It cannot carry bearer material.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CredentialReference {
    pub id: CredentialId,
}

/// Closed Phase 0C payload vocabulary. No variant can carry inference content,
/// prompts, completions, provider secrets, or arbitrary user strings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum JobPayload {
    CatalogRefresh {
        credential: Option<CredentialReference>,
    },
    PricingRefresh {
        credential: Option<CredentialReference>,
    },
    Reconciliation {
        organization_id: Option<OrganizationId>,
    },
    OutboxDispatch {
        outbox_id: Uuid,
    },
    RetentionSweep {
        organization_id: Option<OrganizationId>,
    },
}

impl JobPayload {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::CatalogRefresh { .. } => JobKind::CatalogRefresh,
            Self::PricingRefresh { .. } => JobKind::PricingRefresh,
            Self::Reconciliation { .. } => JobKind::Reconciliation,
            Self::OutboxDispatch { .. } => JobKind::OutboxDispatch,
            Self::RetentionSweep { .. } => JobKind::RetentionSweep,
        }
    }
    pub fn organization_id(&self) -> Option<OrganizationId> {
        match self {
            Self::Reconciliation { organization_id } | Self::RetentionSweep { organization_id } => {
                *organization_id
            }
            Self::CatalogRefresh { .. }
            | Self::PricingRefresh { .. }
            | Self::OutboxDispatch { .. } => None,
        }
    }
    pub fn credential_references(&self) -> Vec<CredentialReference> {
        match self {
            Self::CatalogRefresh { credential } | Self::PricingRefresh { credential } => {
                credential.iter().copied().collect()
            }
            Self::Reconciliation { .. }
            | Self::OutboxDispatch { .. }
            | Self::RetentionSweep { .. } => Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobEnvelope {
    id: JobId,
    payload_schema_version: PayloadSchemaVersion,
    idempotency_key: IdempotencyKey,
    payload: JobPayload,
    correlation_id: CorrelationId,
}

pub type JobSpecification = JobEnvelope;

impl JobEnvelope {
    pub fn new(
        payload: JobPayload,
        payload_schema_version: PayloadSchemaVersion,
        idempotency_key: IdempotencyKey,
        correlation_id: CorrelationId,
    ) -> Self {
        Self {
            id: JobId::new(),
            payload_schema_version,
            idempotency_key,
            payload,
            correlation_id,
        }
    }
    pub fn id(&self) -> JobId {
        self.id
    }
    pub fn kind(&self) -> JobKind {
        self.payload.kind()
    }
    pub fn payload_schema_version(&self) -> PayloadSchemaVersion {
        self.payload_schema_version
    }
    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }
    pub fn payload(&self) -> &JobPayload {
        &self.payload
    }
    pub fn organization_id(&self) -> Option<OrganizationId> {
        self.payload.organization_id()
    }
    pub fn correlation_id(&self) -> CorrelationId {
        self.correlation_id
    }
    pub fn credential_references(&self) -> Vec<CredentialReference> {
        self.payload.credential_references()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum JobError {
    EmptyIdempotencyKey,
    InvalidSchemaVersion,
}
impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid job envelope: {:?}", self)
    }
}
impl std::error::Error for JobError {}

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("accounting recovery failed")]
    Accounting(#[from] AccountingStoreError),
}

/// What the worker drives: accounting recovery, outbox dispatch into webhook
/// deliveries and the usage export queue, the deliveries themselves, usage
/// export, model discovery, price sync, and the backlog gauges.
pub struct Worker {
    /// Batch files, whose content expires with the hourly sweep.
    pub batch_files: crate::persistence::batches::BatchStore,
    /// Runs batches' provider jobs and writes their output (D-021).
    pub batches: batches::BatchRunner,
    /// Present with AWS KMS configured: re-seals secrets under the active
    /// data key.
    pub credential_keys: Option<crate::application::credential_keys::CredentialKeys>,
    pub backlog: BacklogStore,
    pub retention: RetentionStore,
    pub accounting: AccountingStore,
    pub outbox: OutboxStore,
    pub webhooks: WebhookStore,
    pub deliverer: WebhookDeliverer,
    pub usage_export: UsageExportRunner,
    pub discovery: DiscoveryRunner,
    /// Checks accepted model agreements until they are available (D-035).
    pub agreements: agreements::AgreementRunner,
    pub pricing: PricingRunner,
    /// Adopts published prices that differ from active releases (D-024).
    pub adoption: crate::persistence::price_adoption::PriceAdoptionStore,
    pub notifications: crate::persistence::notifications::NotificationStore,
    /// Budgets past their threshold, keys and tokens near expiry.
    pub conditions: crate::persistence::notification_conditions::NotificationConditions,
    pub config: crate::persistence::config_versions::ConfigStore,
    pub trace_content: crate::persistence::trace_content::TraceContentStore,
    /// The trace store, when configured: expired content is deleted from it.
    pub trace_objects: Option<std::sync::Arc<dyn crate::application::trace_objects::TraceObjects>>,
    /// The ClickHouse analytics sink (D-030): writes queued requests and
    /// copies history when `GATEWAY_CLICKHOUSE_URL` is set; otherwise only
    /// turns an abandoned sink off.
    pub analytics_sink: analytics_sink::AnalyticsSinkRunner,
}

const RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const ANALYTICS_SINK_INTERVAL: Duration = Duration::from_secs(1);
const ANALYTICS_SINK_IDLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DISPATCH_INTERVAL: Duration = Duration::from_secs(5);
const USAGE_EXPORT_INTERVAL: Duration = Duration::from_secs(30);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const PRICING_INTERVAL: Duration = Duration::from_secs(60);
/// The longest a differing published price waits to be adopted.
const ADOPTION_INTERVAL: Duration = Duration::from_secs(60 * 60);
const BACKLOG_INTERVAL: Duration = Duration::from_secs(60);
const RESEAL_INTERVAL: Duration = Duration::from_secs(60);
const BATCH_INTERVAL: Duration = Duration::from_secs(15);
/// How often budgets, keys and agent tokens are checked for notifications.
const CONDITIONS_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How often the outbox is looked at when it has nothing pending; with a
/// backlog it is drained without waiting.
const OUTBOX_INTERVAL: Duration = Duration::from_secs(1);
const OUTBOX_BATCH: i64 = 100;
/// Outbox batches dispatched at once while a backlog exists; each holds a
/// connection while its sinks run.
const OUTBOX_CONCURRENCY: usize = 4;
/// Rounds per tick of webhook deliveries and usage export, so neither runs
/// unbounded.
const MAX_BATCHES_PER_TICK: usize = 10;
const SWEEP_BATCH: i64 = 1_000;
/// Batches per hourly sweep, so a backlog drains without one unbounded run.
const MAX_SWEEP_BATCHES: usize = 100;
const OUTBOX_RETENTION_DAYS: i64 = 7;
const DELIVERY_RETENTION_DAYS: i64 = 30;
const FINALIZATION_RETENTION_DAYS: i64 = 7;
const SYNC_JOB_RETENTION_DAYS: i64 = 30;
const MISSING_MODEL_RETENTION_DAYS: i64 = 90;
/// Superseded price snapshots are kept this long after they were last seen.
const PRICE_SNAPSHOT_RETENTION_DAYS: i64 = 365;
/// Configuration versions, except each organization's newest and pinned.
const CONFIG_VERSION_RETENTION_DAYS: i64 = 90;
/// Instance rows are kept this long after the instance was last seen.
const INSTANCE_RETENTION_DAYS: i64 = 7;

/// Phase 0 worker entry point: deliberately does not bind an HTTP listener.
pub async fn recover_accounting_once(store: &AccountingStore) -> Result<u64, WorkerError> {
    Ok(store.recover_expired(Utc::now(), 100).await?)
}

/// Runs until accounting recovery fails. Recovery, dispatch and the sweep
/// each have their own loop, so slow webhook endpoints never delay recovery
/// and a long sweep never delays delivery. Outbox and webhook failures are
/// logged and retried on the next tick, since their rows stay pending.
pub async fn run_worker(worker: Worker) -> Result<(), WorkerError> {
    tokio::select! {
        result = recovery_loop(&worker.accounting) => result,
        () = outbox_loop(&worker.outbox) => Ok(()),
        () = delivery_loop(&worker.deliverer) => Ok(()),
        () = usage_export_loop(&worker.usage_export) => Ok(()),
        () = sweep_loop(&worker) => Ok(()),
        () = discovery_loop(&worker.discovery) => Ok(()),
        () = agreements_loop(&worker.agreements) => Ok(()),
        () = pricing_loop(&worker.pricing, &worker.adoption) => Ok(()),
        () = backlog_loop(&worker.backlog) => Ok(()),
        () = reseal_loop(worker.credential_keys.as_ref()) => Ok(()),
        () = batch_loop(&worker.batches) => Ok(()),
        () = conditions_loop(&worker.conditions) => Ok(()),
        () = analytics_sink_loop(&worker.analytics_sink) => Ok(()),
    }
}

/// Writes requests to ClickHouse every second while any are queued, and
/// copies history; on a worker without the setting, checks every few
/// minutes whether an abandoned sink should be turned off. A failure is
/// logged and retried: the queue keeps every request until it is written.
async fn analytics_sink_loop(runner: &analytics_sink::AnalyticsSinkRunner) {
    let period = if runner.configured() {
        ANALYTICS_SINK_INTERVAL
    } else {
        ANALYTICS_SINK_IDLE_INTERVAL
    };
    let mut tick = ticker(period);
    let mut failures: u32 = 0;
    loop {
        tick.tick().await;
        let result = runner.tick().await;
        ran("analytics_sink", result.is_ok());
        match result {
            Ok(report) => {
                failures = 0;
                if report.failed > 0 {
                    tracing::warn!(failed = report.failed, "analytics sink writes failed");
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                tracing::warn!(error = %error, "analytics sink tick failed");
                // ClickHouse or the database is down: wait before the next
                // try, up to a minute.
                let pause = Duration::from_secs(u64::from(failures.min(6)) * 10);
                tokio::time::sleep(pause).await;
            }
        }
    }
}

/// Raises and resolves the notifications the worker finds by looking:
/// budgets past their threshold, API keys and agent tokens near expiry.
async fn conditions_loop(
    conditions: &crate::persistence::notification_conditions::NotificationConditions,
) {
    let mut tick = ticker(CONDITIONS_INTERVAL);
    loop {
        tick.tick().await;
        let result = conditions.check(Utc::now()).await;
        ran("notification_conditions", result.is_ok());
        if let Err(error) = result {
            tracing::warn!(error = %error, "notification condition check failed");
        }
    }
}

/// Re-seals stored secrets under the active data key, when KMS is
/// configured; otherwise never returns.
async fn reseal_loop(keys: Option<&crate::application::credential_keys::CredentialKeys>) {
    let Some(keys) = keys else {
        return std::future::pending().await;
    };
    let mut tick = ticker(RESEAL_INTERVAL);
    loop {
        tick.tick().await;
        let result = keys.reseal().await;
        ran("credential_reseal", result.is_ok());
        match result {
            Ok(0) => {}
            Ok(resealed) => tracing::info!(resealed, "stored secrets re-sealed"),
            Err(error) => tracing::warn!(error = %error, "credential re-seal failed"),
        }
    }
}

/// Submits, polls and collects batches' provider jobs, and ends batches
/// whose jobs have all finished.
async fn batch_loop(runner: &batches::BatchRunner) {
    let mut tick = ticker(BATCH_INTERVAL);
    loop {
        tick.tick().await;
        let result = runner.run_once(Utc::now()).await;
        ran("batches", result.is_ok());
        if let Err(error) = result {
            tracing::warn!(error = %error, "batch run failed");
        }
    }
}

/// Counts one run of a worker task.
fn ran(task: &'static str, ok: bool) {
    metrics().inc(&WORKER_RUNS, &[task, if ok { "ok" } else { "error" }]);
}

async fn recovery_loop(accounting: &AccountingStore) -> Result<(), WorkerError> {
    let mut recovery = ticker(RECOVERY_INTERVAL);
    loop {
        recovery.tick().await;
        let recovered = recover_accounting_once(accounting).await;
        ran("accounting_recovery", recovered.is_ok());
        let recovered = recovered?;
        metrics().add(&RESERVATIONS_RECOVERED, &[], recovered);
        if recovered > 0 {
            tracing::info!(recovered, "accounting recovery batch completed");
        }
        // Budget leases past their expiry, whoever holds them: a crashed
        // gateway's come back here. A failure is retried next tick; the
        // leased amounts stay reserved meanwhile, so nothing is overspent.
        let returned = crate::persistence::leases::return_expired(accounting.pool(), 100).await;
        ran("budget_lease_expiry", returned.is_ok());
        match returned {
            Ok(0) => {}
            Ok(returned) => tracing::info!(returned, "expired budget leases returned"),
            Err(error) => tracing::warn!(%error, "expired budget leases not returned"),
        }
        // Rate-limit leases ended long since, whoever holds them: a crashed
        // gateway's in-flight counts come back here (its admissions leave
        // the minute's window on their own). Until then they only refuse
        // more, never admit more.
        let returned = crate::persistence::rate_leases::return_expired(
            accounting.pool(),
            1_000,
            crate::persistence::rate_leases::EXPIRE_AFTER,
        )
        .await;
        ran("rate_limit_lease_expiry", returned.is_ok());
        match returned {
            Ok(0) => {}
            Ok(returned) => tracing::info!(returned, "expired rate-limit leases returned"),
            Err(error) => tracing::warn!(%error, "expired rate-limit leases not returned"),
        }
    }
}

/// Drains the outbox: each tick, rounds of [`OUTBOX_CONCURRENCY`] batches
/// run back to back while any comes back full, so a backlog is worked
/// through continuously rather than a fixed number of batches a tick.
/// Webhook deliveries have their own loop, so neither waits on the other.
async fn outbox_loop(outbox: &OutboxStore) {
    let mut tick = ticker(OUTBOX_INTERVAL);
    loop {
        tick.tick().await;
        while dispatch_outbox(outbox).await {
            tokio::task::yield_now().await;
        }
    }
}

async fn delivery_loop(deliverer: &WebhookDeliverer) {
    let mut dispatch = ticker(DISPATCH_INTERVAL);
    loop {
        dispatch.tick().await;
        deliver_webhooks(deliverer).await;
    }
}

async fn usage_export_loop(runner: &UsageExportRunner) {
    let mut tick = ticker(USAGE_EXPORT_INTERVAL);
    loop {
        tick.tick().await;
        export_usage(runner).await;
    }
}

/// Seals, then writes until a round claims less than a full set of
/// batches, at most [`MAX_BATCHES_PER_TICK`] rounds.
async fn export_usage(runner: &UsageExportRunner) {
    for round in 0..MAX_BATCHES_PER_TICK {
        let result = if round == 0 {
            runner.run_once().await
        } else {
            runner.write_due().await
        };
        match result {
            Ok(report) => {
                record_export(&report);
                if report.claimed < usage_export::UPLOAD_BATCH as usize {
                    return;
                }
            }
            Err(error) => {
                ran("usage_export", false);
                tracing::warn!(error = ?error, "usage export failed");
                return;
            }
        }
    }
}

fn record_export(report: &usage_export::ExportReport) {
    ran("usage_export", true);
    metrics().add(&USAGE_EXPORT_OBJECTS, &["written"], report.written as u64);
    metrics().add(&USAGE_EXPORT_OBJECTS, &["failed"], report.failed as u64);
    metrics().add(&USAGE_EXPORT_RECORDS, &[], report.records_written);
    if report.failed > 0 {
        tracing::warn!(
            failed = report.failed,
            "usage export objects failed to write"
        );
    }
    if report.reported_failing > 0 {
        tracing::warn!(
            exports = report.reported_failing,
            "usage exports reported failing"
        );
    }
}

async fn agreements_loop(agreements: &agreements::AgreementRunner) {
    let mut tick = ticker(agreements::CHECK_INTERVAL);
    loop {
        tick.tick().await;
        let result = agreements.run_once(Utc::now()).await;
        ran("model_agreements", result.is_ok());
        if let Err(error) = result {
            tracing::warn!(error = %error, "model agreement checks failed");
        }
    }
}

async fn discovery_loop(discovery: &DiscoveryRunner) {
    let mut tick = ticker(DISCOVERY_INTERVAL);
    loop {
        tick.tick().await;
        let result = discovery.run_once().await;
        ran("model_discovery", result.is_ok());
        match result {
            Ok(report) => {
                metrics().add(&DISCOVERY_JOBS, &["succeeded"], report.succeeded);
                metrics().add(&DISCOVERY_JOBS, &["failed"], report.failed);
                if report.scheduled + report.succeeded + report.failed > 0 {
                    tracing::info!(
                        scheduled = report.scheduled,
                        succeeded = report.succeeded,
                        failed = report.failed,
                        "model discovery batch completed"
                    );
                }
            }
            Err(error) => tracing::warn!(error = ?error, "model discovery failed"),
        }
    }
}

/// Adopts each organization's differing published prices; one
/// organization's failure is logged and leaves the others to run.
async fn adopt_prices(adoption: &crate::persistence::price_adoption::PriceAdoptionStore) {
    use crate::persistence::price_adoption::AdoptionOutcome;
    let organizations = match adoption.organizations().await {
        Ok(organizations) => organizations,
        Err(error) => {
            ran("price_adoption", false);
            tracing::warn!(error = ?error, "price adoption could not list organizations");
            return;
        }
    };
    let mut ok = true;
    for organization_id in organizations {
        match adoption.apply(organization_id, Utc::now()).await {
            Ok(AdoptionOutcome::Activated {
                release_id,
                adopted,
                held,
            }) => {
                metrics().add(&PRICES_ADOPTED, &[], adopted as u64);
                tracing::info!(
                    organization_id = %organization_id.0,
                    %release_id,
                    adopted,
                    held,
                    "published prices adopted"
                );
            }
            Ok(AdoptionOutcome::Unchanged { .. }) => {}
            Ok(AdoptionOutcome::Failed(code)) => {
                ok = false;
                tracing::warn!(organization_id = %organization_id.0, code, "price adoption failed");
            }
            Ok(AdoptionOutcome::NotEnabled | AdoptionOutcome::Stale) => {}
            Err(error) => {
                ok = false;
                tracing::warn!(organization_id = %organization_id.0, error = ?error, "price adoption failed");
            }
        }
    }
    ran("price_adoption", ok);
}

async fn pricing_loop(
    pricing: &PricingRunner,
    adoption: &crate::persistence::price_adoption::PriceAdoptionStore,
) {
    let mut tick = ticker(PRICING_INTERVAL);
    let mut adopted_at: Option<std::time::Instant> = None;
    loop {
        tick.tick().await;
        let result = pricing.run_once().await;
        ran("price_sync", result.is_ok());
        // Adopt after a source published something new, and at least
        // hourly, so a policy turned on or a deployment added since is
        // caught up without waiting for the next change.
        let changed = matches!(&result, Ok(report) if report.changed > 0);
        if changed || adopted_at.is_none_or(|at| at.elapsed() >= ADOPTION_INTERVAL) {
            adopt_prices(adoption).await;
            adopted_at = Some(std::time::Instant::now());
        }
        match result {
            Ok(report) => {
                metrics().add(&PRICE_FETCHES, &["changed"], report.changed);
                metrics().add(&PRICE_FETCHES, &["unchanged"], report.unchanged);
                metrics().add(&PRICE_FETCHES, &["failed"], report.failed);
                if report.changed + report.unchanged + report.failed > 0 {
                    tracing::info!(
                        changed = report.changed,
                        unchanged = report.unchanged,
                        failed = report.failed,
                        "price sync batch completed"
                    );
                }
            }
            Err(error) => tracing::warn!(error = ?error, "price sync failed"),
        }
    }
}

async fn backlog_loop(backlog: &BacklogStore) {
    let mut tick = ticker(BACKLOG_INTERVAL);
    loop {
        tick.tick().await;
        sample_backlog(backlog).await;
    }
}

/// Sets the backlog gauges from one sample; a failed sample leaves the
/// previous values.
pub async fn sample_backlog(backlog: &BacklogStore) {
    let sample = backlog.sample().await;
    ran("backlog_sample", sample.is_ok());
    match sample {
        Ok(sample) => {
            let registry = metrics();
            registry.set(&OUTBOX_PENDING, &[], sample.outbox_pending);
            registry.set(&OUTBOX_OLDEST, &[], sample.outbox_oldest_seconds);
            registry.set(&WEBHOOKS_PENDING, &[], sample.webhooks_pending);
            registry.set(&RECONCILIATION_OPEN, &[], sample.reconciliation_open);
            registry.set(&RESERVATIONS_OVERDUE, &[], sample.reservations_overdue);
            registry.set(&PRICE_SOURCES_FAILING, &[], sample.price_sources_failing);
            registry.set(&USAGE_EXPORT_QUEUED, &[], sample.usage_export_queued);
            registry.set(&USAGE_EXPORTS_FAILING, &[], sample.usage_exports_failing);
        }
        Err(error) => tracing::warn!(error = ?error, "backlog sample failed"),
    }
}

async fn sweep_loop(worker: &Worker) {
    let mut sweep = ticker(SWEEP_INTERVAL);
    loop {
        sweep.tick().await;
        sweep_retained(worker).await;
    }
}

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

/// One round of [`OUTBOX_CONCURRENCY`] batches at once, each in its own
/// transaction claiming its events with `SKIP LOCKED`, so they never take
/// the same event. True when a batch came back full: a backlog remains.
async fn dispatch_outbox(outbox: &OutboxStore) -> bool {
    let sinks = Sinks(&[&WebhookFanout, &UsageExportSink, &AnalyticsSink]);
    let results = futures::future::join_all(
        (0..OUTBOX_CONCURRENCY).map(|_| outbox.dispatch_batch(&sinks, Utc::now(), OUTBOX_BATCH)),
    )
    .await;
    let mut full = false;
    for result in results {
        ran("outbox_dispatch", result.is_ok());
        match result {
            Ok(report) => {
                metrics().add(&OUTBOX_EVENTS, &["dispatched"], report.dispatched as u64);
                metrics().add(&OUTBOX_EVENTS, &["failed"], report.failed as u64);
                if report.failed > 0 {
                    tracing::warn!(failed = report.failed, "outbox events failed to dispatch");
                }
                full |= report.dispatched + report.failed >= OUTBOX_BATCH as usize;
            }
            Err(error) => {
                // Retried on the next tick, since the events stay pending.
                tracing::warn!(error = ?error, "outbox dispatch failed");
                return false;
            }
        }
    }
    full
}

async fn deliver_webhooks(deliverer: &WebhookDeliverer) {
    for _ in 0..MAX_BATCHES_PER_TICK {
        let result = deliverer.deliver_due().await;
        ran("webhook_delivery", result.is_ok());
        match result {
            Ok(report) => {
                metrics().add(&WEBHOOK_ATTEMPTS, &["delivered"], report.delivered as u64);
                metrics().add(&WEBHOOK_ATTEMPTS, &["failed"], report.failed as u64);
                metrics().add(&WEBHOOK_ATTEMPTS, &["parked"], report.parked as u64);
                if report.parked > 0 {
                    tracing::warn!(parked = report.parked, "webhook deliveries parked");
                }
                if report.claimed == 0 {
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(error = ?error, "webhook delivery failed");
                return;
            }
        }
    }
}

/// Sweeps `table` in batches of [`SWEEP_BATCH`] until a short batch, an
/// error, or [`MAX_SWEEP_BATCHES`]. Returns whether no batch failed.
async fn sweep_table<F, Fut, E>(table: &'static str, mut batch: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<u64, E>>,
    E: fmt::Debug,
{
    for _ in 0..MAX_SWEEP_BATCHES {
        match batch().await {
            Ok(swept) => {
                metrics().add(&SWEPT_ROWS, &[table], swept);
                if swept < SWEEP_BATCH as u64 {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(table, error = ?error, "retention sweep failed");
                return false;
            }
        }
    }
    true
}

async fn sweep_retained(worker: &Worker) {
    let now = Utc::now();
    let days = chrono::Duration::days;
    let outbox_cutoff = now - days(OUTBOX_RETENTION_DAYS);
    let delivery_cutoff = now - days(DELIVERY_RETENTION_DAYS);
    let finalization_cutoff = now - days(FINALIZATION_RETENTION_DAYS);
    let job_cutoff = now - days(SYNC_JOB_RETENTION_DAYS);
    let model_cutoff = now - days(MISSING_MODEL_RETENTION_DAYS);
    let snapshot_cutoff = now - days(PRICE_SNAPSHOT_RETENTION_DAYS);
    let version_cutoff = now - days(CONFIG_VERSION_RETENTION_DAYS);
    let instance_cutoff = now - days(INSTANCE_RETENTION_DAYS);
    let notification_cutoff = now - days(crate::persistence::notifications::RETENTION_DAYS);
    let discovery = worker.discovery.store();
    let ok = [
        sweep_table("outbox_event", || {
            worker.outbox.sweep_dispatched(outbox_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("webhook_delivery", || {
            worker.webhooks.sweep_finished(delivery_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("accounting_finalization", || {
            worker
                .accounting
                .sweep_finalizations(finalization_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("catalog_sync_job", || {
            discovery.sweep_jobs(job_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("catalog_discovered_model", || {
            discovery.sweep_models(model_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("price_snapshot", || {
            worker
                .pricing
                .store()
                .sweep_snapshots(snapshot_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("config_version", || {
            worker.config.sweep_versions(version_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("gateway_instance", || {
            worker.config.sweep_instances(instance_cutoff, SWEEP_BATCH)
        })
        .await,
        sweep_table("usage_export_record", || {
            worker.usage_export.store().sweep_expired(now, SWEEP_BATCH)
        })
        .await,
        sweep_table("notification", || {
            worker.notifications.sweep(notification_cutoff, SWEEP_BATCH)
        })
        .await,
    ];
    ran("retention_sweep", ok.iter().all(|ok| *ok));
    // Trace content first: a request record is never purged while its
    // content is still stored.
    let content =
        trace_content::expire_content(&worker.trace_content, worker.trace_objects.as_ref(), now)
            .await;
    ran(
        "trace_content_expiry",
        content.as_ref().is_ok_and(|report| report.failed == 0),
    );
    match content {
        Ok(report) if report.expired > 0 => {
            tracing::info!(expired = report.expired, "trace content expired");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(error = ?error, "trace content expiry failed"),
    }
    let files =
        trace_content::expire_batch_files(&worker.batch_files, worker.trace_objects.as_ref(), now)
            .await;
    ran(
        "batch_file_expiry",
        files.as_ref().is_ok_and(|report| report.failed == 0),
    );
    match files {
        Ok(report) => {
            if report.purged > 0 {
                tracing::info!(purged = report.purged, "batch file content expired");
            }
            if report.failed > 0 {
                tracing::warn!(failed = report.failed, "batch file content not deleted");
            }
        }
        Err(error) => tracing::warn!(error = %error, "batch file expiry failed"),
    }
    // Tenant retention policies.
    let report = retention::purge_expired(&worker.retention, now).await;
    ran(
        "retention_purge",
        report.as_ref().is_ok_and(|report| report.failed == 0),
    );
    match report {
        Ok(report) => {
            let purged = report.purged;
            if purged.traces + purged.reservations + purged.audit_events > 0 {
                tracing::info!(
                    traces = purged.traces,
                    reservations = purged.reservations,
                    audit_events = purged.audit_events,
                    failed_organizations = report.failed,
                    "retention purge completed"
                );
            }
        }
        Err(error) => tracing::warn!(error = ?error, "retention purge failed"),
    }
}
