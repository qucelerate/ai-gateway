//! Checks the model agreements administrators accepted (D-035) until the
//! provider makes them available, then records what that means.
//!
//! Each accepted agreement is checked with the provider's free availability
//! check (Bedrock's `GetFoundationModelAvailability`) every
//! [`CHECK_INTERVAL`] for up to the store's check window, and only through
//! the organization's own credential. An attempt its request never settled
//! is settled from the same check. A check stores its result only while the
//! attempt it checked is still the model's latest. Once it is
//! available, the model's listed identifiers are recorded as entitled
//! evidence and a discovery run is queued for the account, so automatic
//! offers offer them. Only when the administrator asked for it is the model
//! then called once (a 1-token probe, billed by the provider, one per
//! foundation model): a success records `verified`, a refusal `refused`
//! with `access_request_required` (the account still lacks access only the
//! provider grants). Without the probe the agreement stays `available`:
//! available per the provider, not yet called.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    domain::{
        catalog::ProviderFamily,
        model_agreement::{
            AcceptOutcome, AgreementReason, AgreementStatus, ProviderAgreementStatus,
        },
        model_evidence::{Entitlement, EvidenceKey, EvidenceReason, Observation, Verdict},
    },
    jobs::discovery::ProviderAccounts,
    persistence::{
        catalog_sync::CatalogSyncStore,
        model_agreement::{CheckResult, DueAgreement, ModelAgreementStore, SETTLE_AFTER},
        model_evidence::{ModelEvidenceStore, ProbeClaim},
    },
};

/// How often a pending agreement is checked.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(20);
/// How long to wait after a check that failed (the provider, or reaching
/// the account).
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// Agreements checked per tick, one after another: few enough that the
/// whole batch (each at most [`AVAILABILITY_TIMEOUT`] plus
/// [`PROBE_TIMEOUT`]) ends well within the store's claim lease, so no
/// other worker re-claims one mid-batch.
pub const BATCH: i64 = 5;
/// The longest one availability check may take.
const AVAILABILITY_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest one probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// What one tick did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AgreementReport {
    pub checked: u64,
    pub settled: u64,
}

pub struct AgreementRunner {
    store: ModelAgreementStore,
    evidence: ModelEvidenceStore,
    pool: sqlx::PgPool,
    accounts: Arc<dyn ProviderAccounts>,
}

impl AgreementRunner {
    pub fn new(pool: sqlx::PgPool, accounts: Arc<dyn ProviderAccounts>) -> Self {
        Self {
            store: ModelAgreementStore::new(pool.clone()),
            evidence: ModelEvidenceStore::new(pool.clone()),
            pool,
            accounts,
        }
    }

    /// Checks every agreement due at `now`, a batch at most.
    pub async fn run_once(&self, now: DateTime<Utc>) -> Result<AgreementReport, sqlx::Error> {
        let due = self.store.claim_due(now, BATCH).await?;
        let mut report = AgreementReport::default();
        for agreement in due {
            let result = self.check(&agreement, now).await;
            let status = result.status.as_str();
            crate::metrics::global().inc(&crate::metrics::MODEL_AGREEMENT_CHECKS, &[status]);
            report.checked += 1;
            // A newer acceptance replaced the attempt this checked: its own
            // check stands.
            if self.store.finish_check(&agreement, &result, now).await?
                && result.next_check_at.is_none()
            {
                report.settled += 1;
            }
        }
        Ok(report)
    }

    async fn check(&self, agreement: &DueAgreement, now: DateTime<Utc>) -> CheckResult {
        let later = |interval: Duration| {
            now + chrono::Duration::from_std(interval).expect("an interval fits")
        };
        let expired = agreement.check_until.is_none_or(|until| now >= until);
        // The check could not be made: try again later, until the window
        // closes.
        let retry = |reason: AgreementReason, code: Option<String>| CheckResult {
            status: agreement.status,
            reason: expired.then_some(reason),
            detail: expired.then(|| reason.detail().to_owned()),
            provider_code: code,
            next_check_at: (!expired).then(|| later(RETRY_INTERVAL)),
            verified_at: None,
            probed_model: None,
        };
        let Ok(family) = agreement.family.parse::<ProviderFamily>() else {
            return retry(AgreementReason::ProviderError, None);
        };
        // The organization's own credential only (never the operator's).
        let adapter = match self
            .accounts
            .tenant_adapter(
                agreement.organization_id,
                &agreement.adapter_instance_key,
                family,
            )
            .await
        {
            Ok(Some(adapter)) => adapter,
            _ => return retry(AgreementReason::ProviderError, None),
        };
        let Some(agreements) = adapter.model_agreements() else {
            return retry(AgreementReason::ProviderError, None);
        };
        let availability = match tokio::time::timeout(
            AVAILABILITY_TIMEOUT,
            agreements.agreement_availability(&agreement.model),
        )
        .await
        {
            Ok(Ok(Some(availability))) => availability,
            Ok(Ok(None)) | Err(_) => return retry(AgreementReason::ProviderError, None),
            Ok(Err(error)) => {
                return retry(
                    AgreementReason::ProviderError,
                    error.error_type().map(str::to_owned),
                );
            }
        };
        // An attempt its request never settled (the client left, or the
        // call did not answer): AWS's availability says whether the
        // agreement was made.
        if agreement.unsettled {
            let made = matches!(
                availability.agreement,
                ProviderAgreementStatus::Available | ProviderAgreementStatus::Pending
            );
            if made {
                if let Err(error) = self
                    .store
                    .settle_unsettled(agreement, AcceptOutcome::Accepted, None, now)
                    .await
                {
                    tracing::warn!(error = %error, "settling an agreement attempt failed");
                    return retry(AgreementReason::ProviderError, None);
                }
            } else if now < agreement.attempted_at + SETTLE_AFTER {
                return CheckResult {
                    status: AgreementStatus::Pending,
                    reason: None,
                    detail: None,
                    provider_code: None,
                    next_check_at: Some(later(CHECK_INTERVAL)),
                    verified_at: None,
                    probed_model: None,
                };
            } else {
                let reason = AgreementReason::AcceptanceInterrupted;
                if let Err(error) = self
                    .store
                    .settle_unsettled(agreement, AcceptOutcome::Failed, Some(reason), now)
                    .await
                {
                    tracing::warn!(error = %error, "settling an agreement attempt failed");
                    return retry(AgreementReason::ProviderError, None);
                }
                return CheckResult {
                    status: AgreementStatus::Failed,
                    reason: Some(reason),
                    detail: Some(reason.detail().to_owned()),
                    provider_code: None,
                    next_check_at: None,
                    verified_at: None,
                    probed_model: None,
                };
            }
        }
        let stop = |status: AgreementStatus, reason: Option<AgreementReason>| CheckResult {
            status,
            reason,
            detail: reason.map(|reason| reason.detail().to_owned()),
            provider_code: None,
            next_check_at: None,
            verified_at: None,
            probed_model: None,
        };
        match availability.agreement {
            ProviderAgreementStatus::Error => {
                return CheckResult {
                    detail: Some(match availability.error_message {
                        Some(message) => {
                            format!("{} {message}", AgreementReason::AgreementError.detail())
                        }
                        None => AgreementReason::AgreementError.detail().to_owned(),
                    }),
                    ..stop(
                        AgreementStatus::Failed,
                        Some(AgreementReason::AgreementError),
                    )
                };
            }
            ProviderAgreementStatus::Pending | ProviderAgreementStatus::NotAvailable
                if !expired =>
            {
                return CheckResult {
                    next_check_at: Some(later(CHECK_INTERVAL)),
                    ..stop(AgreementStatus::Pending, None)
                };
            }
            ProviderAgreementStatus::Pending => {
                return stop(
                    AgreementStatus::Pending,
                    Some(AgreementReason::StillPending),
                );
            }
            ProviderAgreementStatus::NotAvailable => {
                return stop(
                    AgreementStatus::Failed,
                    Some(AgreementReason::AgreementNotAvailable),
                );
            }
            ProviderAgreementStatus::Available => {}
        }
        // The agreement is in place. Whether the account may call the model
        // is the whole answer's, by discovery's rule: the role's
        // authorization, the account's entitlement and the region as well.
        // Only then is the model recorded entitled; otherwise what is in the
        // way is recorded, and the agreement says so.
        let key = |model: &str| EvidenceKey {
            organization_id: agreement.organization_id,
            adapter_instance_key: agreement.adapter_instance_key.clone(),
            family: agreement.family.clone(),
            model: model.to_owned(),
        };
        let mut ids = agreement.unblocks.clone();
        if !ids.contains(&agreement.model) {
            ids.push(agreement.model.clone());
        }
        let blocked = match availability.entitlement {
            Some(Entitlement::Entitled) => None,
            Some(Entitlement::PermissionMissing) => Some(AgreementReason::InvokePermissionMissing),
            Some(_) => Some(AgreementReason::AccessRequestRequired),
            // Not every field known (or the region does not serve it): what
            // is known to be in the way still is, and nothing is recorded.
            None if availability.authorized == Some(false) => {
                Some(AgreementReason::InvokePermissionMissing)
            }
            None if availability.entitled == Some(false) => {
                Some(AgreementReason::AccessRequestRequired)
            }
            None => None,
        };
        if let Some(entitlement) = availability.entitlement {
            for id in &ids {
                let observation = Observation::of_entitlement(key(id), entitlement, now);
                if let Err(error) = self.evidence.record(&observation).await {
                    tracing::warn!(error = %error, "recording an agreement's evidence failed");
                }
            }
        }
        if let Some(reason) = blocked {
            self.queue_discovery(agreement).await;
            return stop(AgreementStatus::Refused, Some(reason));
        }
        if availability.region_available == Some(false) {
            return stop(AgreementStatus::Refused, Some(AgreementReason::NotFound));
        }
        // The one billed probe, taken first so neither another worker nor a
        // restart after the call probes again, and only if the latest
        // attempt asked for it.
        let mut probe = agreement.probe
            && match self.store.take_probe(agreement).await {
                Ok(taken) => taken,
                Err(error) => {
                    tracing::warn!(error = %error, "taking an agreement's probe failed");
                    false
                }
            };
        // One billed call per foundation model, through the identifier most
        // likely to answer: a global profile, another profile, the model.
        let probed = probe_target(agreement);
        // Claimed on the record discovery claims with, before discovery is
        // queued: one probe of an identifier, whichever path makes it. One
        // discovery attempted within its retry window is not probed again:
        // that probe's evidence stands for it.
        if probe {
            let retry_before = now
                - chrono::Duration::from_std(crate::jobs::discovery::PROBE_RETRY).expect("fits");
            probe = match self
                .evidence
                .claim_probe(
                    agreement.organization_id,
                    &agreement.adapter_instance_key,
                    &agreement.family,
                    &probed,
                    now,
                    retry_before,
                )
                .await
            {
                Ok(claim) => claim != ProbeClaim::Busy,
                Err(error) => {
                    tracing::warn!(error = %error, "claiming an agreement's probe failed");
                    false
                }
            };
        }
        self.queue_discovery(agreement).await;
        if !probe {
            return stop(
                AgreementStatus::Available,
                Some(AgreementReason::NotVerified),
            );
        }
        let result = match tokio::time::timeout(PROBE_TIMEOUT, adapter.probe_model(&probed)).await {
            Ok(result) => result,
            Err(_) => {
                return CheckResult {
                    probed_model: Some(probed),
                    ..stop(
                        AgreementStatus::Available,
                        Some(AgreementReason::NotVerified),
                    )
                };
            }
        };
        let observation = Observation::of_probe(key(&probed), result.as_ref().map(|_| ()), now);
        if let Some(observation) = &observation
            && let Err(error) = self.evidence.record(observation).await
        {
            tracing::warn!(error = %error, "recording an agreement's probe failed");
        }
        let code = result
            .as_ref()
            .err()
            .and_then(|error| error.error_type())
            .map(str::to_owned);
        match observation.map(|observation| observation.verdict) {
            Some(Verdict::Verified) => CheckResult {
                verified_at: Some(now),
                probed_model: Some(probed),
                ..stop(AgreementStatus::Verified, None)
            },
            Some(Verdict::Refused(EvidenceReason::NotFound)) => CheckResult {
                provider_code: code,
                probed_model: Some(probed),
                ..stop(AgreementStatus::Refused, Some(AgreementReason::NotFound))
            },
            // Refused after the agreement is in place: AWS has not enabled
            // the model for the account.
            Some(Verdict::Refused(_) | Verdict::RequiresAction(_)) => CheckResult {
                provider_code: code,
                probed_model: Some(probed),
                ..stop(
                    AgreementStatus::Refused,
                    Some(AgreementReason::AccessRequestRequired),
                )
            },
            Some(Verdict::Entitled) | None => CheckResult {
                provider_code: code,
                probed_model: Some(probed),
                ..stop(
                    AgreementStatus::Available,
                    Some(AgreementReason::NotVerified),
                )
            },
        }
    }

    /// Queues a discovery run for the account, so its listing records the
    /// model's identifiers and automatic offers offer them; an account with
    /// a run already queued keeps it.
    async fn queue_discovery(&self, agreement: &DueAgreement) {
        let Some(actor) = agreement.accepted_by else {
            return;
        };
        let result = async {
            let mut conn = self.pool.acquire().await?;
            CatalogSyncStore::insert_manual(
                &mut conn,
                agreement.organization_id,
                Uuid::new_v4(),
                &agreement.adapter_instance_key,
                &agreement.family,
                actor,
            )
            .await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(error = %error, "queueing discovery after an agreement failed");
        }
    }
}

/// The identifier a verification probe calls: a global inference profile,
/// else another profile, else the foundation model itself (many newer
/// models answer only through a profile).
pub fn probe_target(agreement: &DueAgreement) -> String {
    let profiles: Vec<&String> = agreement
        .unblocks
        .iter()
        .filter(|id| **id != agreement.model)
        .collect();
    profiles
        .iter()
        .find(|id| id.starts_with("global."))
        .or_else(|| profiles.first())
        .map(|id| (**id).clone())
        .unwrap_or_else(|| agreement.model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::identity::OrganizationId;

    fn due(model: &str, unblocks: &[&str]) -> DueAgreement {
        DueAgreement {
            id: Uuid::new_v4(),
            organization_id: OrganizationId::new(),
            adapter_instance_key: "bedrock".into(),
            family: "bedrock".into(),
            model: model.into(),
            unblocks: unblocks.iter().map(|id| (*id).to_owned()).collect(),
            status: AgreementStatus::Pending,
            probe: true,
            accepted_by: None,
            check_until: None,
            attempt_id: Uuid::new_v4(),
            unsettled: false,
            attempted_at: Utc::now(),
        }
    }

    #[test]
    fn probes_go_through_a_global_profile_first() {
        let base = "anthropic.claude-haiku-5-5";
        assert_eq!(
            probe_target(&due(
                base,
                &[
                    base,
                    "global.anthropic.claude-haiku-5-5",
                    "us.anthropic.claude-haiku-5-5"
                ]
            )),
            "global.anthropic.claude-haiku-5-5"
        );
        assert_eq!(
            probe_target(&due(base, &[base, "us.anthropic.claude-haiku-5-5"])),
            "us.anthropic.claude-haiku-5-5"
        );
        assert_eq!(probe_target(&due(base, &[base])), base);
        assert_eq!(probe_target(&due(base, &[])), base);
    }
}
