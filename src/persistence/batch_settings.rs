//! An organization's batch settings and its connections' batch staging
//! (migration 0105, D-021).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    domain::{
        batch::{DEFAULT_EMULATED_CONCURRENCY, StagingKind, ValidStaging},
        identity::{OrganizationId, UserAccountId},
    },
    provider::batch::BatchStaging,
};

/// The organization's batch settings, as the admin API returns them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchSettingsView {
    /// Whether lines no native batch can take run as ordinary requests at
    /// the on-demand price.
    pub emulated_allowed: bool,
    pub emulated_concurrency: u32,
    /// 0 while never set.
    pub version: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

impl BatchSettingsView {
    fn unset() -> Self {
        Self {
            emulated_allowed: false,
            emulated_concurrency: DEFAULT_EMULATED_CONCURRENCY,
            version: 0,
            updated_at: None,
        }
    }
}

/// A connection's batch staging, as the admin API returns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchStagingView {
    pub connection: String,
    /// `s3` (Bedrock) or `gcs` (Gemini on Agent Platform).
    pub kind: String,
    pub bucket: String,
    pub prefix: String,
    pub region: Option<String>,
    pub role_arn: Option<String>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct SettingsRow {
    emulated_allowed: bool,
    emulated_concurrency: i32,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct StagingRow {
    connection: String,
    kind: String,
    bucket: String,
    prefix: String,
    region: Option<String>,
    role_arn: Option<String>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<StagingRow> for BatchStagingView {
    fn from(row: StagingRow) -> Self {
        Self {
            connection: row.connection,
            kind: row.kind,
            bucket: row.bucket,
            prefix: row.prefix,
            region: row.region,
            role_arn: row.role_arn,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

const SETTINGS_SELECT: &str =
    "SELECT s.emulated_allowed, s.emulated_concurrency, v.version, s.updated_at
    FROM gateway.organization_batch_settings s
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = s.organization_id
     AND v.resource_type = 'batch_settings' AND v.resource_id = s.id
    WHERE s.organization_id = $1";

const STAGING_SELECT: &str =
    "SELECT s.connection, s.kind, s.bucket, s.prefix, s.region, s.role_arn,
        v.version, s.created_at, s.updated_at
    FROM gateway.batch_staging s
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = s.organization_id
     AND v.resource_type = 'batch_staging' AND v.resource_id = s.id
    WHERE s.organization_id = $1 AND s.deleted_at IS NULL";

#[derive(Clone)]
pub struct BatchSettingsStore {
    pool: PgPool,
}

impl BatchSettingsStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's settings; never set, emulated batches are off.
    pub async fn settings(
        &self,
        organization_id: OrganizationId,
    ) -> Result<BatchSettingsView, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Ok(Self::settings_view(&mut conn, organization_id)
            .await?
            .unwrap_or_else(BatchSettingsView::unset))
    }

    pub async fn settings_view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<BatchSettingsView>, sqlx::Error> {
        let row = sqlx::query_as::<_, SettingsRow>(SETTINGS_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row.map(|row| BatchSettingsView {
            emulated_allowed: row.emulated_allowed,
            emulated_concurrency: u32::try_from(row.emulated_concurrency)
                .unwrap_or(DEFAULT_EMULATED_CONCURRENCY),
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: Some(row.updated_at),
        }))
    }

    /// Insert or replace the settings; true when a row was created.
    pub async fn upsert_settings(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        emulated_allowed: bool,
        emulated_concurrency: u32,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.organization_batch_settings
                 (organization_id, id, emulated_allowed, emulated_concurrency, updated_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (organization_id) DO UPDATE
             SET emulated_allowed = EXCLUDED.emulated_allowed,
                 emulated_concurrency = EXCLUDED.emulated_concurrency,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(emulated_allowed)
        .bind(i32::try_from(emulated_concurrency).unwrap_or(1))
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }

    /// The connections' staging, by connection.
    pub async fn staging_list(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Vec<BatchStagingView>, sqlx::Error> {
        let rows =
            sqlx::query_as::<_, StagingRow>(&format!("{STAGING_SELECT} ORDER BY s.connection"))
                .bind(organization_id.0)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn staging_view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        connection: &str,
    ) -> Result<Option<BatchStagingView>, sqlx::Error> {
        let row =
            sqlx::query_as::<_, StagingRow>(&format!("{STAGING_SELECT} AND s.connection = $2"))
                .bind(organization_id.0)
                .bind(connection)
                .fetch_optional(&mut *conn)
                .await?;
        Ok(row.map(Into::into))
    }

    /// The family of the organization's connection `connection`: its stored
    /// credential's, else its active catalog's provider account's.
    pub async fn connection_family(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        connection: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT family FROM (
                 SELECT family, 0 AS rank FROM gateway.provider_credential
                 WHERE organization_id = $1 AND adapter_instance_key = $2
                 UNION ALL
                 SELECT p.family, 1 FROM gateway.catalog_provider_account p
                 JOIN gateway.catalog_active_release a
                   ON a.organization_id = p.organization_id AND a.release_id = p.release_id
                 WHERE p.organization_id = $1 AND p.adapter_instance_key = $2
             ) f ORDER BY rank LIMIT 1",
        )
        .bind(organization_id.0)
        .bind(connection)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Insert or replace a connection's staging, reviving a deleted one;
    /// true when a row was created.
    pub async fn upsert_staging(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        connection: &str,
        kind: StagingKind,
        staging: &ValidStaging,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.batch_staging
                 (organization_id, id, connection, kind, bucket, prefix, region, role_arn, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (organization_id, connection) DO UPDATE
             SET kind = EXCLUDED.kind, bucket = EXCLUDED.bucket, prefix = EXCLUDED.prefix,
                 region = EXCLUDED.region, role_arn = EXCLUDED.role_arn,
                 updated_by = EXCLUDED.updated_by, updated_at = CURRENT_TIMESTAMP,
                 deleted_at = NULL
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(connection)
        .bind(kind.as_str())
        .bind(&staging.bucket)
        .bind(&staging.prefix)
        .bind(&staging.region)
        .bind(&staging.role_arn)
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }

    /// Hides a connection's staging; the row stays. True when it was shown.
    pub async fn delete_staging(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        connection: &str,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.batch_staging
             SET deleted_at = CURRENT_TIMESTAMP, updated_by = $3, updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND connection = $2 AND deleted_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(connection)
        .bind(actor.0)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }

    /// The staging a connection's batch jobs use, unless none is set.
    pub async fn staging_for(
        &self,
        organization_id: Uuid,
        connection: &str,
    ) -> Result<Option<BatchStaging>, sqlx::Error> {
        self.staging_for_job(organization_id, connection, false)
            .await
    }

    /// [`Self::staging_for`], or for a job already sent also a staging
    /// deleted since, which still holds the job's input and output.
    pub async fn staging_for_job(
        &self,
        organization_id: Uuid,
        connection: &str,
        include_deleted: bool,
    ) -> Result<Option<BatchStaging>, sqlx::Error> {
        let row: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT bucket, prefix, region, role_arn FROM gateway.batch_staging
             WHERE organization_id = $1 AND connection = $2 AND (deleted_at IS NULL OR $3)",
        )
        .bind(organization_id)
        .bind(connection)
        .bind(include_deleted)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(bucket, prefix, region, role_arn)| BatchStaging {
            bucket,
            prefix,
            region,
            role_arn,
        }))
    }
}
