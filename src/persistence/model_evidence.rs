//! Model availability evidence storage (D-018). Data planes write what their
//! traffic shows; the admin API and automatic offers read it.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

use crate::domain::{
    identity::OrganizationId,
    model_evidence::{EvidenceState, EvidenceView, Observation},
};

#[derive(Clone)]
pub struct ModelEvidenceStore {
    pool: PgPool,
}

#[derive(FromRow)]
pub(crate) struct EvidenceRow {
    pub(crate) model: String,
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) source: String,
    pub(crate) deployment: Option<String>,
    pub(crate) provider_status: Option<i16>,
    pub(crate) provider_code: Option<String>,
    pub(crate) observed_at: DateTime<Utc>,
    pub(crate) verified_at: Option<DateTime<Utc>>,
}

impl EvidenceRow {
    /// The view, or `None` for a row whose values this build does not know
    /// (written by a newer one).
    pub(crate) fn view(self) -> Option<EvidenceView> {
        Some(EvidenceView {
            state: self.state.parse().ok()?,
            reason: match self.reason {
                Some(reason) => Some(reason.parse().ok()?),
                None => None,
            },
            source: self.source.parse().ok()?,
            deployment: self.deployment,
            provider_status: self
                .provider_status
                .and_then(|status| u16::try_from(status).ok()),
            provider_code: self.provider_code,
            observed_at: self.observed_at,
            verified_at: self.verified_at,
        })
    }
}

pub(crate) const EVIDENCE_COLUMNS: &str = "model, state, reason, source, deployment, \
    provider_status, provider_code, observed_at, verified_at";

/// Whether an incoming observation (`EXCLUDED`) replaces the recorded one
/// (`e`): it is at least as new, and a control-plane check never replaces
/// what a call showed. A call that succeeded or was refused is stronger
/// than a check that the identity holds, or lacks, a permission. The
/// exception is an organization policy that forbids the model
/// (`policy_denied`): the provider enforces it on every call, so it
/// outranks what an earlier call showed, and the model, no longer probed,
/// would otherwise keep that evidence.
const TAKES: &str = "e.observed_at <= EXCLUDED.observed_at \
    AND NOT (EXCLUDED.source = 'control_plane' AND e.state IN ('verified', 'refused') \
             AND EXCLUDED.reason IS DISTINCT FROM 'policy_denied')";

impl ModelEvidenceStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Records `observation`. Observations may arrive out of order: the
    /// newest `observed_at` sets the verdict, and a success advances
    /// `verified_at` whenever it arrives, so a refusal keeps the last one.
    /// `refused_since` marks the start of the current run of refusals.
    pub async fn record(&self, observation: &Observation) -> Result<(), sqlx::Error> {
        let state = observation.verdict.state();
        let verified_at = (state == EvidenceState::Verified).then_some(observation.observed_at);
        sqlx::query(&format!(
            "INSERT INTO gateway.catalog_model_evidence AS e
                 (organization_id, adapter_instance_key, family, model, state, reason, source,
                  deployment, provider_status, provider_code, observed_at, verified_at,
                  refused_since)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (organization_id, adapter_instance_key, family, model) DO UPDATE SET
                 state = CASE WHEN {TAKES} THEN EXCLUDED.state ELSE e.state END,
                 reason = CASE WHEN {TAKES} THEN EXCLUDED.reason ELSE e.reason END,
                 source = CASE WHEN {TAKES} THEN EXCLUDED.source ELSE e.source END,
                 deployment = CASE WHEN {TAKES} THEN EXCLUDED.deployment ELSE e.deployment END,
                 provider_status = CASE WHEN {TAKES} THEN EXCLUDED.provider_status ELSE e.provider_status END,
                 provider_code = CASE WHEN {TAKES} THEN EXCLUDED.provider_code ELSE e.provider_code END,
                 observed_at = CASE WHEN {TAKES} THEN EXCLUDED.observed_at ELSE e.observed_at END,
                 -- A refusal continues a run of refusals; anything else ends it.
                 refused_since = CASE
                     -- An older success that ended the recorded run: the
                     -- current run began after it, at the latest by the
                     -- newest refusal.
                     WHEN NOT ({TAKES}) THEN
                         CASE WHEN EXCLUDED.verified_at >= e.refused_since
                             THEN e.observed_at ELSE e.refused_since END
                     WHEN EXCLUDED.state = 'refused' AND e.state = 'refused'
                         THEN e.refused_since
                     ELSE EXCLUDED.refused_since END,
                 -- GREATEST ignores NULLs.
                 verified_at = GREATEST(e.verified_at, EXCLUDED.verified_at)"
        ))
        .bind(observation.key.organization_id.0)
        .bind(&observation.key.adapter_instance_key)
        .bind(&observation.key.family)
        .bind(&observation.key.model)
        .bind(state.as_str())
        .bind(observation.verdict.reason().map(|reason| reason.as_str()))
        .bind(observation.source.as_str())
        .bind(observation.deployment.as_deref())
        .bind(
            observation
                .provider_status
                .and_then(|status| i16::try_from(status).ok()),
        )
        .bind(observation.provider_code.as_deref())
        .bind(observation.observed_at)
        .bind(verified_at)
        .bind((state == EvidenceState::Refused).then_some(observation.observed_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The models of `organization_id` whose evidence says the account cannot
    /// call them (`refused` or `requires_action`), as
    /// `(adapter_instance_key, family, model)`, as evidence is keyed.
    /// Routing skips them where a route has another deployment (D-023).
    pub async fn unavailable(
        &self,
        organization_id: OrganizationId,
    ) -> Result<std::collections::HashSet<(String, String, String)>, sqlx::Error> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT adapter_instance_key, family, model FROM gateway.catalog_model_evidence
             WHERE organization_id = $1 AND state IN ('refused', 'requires_action')",
        )
        .bind(organization_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// The evidence for each model of one account, by model.
    pub async fn for_account(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: &str,
    ) -> Result<HashMap<String, EvidenceView>, sqlx::Error> {
        let rows: Vec<EvidenceRow> = sqlx::query_as(&format!(
            "SELECT {EVIDENCE_COLUMNS} FROM gateway.catalog_model_evidence
             WHERE organization_id = $1 AND adapter_instance_key = $2 AND family = $3"
        ))
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let model = row.model.clone();
                row.view().map(|view| (model, view))
            })
            .collect())
    }

    /// Claims one listed model's probe outside discovery (an agreement's
    /// `verify`), on the same record discovery claims with, so the two
    /// never both call it: claimed unless attempted after `retry_before`.
    /// A model discovery has not listed has nothing to share.
    pub async fn claim_probe(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: &str,
        model: &str,
        now: DateTime<Utc>,
        retry_before: DateTime<Utc>,
    ) -> Result<ProbeClaim, sqlx::Error> {
        let (listed, claimed) = sqlx::query_as::<_, (bool, bool)>(
            "WITH listed AS (
                 SELECT organization_id, adapter_instance_key, family, model
                 FROM gateway.catalog_discovered_model
                 WHERE organization_id = $1 AND adapter_instance_key = $2 AND family = $3
                   AND model = $4
             ), claimed AS (
                 INSERT INTO gateway.catalog_model_probe AS p
                     (organization_id, adapter_instance_key, family, model, attempted_at)
                 SELECT organization_id, adapter_instance_key, family, model, $5 FROM listed
                 ON CONFLICT (organization_id, adapter_instance_key, family, model)
                 DO UPDATE SET attempted_at = EXCLUDED.attempted_at
                 WHERE p.attempted_at <= $6
                 RETURNING 1
             )
             SELECT EXISTS (SELECT 1 FROM listed), EXISTS (SELECT 1 FROM claimed)",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .bind(model)
        .bind(now)
        .bind(retry_before)
        .fetch_one(&self.pool)
        .await?;
        Ok(match (listed, claimed) {
            (false, _) => ProbeClaim::Unlisted,
            (true, true) => ProbeClaim::Claimed,
            (true, false) => ProbeClaim::Busy,
        })
    }

    /// Claims the listed models of one account that discovery should probe
    /// next (D-018), at most `limit`, by recording the attempt at `now`. A
    /// model attempted after `retry_before` is not claimed, so two workers
    /// never probe one model at once, and one whose probe showed nothing
    /// waits its turn. Only models the account would serve are probed:
    /// deployed in the active release, or every listed model when the
    /// account has automatic offers, except `deployed_only` (models whose
    /// probe costs the tenant), which only a release's deployment makes
    /// candidates. Of those, each that no call has
    /// settled (no evidence, or a check that found nothing in the way),
    /// then refusals last observed before `recheck_before`, so a model the
    /// provider later allows is found; within each, those a release deploys
    /// and those tried longest ago first. Never one of `blocked` (the
    /// current listing's checks found something in the tenant's way), one
    /// needing action, missing from the listing, or callable only through a
    /// profile.
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_probes(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: &str,
        blocked: &[String],
        deployed_only: &[String],
        now: DateTime<Utc>,
        retry_before: DateTime<Utc>,
        recheck_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "WITH candidate AS (
                 SELECT m.model, e.state, p.attempted_at,
                        EXISTS (
                            SELECT 1
                            FROM gateway.catalog_active_release a
                            JOIN gateway.catalog_deployment d
                              ON d.organization_id = a.organization_id
                             AND d.release_id = a.release_id
                            JOIN gateway.catalog_provider_account pa
                              ON pa.organization_id = d.organization_id
                             AND pa.release_id = d.release_id AND pa.id = d.provider_account_id
                            WHERE a.organization_id = m.organization_id
                              AND d.adapter_instance_key = m.adapter_instance_key
                              AND pa.family = m.family AND d.model = m.model
                        ) AS deployed
                 FROM gateway.catalog_discovered_model m
                 LEFT JOIN gateway.catalog_model_evidence e
                   ON e.organization_id = m.organization_id
                  AND e.adapter_instance_key = m.adapter_instance_key
                  AND e.family = m.family
                  AND e.model = m.model
                 LEFT JOIN gateway.catalog_model_probe p
                   ON p.organization_id = m.organization_id
                  AND p.adapter_instance_key = m.adapter_instance_key
                  AND p.family = m.family
                  AND p.model = m.model
                 WHERE m.organization_id = $1 AND m.adapter_instance_key = $2
                   AND m.family = $3
                   AND m.missing_since IS NULL
                   AND m.availability NOT IN ('profile_only', 'endpoint_only')
                   AND NOT (m.model = ANY($4))
                   AND (p.attempted_at IS NULL OR p.attempted_at < $6)
                   AND (e.state IS NULL OR e.state = 'entitled'
                        OR (e.state = 'refused' AND e.observed_at < $7))
             ), chosen AS (
                 SELECT model FROM candidate
                 WHERE deployed OR (NOT (model = ANY($9)) AND EXISTS (
                     SELECT 1 FROM gateway.catalog_auto_offer o
                     WHERE o.organization_id = $1 AND o.adapter_instance_key = $2
                 ))
                 ORDER BY state = 'refused', deployed DESC,
                          attempted_at NULLS FIRST, model
                 LIMIT $8
             )
             INSERT INTO gateway.catalog_model_probe AS p
                 (organization_id, adapter_instance_key, family, model, attempted_at)
             SELECT $1, $2, $3, model, $5 FROM chosen
             ON CONFLICT (organization_id, adapter_instance_key, family, model)
             DO UPDATE SET attempted_at = EXCLUDED.attempted_at
             -- Another worker's claim since this one read wins.
             WHERE p.attempted_at < $6
             RETURNING p.model",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .bind(blocked)
        .bind(now)
        .bind(retry_before)
        .bind(recheck_before)
        .bind(limit)
        .bind(deployed_only)
        .fetch_all(&self.pool)
        .await
    }
}

/// What [`ModelEvidenceStore::claim_probe`] found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeClaim {
    /// Claimed: this caller probes it.
    Claimed,
    /// Attempted recently (by discovery or another check): not probed now.
    Busy,
    /// Not listed by discovery, which therefore cannot probe it either.
    Unlisted,
}
