//! Model agreements accepted through the gateway (D-035, migration 0142):
//! where each stands, and every acceptance as made.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::{OrganizationId, UserAccountId},
    model_agreement::{
        AcceptOutcome, AgreementReason, AgreementStatus, AgreementView, Rate, bounded_text,
    },
};

/// How long the worker keeps checking an agreement it was told about.
pub const CHECK_WINDOW: chrono::Duration = chrono::Duration::minutes(30);
/// How long a claimed check is the claimer's before another may take it:
/// longer than a whole batch of checks can take (see
/// `jobs::agreements::BATCH`), so a lease never runs out mid-batch.
pub const CLAIM_LEASE: chrono::Duration = chrono::Duration::minutes(10);
/// How long after an attempt was recorded as `accepting` the worker
/// settles it itself: longer than a request may spend calling the provider.
pub const SETTLE_AFTER: chrono::Duration = chrono::Duration::minutes(5);

#[derive(Clone)]
pub struct ModelAgreementStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct AgreementRow {
    model: String,
    unblocks: Vec<String>,
    offer_id: String,
    status: String,
    reason: Option<String>,
    detail: Option<String>,
    provider_code: Option<String>,
    accepted_by: Option<Uuid>,
    accepted_at: DateTime<Utc>,
    checked_at: Option<DateTime<Utc>>,
    next_check_at: Option<DateTime<Utc>>,
    probe: bool,
    verified_at: Option<DateTime<Utc>>,
    probed_model: Option<String>,
}

const AGREEMENT_COLUMNS: &str = "model, unblocks, offer_id, status, reason, detail, \
    provider_code, accepted_by, accepted_at, checked_at, next_check_at, probe, verified_at, \
    probed_model";

/// An agreement as stored, with the identifiers it was accepted for.
#[derive(Clone, Debug)]
pub struct StoredAgreement {
    pub model: String,
    pub unblocks: Vec<String>,
    pub view: AgreementView,
}

impl AgreementRow {
    fn stored(self) -> Option<StoredAgreement> {
        Some(StoredAgreement {
            view: AgreementView {
                status: AgreementStatus::parse(&self.status)?,
                reason: self.reason.as_deref().and_then(AgreementReason::parse),
                detail: self.detail,
                provider_code: self.provider_code,
                offer_id: self.offer_id,
                accepted_by: self.accepted_by,
                accepted_at: self.accepted_at,
                checked_at: self.checked_at,
                verified_at: self.verified_at,
                probe: self.probe,
                probed_model: self.probed_model,
                checking: self.next_check_at.is_some(),
            },
            model: self.model,
            unblocks: self.unblocks,
        })
    }
}

/// One model's acceptance, to record.
#[derive(Clone, Debug)]
pub struct Acceptance {
    pub model: String,
    pub unblocks: Vec<String>,
    pub offer_id: Option<String>,
    pub rate_card: Vec<Rate>,
    pub rate_card_region: Option<String>,
    pub outcome: AcceptOutcome,
    pub reason: Option<AgreementReason>,
    pub provider_code: Option<String>,
    /// Whether to probe the model once the agreement is available.
    pub probe: bool,
}

/// An agreement the worker should check now.
#[derive(Clone, Debug)]
pub struct DueAgreement {
    pub id: Uuid,
    pub organization_id: OrganizationId,
    pub adapter_instance_key: String,
    pub family: String,
    pub model: String,
    pub unblocks: Vec<String>,
    pub status: AgreementStatus,
    pub probe: bool,
    pub accepted_by: Option<UserAccountId>,
    pub check_until: Option<DateTime<Utc>>,
    /// The latest attempt, which the check's result is stored against.
    pub attempt_id: Uuid,
    /// The attempt is still `accepting`: its request never settled it.
    pub unsettled: bool,
    /// When the attempt was recorded.
    pub attempted_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct DueRow {
    id: Uuid,
    organization_id: Uuid,
    adapter_instance_key: String,
    family: String,
    model: String,
    unblocks: Vec<String>,
    status: String,
    probe: bool,
    accepted_by: Option<Uuid>,
    check_until: Option<DateTime<Utc>>,
    attempt_id: Uuid,
    attempt_outcome: Option<String>,
    attempted_at: Option<DateTime<Utc>>,
}

/// What a check found, to store.
#[derive(Clone, Debug)]
pub struct CheckResult {
    pub status: AgreementStatus,
    pub reason: Option<AgreementReason>,
    pub detail: Option<String>,
    pub provider_code: Option<String>,
    /// When to check again; `None` stops.
    pub next_check_at: Option<DateTime<Utc>>,
    pub verified_at: Option<DateTime<Utc>>,
    pub probed_model: Option<String>,
}

/// The per-model row's id: stable per organization, account, family and
/// model.
pub fn agreement_id(
    organization_id: OrganizationId,
    adapter_instance_key: &str,
    family: &str,
    model: &str,
) -> Uuid {
    crate::application::admin_identities::deterministic_target_id(&[
        "model_agreement",
        &organization_id.0.to_string(),
        adapter_instance_key,
        family,
        model,
    ])
}

/// Who and what an attempt is recorded for.
#[derive(Clone, Copy, Debug)]
pub struct AttemptContext<'a> {
    pub organization_id: OrganizationId,
    pub adapter_instance_key: &'a str,
    pub family: &'a str,
    pub actor: UserAccountId,
    pub request_id: Uuid,
}

fn audit_error(error: crate::persistence::admin::AdminStoreError) -> sqlx::Error {
    match error {
        crate::persistence::admin::AdminStoreError::Database(error) => error,
        _ => sqlx::Error::Decode("audit event".into()),
    }
}

/// Writes one audit event about agreements, naming `entity`.
async fn audit(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: AuditActor,
    action: &str,
    entity: Uuid,
    outcome: AuditOutcome,
    request_id: Uuid,
) -> Result<(), sqlx::Error> {
    crate::persistence::admin::insert_audit_on(
        conn,
        &AdminAuditEvent::new(
            actor,
            organization_id,
            None,
            AuditTargetResource::ModelAgreement,
            AuditAction::new(action.to_owned()).expect("audit action constant valid"),
            entity,
            outcome,
            request_id,
            None,
            None,
        ),
    )
    .await
    .map_err(audit_error)
}

fn audit_outcome(outcome: AcceptOutcome) -> AuditOutcome {
    match outcome {
        AcceptOutcome::Failed => AuditOutcome::Failed,
        _ => AuditOutcome::Succeeded,
    }
}

impl ModelAgreementStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The account's agreements, by model.
    pub async fn for_account(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: &str,
    ) -> Result<HashMap<String, StoredAgreement>, sqlx::Error> {
        Ok(sqlx::query_as::<_, AgreementRow>(&format!(
            "SELECT {AGREEMENT_COLUMNS} FROM gateway.model_agreement
             WHERE organization_id = $1 AND adapter_instance_key = $2 AND family = $3"
        ))
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .filter_map(AgreementRow::stored)
        .map(|stored| (stored.model.clone(), stored))
        .collect())
    }

    async fn insert_attempt(
        conn: &mut PgConnection,
        id: Uuid,
        context: AttemptContext<'_>,
        acceptance: &Acceptance,
        outcome: &str,
        settled_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let rate_card = serde_json::to_value(&acceptance.rate_card)
            .map_err(|_| sqlx::Error::Decode("rate card is not serializable".into()))?;
        sqlx::query(
            "INSERT INTO gateway.model_agreement_acceptance
                 (id, organization_id, adapter_instance_key, family, model, offer_id, rate_card,
                  outcome, reason, provider_code, actor_user_id, request_id, created_at,
                  settled_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(id)
        .bind(context.organization_id.0)
        .bind(context.adapter_instance_key)
        .bind(context.family)
        .bind(&acceptance.model)
        .bind(acceptance.offer_id.as_deref())
        .bind(&rate_card)
        .bind(outcome)
        .bind(acceptance.reason.map(AgreementReason::as_str))
        .bind(acceptance.provider_code.as_deref())
        .bind(context.actor.0)
        .bind(context.request_id)
        .bind(now)
        .bind(settled_at)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Makes `attempt` the model's latest, pending, first checked at
    /// `first_check`.
    async fn upsert_pending(
        conn: &mut PgConnection,
        context: AttemptContext<'_>,
        acceptance: &Acceptance,
        attempt: Uuid,
        first_check: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let offer = acceptance.offer_id.as_deref().unwrap_or_default();
        sqlx::query(
            "INSERT INTO gateway.model_agreement AS a
                 (id, organization_id, adapter_instance_key, family, model, unblocks, offer_id,
                  status, accepted_by, accepted_at, next_check_at, check_until, probe,
                  attempt_id, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8, $9, $10, $11, $12, $13, $9)
             ON CONFLICT (organization_id, adapter_instance_key, family, model) DO UPDATE SET
                 unblocks = EXCLUDED.unblocks, offer_id = EXCLUDED.offer_id,
                 status = 'pending', reason = NULL, detail = NULL, provider_code = NULL,
                 accepted_by = EXCLUDED.accepted_by, accepted_at = EXCLUDED.accepted_at,
                 next_check_at = EXCLUDED.next_check_at, check_until = EXCLUDED.check_until,
                 probe = EXCLUDED.probe, probed_model = NULL, attempt_id = EXCLUDED.attempt_id,
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(agreement_id(
            context.organization_id,
            context.adapter_instance_key,
            context.family,
            &acceptance.model,
        ))
        .bind(context.organization_id.0)
        .bind(context.adapter_instance_key)
        .bind(context.family)
        .bind(&acceptance.model)
        .bind(&acceptance.unblocks)
        .bind(offer.chars().take(64).collect::<String>())
        .bind(context.actor.0)
        .bind(now)
        .bind(first_check)
        .bind(first_check + CHECK_WINDOW)
        .bind(acceptance.probe)
        .bind(attempt)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Records one model's settled acceptance (one that made no agreement,
    /// or found one in place): the attempt, its audit event, and (unless it
    /// failed) the agreement for the worker to check from `now`. Returns
    /// the attempt's id.
    pub async fn record(
        conn: &mut PgConnection,
        context: AttemptContext<'_>,
        acceptance: &Acceptance,
        now: DateTime<Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        Self::insert_attempt(
            conn,
            id,
            context,
            acceptance,
            acceptance.outcome.as_str(),
            None,
            now,
        )
        .await?;
        // The event names the attempt, which holds the account, model,
        // offer and rate card: audit metadata takes only an allowlist of
        // keys (migration 0026).
        audit(
            conn,
            context.organization_id,
            AuditActor::User(context.actor),
            &format!("model_agreement.{}", acceptance.outcome.as_str()),
            id,
            audit_outcome(acceptance.outcome),
            context.request_id,
        )
        .await?;
        if acceptance.outcome != AcceptOutcome::Failed {
            Self::upsert_pending(conn, context, acceptance, id, now, now).await?;
        }
        Ok(id)
    }

    /// Records, before the provider is called, that the gateway is about to
    /// accept a model's offer: the attempt as `accepting`, audited
    /// (`model_agreement.accepting`), and the agreement pending, left for
    /// the worker to settle after [`SETTLE_AFTER`] should the request never
    /// do so. Returns the attempt's id.
    pub async fn begin_accepting(
        conn: &mut PgConnection,
        context: AttemptContext<'_>,
        acceptance: &Acceptance,
        now: DateTime<Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        Self::insert_attempt(conn, id, context, acceptance, "accepting", None, now).await?;
        audit(
            conn,
            context.organization_id,
            AuditActor::User(context.actor),
            "model_agreement.accepting",
            id,
            AuditOutcome::Succeeded,
            context.request_id,
        )
        .await?;
        Self::upsert_pending(conn, context, acceptance, id, now + SETTLE_AFTER, now).await?;
        Ok(id)
    }

    /// Settles an `accepting` attempt with what the provider answered, and
    /// audits it. `Unconfirmed` leaves it `accepting` for the worker to
    /// settle from the provider's availability, now. Nothing changes for an
    /// attempt someone already settled, nor the agreement of a newer one.
    #[allow(clippy::too_many_arguments)]
    pub async fn settle(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        attempt: Uuid,
        actor: AuditActor,
        request_id: Uuid,
        outcome: AcceptOutcome,
        reason: Option<AgreementReason>,
        provider_code: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        if outcome == AcceptOutcome::Unconfirmed {
            sqlx::query(
                "UPDATE gateway.model_agreement SET next_check_at = $3, updated_at = $3
                 WHERE organization_id = $1 AND attempt_id = $2",
            )
            .bind(organization_id.0)
            .bind(attempt)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            return Ok(true);
        }
        let settled: Option<Uuid> = sqlx::query_scalar(
            "UPDATE gateway.model_agreement_acceptance
             SET outcome = $3, reason = $4, provider_code = $5, settled_at = $6
             WHERE organization_id = $1 AND id = $2 AND outcome = 'accepting'
             RETURNING id",
        )
        .bind(organization_id.0)
        .bind(attempt)
        .bind(outcome.as_str())
        .bind(reason.map(AgreementReason::as_str))
        .bind(provider_code.map(|code| code.chars().take(128).collect::<String>()))
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        if settled.is_none() {
            return Ok(false);
        }
        audit(
            conn,
            organization_id,
            actor,
            &format!("model_agreement.{}", outcome.as_str()),
            attempt,
            audit_outcome(outcome),
            request_id,
        )
        .await?;
        let (status, next) = if outcome == AcceptOutcome::Failed {
            ("failed", None)
        } else {
            ("pending", Some(now))
        };
        sqlx::query(
            "UPDATE gateway.model_agreement SET
                 status = $3, reason = $4, detail = $5, provider_code = $6,
                 next_check_at = $7, updated_at = $8
             WHERE organization_id = $1 AND attempt_id = $2",
        )
        .bind(organization_id.0)
        .bind(attempt)
        .bind(status)
        .bind(reason.map(AgreementReason::as_str))
        .bind(reason.map(|reason| reason.detail()))
        .bind(provider_code.map(|code| code.chars().take(128).collect::<String>()))
        .bind(next)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        Ok(true)
    }

    /// Audits a refused acceptance request (`model_agreement.accept`,
    /// outcome `failed`): an offer that changed, or a role that may not
    /// read offers. Nothing was accepted.
    pub async fn audit_refusal(
        &self,
        context: AttemptContext<'_>,
        entity: Uuid,
    ) -> Result<(), sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        audit(
            &mut conn,
            context.organization_id,
            AuditActor::User(context.actor),
            "model_agreement.accept",
            entity,
            AuditOutcome::Failed,
            context.request_id,
        )
        .await
    }

    /// Settles, as the worker, an attempt its request never settled.
    pub async fn settle_unsettled(
        &self,
        agreement: &DueAgreement,
        outcome: AcceptOutcome,
        reason: Option<AgreementReason>,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let settled = Self::settle(
            &mut tx,
            agreement.organization_id,
            agreement.attempt_id,
            AuditActor::System,
            Uuid::new_v4(),
            outcome,
            reason,
            None,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(settled)
    }

    /// Claims at most `limit` agreements due a check at `now`, leasing each
    /// for [`CLAIM_LEASE`] so two workers never check one at once.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<DueAgreement>, sqlx::Error> {
        let rows: Vec<DueRow> = sqlx::query_as(
            "WITH claimed AS (
                 UPDATE gateway.model_agreement a SET next_check_at = $2
                 WHERE a.id IN (
                     SELECT id FROM gateway.model_agreement
                     WHERE next_check_at IS NOT NULL AND next_check_at <= $1
                     ORDER BY next_check_at
                     LIMIT $3
                     FOR UPDATE SKIP LOCKED)
                 RETURNING a.id, a.organization_id, a.adapter_instance_key, a.family, a.model,
                           a.unblocks, a.status, a.probe, a.accepted_by, a.check_until,
                           a.attempt_id)
             SELECT c.*, t.outcome AS attempt_outcome, t.created_at AS attempted_at
             FROM claimed c
             LEFT JOIN gateway.model_agreement_acceptance t ON t.id = c.attempt_id",
        )
        .bind(now)
        .bind(now + CLAIM_LEASE)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(DueAgreement {
                    id: row.id,
                    organization_id: OrganizationId(row.organization_id),
                    adapter_instance_key: row.adapter_instance_key,
                    family: row.family,
                    model: row.model,
                    unblocks: row.unblocks,
                    status: AgreementStatus::parse(&row.status)?,
                    probe: row.probe,
                    accepted_by: row.accepted_by.map(UserAccountId),
                    check_until: row.check_until,
                    attempt_id: row.attempt_id,
                    unsettled: row.attempt_outcome.as_deref() == Some("accepting"),
                    attempted_at: row.attempted_at.unwrap_or(now),
                })
            })
            .collect())
    }

    /// Takes the agreement's one billed probe, if its latest attempt still
    /// asks for one: true at most once per attempt, so neither a second
    /// worker nor a restart after the probe probes again.
    pub async fn take_probe(&self, agreement: &DueAgreement) -> Result<bool, sqlx::Error> {
        let taken: Option<Uuid> = sqlx::query_scalar(
            "UPDATE gateway.model_agreement SET probe = false
             WHERE id = $1 AND attempt_id = $2 AND probe
             RETURNING id",
        )
        .bind(agreement.id)
        .bind(agreement.attempt_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(taken.is_some())
    }

    /// Stores what a check of `agreement` found at `now`, unless a newer
    /// attempt replaced the one it checked. Returns whether it was stored.
    pub async fn finish_check(
        &self,
        agreement: &DueAgreement,
        result: &CheckResult,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let done = sqlx::query(
            "UPDATE gateway.model_agreement SET
                 status = $3, reason = $4, detail = $5, provider_code = $6,
                 next_check_at = $7, verified_at = COALESCE($8, verified_at),
                 probed_model = COALESCE($9, probed_model), checked_at = $10, updated_at = $10
             WHERE id = $1 AND attempt_id = $2",
        )
        .bind(agreement.id)
        .bind(agreement.attempt_id)
        .bind(result.status.as_str())
        .bind(result.reason.map(AgreementReason::as_str))
        .bind(result.detail.as_deref().map(bounded_text))
        .bind(
            result
                .provider_code
                .as_deref()
                .map(|code| code.chars().take(128).collect::<String>()),
        )
        .bind(result.next_check_at)
        .bind(result.verified_at)
        .bind(result.probed_model.as_deref())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}
