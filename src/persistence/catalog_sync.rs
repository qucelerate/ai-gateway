//! Model discovery storage: sync jobs and the models provider accounts list.
//!
//! The worker schedules a job for every provider account of each active
//! release that has not had one for the sync interval, claims jobs under a
//! lease, and records each listing. A job's attempt count fences a worker
//! whose lease expired and whose job was claimed again. Nothing here writes
//! to a catalog release.

use std::{collections::HashMap, str::FromStr};

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::persistence::notifications::NotificationChange;

use crate::domain::{
    discovery::{
        AccountDrift, DeployableModel, DeployableModelQuery, DeployableModelView,
        DeploymentDetails, DiscoveredModel, DiscoveredModelQuery, DiscoveredModelView,
        DiscoveryDrift, ERROR_ATTEMPTS_EXHAUSTED, ListingState, ModelAvailability, SyncJobView,
        SyncStatus, SyncTrigger, UnlistedDeployment,
    },
    identity::{OrganizationId, UserAccountId},
};

/// Claims per job, counting claims by workers that stopped mid-run.
pub const MAX_SYNC_ATTEMPTS: i32 = 3;

#[derive(Clone)]
pub struct CatalogSyncStore {
    pool: PgPool,
}

/// A job a worker holds under a lease. `attempt` fences its result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimedSyncJob {
    pub id: Uuid,
    pub organization_id: OrganizationId,
    pub adapter_instance_key: String,
    pub family: String,
    pub attempt: i32,
}

#[derive(FromRow)]
struct JobRow {
    id: Uuid,
    adapter_instance_key: String,
    family: String,
    trigger: String,
    status: String,
    attempts: i32,
    error_code: Option<String>,
    models_listed: Option<i32>,
    models_added: Option<i32>,
    models_missing: Option<i32>,
    requested_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobRow> for SyncJobView {
    type Error = sqlx::Error;
    fn try_from(row: JobRow) -> Result<Self, Self::Error> {
        let count = |value: Option<i32>| value.map(|value| value.max(0) as u32);
        Ok(Self {
            id: row.id,
            adapter_instance_key: row.adapter_instance_key,
            provider_family: row.family,
            trigger: SyncTrigger::from_str(&row.trigger).map_err(decode)?,
            status: SyncStatus::from_str(&row.status).map_err(decode)?,
            attempts: row.attempts.max(0) as u32,
            error_code: row.error_code,
            models_listed: count(row.models_listed),
            models_added: count(row.models_added),
            models_missing: count(row.models_missing),
            requested_by: row.requested_by,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

fn decode(error: impl std::error::Error + Send + Sync + 'static) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}

const JOB_COLUMNS: &str = "id, adapter_instance_key, family, trigger, status, attempts,
    error_code, models_listed, models_added, models_missing, requested_by, created_at,
    started_at, finished_at";

#[derive(FromRow)]
struct ModelRow {
    adapter_instance_key: String,
    family: String,
    model: String,
    display_name: Option<String>,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    missing_since: Option<DateTime<Utc>>,
    deployed: bool,
    operations: Option<Vec<String>>,
    availability: String,
    evidence_state: Option<String>,
    evidence_reason: Option<String>,
    evidence_source: Option<String>,
    evidence_deployment: Option<String>,
    evidence_provider_status: Option<i16>,
    evidence_provider_code: Option<String>,
    evidence_observed_at: Option<DateTime<Utc>>,
    evidence_verified_at: Option<DateTime<Utc>>,
    deployment_format: Option<String>,
    deployment_model: Option<String>,
    deployment_version: Option<String>,
    deployment_sku: Option<String>,
    deployment_capacity: Option<i32>,
    deployment_content_filter: Option<String>,
    deployment_content_filter_policy:
        Option<sqlx::types::Json<crate::domain::discovery::ContentFilterPolicy>>,
}

impl From<ModelRow> for DiscoveredModelView {
    fn from(row: ModelRow) -> Self {
        let evidence = match (
            row.evidence_state,
            row.evidence_source,
            row.evidence_observed_at,
        ) {
            (Some(state), Some(source), Some(observed_at)) => {
                crate::persistence::model_evidence::EvidenceRow {
                    model: row.model.clone(),
                    state,
                    reason: row.evidence_reason,
                    source,
                    deployment: row.evidence_deployment,
                    provider_status: row.evidence_provider_status,
                    provider_code: row.evidence_provider_code,
                    observed_at,
                    verified_at: row.evidence_verified_at,
                }
                .view()
            }
            _ => None,
        };
        let deployment = DeploymentDetails {
            format: row.deployment_format,
            model: row.deployment_model,
            version: row.deployment_version,
            sku: row.deployment_sku,
            capacity: row.deployment_capacity,
            content_filter: row.deployment_content_filter,
            content_filter_policy: row.deployment_content_filter_policy.map(|policy| policy.0),
        };
        Self {
            evidence,
            deployment: (deployment != DeploymentDetails::default()).then_some(deployment),
            operations: row.operations,
            // The column's check admits only the two values.
            availability: row
                .availability
                .parse()
                .unwrap_or(ModelAvailability::Unknown),
            adapter_instance_key: row.adapter_instance_key,
            provider_family: row.family,
            model: row.model,
            display_name: row.display_name,
            first_seen_at: row.first_seen_at,
            last_seen_at: row.last_seen_at,
            missing_since: row.missing_since,
            deployed: row.deployed,
        }
    }
}

/// The model-collection keyset: account key, family, then model.
pub type ModelKeyset = (String, String, String);

/// The deployable-collection keyset: account key, family, format, model,
/// then version.
pub type DeployableKeyset = (String, String, String, String, String);

#[derive(FromRow)]
struct DeployableRow {
    adapter_instance_key: String,
    family: String,
    format: String,
    model: String,
    version: String,
    lifecycle_status: Option<String>,
    default_version: bool,
    skus: Vec<String>,
    max_capacity: Option<i32>,
    retires_at: Option<DateTime<Utc>>,
    operations: Option<Vec<String>>,
    listed_at: DateTime<Utc>,
    deployments: Vec<String>,
}

impl From<DeployableRow> for DeployableModelView {
    fn from(row: DeployableRow) -> Self {
        Self {
            adapter_instance_key: row.adapter_instance_key,
            provider_family: row.family,
            format: row.format,
            model: row.model,
            version: row.version,
            lifecycle_status: row.lifecycle_status,
            default_version: row.default_version,
            skus: row.skus,
            max_capacity: row.max_capacity,
            retires_at: row.retires_at,
            operations: row.operations,
            deployments: row.deployments,
            listed_at: row.listed_at,
        }
    }
}

impl CatalogSyncStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Queues a scheduled job for up to `limit` provider accounts of active
    /// releases that have no open job and none for their family created
    /// since `due_before`.
    pub async fn schedule_due(
        &self,
        now: DateTime<Utc>,
        due_before: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO gateway.catalog_sync_job
                 (id, organization_id, adapter_instance_key, family, trigger, created_at)
             SELECT gen_random_uuid(), p.organization_id, p.adapter_instance_key, p.family,
                    'scheduled', $1
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = a.organization_id AND p.release_id = a.release_id
             WHERE NOT EXISTS (
                 SELECT 1 FROM gateway.catalog_sync_job j
                 WHERE j.organization_id = p.organization_id
                   AND j.adapter_instance_key = p.adapter_instance_key
                   AND (j.status IN ('queued', 'running')
                        -- A key redeclared with another family is a new
                        -- account, so the old family's jobs do not count.
                        OR (j.created_at > $2 AND j.family = p.family))
             )
             ORDER BY p.organization_id, p.adapter_instance_key
             LIMIT $3
             ON CONFLICT (organization_id, adapter_instance_key)
                 WHERE status IN ('queued', 'running') DO NOTHING",
        )
        .bind(now)
        .bind(due_before)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Fails jobs whose last attempt's lease expired, then claims up to
    /// `limit` queued or expired jobs of organizations that are not
    /// suspended, oldest first.
    pub async fn claim(
        &self,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ClaimedSyncJob>, sqlx::Error> {
        sqlx::query(
            "UPDATE gateway.catalog_sync_job
             SET status = 'failed', error_code = $3, lease_expires_at = NULL, finished_at = $1
             WHERE status = 'running' AND lease_expires_at <= $1 AND attempts >= $2",
        )
        .bind(now)
        .bind(MAX_SYNC_ATTEMPTS)
        .bind(ERROR_ATTEMPTS_EXHAUSTED)
        .execute(&self.pool)
        .await?;
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, i32)>(
            "UPDATE gateway.catalog_sync_job j
             SET status = 'running', attempts = j.attempts + 1, started_at = $1,
                 lease_expires_at = $2
             FROM (
                 SELECT s.id FROM gateway.catalog_sync_job s
                 -- A suspended (or deleted) organization's credentials are
                 -- not used: its jobs wait.
                 JOIN gateway.organization o
                   ON o.id = s.organization_id AND o.suspended_at IS NULL
                 WHERE (s.status = 'queued' OR (s.status = 'running' AND s.lease_expires_at <= $1))
                   AND s.attempts < $3
                 ORDER BY s.created_at, s.id
                 LIMIT $4
                 FOR UPDATE OF s SKIP LOCKED
             ) due
             WHERE j.id = due.id
             RETURNING j.id, j.organization_id, j.adapter_instance_key, j.family, j.attempts",
        )
        .bind(now)
        .bind(lease_until)
        .bind(MAX_SYNC_ATTEMPTS)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, organization_id, adapter_instance_key, family, attempt)| ClaimedSyncJob {
                    id,
                    organization_id: OrganizationId(organization_id),
                    adapter_instance_key,
                    family,
                    attempt,
                },
            )
            .collect())
    }

    /// Records a successful listing: listed models are upserted as seen now,
    /// and previously listed models the listing omits are marked missing.
    /// With `deployable`, they replace the account's deployable models;
    /// without, those stay as they were. Returns `false`, changing nothing,
    /// when `job` no longer holds the claim. `notify` is applied, best
    /// effort, in the same transaction.
    pub async fn finish_listed(
        &self,
        job: &ClaimedSyncJob,
        models: &[DiscoveredModel],
        deployable: Option<&[DeployableModel]>,
        now: DateTime<Utc>,
        notify: Option<&NotificationChange>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if !Self::lock_claim(&mut tx, job).await? {
            return Ok(false);
        }
        let ids: Vec<&str> = models.iter().map(|model| model.model.as_str()).collect();
        let names: Vec<Option<&str>> = models
            .iter()
            .map(|model| model.display_name.as_deref())
            .collect();
        // Comma-joined, since UNNEST cannot spread a ragged text[][].
        let operations: Vec<Option<String>> = models
            .iter()
            .map(|model| {
                model.operations.as_ref().map(|operations| {
                    operations
                        .iter()
                        .map(|operation| operation.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
            })
            .collect();
        let availability: Vec<&str> = models
            .iter()
            .map(|model| model.availability.as_str())
            .collect();
        let detail = |field: fn(&DeploymentDetails) -> Option<&str>| -> Vec<Option<&str>> {
            models
                .iter()
                .map(|model| model.deployment.as_ref().and_then(field))
                .collect()
        };
        let formats = detail(|details| details.format.as_deref());
        let deployed_models = detail(|details| details.model.as_deref());
        let versions = detail(|details| details.version.as_deref());
        let skus = detail(|details| details.sku.as_deref());
        let content_filters = detail(|details| details.content_filter.as_deref());
        // As text, cast in the query: one JSON document per row.
        let content_filter_policies: Vec<Option<String>> = models
            .iter()
            .map(|model| {
                model
                    .deployment
                    .as_ref()
                    .and_then(|details| details.content_filter_policy.as_ref())
                    .and_then(|policy| serde_json::to_string(policy).ok())
                    .filter(|policy| policy.len() <= 65536)
            })
            .collect();
        let capacities: Vec<Option<i32>> = models
            .iter()
            .map(|model| {
                model
                    .deployment
                    .as_ref()
                    .and_then(|details| details.capacity)
            })
            .collect();
        let added = sqlx::query_scalar::<_, i64>(
            "WITH upserted AS (
                 INSERT INTO gateway.catalog_discovered_model AS m
                     (organization_id, adapter_instance_key, family, model, display_name,
                      first_seen_at, last_seen_at, operations, availability,
                      deployment_format, deployment_model, deployment_version,
                      deployment_sku, deployment_capacity, deployment_content_filter,
                      deployment_content_filter_policy)
                 SELECT $1, $2, $3, listed.model, listed.display_name, $6, $6,
                        string_to_array(listed.operations, ','), listed.availability,
                        listed.format, listed.deployed_model, listed.version,
                        listed.sku, listed.capacity, listed.content_filter,
                        listed.content_filter_policy::jsonb
                 FROM UNNEST($4::text[], $5::text[], $7::text[], $8::text[],
                             $9::text[], $10::text[], $11::text[], $12::text[], $13::int4[],
                             $14::text[], $15::text[])
                     AS listed (model, display_name, operations, availability,
                                format, deployed_model, version, sku, capacity,
                                content_filter, content_filter_policy)
                 ON CONFLICT (organization_id, adapter_instance_key, family, model) DO UPDATE
                 SET display_name = EXCLUDED.display_name,
                     operations = EXCLUDED.operations,
                     availability = EXCLUDED.availability,
                     deployment_format = EXCLUDED.deployment_format,
                     deployment_model = EXCLUDED.deployment_model,
                     deployment_version = EXCLUDED.deployment_version,
                     deployment_sku = EXCLUDED.deployment_sku,
                     deployment_capacity = EXCLUDED.deployment_capacity,
                     deployment_content_filter = EXCLUDED.deployment_content_filter,
                     deployment_content_filter_policy = EXCLUDED.deployment_content_filter_policy,
                     last_seen_at = GREATEST(m.last_seen_at, EXCLUDED.last_seen_at),
                     missing_since = NULL
                 RETURNING (xmax = 0) AS inserted
             )
             SELECT count(*) FILTER (WHERE inserted) FROM upserted",
        )
        .bind(job.organization_id.0)
        .bind(&job.adapter_instance_key)
        .bind(&job.family)
        .bind(&ids)
        .bind(&names)
        .bind(now)
        .bind(&operations)
        .bind(&availability)
        .bind(&formats)
        .bind(&deployed_models)
        .bind(&versions)
        .bind(&skus)
        .bind(&capacities)
        .bind(&content_filters)
        .bind(&content_filter_policies)
        .fetch_one(&mut *tx)
        .await?;
        let missing = sqlx::query(
            "UPDATE gateway.catalog_discovered_model
             SET missing_since = $5
             WHERE organization_id = $1 AND adapter_instance_key = $2 AND family = $3
               AND missing_since IS NULL AND last_seen_at < $5 AND model <> ALL($4)",
        )
        .bind(job.organization_id.0)
        .bind(&job.adapter_instance_key)
        .bind(&job.family)
        .bind(&ids)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "UPDATE gateway.catalog_sync_job
             SET status = 'succeeded', lease_expires_at = NULL, finished_at = $2,
                 models_listed = $3, models_added = $4, models_missing = $5
             WHERE id = $1",
        )
        .bind(job.id)
        .bind(now)
        .bind(clamp(models.len() as u64))
        .bind(clamp(added.max(0) as u64))
        .bind(clamp(missing))
        .execute(&mut *tx)
        .await?;
        if let Some(deployable) = deployable {
            Self::replace_deployable(&mut tx, job, deployable, now).await?;
        }
        if let Some(change) = notify {
            change.apply_best_effort(&mut tx, now).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn replace_deployable(
        tx: &mut PgConnection,
        job: &ClaimedSyncJob,
        models: &[DeployableModel],
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "DELETE FROM gateway.catalog_deployable_model
             WHERE organization_id = $1 AND adapter_instance_key = $2 AND family = $3",
        )
        .bind(job.organization_id.0)
        .bind(&job.adapter_instance_key)
        .bind(&job.family)
        .execute(&mut *tx)
        .await?;
        let text = |field: fn(&DeployableModel) -> &str| -> Vec<&str> {
            models.iter().map(field).collect()
        };
        let lifecycle: Vec<Option<&str>> = models
            .iter()
            .map(|model| model.lifecycle_status.as_deref())
            .collect();
        let defaults: Vec<bool> = models.iter().map(|model| model.default_version).collect();
        // Each model's SKUs as a JSON array, since UNNEST cannot spread a
        // ragged text[][] and a SKU name may contain a comma.
        let skus: Vec<String> = models
            .iter()
            .map(|model| serde_json::to_string(&model.skus).unwrap_or_else(|_| "[]".into()))
            .collect();
        let capacities: Vec<Option<i32>> = models.iter().map(|model| model.max_capacity).collect();
        let retires: Vec<Option<DateTime<Utc>>> =
            models.iter().map(|model| model.retires_at).collect();
        let operations: Vec<Option<String>> = models
            .iter()
            .map(|model| {
                model.operations.as_ref().map(|operations| {
                    operations
                        .iter()
                        .map(|operation| operation.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
            })
            .collect();
        sqlx::query(
            "INSERT INTO gateway.catalog_deployable_model
                 (organization_id, adapter_instance_key, family, format, model, version,
                  lifecycle_status, default_version, skus, max_capacity, retires_at,
                  operations, listed_at)
             SELECT $1, $2, $3, listed.format, listed.model, listed.version,
                    listed.lifecycle_status, listed.default_version,
                    ARRAY(SELECT jsonb_array_elements_text(listed.skus::jsonb)),
                    listed.max_capacity, listed.retires_at,
                    string_to_array(listed.operations, ','), $4
             FROM UNNEST($5::text[], $6::text[], $7::text[], $8::text[], $9::bool[],
                         $10::text[], $11::int4[], $12::timestamptz[], $13::text[])
                 AS listed (format, model, version, lifecycle_status, default_version,
                            skus, max_capacity, retires_at, operations)",
        )
        .bind(job.organization_id.0)
        .bind(&job.adapter_instance_key)
        .bind(&job.family)
        .bind(now)
        .bind(text(|model| &model.format))
        .bind(text(|model| &model.model))
        .bind(text(|model| &model.version))
        .bind(&lifecycle)
        .bind(&defaults)
        .bind(&skus)
        .bind(&capacities)
        .bind(&retires)
        .bind(&operations)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Finishes a job without a listing. Returns `false`, changing nothing,
    /// when `job` no longer holds the claim. `notify` is applied, best
    /// effort, in the same transaction.
    pub async fn finish_unlisted(
        &self,
        job: &ClaimedSyncJob,
        status: SyncStatus,
        error_code: &str,
        now: DateTime<Utc>,
        notify: Option<&NotificationChange>,
    ) -> Result<bool, sqlx::Error> {
        debug_assert!(matches!(
            status,
            SyncStatus::Failed | SyncStatus::Unsupported
        ));
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE gateway.catalog_sync_job
             SET status = $3, error_code = $4, lease_expires_at = NULL, finished_at = $5
             WHERE id = $1 AND status = 'running' AND attempts = $2",
        )
        .bind(job.id)
        .bind(job.attempt)
        .bind(status.as_str())
        .bind(error_code)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Ok(false);
        }
        if let Some(change) = notify {
            change.apply_best_effort(&mut tx, now).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn lock_claim(
        conn: &mut PgConnection,
        job: &ClaimedSyncJob,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM gateway.catalog_sync_job
             WHERE id = $1 AND status = 'running' AND attempts = $2 FOR UPDATE",
        )
        .bind(job.id)
        .bind(job.attempt)
        .fetch_optional(conn)
        .await?
        .is_some())
    }

    /// The family of `adapter_instance_key` in the organization's active
    /// release, or `None` when the release does not declare it.
    pub async fn active_account_family(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT p.family
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = a.organization_id AND p.release_id = a.release_id
             WHERE a.organization_id = $1 AND p.adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .fetch_optional(conn)
        .await
    }

    /// Queues a manual job, or returns `None` when the account already has a
    /// queued or running job.
    pub async fn insert_manual(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        adapter_instance_key: &str,
        family: &str,
        requested_by: UserAccountId,
    ) -> Result<Option<SyncJobView>, sqlx::Error> {
        let row = sqlx::query_as::<_, JobRow>(&format!(
            "INSERT INTO gateway.catalog_sync_job
                 (id, organization_id, adapter_instance_key, family, trigger, requested_by)
             VALUES ($1, $2, $3, $4, 'manual', $5)
             ON CONFLICT (organization_id, adapter_instance_key)
                 WHERE status IN ('queued', 'running') DO NOTHING
             RETURNING {JOB_COLUMNS}"
        ))
        .bind(id)
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family)
        .bind(requested_by.0)
        .fetch_optional(conn)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub async fn get(
        &self,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<SyncJobView>, sqlx::Error> {
        let row = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {JOB_COLUMNS} FROM gateway.catalog_sync_job
             WHERE organization_id = $1 AND id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    /// One page of jobs, newest first, optionally for one account key.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: Option<&str>,
        page_size: u32,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<(Vec<SyncJobView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, JobRow>(&format!(
            "SELECT {JOB_COLUMNS} FROM gateway.catalog_sync_job
             WHERE organization_id = $1
               AND ($2::text IS NULL OR adapter_instance_key = $2)
               AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4))
             ORDER BY created_at DESC, id DESC
             LIMIT $5"
        ))
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(after.map(|(created_at, _)| created_at))
        .bind(after.map(|(_, id)| id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        page(rows, page_size)
    }

    /// One page of discovered models ordered by account key, family and
    /// model. `deployed` is judged against the active release.
    pub async fn list_models(
        &self,
        organization_id: OrganizationId,
        query: &DiscoveredModelQuery,
        page_size: u32,
        after: Option<&ModelKeyset>,
    ) -> Result<(Vec<DiscoveredModelView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, ModelRow>(
            "SELECT * FROM (
                 SELECT m.adapter_instance_key, m.family, m.model, m.display_name,
                        m.first_seen_at, m.last_seen_at, m.missing_since,
                        m.operations, m.availability,
                        m.deployment_format, m.deployment_model, m.deployment_version,
                        m.deployment_sku, m.deployment_capacity,
                        m.deployment_content_filter, m.deployment_content_filter_policy,
                        e.state AS evidence_state, e.reason AS evidence_reason,
                        e.source AS evidence_source, e.deployment AS evidence_deployment,
                        e.provider_status AS evidence_provider_status,
                        e.provider_code AS evidence_provider_code,
                        e.observed_at AS evidence_observed_at,
                        e.verified_at AS evidence_verified_at,
                        EXISTS (
                            SELECT 1
                            FROM gateway.catalog_active_release a
                            JOIN gateway.catalog_deployment d
                              ON d.organization_id = a.organization_id
                             AND d.release_id = a.release_id
                            JOIN gateway.catalog_provider_account p
                              ON p.organization_id = d.organization_id
                             AND p.release_id = d.release_id AND p.id = d.provider_account_id
                            WHERE a.organization_id = m.organization_id
                              AND d.adapter_instance_key = m.adapter_instance_key
                              AND p.family = m.family AND d.model = m.model
                        ) AS deployed
                 FROM gateway.catalog_discovered_model m
                 LEFT JOIN gateway.catalog_model_evidence e
                   ON e.organization_id = m.organization_id
                  AND e.adapter_instance_key = m.adapter_instance_key
                  AND e.family = m.family
                  AND e.model = m.model
                 WHERE m.organization_id = $1
                   AND ($2::text IS NULL OR m.adapter_instance_key = $2)
                   AND ($3::bool IS NULL OR (m.missing_since IS NOT NULL) = $3)
                   AND ($5::text IS NULL
                        OR (m.adapter_instance_key, m.family, m.model) > ($5, $6, $7))
                   AND ($9::text[] IS NULL OR coalesce(e.state, 'none') = ANY($9))
             ) listed
             WHERE ($4::bool IS NULL OR deployed = $4)
             ORDER BY adapter_instance_key, family, model
             LIMIT $8",
        )
        .bind(organization_id.0)
        .bind(query.adapter_instance_key.as_deref())
        .bind(query.state.map(|state| state == ListingState::Missing))
        .bind(query.deployed)
        .bind(after.map(|(key, _, _)| key.as_str()))
        .bind(after.map(|(_, family, _)| family.as_str()))
        .bind(after.map(|(_, _, model)| model.as_str()))
        .bind(i64::from(page_size) + 1)
        .bind(query.evidence.as_ref().map(|filters| {
            filters
                .iter()
                .map(|filter| filter.as_str())
                .collect::<Vec<_>>()
        }))
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter()
                .take(page_size as usize)
                .map(Into::into)
                .collect(),
            has_more,
        ))
    }

    /// One page of deployable models ordered by account key, family,
    /// format, model and version, each with the account's listed (not
    /// missing) deployments serving it.
    pub async fn list_deployable(
        &self,
        organization_id: OrganizationId,
        query: &DeployableModelQuery,
        page_size: u32,
        after: Option<&DeployableKeyset>,
    ) -> Result<(Vec<DeployableModelView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, DeployableRow>(
            "SELECT * FROM (
                 SELECT d.adapter_instance_key, d.family, d.format, d.model, d.version,
                        d.lifecycle_status, d.default_version, d.skus, d.max_capacity,
                        d.retires_at, d.operations, d.listed_at,
                        ARRAY(
                            SELECT m.model FROM gateway.catalog_discovered_model m
                            WHERE m.organization_id = d.organization_id
                              AND m.adapter_instance_key = d.adapter_instance_key
                              AND m.family = d.family
                              AND m.missing_since IS NULL
                              AND lower(m.deployment_format) = lower(d.format)
                              AND m.deployment_model = d.model
                              AND m.deployment_version = d.version
                            ORDER BY m.model
                            LIMIT 100
                        ) AS deployments
                 FROM gateway.catalog_deployable_model d
                 WHERE d.organization_id = $1
                   AND ($2::text IS NULL OR d.adapter_instance_key = $2)
                   AND ($10::text IS NULL OR lower(d.lifecycle_status) = lower($10))
                   AND ($11::text IS NULL OR $11 = ANY(d.operations))
                   AND ($4::text IS NULL
                        OR (d.adapter_instance_key, d.family, d.format, d.model, d.version)
                           > ($4, $5, $6, $7, $8))
             ) listed
             WHERE ($3::bool IS NULL OR (cardinality(deployments) > 0) = $3)
             ORDER BY adapter_instance_key, family, format, model, version
             LIMIT $9",
        )
        .bind(organization_id.0)
        .bind(query.adapter_instance_key.as_deref())
        .bind(query.deployed)
        .bind(after.map(|keyset| keyset.0.as_str()))
        .bind(after.map(|keyset| keyset.1.as_str()))
        .bind(after.map(|keyset| keyset.2.as_str()))
        .bind(after.map(|keyset| keyset.3.as_str()))
        .bind(after.map(|keyset| keyset.4.as_str()))
        .bind(i64::from(page_size) + 1)
        .bind(query.lifecycle_status.as_deref())
        .bind(query.operation.map(|operation| operation.as_str()))
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok((
            rows.into_iter()
                .take(page_size as usize)
                .map(Into::into)
                .collect(),
            has_more,
        ))
    }

    /// Drift between the active release and its accounts' latest listings,
    /// or `None` when the organization has no active release.
    pub async fn drift(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<DiscoveryDrift>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // One snapshot, so the counts and lists agree with each other.
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
        let accounts = sqlx::query_as::<_, (String, String, Option<DateTime<Utc>>)>(
            "SELECT p.adapter_instance_key, p.family,
                    (SELECT max(j.finished_at) FROM gateway.catalog_sync_job j
                     WHERE j.organization_id = p.organization_id
                       AND j.adapter_instance_key = p.adapter_instance_key
                       AND j.family = p.family AND j.status = 'succeeded')
             FROM gateway.catalog_provider_account p
             WHERE p.organization_id = $1 AND p.release_id = $2
             ORDER BY p.adapter_instance_key",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut last_jobs: HashMap<String, SyncJobView> = HashMap::new();
        for row in sqlx::query_as::<_, JobRow>(&format!(
            "SELECT DISTINCT ON (adapter_instance_key) {JOB_COLUMNS}
             FROM gateway.catalog_sync_job
             WHERE organization_id = $1
             ORDER BY adapter_instance_key, created_at DESC, id DESC"
        ))
        .bind(organization_id.0)
        .fetch_all(&mut *tx)
        .await?
        {
            let job = SyncJobView::try_from(row)?;
            last_jobs.insert(job.adapter_instance_key.clone(), job);
        }
        let undeployed: HashMap<String, i64> = sqlx::query_as::<_, (String, i64)>(
            "SELECT m.adapter_instance_key, count(*)
             FROM gateway.catalog_provider_account p
             JOIN gateway.catalog_discovered_model m
               ON m.organization_id = p.organization_id
              AND m.adapter_instance_key = p.adapter_instance_key AND m.family = p.family
             WHERE p.organization_id = $1 AND p.release_id = $2 AND m.missing_since IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.catalog_deployment d
                   WHERE d.organization_id = p.organization_id AND d.release_id = p.release_id
                     AND d.provider_account_id = p.id AND d.model = m.model
               )
             GROUP BY m.adapter_instance_key",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let mut unlisted: HashMap<String, Vec<UnlistedDeployment>> = HashMap::new();
        for (key, deployment, model, missing_since) in
            sqlx::query_as::<_, (String, String, String, Option<DateTime<Utc>>)>(
                "SELECT d.adapter_instance_key, d.name, d.model, m.missing_since
                 FROM gateway.catalog_deployment d
                 JOIN gateway.catalog_provider_account p
                   ON p.organization_id = d.organization_id AND p.release_id = d.release_id
                  AND p.id = d.provider_account_id
                 LEFT JOIN gateway.catalog_discovered_model m
                   ON m.organization_id = d.organization_id
                  AND m.adapter_instance_key = d.adapter_instance_key
                  AND m.family = p.family AND m.model = d.model
                 WHERE d.organization_id = $1 AND d.release_id = $2
                   AND (m.model IS NULL OR m.missing_since IS NOT NULL)
                 ORDER BY d.adapter_instance_key, d.name",
            )
            .bind(organization_id.0)
            .bind(release_id)
            .fetch_all(&mut *tx)
            .await?
        {
            unlisted.entry(key).or_default().push(UnlistedDeployment {
                deployment,
                model,
                missing_since,
            });
        }
        tx.commit().await?;
        let accounts = accounts
            .into_iter()
            .map(|(key, family, last_synced_at)| {
                let synced = last_synced_at.is_some();
                AccountDrift {
                    last_job: last_jobs
                        .remove(&key)
                        .filter(|job| job.provider_family == family),
                    last_synced_at,
                    undeployed_models: synced
                        .then(|| undeployed.get(&key).copied().unwrap_or(0).max(0) as u64),
                    unlisted_deployments: synced.then(|| unlisted.remove(&key).unwrap_or_default()),
                    adapter_instance_key: key,
                    provider_family: family,
                }
            })
            .collect();
        Ok(Some(DiscoveryDrift {
            release_id,
            accounts,
        }))
    }

    /// Deletes up to `limit` jobs finished before `cutoff`, keeping each
    /// account's latest successful job so its drift stays known.
    pub async fn sweep_jobs(&self, cutoff: DateTime<Utc>, limit: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.catalog_sync_job
             WHERE id IN (
                 SELECT j.id FROM gateway.catalog_sync_job j
                 WHERE j.finished_at < $1
                   AND NOT (j.status = 'succeeded' AND NOT EXISTS (
                       SELECT 1 FROM gateway.catalog_sync_job n
                       WHERE n.organization_id = j.organization_id
                         AND n.adapter_instance_key = j.adapter_instance_key
                         AND n.family = j.family AND n.status = 'succeeded'
                         AND (n.finished_at, n.id) > (j.finished_at, j.id)
                   ))
                 ORDER BY j.finished_at
                 LIMIT $2
             )",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Deletes up to `limit` models missing since before `cutoff`.
    pub async fn sweep_models(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.catalog_discovered_model
             WHERE ctid IN (
                 SELECT ctid FROM gateway.catalog_discovered_model
                 WHERE missing_since < $1
                 ORDER BY missing_since
                 LIMIT $2
             )",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn page(rows: Vec<JobRow>, page_size: u32) -> Result<(Vec<SyncJobView>, bool), sqlx::Error> {
    let has_more = rows.len() > page_size as usize;
    let items = rows
        .into_iter()
        .take(page_size as usize)
        .map(TryInto::try_into)
        .collect::<Result<_, _>>()?;
    Ok((items, has_more))
}

fn clamp(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
