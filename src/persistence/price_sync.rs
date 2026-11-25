//! Price sync storage: configured price sources, the snapshots each fetch
//! records, and drift between an active release and those snapshots.
//!
//! The worker registers the sources it is configured with, claims one due
//! source at a time under a lease, and records the fetch. A lease token
//! fences a worker whose lease expired and whose source was claimed again.
//! A fetch identical to the current snapshot only confirms it. Nothing here
//! writes to a catalog release.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::notifications::NotificationKind;
use crate::persistence::notifications::NotificationChange;

use super::price_mappings::PriceMappingStore;
use crate::domain::{
    auto_offer::{ListedModel, price_list_items},
    catalog::ProviderFamily,
    discovery::ModelAvailability,
    identity::OrganizationId,
    pricing::{
        CatalogPriceRow, CatalogPriceSource, DimensionPrices, ObservationSet, ObservedPrice,
        PerTokens, PriceAmount, PriceDrift, PriceMappingRow, PriceObservationQuery,
        PriceObservationView, PriceSnapshotView, PriceSourceView, SourceObservations, batch_item,
        compare,
    },
    public_prices::AWS_PRICE_LIST_SOURCE,
};

/// Consecutive failed fetches before a source waits for its next daily fetch.
pub const MAX_FETCH_FAILURES: i32 = 3;

/// The least time between a fetch and one an administrator asks for. Sources
/// are gateway-wide, so this bounds what every organization together can ask
/// of a price list's publisher.
pub const MIN_REQUESTED_FETCH_INTERVAL: chrono::Duration = chrono::Duration::minutes(10);

/// What asking for a fetch now did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FetchRequest {
    /// The source is due; the worker fetches it on its next tick.
    Due(PriceSourceView),
    /// The source was fetched, or is being fetched, too recently; ask again
    /// after this many seconds.
    TooSoon(u64),
}

#[derive(Clone)]
pub struct PriceSyncStore {
    pool: PgPool,
}

/// A source a worker holds under a lease. `lease_token` fences its result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedSource {
    pub source: String,
    pub lease_token: Uuid,
}

/// What a recorded fetch did to the source's current snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchOutcome {
    /// The prices differ from the current snapshot, or there was none.
    Changed(Uuid),
    /// The prices are the current snapshot's; it is confirmed.
    Unchanged(Uuid),
}

#[derive(FromRow)]
struct SourceRow {
    source: String,
    kind: String,
    family: String,
    next_fetch_at: DateTime<Utc>,
    last_attempt_at: Option<DateTime<Utc>>,
    last_success_at: Option<DateTime<Utc>>,
    last_error_code: Option<String>,
    snapshot_id: Option<Uuid>,
    source_version: Option<String>,
    fetched_at: Option<DateTime<Utc>>,
    confirmed_at: Option<DateTime<Utc>>,
    observations: Option<i32>,
    unmapped: Option<i32>,
}

impl From<SourceRow> for PriceSourceView {
    fn from(row: SourceRow) -> Self {
        let snapshot = match (row.snapshot_id, row.fetched_at, row.confirmed_at) {
            (Some(id), Some(fetched_at), Some(confirmed_at)) => Some(PriceSnapshotView {
                id,
                source_version: row.source_version,
                fetched_at,
                confirmed_at,
                observations: row.observations.unwrap_or(0).max(0) as u32,
                unmapped: row.unmapped.unwrap_or(0).max(0) as u32,
            }),
            _ => None,
        };
        Self {
            source: row.source,
            kind: row.kind,
            provider_family: row.family,
            last_attempt_at: row.last_attempt_at,
            last_success_at: row.last_success_at,
            last_error_code: row.last_error_code,
            next_fetch_at: row.next_fetch_at,
            snapshot,
        }
    }
}

const SOURCE_COLUMNS: &str = "p.source, p.kind, p.family, p.next_fetch_at, p.last_attempt_at,
    p.last_success_at, p.last_error_code, s.id AS snapshot_id, s.source_version, s.fetched_at,
    s.confirmed_at, s.observations, s.unmapped";

#[derive(FromRow)]
struct CatalogRow {
    deployment: String,
    operation: String,
    model: String,
    location: Option<String>,
    family: String,
    adapter_instance_key: String,
    currency: String,
    overridden: bool,
    input: i64,
    output: i64,
    cached_input: i64,
    reasoning: i64,
    other: i64,
}

#[derive(FromRow)]
struct ObservationRow {
    source: String,
    family: String,
    snapshot_id: Uuid,
    item: String,
    model: Option<String>,
    region: Option<String>,
    currency: String,
    per_tokens: i32,
    input: Option<String>,
    output: Option<String>,
    cached_input: Option<String>,
    reasoning: Option<String>,
    other: Option<String>,
}

#[derive(FromRow)]
struct MappingRow {
    deployment: String,
    source: String,
    item: String,
    region: Option<String>,
}

/// Where an observation page ends: the source, its snapshot then, and the
/// observation's position in that snapshot.
pub type ObservationKeyset = (String, Uuid, i32);

#[derive(FromRow)]
struct ListedRow {
    source: String,
    snapshot_id: Uuid,
    ordinal: i32,
    item: String,
    model: Option<String>,
    region: Option<String>,
    currency: String,
    per_tokens: i32,
    input: Option<String>,
    output: Option<String>,
    cached_input: Option<String>,
    reasoning: Option<String>,
    other: Option<String>,
}

fn decode(message: &'static str) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

fn stored_amount(value: Option<String>) -> Result<Option<PriceAmount>, sqlx::Error> {
    value
        .map(|value| PriceAmount::parse(&value).ok_or_else(|| decode("stored price amount")))
        .transpose()
}

fn stored_prices(
    per_tokens: i32,
    input: Option<String>,
    output: Option<String>,
    cached_input: Option<String>,
    reasoning: Option<String>,
    other: Option<String>,
) -> Result<(PerTokens, DimensionPrices), sqlx::Error> {
    Ok((
        PerTokens::new(per_tokens.max(0) as u64).ok_or_else(|| decode("stored per_tokens"))?,
        DimensionPrices {
            input: stored_amount(input)?,
            output: stored_amount(output)?,
            cached_input: stored_amount(cached_input)?,
            reasoning: stored_amount(reasoning)?,
            other: stored_amount(other)?,
        },
    ))
}

/// `text` as a case-insensitive `LIKE` pattern matching it anywhere.
fn contains_pattern(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

impl PriceSyncStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organizations' price mappings, in the same database.
    pub fn mappings(&self) -> PriceMappingStore {
        PriceMappingStore::new(self.pool.clone())
    }

    /// Records that this worker has `sources` configured. A new or retired
    /// one is due at once, without the snapshot it had before retirement.
    /// A source no worker has registered since `retire_before`, and that no
    /// live lease holds, is retired: no longer fetched or reported. Workers
    /// with different configurations, as during a rolling change, therefore
    /// keep each other's sources. A retired source's failure warnings are
    /// resolved, since nothing fetches it any more.
    pub async fn register(
        &self,
        sources: &[(String, &'static str, ProviderFamily)],
        now: DateTime<Utc>,
        retire_before: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let ids: Vec<&str> = sources.iter().map(|(id, _, _)| id.as_str()).collect();
        let kinds: Vec<&str> = sources.iter().map(|(_, kind, _)| *kind).collect();
        let families: Vec<&str> = sources.iter().map(|(_, _, f)| f.as_str()).collect();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO gateway.price_source (source, kind, family, next_fetch_at, registered_at)
             SELECT source, kind, family, $4, $4
             FROM UNNEST($1::text[], $2::text[], $3::text[]) AS t(source, kind, family)
             ON CONFLICT (source) DO UPDATE
             SET registered_at = EXCLUDED.registered_at,
                 retired_at = NULL,
                 next_fetch_at = CASE WHEN price_source.retired_at IS NULL
                                      THEN price_source.next_fetch_at
                                      ELSE EXCLUDED.next_fetch_at END,
                 current_snapshot_id = CASE WHEN price_source.retired_at IS NULL
                                            THEN price_source.current_snapshot_id END,
                 failures = CASE WHEN price_source.retired_at IS NULL
                                 THEN price_source.failures ELSE 0 END,
                 last_error_code = CASE WHEN price_source.retired_at IS NULL
                                        THEN price_source.last_error_code END",
        )
        .bind(&ids)
        .bind(&kinds)
        .bind(&families)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let retired = sqlx::query_scalar::<_, String>(
            "UPDATE gateway.price_source
             SET retired_at = $1, lease_token = NULL, lease_expires_at = NULL
             WHERE retired_at IS NULL AND registered_at < $2
               AND (lease_expires_at IS NULL OR lease_expires_at <= $1)
             RETURNING source",
        )
        .bind(now)
        .bind(retire_before)
        .fetch_all(&mut *tx)
        .await?;
        for source in retired {
            NotificationChange::ResolveEverywhere(NotificationKind::PriceSourceFailing, source)
                .apply_best_effort(&mut tx, now)
                .await?;
        }
        tx.commit().await
    }

    /// Claims the most overdue of `sources` that no live lease holds.
    pub async fn claim(
        &self,
        sources: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<ClaimedSource>, sqlx::Error> {
        let lease_token = Uuid::new_v4();
        let source = sqlx::query_scalar::<_, String>(
            "WITH due AS (
                 SELECT source FROM gateway.price_source
                 WHERE source = ANY($1) AND retired_at IS NULL AND next_fetch_at <= $2
                   AND (lease_expires_at IS NULL OR lease_expires_at <= $2)
                 ORDER BY next_fetch_at, source
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED)
             UPDATE gateway.price_source p
             SET lease_token = $3, lease_expires_at = $4, last_attempt_at = $2
             FROM due WHERE p.source = due.source
             RETURNING p.source",
        )
        .bind(sources)
        .bind(now)
        .bind(lease_token)
        .bind(lease_until)
        .fetch_optional(&self.pool)
        .await?;
        Ok(source.map(|source| ClaimedSource {
            source,
            lease_token,
        }))
    }

    /// Records a successful fetch and schedules the next. `None` when the
    /// lease was lost, in which case nothing is written. The source's
    /// failure warnings are resolved in the same transaction.
    pub async fn record_success(
        &self,
        claim: &ClaimedSource,
        set: &ObservationSet,
        now: DateTime<Utc>,
        next_fetch_at: DateTime<Utc>,
    ) -> Result<Option<FetchOutcome>, sqlx::Error> {
        let digest = set.digest();
        let mut tx = self.pool.begin().await?;
        let Some((current, current_digest)) = sqlx::query_as::<_, (Option<Uuid>, Option<Vec<u8>>)>(
            "SELECT p.current_snapshot_id, s.digest
                 FROM gateway.price_source p
                 LEFT JOIN gateway.price_snapshot s ON s.id = p.current_snapshot_id
                 WHERE p.source = $1 AND p.lease_token = $2
                 FOR UPDATE OF p",
        )
        .bind(&claim.source)
        .bind(claim.lease_token)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let outcome = match current {
            Some(id) if current_digest.as_deref() == Some(digest.as_slice()) => {
                sqlx::query(
                    "UPDATE gateway.price_snapshot SET confirmed_at = $2, source_version = $3
                     WHERE id = $1",
                )
                .bind(id)
                .bind(now)
                .bind(set.source_version.as_deref())
                .execute(&mut *tx)
                .await?;
                FetchOutcome::Unchanged(id)
            }
            _ => {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO gateway.price_snapshot
                         (id, source, source_version, digest, observations, unmapped,
                          fetched_at, confirmed_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
                )
                .bind(id)
                .bind(&claim.source)
                .bind(set.source_version.as_deref())
                .bind(digest.as_slice())
                .bind(set.observations.len() as i32)
                .bind(set.unmapped() as i32)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                insert_observations(&mut tx, id, &set.observations).await?;
                FetchOutcome::Changed(id)
            }
        };
        let snapshot_id = match outcome {
            FetchOutcome::Changed(id) | FetchOutcome::Unchanged(id) => id,
        };
        sqlx::query(
            "UPDATE gateway.price_source
             SET lease_token = NULL, lease_expires_at = NULL, failures = 0,
                 last_success_at = $2, last_error_code = NULL,
                 current_snapshot_id = $3, next_fetch_at = $4
             WHERE source = $1",
        )
        .bind(&claim.source)
        .bind(now)
        .bind(snapshot_id)
        .bind(next_fetch_at)
        .execute(&mut *tx)
        .await?;
        NotificationChange::ResolveEverywhere(
            NotificationKind::PriceSourceFailing,
            claim.source.clone(),
        )
        .apply_best_effort(&mut tx, now)
        .await?;
        tx.commit().await?;
        Ok(Some(outcome))
    }

    /// Records a failed fetch under `code`. The source is retried after
    /// `retry_delay` times its consecutive failures, until
    /// [`MAX_FETCH_FAILURES`], then waits for `next_fetch_at`. `false` when
    /// the lease was lost. `notify` is applied, best effort, in the same
    /// transaction.
    pub async fn record_failure(
        &self,
        claim: &ClaimedSource,
        code: &str,
        now: DateTime<Utc>,
        retry_delay: chrono::Duration,
        next_fetch_at: DateTime<Utc>,
        notify: Option<&NotificationChange>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE gateway.price_source
             SET lease_token = NULL, lease_expires_at = NULL, last_error_code = $3,
                 failures = CASE WHEN failures + 1 >= $4 THEN 0 ELSE failures + 1 END,
                 next_fetch_at = CASE WHEN failures + 1 >= $4 THEN $6
                                      ELSE $5 + make_interval(secs => $7 * (failures + 1)) END
             WHERE source = $1 AND lease_token = $2",
        )
        .bind(&claim.source)
        .bind(claim.lease_token)
        .bind(code)
        .bind(MAX_FETCH_FAILURES)
        .bind(now)
        .bind(next_fetch_at)
        .bind(retry_delay.num_seconds() as f64)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Ok(false);
        }
        if let Some(change) = notify {
            change.apply_best_effort(&mut tx, now).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Makes `source` due now unless it was attempted within
    /// [`MIN_REQUESTED_FETCH_INTERVAL`] (a fetch in progress was). `None`
    /// when no worker has the source configured.
    pub async fn request_fetch(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        source: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<FetchRequest>, sqlx::Error> {
        let Some(last_attempt_at) = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            "SELECT last_attempt_at FROM gateway.price_source
             WHERE source = $1 AND retired_at IS NULL FOR UPDATE",
        )
        .bind(source)
        .fetch_optional(&mut **tx)
        .await?
        else {
            return Ok(None);
        };
        if let Some(wait) = last_attempt_at
            .map(|at| at + MIN_REQUESTED_FETCH_INTERVAL - now)
            .filter(|wait| *wait > chrono::Duration::zero())
        {
            let seconds = wait.num_seconds() + i64::from(wait.subsec_nanos() > 0);
            return Ok(Some(FetchRequest::TooSoon(seconds.max(1) as u64)));
        }
        sqlx::query(
            "UPDATE gateway.price_source SET next_fetch_at = LEAST(next_fetch_at, $2)
             WHERE source = $1",
        )
        .bind(source)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        let row = sqlx::query_as::<_, SourceRow>(&format!(
            "SELECT {SOURCE_COLUMNS}
             FROM gateway.price_source p
             LEFT JOIN gateway.price_snapshot s ON s.id = p.current_snapshot_id
             WHERE p.source = $1"
        ))
        .bind(source)
        .fetch_one(&mut **tx)
        .await?;
        Ok(Some(FetchRequest::Due(row.into())))
    }

    /// Every source and its current snapshot, by source.
    pub async fn sources(&self) -> Result<Vec<PriceSourceView>, sqlx::Error> {
        Ok(sqlx::query_as::<_, SourceRow>(&format!(
            "SELECT {SOURCE_COLUMNS}
             FROM gateway.price_source p
             LEFT JOIN gateway.price_snapshot s ON s.id = p.current_snapshot_id
             WHERE p.retired_at IS NULL
             ORDER BY p.source"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(PriceSourceView::from)
        .collect())
    }

    /// Compares every price of the organization's active release, as billed
    /// now (an effective price override replaces the release's price), with
    /// each source's current snapshot. `None` without an active release.
    pub async fn drift(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<PriceDrift>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // One snapshot, so prices, sources and observations agree.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let Some(release_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        let prices = sqlx::query_as::<_, CatalogRow>(
            "SELECT d.name AS deployment, pr.operation, d.model, d.location, p.family,
                    p.adapter_instance_key,
                    COALESCE(o.currency, pr.currency)::TEXT AS currency,
                    o.id IS NOT NULL AS overridden,
                    COALESCE(o.input_minor_per_token, pr.input_minor_per_token) AS input,
                    COALESCE(o.output_minor_per_token, pr.output_minor_per_token) AS output,
                    COALESCE(o.cached_input_minor_per_token, pr.cached_input_minor_per_token)
                        AS cached_input,
                    COALESCE(o.reasoning_minor_per_token, pr.reasoning_minor_per_token)
                        AS reasoning,
                    COALESCE(o.other_minor_per_token, pr.other_minor_per_token) AS other
             FROM gateway.catalog_price pr
             JOIN gateway.catalog_deployment d
               ON d.organization_id = pr.organization_id AND d.release_id = pr.release_id
              AND d.id = pr.deployment_id
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             LEFT JOIN LATERAL (
                 SELECT po.* FROM gateway.catalog_price_override po
                 WHERE po.organization_id = pr.organization_id AND po.deployment = d.name
                   AND po.operation = pr.operation AND po.revoked_at IS NULL
                   AND po.effective_from <= now()
                   AND (po.effective_until IS NULL OR po.effective_until > now())
                 ORDER BY po.effective_from DESC, po.created_at DESC, po.id DESC
                 LIMIT 1) o ON true
             WHERE pr.organization_id = $1 AND pr.release_id = $2
             ORDER BY d.name, pr.operation",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&mut *tx)
        .await?;
        let sources = sqlx::query_as::<_, SourceRow>(&format!(
            "SELECT {SOURCE_COLUMNS}
             FROM gateway.price_source p
             LEFT JOIN gateway.price_snapshot s ON s.id = p.current_snapshot_id
             WHERE p.retired_at IS NULL
             ORDER BY p.source"
        ))
        .fetch_all(&mut *tx)
        .await?;
        let mappings = sqlx::query_as::<_, MappingRow>(
            "SELECT deployment, source, item, region FROM gateway.catalog_price_mapping
             WHERE organization_id = $1
             ORDER BY deployment, source",
        )
        .bind(organization_id.0)
        .fetch_all(&mut *tx)
        .await?;
        // What the listing names each Bedrock model, so a deployment is
        // compared with the AWS Price List item named after it, as
        // automatic offers price it.
        let bedrock_keys: Vec<&str> = prices
            .iter()
            .filter(|price| price.family == ProviderFamily::Bedrock.as_str())
            .map(|price| price.adapter_instance_key.as_str())
            .collect();
        let named = sqlx::query_as::<_, (String, String, String)>(
            "SELECT adapter_instance_key, model, display_name
             FROM gateway.catalog_discovered_model
             WHERE organization_id = $1 AND family = $2 AND adapter_instance_key = ANY($3)
               AND display_name IS NOT NULL",
        )
        .bind(organization_id.0)
        .bind(ProviderFamily::Bedrock.as_str())
        .bind(&bedrock_keys)
        .fetch_all(&mut *tx)
        .await?;
        let mut listings: HashMap<&str, Vec<ListedModel>> = HashMap::new();
        for (key, model, display_name) in &named {
            listings.entry(key.as_str()).or_default().push(ListedModel {
                model: model.clone(),
                display_name: Some(display_name.clone()),
                operations: None,
                availability: ModelAvailability::Listed,
                missing: false,
                blocked: None,
                unchecked: false,
            });
        }
        let listing_items: Vec<Option<(String, String)>> = prices
            .iter()
            .map(|price| {
                let listed = listings.get(price.adapter_instance_key.as_str())?;
                price_list_items(ProviderFamily::Bedrock, listed, &price.model)
            })
            .collect();
        let models: Vec<&str> = prices.iter().map(|price| price.model.as_str()).collect();
        // Mapped and listing-name items, each with its batch item, which
        // batch operations are compared with.
        let mut wanted: Vec<(&str, String)> = mappings
            .iter()
            .map(|m| (m.source.as_str(), m.item.clone()))
            .collect();
        for (standard, long_context) in listing_items.iter().flatten() {
            for item in [standard, long_context] {
                wanted.push((AWS_PRICE_LIST_SOURCE, item.clone()));
            }
        }
        let batch: Vec<(&str, String)> = wanted
            .iter()
            .map(|(source, item)| (*source, batch_item(item)))
            .collect();
        wanted.extend(batch);
        let mapped_sources: Vec<&str> = wanted.iter().map(|(source, _)| *source).collect();
        let mapped_items: Vec<&str> = wanted.iter().map(|(_, item)| item.as_str()).collect();
        let observations = sqlx::query_as::<_, ObservationRow>(
            "SELECT p.source, p.family, o.snapshot_id, o.item, o.model, o.region,
                    o.currency::TEXT AS currency, o.per_tokens,
                    o.input::TEXT AS input, o.output::TEXT AS output,
                    o.cached_input::TEXT AS cached_input, o.reasoning::TEXT AS reasoning,
                    o.other::TEXT AS other
             FROM gateway.price_source p
             JOIN gateway.price_observation o ON o.snapshot_id = p.current_snapshot_id
             WHERE p.retired_at IS NULL
               AND (o.model = ANY($1)
                    OR (p.source, o.item) IN (SELECT * FROM UNNEST($2::text[], $3::text[])))
             ORDER BY p.source, o.ordinal",
        )
        .bind(&models)
        .bind(&mapped_sources)
        .bind(&mapped_items)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        let prices = prices
            .into_iter()
            .zip(listing_items)
            .map(|(row, listing_items)| {
                Ok(CatalogPriceRow {
                    listing_items,
                    family: ProviderFamily::from_str(&row.family)
                        .map_err(|_| decode("catalog provider family"))?,
                    deployment: row.deployment,
                    operation: row.operation,
                    model: row.model,
                    location: row.location,
                    currency: row.currency,
                    price_source: if row.overridden {
                        CatalogPriceSource::Override
                    } else {
                        CatalogPriceSource::Release
                    },
                    per_token: [
                        row.input,
                        row.output,
                        row.cached_input,
                        row.reasoning,
                        row.other,
                    ],
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let sources: Vec<PriceSourceView> = sources.into_iter().map(Into::into).collect();
        let mut observed: Vec<SourceObservations> = sources
            .iter()
            .filter_map(|source| {
                Some(SourceObservations {
                    source: source.source.clone(),
                    family: ProviderFamily::from_str(&source.provider_family).ok()?,
                    snapshot_id: source.snapshot.as_ref()?.id,
                    observations: Vec::new(),
                })
            })
            .collect();
        for row in observations {
            let Some(entry) = observed
                .iter_mut()
                .find(|entry| entry.source == row.source && entry.snapshot_id == row.snapshot_id)
            else {
                continue;
            };
            debug_assert_eq!(entry.family.as_str(), row.family);
            let (per_tokens, prices) = stored_prices(
                row.per_tokens,
                row.input,
                row.output,
                row.cached_input,
                row.reasoning,
                row.other,
            )?;
            entry.observations.push(ObservedPrice {
                item: row.item,
                model: row.model,
                region: row.region,
                currency: row.currency,
                per_tokens,
                prices,
            });
        }
        let mappings: Vec<PriceMappingRow> = mappings
            .into_iter()
            .map(|row| PriceMappingRow {
                deployment: row.deployment,
                source: row.source,
                item: row.item,
                region: row.region,
            })
            .collect();
        Ok(Some(PriceDrift {
            release_id,
            prices: compare(&prices, &observed, &mappings),
            sources,
        }))
    }

    /// The current snapshots of `family`'s sources, with their prices for
    /// `models`, for the items the organization maps to `deployments` and
    /// for `items` as (source, item), and those mappings. Read on the
    /// caller's connection.
    pub async fn published(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        family: ProviderFamily,
        models: &[&str],
        deployments: &[&str],
        items: &[(&str, &str)],
    ) -> Result<(Vec<SourceObservations>, Vec<PriceMappingRow>), sqlx::Error> {
        let sources = sqlx::query_as::<_, (String, Uuid)>(
            "SELECT source, current_snapshot_id FROM gateway.price_source
             WHERE family = $1 AND retired_at IS NULL AND current_snapshot_id IS NOT NULL
             ORDER BY source",
        )
        .bind(family.as_str())
        .fetch_all(&mut *conn)
        .await?;
        let mappings = sqlx::query_as::<_, MappingRow>(
            "SELECT m.deployment, m.source, m.item, m.region
             FROM gateway.catalog_price_mapping m
             JOIN gateway.price_source p ON p.source = m.source
             WHERE m.organization_id = $1 AND p.family = $2 AND m.deployment = ANY($3)
             ORDER BY m.deployment, m.source",
        )
        .bind(organization_id.0)
        .bind(family.as_str())
        .bind(deployments)
        .fetch_all(&mut *conn)
        .await?;
        let (mapped_sources, mapped_items): (Vec<&str>, Vec<&str>) = mappings
            .iter()
            .map(|m| (m.source.as_str(), m.item.as_str()))
            .chain(items.iter().copied())
            .unzip();
        let observations = sqlx::query_as::<_, ObservationRow>(
            "SELECT p.source, p.family, o.snapshot_id, o.item, o.model, o.region,
                    o.currency::TEXT AS currency, o.per_tokens,
                    o.input::TEXT AS input, o.output::TEXT AS output,
                    o.cached_input::TEXT AS cached_input, o.reasoning::TEXT AS reasoning,
                    o.other::TEXT AS other
             FROM gateway.price_source p
             JOIN gateway.price_observation o ON o.snapshot_id = p.current_snapshot_id
             WHERE p.family = $1 AND p.retired_at IS NULL
               AND (o.model = ANY($2)
                    OR (p.source, o.item) IN (SELECT * FROM UNNEST($3::text[], $4::text[])))
             ORDER BY p.source, o.ordinal",
        )
        .bind(family.as_str())
        .bind(models)
        .bind(&mapped_sources)
        .bind(&mapped_items)
        .fetch_all(&mut *conn)
        .await?;
        let mut observed: Vec<SourceObservations> = sources
            .into_iter()
            .map(|(source, snapshot_id)| SourceObservations {
                source,
                family,
                snapshot_id,
                observations: Vec::new(),
            })
            .collect();
        for row in observations {
            let Some(entry) = observed
                .iter_mut()
                .find(|entry| entry.source == row.source && entry.snapshot_id == row.snapshot_id)
            else {
                continue;
            };
            let (per_tokens, prices) = stored_prices(
                row.per_tokens,
                row.input,
                row.output,
                row.cached_input,
                row.reasoning,
                row.other,
            )?;
            entry.observations.push(ObservedPrice {
                item: row.item,
                model: row.model,
                region: row.region,
                currency: row.currency,
                per_tokens,
                prices,
            });
        }
        let mappings = mappings
            .into_iter()
            .map(|row| PriceMappingRow {
                deployment: row.deployment,
                source: row.source,
                item: row.item,
                region: row.region,
            })
            .collect();
        Ok((observed, mappings))
    }

    /// One page of the prices sources currently publish, by source and
    /// position in its snapshot, after the `after` keyset. Retired sources
    /// are left out. Also returns the keyset of the next page, if any.
    pub async fn observations(
        &self,
        query: &PriceObservationQuery,
        page_size: u32,
        after: Option<&ObservationKeyset>,
    ) -> Result<Option<(Vec<PriceObservationView>, Option<ObservationKeyset>)>, sqlx::Error> {
        // Positions are only meaningful within one snapshot, so a page may
        // not continue once the source it ended in has published another.
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        if let Some((source, snapshot, _)) = after {
            let current = sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT current_snapshot_id FROM gateway.price_source
                 WHERE source = $1 AND retired_at IS NULL",
            )
            .bind(source)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            if current != Some(*snapshot) {
                return Ok(None);
            }
        }
        let rows = sqlx::query_as::<_, ListedRow>(
            "SELECT p.source, o.snapshot_id, o.ordinal, o.item, o.model, o.region,
                    o.currency::TEXT AS currency, o.per_tokens,
                    o.input::TEXT AS input, o.output::TEXT AS output,
                    o.cached_input::TEXT AS cached_input, o.reasoning::TEXT AS reasoning,
                    o.other::TEXT AS other
             FROM gateway.price_source p
             JOIN gateway.price_observation o ON o.snapshot_id = p.current_snapshot_id
             WHERE p.retired_at IS NULL
               AND ($1::text IS NULL OR p.source = $1)
               AND ($2::text IS NULL OR o.item ILIKE $2 OR o.model ILIKE $2)
               AND ($3::text IS NULL OR o.region = $3)
               AND ($4::text IS NULL OR (p.source, o.ordinal) > ($4, $5))
             ORDER BY p.source, o.ordinal
             LIMIT $6",
        )
        .bind(query.source.as_deref())
        .bind(query.text.as_deref().map(contains_pattern))
        .bind(query.region.as_deref())
        .bind(after.map(|(source, _, _)| source.as_str()))
        .bind(after.map_or(0, |(_, _, ordinal)| *ordinal))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let has_more = rows.len() > page_size as usize;
        let mut items = Vec::with_capacity(rows.len().min(page_size as usize));
        let mut last = None;
        for row in rows.into_iter().take(page_size as usize) {
            let (per_tokens, prices) = stored_prices(
                row.per_tokens,
                row.input,
                row.output,
                row.cached_input,
                row.reasoning,
                row.other,
            )?;
            last = Some((row.source.clone(), row.snapshot_id, row.ordinal));
            items.push(PriceObservationView {
                source: row.source,
                snapshot_id: row.snapshot_id,
                item: row.item,
                model: row.model,
                region: row.region,
                currency: row.currency,
                per_tokens,
                prices,
            });
        }
        Ok(Some((items, has_more.then_some(last).flatten())))
    }

    /// Deletes up to `limit` snapshots last confirmed before `cutoff` that
    /// are not current for a configured source, with their observations.
    pub async fn sweep_snapshots(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM gateway.price_snapshot WHERE id IN (
                 SELECT s.id FROM gateway.price_snapshot s
                 WHERE s.confirmed_at < $1
                   AND NOT EXISTS (SELECT 1 FROM gateway.price_source p
                                   WHERE p.current_snapshot_id = s.id
                                     AND p.retired_at IS NULL)
                 ORDER BY s.confirmed_at
                 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

async fn insert_observations(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    snapshot_id: Uuid,
    observations: &[ObservedPrice],
) -> Result<(), sqlx::Error> {
    let amounts = |get: fn(&DimensionPrices) -> Option<&PriceAmount>| -> Vec<Option<String>> {
        observations
            .iter()
            .map(|o| get(&o.prices).map(|amount| amount.as_str().to_owned()))
            .collect()
    };
    sqlx::query(
        "INSERT INTO gateway.price_observation
             (snapshot_id, ordinal, item, model, region, currency, per_tokens,
              input, output, cached_input, reasoning, other)
         SELECT $1, t.ordinal - 1, t.item, t.model, t.region, t.currency, t.per_tokens,
                t.input::NUMERIC, t.output::NUMERIC, t.cached_input::NUMERIC,
                t.reasoning::NUMERIC, t.other::NUMERIC
         FROM UNNEST($2::text[], $3::text[], $4::text[], $5::text[], $6::int4[],
                     $7::text[], $8::text[], $9::text[], $10::text[], $11::text[])
              WITH ORDINALITY
              AS t(item, model, region, currency, per_tokens,
                   input, output, cached_input, reasoning, other, ordinal)",
    )
    .bind(snapshot_id)
    .bind(
        observations
            .iter()
            .map(|o| o.item.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        observations
            .iter()
            .map(|o| o.model.as_deref())
            .collect::<Vec<_>>(),
    )
    .bind(
        observations
            .iter()
            .map(|o| o.region.as_deref())
            .collect::<Vec<_>>(),
    )
    .bind(
        observations
            .iter()
            .map(|o| o.currency.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(
        observations
            .iter()
            .map(|o| o.per_tokens.count() as i32)
            .collect::<Vec<_>>(),
    )
    .bind(amounts(|p| p.input.as_ref()))
    .bind(amounts(|p| p.output.as_ref()))
    .bind(amounts(|p| p.cached_input.as_ref()))
    .bind(amounts(|p| p.reasoning.as_ref()))
    .bind(amounts(|p| p.other.as_ref()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}
