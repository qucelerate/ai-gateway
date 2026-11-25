//! Model discovery: lists the models each active provider account can call.
//!
//! Every tick schedules a job for each provider account in an active release
//! that has not had one within the sync interval, then runs the jobs it can
//! claim. A listing that fails is recorded with a fixed error code; provider
//! text never reaches the job row or the log.

use std::{str::FromStr, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    domain::{
        catalog::ProviderFamily,
        discovery::{ERROR_NOT_CONFIGURED, SyncStatus, error_code},
        identity::OrganizationId,
        inference::{ProviderError, ProviderErrorCategory},
        notifications::{NewNotification, NotificationKind},
    },
    persistence::{
        auto_offer::{ApplyOutcome, AutoOfferStore},
        catalog_sync::{CatalogSyncStore, ClaimedSyncJob},
        model_evidence::ModelEvidenceStore,
        notifications::NotificationChange,
    },
    provider::InferenceProviderAdapter,
};

/// The account's adapter and listing, or `None` when the account has no
/// credential.
type ListingResult = Result<
    Option<(
        Arc<dyn InferenceProviderAdapter>,
        Vec<crate::domain::discovery::DiscoveredModel>,
    )>,
    ProviderError,
>;

/// How often each active provider account is listed.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a claim lasts; a listing is abandoned well before it expires.
const LEASE: Duration = Duration::from_secs(5 * 60);
const LISTING_TIMEOUT: Duration = Duration::from_secs(120);
// The listing and then the catalog of deployable models each get the
// listing timeout, and both must finish inside the lease.
const _: () = assert!(2 * LISTING_TIMEOUT.as_secs() < LEASE.as_secs());
/// Most models one listing probes (D-018); the rest wait for the next.
const MAX_PROBES: i64 = 32;
/// Probes run at once on one account.
const PROBE_CONCURRENCY: usize = 4;
/// How long a listing spends probing. Probes run after the job is finished,
/// so they hold no lease, only the worker's loop.
const PROBE_BUDGET: Duration = Duration::from_secs(60);
/// A model's probe is not tried again within this long of the last
/// attempt, whatever it showed: it is billed, and a probe that showed
/// nothing yields to models not yet tried.
pub const PROBE_RETRY: Duration = Duration::from_secs(60 * 60);
/// A refusal is probed again once it is this old, so a model the provider
/// later allows is found without traffic, which a refused model no longer
/// gets.
const PROBE_RECHECK: Duration = Duration::from_secs(24 * 60 * 60);
const SCHEDULE_BATCH: i64 = 100;
/// Jobs per tick, so a backlog cannot hold the loop indefinitely. Jobs are
/// claimed one at a time, so no claim's lease runs while it waits.
const MAX_JOBS_PER_TICK: usize = 100;

/// Resolves the adapter that lists an organization's provider account.
#[async_trait]
pub trait ProviderAccounts: Send + Sync {
    /// The adapter for `adapter_instance_key`, or `None` when neither the
    /// organization nor the process has a credential for it.
    async fn adapter(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError>;

    /// The adapter for the organization's own stored credential only,
    /// never the process's: what may accept model agreements (D-035), which
    /// subscribe the account it calls. `None` (the default) when the
    /// organization stored none.
    async fn tenant_adapter(
        &self,
        _organization_id: OrganizationId,
        _adapter_instance_key: &str,
        _family: ProviderFamily,
    ) -> Result<Option<Arc<dyn InferenceProviderAdapter>>, ProviderError> {
        Ok(None)
    }
}

/// What one discovery tick did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryReport {
    pub scheduled: u64,
    pub succeeded: u64,
    pub failed: u64,
}

pub struct DiscoveryRunner {
    store: CatalogSyncStore,
    accounts: Arc<dyn ProviderAccounts>,
    listing_timeout: Duration,
    offers: Option<AutoOfferStore>,
    evidence: Option<ModelEvidenceStore>,
    /// Whether a failing listing raises a notification (and a successful
    /// one resolves it).
    notify: bool,
}

impl DiscoveryRunner {
    pub fn new(store: CatalogSyncStore, accounts: Arc<dyn ProviderAccounts>) -> Self {
        Self {
            store,
            accounts,
            listing_timeout: LISTING_TIMEOUT,
            offers: None,
            evidence: None,
            notify: false,
        }
    }

    /// Tells the organization when an account's listing fails, and resolves
    /// it once a listing succeeds.
    pub fn with_notifications(mut self) -> Self {
        self.notify = true;
        self
    }

    /// Offers the models of opted-in accounts after each listing.
    pub fn with_auto_offers(mut self, offers: AutoOfferStore) -> Self {
        self.offers = Some(offers);
        self
    }

    /// Records what a listing's control-plane checks found about calling
    /// each model (D-018) as evidence.
    pub fn with_evidence(mut self, evidence: ModelEvidenceStore) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// Abandons a listing after `timeout` rather than the default two
    /// minutes. It must stay well inside the claim lease.
    pub fn with_listing_timeout(mut self, timeout: Duration) -> Self {
        self.listing_timeout = timeout.min(LISTING_TIMEOUT);
        self
    }

    pub fn store(&self) -> &CatalogSyncStore {
        &self.store
    }

    /// Schedules due accounts, then runs claimable jobs until none remain or
    /// the batch limit is reached.
    pub async fn run_once(&self) -> Result<DiscoveryReport, sqlx::Error> {
        let now = Utc::now();
        let due_before = now - chrono::Duration::from_std(SYNC_INTERVAL).expect("interval fits");
        let mut report = DiscoveryReport {
            scheduled: self
                .store
                .schedule_due(now, due_before, SCHEDULE_BATCH)
                .await?,
            ..DiscoveryReport::default()
        };
        for _ in 0..MAX_JOBS_PER_TICK {
            let now = Utc::now();
            let lease_until = now + chrono::Duration::from_std(LEASE).expect("lease fits");
            let Some(job) = self.store.claim(now, lease_until, 1).await?.pop() else {
                break;
            };
            if self.run(&job).await? {
                report.succeeded += 1;
            } else {
                report.failed += 1;
            }
        }
        Ok(report)
    }

    /// Lists one claimed job's account and records the outcome. Returns
    /// whether the listing succeeded.
    async fn run(&self, job: &ClaimedSyncJob) -> Result<bool, sqlx::Error> {
        let listing = match ProviderFamily::from_str(&job.family) {
            // The timeout covers resolving the adapter too, which may read
            // a stored credential.
            Ok(family) => tokio::time::timeout(self.listing_timeout, self.list(job, family))
                .await
                .unwrap_or_else(|_| Err(ProviderError::new(ProviderErrorCategory::Timeout))),
            Err(_) => Err(ProviderError::new(ProviderErrorCategory::Internal)),
        };
        let now = Utc::now();
        let (status, code) = match listing {
            Ok(Some((adapter, models))) => {
                let deployable = self.list_deployable(job, adapter.as_ref()).await;
                let now = Utc::now();
                let resolve = self.notify.then(|| {
                    NotificationChange::Resolve(
                        job.organization_id,
                        NotificationKind::ProviderAccountSyncFailing,
                        job.adapter_instance_key.clone(),
                    )
                });
                let listed = self
                    .store
                    .finish_listed(job, &models, deployable.as_deref(), now, resolve.as_ref())
                    .await?;
                if listed {
                    self.record_entitlements(job, &models, now).await;
                    self.probe(job, adapter.as_ref(), &models).await;
                    // Where a call could subscribe the account, a model the
                    // listing's checks did not answer for is not newly
                    // offered either.
                    let unchecked: Vec<String> = if adapter.probes_need_entitlement() {
                        models
                            .iter()
                            .filter(|model| model.entitlement.is_none())
                            .map(|model| model.model.clone())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    self.offer(job, &unchecked).await;
                }
                return Ok(listed);
            }
            Ok(None) => (SyncStatus::Failed, ERROR_NOT_CONFIGURED),
            Err(error) if error.category() == ProviderErrorCategory::UnsupportedFeature => {
                (SyncStatus::Unsupported, error_code(error.category()))
            }
            Err(error) => {
                tracing::warn!(
                    job_id = %job.id,
                    organization_id = %job.organization_id.0,
                    adapter_instance_key = %job.adapter_instance_key,
                    category = error_code(error.category()),
                    "model discovery failed"
                );
                (SyncStatus::Failed, error_code(error.category()))
            }
        };
        // A failure raises the warning. Unsupported is no failure: listing
        // cannot work for the account as configured, so a warning from an
        // earlier failure is resolved rather than left open.
        let change = self.notify.then(|| match status {
            SyncStatus::Failed => failing_notification(job, code),
            _ => NotificationChange::Resolve(
                job.organization_id,
                NotificationKind::ProviderAccountSyncFailing,
                job.adapter_instance_key.clone(),
            ),
        });
        self.store
            .finish_unlisted(job, status, code, now, change.as_ref())
            .await?;
        Ok(false)
    }

    /// Records each listed model's entitlement, if the listing checked one.
    /// A write that fails is logged and tried again at the next listing; it
    /// never fails the listing.
    async fn record_entitlements(
        &self,
        job: &ClaimedSyncJob,
        models: &[crate::domain::discovery::DiscoveredModel],
        now: chrono::DateTime<Utc>,
    ) {
        let Some(evidence) = &self.evidence else {
            return;
        };
        for model in models {
            let Some(entitlement) = model.entitlement else {
                continue;
            };
            let observation = crate::domain::model_evidence::Observation::of_entitlement(
                crate::domain::model_evidence::EvidenceKey {
                    organization_id: job.organization_id,
                    adapter_instance_key: job.adapter_instance_key.clone(),
                    family: job.family.clone(),
                    model: model.model.clone(),
                },
                entitlement,
                now,
            );
            let result = evidence.record(&observation).await;
            let outcome = if result.is_ok() { "written" } else { "failed" };
            crate::metrics::global().inc(&crate::metrics::MODEL_EVIDENCE, &[outcome]);
            if let Err(error) = result {
                tracing::warn!(
                    organization_id = %job.organization_id.0,
                    adapter_instance_key = %job.adapter_instance_key,
                    error = %error,
                    "recording model entitlement failed"
                );
                return;
            }
        }
    }

    /// Probes the account's candidate models (see
    /// [`ModelEvidenceStore::claim_probes`]) and records what each call
    /// showed, before offers are planned. Bounded in number, concurrency and
    /// time; a credential the provider rejects or a throttle ends the
    /// round. A family that does not probe claims nothing. Never fails the listing.
    async fn probe(
        &self,
        job: &ClaimedSyncJob,
        adapter: &dyn InferenceProviderAdapter,
        models: &[crate::domain::discovery::DiscoveredModel],
    ) {
        use crate::domain::model_evidence::{Entitlement, EvidenceKey, EvidenceState, Observation};
        use futures::StreamExt;

        let Some(evidence) = &self.evidence else {
            return;
        };
        if !adapter.probes_models() {
            return;
        }
        // What this listing's checks found in the tenant's way outranks an
        // older refusal on record; a model the adapter cannot call is not
        // probed at all. Where a call could subscribe the account (Bedrock's
        // Marketplace models), a model the checks did not answer for waits
        // for a listing whose checks do.
        let need_entitlement = adapter.probes_need_entitlement();
        let blocked = models
            .iter()
            .filter(|model| {
                model
                    .entitlement
                    .is_some_and(|entitlement| entitlement != Entitlement::Entitled)
                    || (need_entitlement && model.entitlement.is_none())
                    || !adapter.probes_model(&model.model)
            })
            .map(|model| model.model.clone())
            .collect::<Vec<_>>();
        let deployed_only = models
            .iter()
            .filter(|model| adapter.probes_only_when_deployed(&model.model))
            .map(|model| model.model.clone())
            .collect::<Vec<_>>();
        let now = Utc::now();
        let ago = |duration: Duration| now - chrono::Duration::from_std(duration).expect("fits");
        let candidates = match evidence
            .claim_probes(
                job.organization_id,
                &job.adapter_instance_key,
                &job.family,
                &blocked,
                &deployed_only,
                now,
                ago(PROBE_RETRY),
                ago(PROBE_RECHECK),
                MAX_PROBES,
            )
            .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(
                    organization_id = %job.organization_id.0,
                    adapter_instance_key = %job.adapter_instance_key,
                    error = %error,
                    "choosing models to probe failed"
                );
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
        let mut probes = futures::stream::iter(candidates)
            .map(|model| async move {
                let result = adapter.probe_model(&model).await;
                (model, result)
            })
            .buffer_unordered(PROBE_CONCURRENCY);
        while let Ok(Some((model, result))) = tokio::time::timeout_at(deadline, probes.next()).await
        {
            if let Err(error) = &result
                && !crate::domain::model_evidence::is_base_model_quota(error)
            {
                // A model's own quota refusing it is about that model (see
                // `Observation::of_probe`), not a throttle on the account.
                match error.category() {
                    // Nothing was called.
                    ProviderErrorCategory::UnsupportedFeature => break,
                    // The call was made, and shows nothing about the model.
                    ProviderErrorCategory::Authentication | ProviderErrorCategory::RateLimited => {
                        crate::metrics::global()
                            .inc(&crate::metrics::MODEL_PROBES, &["inconclusive"]);
                        break;
                    }
                    _ => {}
                }
            }
            let observation = Observation::of_probe(
                EvidenceKey {
                    organization_id: job.organization_id,
                    adapter_instance_key: job.adapter_instance_key.clone(),
                    family: job.family.clone(),
                    model,
                },
                result.as_ref().map(|_| ()),
                Utc::now(),
            );
            let Some(observation) = observation else {
                crate::metrics::global().inc(&crate::metrics::MODEL_PROBES, &["inconclusive"]);
                continue;
            };
            let outcome = match evidence.record(&observation).await {
                Ok(()) if observation.verdict.state() == EvidenceState::Verified => "verified",
                Ok(()) => "refused",
                Err(error) => {
                    tracing::warn!(
                        organization_id = %job.organization_id.0,
                        adapter_instance_key = %job.adapter_instance_key,
                        error = %error,
                        "recording a probe failed"
                    );
                    "failed"
                }
            };
            crate::metrics::global().inc(&crate::metrics::MODEL_PROBES, &[outcome]);
        }
    }

    /// Offers an opted-in account's listed models. A run that cannot
    /// finish leaves the catalog as it was and is retried after the next
    /// listing; it never fails the listing.
    async fn offer(&self, job: &ClaimedSyncJob, unchecked: &[String]) {
        let (Some(offers), Ok(family)) = (&self.offers, ProviderFamily::from_str(&job.family))
        else {
            return;
        };
        match offers
            .apply(
                job.organization_id,
                &job.adapter_instance_key,
                family,
                unchecked,
                Utc::now(),
            )
            .await
        {
            Ok(ApplyOutcome::NotEnabled | ApplyOutcome::Unchanged) => {}
            Ok(ApplyOutcome::Activated {
                release_id,
                added,
                updated,
                removed,
            }) => tracing::info!(
                organization_id = %job.organization_id.0,
                adapter_instance_key = %job.adapter_instance_key,
                %release_id,
                added,
                updated,
                removed,
                "offered discovered models"
            ),
            Ok(ApplyOutcome::Failed(code)) => tracing::warn!(
                organization_id = %job.organization_id.0,
                adapter_instance_key = %job.adapter_instance_key,
                code,
                "discovered models not offered"
            ),
            Err(error) => tracing::warn!(
                organization_id = %job.organization_id.0,
                adapter_instance_key = %job.adapter_instance_key,
                error = %error,
                "offering discovered models failed"
            ),
        }
    }

    /// The account's adapter and listing, or `None` when the account has
    /// no credential.
    async fn list(&self, job: &ClaimedSyncJob, family: ProviderFamily) -> ListingResult {
        let Some(adapter) = self
            .accounts
            .adapter(job.organization_id, &job.adapter_instance_key, family)
            .await?
        else {
            return Ok(None);
        };
        let models = adapter.list_models().await?;
        Ok(Some((adapter, models)))
    }

    /// The account's catalog of deployable models where its family has one,
    /// under its own timeout. One that cannot be read in time is logged and
    /// left as it was: it never fails, nor discards, the listing.
    async fn list_deployable(
        &self,
        job: &ClaimedSyncJob,
        adapter: &dyn InferenceProviderAdapter,
    ) -> Option<Vec<crate::domain::discovery::DeployableModel>> {
        let read = tokio::time::timeout(self.listing_timeout, adapter.list_deployable_models())
            .await
            .unwrap_or_else(|_| Err(ProviderError::new(ProviderErrorCategory::Timeout)));
        match read {
            Ok(deployable) => deployable,
            Err(error) => {
                tracing::warn!(
                    job_id = %job.id,
                    organization_id = %job.organization_id.0,
                    adapter_instance_key = %job.adapter_instance_key,
                    category = error_code(error.category()),
                    "listing deployable models failed"
                );
                None
            }
        }
    }
}

/// Raised with the failed listing it reports, in the same transaction, so
/// an older failure never reopens what a newer listing resolved; best
/// effort, as evidence is: it never fails the listing's record.
fn failing_notification(job: &ClaimedSyncJob, code: &str) -> NotificationChange {
    let account = &job.adapter_instance_key;
    NotificationChange::Raise(
        job.organization_id,
        NewNotification::new(
            NotificationKind::ProviderAccountSyncFailing,
            account,
            format!("Listing {account}'s models failed"),
            format!(
                "The gateway could not list the models of {account} ({code}). What it offered \
                 before stays; check the account's credential and access. The gateway tries \
                 again."
            ),
        )
        .with_link("/admin/connections/discovery")
        .with_data(serde_json::json!({
            "adapter_instance_key": account,
            "family": job.family,
            "error_code": code,
            "sync_job_id": job.id,
        })),
    )
}
