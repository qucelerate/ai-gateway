//! Price mapping rows: which published item prices a deployment at one price
//! source. Writes run on the admin mutation's transaction; drift reads them
//! in `persistence::price_sync`.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, UserAccountId},
    pricing::{PriceMappingInput, PriceMappingView},
};
use crate::persistence::catalog::lock_catalog;

#[derive(Clone)]
pub struct PriceMappingStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct MappingRow {
    id: Uuid,
    deployment: String,
    source: String,
    item: String,
    region: Option<String>,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<MappingRow> for PriceMappingView {
    fn from(row: MappingRow) -> Self {
        Self {
            id: row.id,
            deployment: row.deployment,
            source: row.source,
            item: row.item,
            region: row.region,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

const MAPPING_SELECT: &str = "SELECT m.id, m.deployment, m.source, m.item, m.region, v.version,
        m.created_at, m.updated_at
    FROM gateway.catalog_price_mapping m
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = m.organization_id
     AND v.resource_type = 'price_mapping' AND v.resource_id = m.id";

impl PriceMappingStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page of mappings ordered by deployment and source, after the
    /// `after` keyset. The flag reports whether more rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<&(String, String)>,
    ) -> Result<(Vec<PriceMappingView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, MappingRow>(&format!(
            "{MAPPING_SELECT}
             WHERE m.organization_id = $1
               AND ($2::text IS NULL OR (m.deployment, m.source) > ($2, $3))
             ORDER BY m.deployment, m.source
             LIMIT $4"
        ))
        .bind(organization_id.0)
        .bind(after.map(|(deployment, _)| deployment.as_str()))
        .bind(after.map(|(_, source)| source.as_str()))
        .bind(i64::from(page_size) + 1)
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

    /// One mapping on the caller's connection, so a mutation can read the
    /// row and version its own transaction wrote.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<PriceMappingView>, sqlx::Error> {
        let row = sqlx::query_as::<_, MappingRow>(&format!(
            "{MAPPING_SELECT} WHERE m.organization_id = $1 AND m.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(Into::into))
    }

    /// The provider family of a price source that is not retired. The row
    /// is share-locked so it cannot be retired before the mapping commits.
    pub async fn source_family(
        conn: &mut PgConnection,
        source: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT family FROM gateway.price_source
             WHERE source = $1 AND retired_at IS NULL FOR SHARE",
        )
        .bind(source)
        .fetch_optional(&mut *conn)
        .await
    }

    /// The provider family of the active release's deployment named
    /// `deployment`, if it has one.
    pub async fn deployment_family(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        deployment: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT p.family
             FROM gateway.catalog_active_release a
             JOIN gateway.catalog_deployment d
               ON d.organization_id = a.organization_id AND d.release_id = a.release_id
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             WHERE a.organization_id = $1 AND d.name = $2",
        )
        .bind(organization_id.0)
        .bind(deployment)
        .fetch_optional(&mut *conn)
        .await
    }

    /// The provider family of the account whose automatic offer would name
    /// a deployment `deployment`, if one does: a mapping may then price a
    /// listed model before it is offered.
    pub async fn offered_deployment_family(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        deployment: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT d.family
             FROM gateway.catalog_auto_offered_model m
             JOIN gateway.catalog_discovered_model d
               ON d.organization_id = m.organization_id
              AND d.adapter_instance_key = m.adapter_instance_key AND d.model = m.model
             WHERE m.organization_id = $1 AND m.deployment = $2
             ORDER BY d.family
             LIMIT 1",
        )
        .bind(organization_id.0)
        .bind(deployment)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Insert or replace the mapping. Returns true when a row was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        deployment: &str,
        source: &str,
        input: &PriceMappingInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        // A mapping decides which published price a deployment is billed
        // at, so it changes under the catalog lock automatic price updates
        // adopt under (D-024).
        lock_catalog(&mut *conn, organization_id).await?;
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.catalog_price_mapping
                 (id, organization_id, deployment, source, item, region, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
             ON CONFLICT (id) DO UPDATE
             SET item = EXCLUDED.item,
                 region = EXCLUDED.region,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(deployment)
        .bind(source)
        .bind(&input.item)
        .bind(input.region.as_deref())
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }

    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        lock_catalog(&mut *conn, organization_id).await?;
        Ok(sqlx::query(
            "DELETE FROM gateway.catalog_price_mapping WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }
}
