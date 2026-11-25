//! Automatic price adoption (D-024): the organization's policy, the prices
//! held for an admin, and the run that adopts published prices.
//!
//! A run holds the catalog lock (as every release and price mapping write
//! does), then reads the drift report, so what it compares is what is
//! billed until it commits. It
//! adopts what the planner adopts in one system release, records each
//! adopted price, and keeps the open holds in step: a new proposal replaces
//! the hold it supersedes, and a hold whose price the catalog now bills (an
//! admin adopted it) or that no longer differs is resolved.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::notifications::{MAX_SUBJECT_CHARS, NewNotification, NotificationKind};
use crate::domain::{
    audit::AuditTargetResource,
    catalog::CatalogManifest,
    identity::{OrganizationId, UserAccountId},
    price_adoption::{
        DimensionChange, HeldPrice, HoldReason, HoldStatus, PriceHoldView, PricePolicyInput,
        PricePolicyView, ProposedPrice, plan,
    },
    pricing::PriceDimension,
};
use crate::persistence::{
    auto_offer::{SystemRelease, activate_system_release, audit},
    catalog::lock_catalog,
    catalog_admin::CatalogAdminStore,
    notifications::NotificationStore,
    price_sync::PriceSyncStore,
};

const PRICING_LINK: &str = "/admin/models/pricing";

/// What one organization's run did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdoptionOutcome {
    /// The organization turned adoption off, or has no active release.
    NotEnabled,
    /// The active release changed while the report was read.
    Stale,
    /// The release would not validate or would break a price override.
    Failed(&'static str),
    Unchanged {
        held: usize,
    },
    Activated {
        release_id: Uuid,
        adopted: usize,
        held: usize,
    },
}

pub const ERROR_INVALID_RELEASE: &str = "invalid_release";
pub const ERROR_INCOMPATIBLE_PRICE_OVERRIDE: &str = "incompatible_price_override";

#[derive(Clone)]
pub struct PriceAdoptionStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct PolicyRow {
    auto_adopt: bool,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
}

const POLICY_SELECT: &str = "SELECT p.auto_adopt, v.version, p.updated_at
    FROM gateway.catalog_price_policy p
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = p.organization_id
     AND v.resource_type = 'catalog_price_policy' AND v.resource_id = p.id
    WHERE p.organization_id = $1";

impl From<PolicyRow> for PricePolicyView {
    fn from(row: PolicyRow) -> Self {
        Self {
            auto_adopt: row.auto_adopt,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: Some(row.updated_at),
        }
    }
}

#[derive(FromRow)]
struct HoldRow {
    id: Uuid,
    deployment: String,
    operation: String,
    model: String,
    currency: String,
    source: String,
    reason: String,
    changes: serde_json::Value,
    status: String,
    release_id: Uuid,
    created_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    version: Option<i64>,
}

const HOLD_SELECT: &str = "SELECT h.id, h.deployment, h.operation, h.model, h.currency,
        h.source, h.reason, h.changes, h.status, h.release_id, h.created_at,
        h.resolved_at, v.version
    FROM gateway.catalog_price_hold h
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = h.organization_id
     AND v.resource_type = 'catalog_price_hold' AND v.resource_id = h.id";

impl TryFrom<HoldRow> for PriceHoldView {
    type Error = sqlx::Error;

    fn try_from(row: HoldRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            deployment: row.deployment,
            operation: row.operation,
            model: row.model,
            currency: row.currency,
            source: row.source,
            reason: HoldReason::from_str(&row.reason).map_err(|()| decode("hold reason"))?,
            changes: serde_json::from_value(row.changes).map_err(|_| decode("hold changes"))?,
            status: HoldStatus::from_str(&row.status).map_err(|()| decode("hold status"))?,
            release_id: row.release_id,
            created_at: row.created_at,
            resolved_at: row.resolved_at,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
        })
    }
}

fn decode(what: &'static str) -> sqlx::Error {
    sqlx::Error::Decode(format!("invalid stored {what}").into())
}

impl PriceAdoptionStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's policy; one never set adopts.
    pub async fn policy(
        &self,
        organization_id: OrganizationId,
    ) -> Result<PricePolicyView, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map_or_else(PricePolicyView::unset, Into::into))
    }

    /// The policy on the caller's connection, so a mutation reads what its
    /// own transaction wrote.
    pub async fn policy_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<PricePolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row.map(Into::into))
    }

    /// Insert or replace the policy. Returns true when a row was created.
    pub async fn upsert_policy(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        input: &PricePolicyInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.catalog_price_policy
                 (organization_id, id, auto_adopt, updated_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (organization_id) DO UPDATE
             SET auto_adopt = EXCLUDED.auto_adopt,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(input.auto_adopt)
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }

    /// The organization's holds, newest first; only the open ones unless
    /// `all`.
    pub async fn holds(
        &self,
        organization_id: OrganizationId,
        all: bool,
    ) -> Result<Vec<PriceHoldView>, sqlx::Error> {
        sqlx::query_as::<_, HoldRow>(&format!(
            "{HOLD_SELECT}
             WHERE h.organization_id = $1 AND ($2 OR h.status = 'held')
             -- Open holds, then dismissed ones, first: resolved ones
             -- accumulate and must not crowd them out.
             ORDER BY h.status = 'held' DESC, h.status = 'dismissed' DESC,
                      h.created_at DESC, h.id
             LIMIT 500"
        ))
        .bind(organization_id.0)
        .bind(all)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(PriceHoldView::try_from)
        .collect()
    }

    pub async fn hold_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<PriceHoldView>, sqlx::Error> {
        sqlx::query_as::<_, HoldRow>(&format!(
            "{HOLD_SELECT} WHERE h.organization_id = $1 AND h.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        .map(PriceHoldView::try_from)
        .transpose()
    }

    /// Dismisses an open hold; `false` when it is not open.
    pub async fn dismiss(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        let dismissed = sqlx::query_as::<_, (String, String)>(
            "UPDATE gateway.catalog_price_hold
             SET status = 'dismissed', resolved_at = CURRENT_TIMESTAMP, resolved_by = $3
             WHERE organization_id = $1 AND id = $2 AND status = 'held'
             RETURNING deployment, operation",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(actor.0)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((deployment, operation)) = dismissed else {
            return Ok(false);
        };
        // The admin acted on it.
        NotificationStore::resolve_on(
            conn,
            organization_id,
            NotificationKind::PriceHeld,
            &hold_subject(&deployment, &operation),
            Utc::now(),
        )
        .await?;
        Ok(true)
    }

    /// Organizations whose prices follow their sources: not suspended (or
    /// deleted), with an active release, and not turned off.
    pub async fn organizations(&self) -> Result<Vec<OrganizationId>, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT o.id FROM gateway.organization o
             JOIN gateway.catalog_active_release a ON a.organization_id = o.id
             LEFT JOIN gateway.catalog_price_policy p ON p.organization_id = o.id
             WHERE o.suspended_at IS NULL AND COALESCE(p.auto_adopt, true)
             ORDER BY o.id",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(OrganizationId)
        .collect())
    }

    /// Adopts the organization's differing published prices.
    pub async fn apply(
        &self,
        organization_id: OrganizationId,
        now: DateTime<Utc>,
    ) -> Result<AdoptionOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_catalog(&mut tx, organization_id).await?;
        // Read once the lock is held: mappings and activations change under
        // it, so the comparison is of what is billed until this commits.
        let Some(drift) = PriceSyncStore::new(self.pool.clone())
            .drift(organization_id)
            .await?
        else {
            return Ok(AdoptionOutcome::NotEnabled);
        };
        let enabled = sqlx::query_scalar::<_, bool>(
            "SELECT o.suspended_at IS NULL AND COALESCE(p.auto_adopt, true)
             FROM gateway.organization o
             LEFT JOIN gateway.catalog_price_policy p ON p.organization_id = o.id
             WHERE o.id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !enabled {
            return Ok(AdoptionOutcome::NotEnabled);
        }
        let active = sqlx::query_scalar::<_, Uuid>(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        if active != Some(drift.release_id) {
            return Ok(AdoptionOutcome::Stale);
        }
        // A sealed release never changes, so it may be read outside the
        // transaction.
        let manifest = CatalogAdminStore::new(self.pool.clone())
            .release_manifest(organization_id, drift.release_id)
            .await
            .map_err(|_| decode("active release manifest"))?
            .ok_or_else(|| decode("active release manifest"))?;
        let dismissed: HashSet<String> = sqlx::query_scalar(
            "SELECT fingerprint FROM gateway.catalog_price_hold
             WHERE organization_id = $1 AND status = 'dismissed'",
        )
        .bind(organization_id.0)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let planned = plan(&manifest, &drift, &dismissed, now);

        let mut outcome = AdoptionOutcome::Unchanged {
            held: planned.held.len(),
        };
        let mut billed = &manifest;
        // The release in effect once this commits, which holds are against.
        let mut billed_release = drift.release_id;
        if let Some(next) = &planned.manifest {
            let release_id = match activate_system_release(&mut tx, organization_id, next).await? {
                SystemRelease::Activated(release_id) => release_id,
                SystemRelease::Invalid => {
                    return Ok(AdoptionOutcome::Failed(ERROR_INVALID_RELEASE));
                }
                SystemRelease::IncompatiblePriceOverride => {
                    tx.rollback().await?;
                    return Ok(AdoptionOutcome::Failed(ERROR_INCOMPATIBLE_PRICE_OVERRIDE));
                }
            };
            audit(
                &mut tx,
                organization_id,
                AuditTargetResource::CatalogRelease,
                "catalog_release.prices_adopted",
                release_id,
                Uuid::new_v4(),
                None,
                None,
            )
            .await?;
            for adopted in &planned.adopted {
                record_adoption(
                    &mut tx,
                    organization_id,
                    release_id,
                    drift.release_id,
                    adopted,
                )
                .await?;
            }
            NotificationStore::raise_on(
                &mut tx,
                organization_id,
                adopted_notification(release_id, drift.release_id, &planned.adopted),
                now,
            )
            .await?;
            billed = next;
            billed_release = release_id;
            outcome = AdoptionOutcome::Activated {
                release_id,
                adopted: planned.adopted.len(),
                held: planned.held.len(),
            };
        }
        sync_holds(
            &mut tx,
            organization_id,
            billed_release,
            &planned.held,
            billed,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(outcome)
    }
}

async fn record_adoption(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    release_id: Uuid,
    previous_release_id: Uuid,
    adopted: &ProposedPrice,
) -> Result<(), sqlx::Error> {
    let changes: Vec<serde_json::Value> = adopted
        .changes
        .iter()
        .map(|change| {
            serde_json::json!({
                "dimension": change.dimension,
                "before": change.catalog,
                "after": change.proposed,
                "observed": change.observed,
            })
        })
        .collect();
    sqlx::query(
        "INSERT INTO gateway.catalog_price_adoption
             (id, organization_id, release_id, previous_release_id, deployment, operation,
              model, currency, source, snapshot_id, item, changes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id.0)
    .bind(release_id)
    .bind(previous_release_id)
    .bind(&adopted.deployment)
    .bind(&adopted.operation)
    .bind(&adopted.model)
    .bind(&adopted.currency)
    .bind(&adopted.source)
    .bind(adopted.snapshot_id)
    .bind(&adopted.item)
    .bind(serde_json::Value::Array(changes))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Keeps the open holds in step with what this run held: an unchanged
/// proposal keeps its hold, a new one replaces the one it supersedes, and
/// a hold no longer proposed is resolved, as adopted when the catalog now
/// bills what it held.
async fn sync_holds(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    release_id: Uuid,
    held: &[HeldPrice],
    billed: &CatalogManifest,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let open = sqlx::query_as::<_, (Uuid, String, String, String, serde_json::Value)>(
        "SELECT id, deployment, operation, fingerprint, changes
         FROM gateway.catalog_price_hold
         WHERE organization_id = $1 AND status = 'held' FOR UPDATE",
    )
    .bind(organization_id.0)
    .fetch_all(&mut *conn)
    .await?;
    let proposed: HashMap<(&str, &str), &HeldPrice> = held
        .iter()
        .map(|hold| {
            (
                (
                    hold.proposal.deployment.as_str(),
                    hold.proposal.operation.as_str(),
                ),
                hold,
            )
        })
        .collect();
    // Still-proposed holds, by deployment and operation, with their ids.
    let mut kept = HashMap::new();
    for (id, deployment, operation, fingerprint, changes) in &open {
        if let Some(hold) = proposed.get(&(deployment.as_str(), operation.as_str()))
            && hold.fingerprint == *fingerprint
        {
            // Still proposed: now against the release in effect.
            sqlx::query("UPDATE gateway.catalog_price_hold SET release_id = $2 WHERE id = $1")
                .bind(id)
                .bind(release_id)
                .execute(&mut *conn)
                .await?;
            kept.insert((deployment.clone(), operation.clone()), *id);
            continue;
        }
        let changes: Vec<DimensionChange> =
            serde_json::from_value(changes.clone()).map_err(|_| decode("hold changes"))?;
        let status = if bills(billed, deployment, operation, &changes) {
            HoldStatus::Adopted
        } else {
            HoldStatus::Superseded
        };
        sqlx::query(
            "UPDATE gateway.catalog_price_hold
             SET status = $2, resolved_at = CURRENT_TIMESTAMP
             WHERE id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .execute(&mut *conn)
        .await?;
        // A replacing hold raises the same subject again below, updating the
        // open notification rather than resolving it and raising another.
        if !proposed.contains_key(&(deployment.as_str(), operation.as_str())) {
            NotificationStore::resolve_on(
                conn,
                organization_id,
                NotificationKind::PriceHeld,
                &hold_subject(deployment, operation),
                now,
            )
            .await?;
        }
    }
    for hold in held {
        let key = (
            hold.proposal.deployment.clone(),
            hold.proposal.operation.clone(),
        );
        if let Some(hold_id) = kept.get(&key) {
            // Raised again, so a hold from before notifications existed has
            // one too; an open one keeps its identity and read state.
            NotificationStore::raise_on(
                conn,
                organization_id,
                held_notification(hold, *hold_id),
                now,
            )
            .await?;
            continue;
        }
        let changes =
            serde_json::to_value(&hold.proposal.changes).map_err(|_| decode("hold changes"))?;
        let hold_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.catalog_price_hold
                 (id, organization_id, release_id, deployment, operation, model, currency,
                  source, snapshot_id, reason, changes, fingerprint)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(hold_id)
        .bind(organization_id.0)
        .bind(release_id)
        .bind(&hold.proposal.deployment)
        .bind(&hold.proposal.operation)
        .bind(&hold.proposal.model)
        .bind(&hold.proposal.currency)
        .bind(&hold.proposal.source)
        .bind(hold.proposal.snapshot_id)
        .bind(hold.reason.as_str())
        .bind(changes)
        .bind(&hold.fingerprint)
        .execute(&mut *conn)
        .await?;
        // Version 1, so an admin's dismissal is versioned like any write.
        sqlx::query(
            "INSERT INTO gateway.admin_resource_version
                 (organization_id, resource_type, resource_id)
             VALUES ($1, 'catalog_price_hold', $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(organization_id.0)
        .bind(hold_id)
        .execute(&mut *conn)
        .await?;
        NotificationStore::raise_on(conn, organization_id, held_notification(hold, hold_id), now)
            .await?;
    }
    Ok(())
}

/// A held price's notification subject: one per deployment and operation.
///
/// `{deployment}/{operation}` while it fits a notification subject; past
/// that, a digest of both, so raising, updating and resolving all name the
/// same subject and two long names never collide by truncation.
fn hold_subject(deployment: &str, operation: &str) -> String {
    let plain = format!("{deployment}/{operation}");
    if plain.chars().count() <= MAX_SUBJECT_CHARS {
        return plain;
    }
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(deployment.as_bytes());
    digest.update([0]);
    digest.update(operation.as_bytes());
    let hex: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

fn held_notification(hold: &HeldPrice, hold_id: Uuid) -> NewNotification {
    let proposal = &hold.proposal;
    let why = match hold.reason {
        HoldReason::LargeIncrease => "it raises a price by more than automatic updates allow",
        HoldReason::LargeDecrease => "it lowers a price by more than automatic updates allow",
        HoldReason::ToZero => "it would make a price zero",
        HoldReason::SourcesDisagree => "its price sources disagree",
    };
    NewNotification::new(
        NotificationKind::PriceHeld,
        hold_subject(&proposal.deployment, &proposal.operation),
        format!(
            "Price change held for {} ({})",
            proposal.deployment, proposal.operation
        ),
        format!(
            "The price {} publishes for {} was not adopted because {why}. Adopt or dismiss it in Price changes.",
            proposal.source, proposal.model
        ),
    )
    .with_link(PRICING_LINK)
    .with_data(serde_json::json!({
        "hold_id": hold_id,
        "deployment": proposal.deployment,
        "operation": proposal.operation,
        "model": proposal.model,
        "source": proposal.source,
        "reason": hold.reason.as_str(),
    }))
}

/// Deployments named in an adoption notification; the rest are counted.
const ADOPTED_NAMED: usize = 5;

fn adopted_notification(
    release_id: Uuid,
    previous_release_id: Uuid,
    adopted: &[ProposedPrice],
) -> NewNotification {
    let mut deployments: Vec<&str> = adopted.iter().map(|p| p.deployment.as_str()).collect();
    deployments.sort_unstable();
    deployments.dedup();
    let mut named = deployments
        .iter()
        .take(ADOPTED_NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if deployments.len() > ADOPTED_NAMED {
        named.push_str(&format!(" and {} more", deployments.len() - ADOPTED_NAMED));
    }
    let count = adopted.len();
    NewNotification::new(
        NotificationKind::PriceAdopted,
        release_id.to_string(),
        format!(
            "Adopted {count} published price{}",
            if count == 1 { "" } else { "s" }
        ),
        format!("Prices now follow what their providers publish for {named}."),
    )
    .with_link(PRICING_LINK)
    .with_data(serde_json::json!({
        "release_id": release_id,
        "previous_release_id": previous_release_id,
        "adopted": count,
        "deployments": deployments.iter().take(50).collect::<Vec<_>>(),
    }))
}

/// Whether `manifest` bills every proposed dimension of the held price.
fn bills(
    manifest: &CatalogManifest,
    deployment: &str,
    operation: &str,
    changes: &[DimensionChange],
) -> bool {
    manifest
        .prices
        .iter()
        .find(|price| price.deployment == deployment && price.operation == operation)
        .is_some_and(|price| {
            changes.iter().all(|change| {
                let billed = match change.dimension {
                    PriceDimension::Input => price.input_minor_per_token,
                    PriceDimension::Output => price.output_minor_per_token,
                    PriceDimension::CachedInput => price.cached_input_minor_per_token,
                    PriceDimension::Reasoning => price.reasoning_minor_per_token,
                    PriceDimension::Other => price.other_minor_per_token,
                };
                billed == change.proposed
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_subjects_fit_and_stay_distinct() {
        assert_eq!(hold_subject("gpt-a", "chat"), "gpt-a/chat");
        let long = "d".repeat(255);
        let chat = hold_subject(&long, "chat");
        let stream = hold_subject(&long, "chat_stream");
        assert!(chat.starts_with("sha256:") && chat.len() <= MAX_SUBJECT_CHARS);
        assert_ne!(chat, stream, "no collision by truncation");
        assert_eq!(chat, hold_subject(&long, "chat"), "stable");
        // Raising keeps the subject as it is, so resolving finds it.
        let raised =
            NewNotification::new(NotificationKind::PriceHeld, chat.clone(), "t", "").bounded();
        assert_eq!(raised.subject, chat);
        // Exactly at the limit, it stays readable.
        let fits = "e".repeat(MAX_SUBJECT_CHARS - "/chat".len());
        assert_eq!(hold_subject(&fits, "chat"), format!("{fits}/chat"));
    }
}
