//! Organization price override rows. Route resolution reads the effective
//! override in `CatalogStore::resolve_active`; this store serves the admin
//! API. Rows are insert-only apart from a one-time revocation, which the
//! database also enforces.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, UserAccountId},
    price_override::{
        NewPriceOverride, PriceOverrideQuery, PriceOverrideStatus, PriceOverrideView, price_version,
    },
};

use super::reporting::TimeKeyset;

#[derive(Clone)]
pub struct PriceOverrideStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct OverrideRow {
    id: Uuid,
    deployment: String,
    operation: String,
    input_minor_per_token: i64,
    output_minor_per_token: i64,
    cached_input_minor_per_token: i64,
    reasoning_minor_per_token: i64,
    other_minor_per_token: i64,
    currency: String,
    confidence: String,
    price_version: String,
    reason: String,
    effective_from: DateTime<Utc>,
    effective_until: Option<DateTime<Utc>>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<Uuid>,
    revoke_reason: Option<String>,
    version: Option<i64>,
    now: DateTime<Utc>,
}

impl From<OverrideRow> for PriceOverrideView {
    fn from(row: OverrideRow) -> Self {
        Self {
            id: row.id,
            deployment: row.deployment,
            operation: row.operation,
            input_minor_per_token: row.input_minor_per_token,
            output_minor_per_token: row.output_minor_per_token,
            cached_input_minor_per_token: row.cached_input_minor_per_token,
            reasoning_minor_per_token: row.reasoning_minor_per_token,
            other_minor_per_token: row.other_minor_per_token,
            currency: row.currency,
            confidence: row.confidence,
            price_version: row.price_version,
            reason: row.reason,
            // Database time, the clock route resolution uses. Wall-clock
            // rather than transaction start, so a create reports the
            // override it just started as active.
            status: PriceOverrideStatus::at(
                row.now,
                row.effective_from,
                row.effective_until,
                row.revoked_at,
            ),
            effective_from: row.effective_from,
            effective_until: row.effective_until,
            created_by: row.created_by,
            created_at: row.created_at,
            revoked_at: row.revoked_at,
            revoked_by: row.revoked_by,
            revoke_reason: row.revoke_reason,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
        }
    }
}

const OVERRIDE_SELECT: &str = "SELECT o.id, o.deployment, o.operation,
        o.input_minor_per_token, o.output_minor_per_token, o.cached_input_minor_per_token,
        o.reasoning_minor_per_token, o.other_minor_per_token, o.currency::TEXT AS currency,
        o.confidence, o.version AS price_version, o.reason, o.effective_from,
        o.effective_until, o.created_by, o.created_at, o.revoked_at, o.revoked_by,
        o.revoke_reason, v.version, clock_timestamp() AS now
    FROM gateway.catalog_price_override o
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = o.organization_id
     AND v.resource_type = 'price_override' AND v.resource_id = o.id";

impl PriceOverrideStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page, newest first, after the `keyset`. The flag reports whether
    /// more rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        query: &PriceOverrideQuery,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<(Vec<PriceOverrideView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, OverrideRow>(&format!(
            "{OVERRIDE_SELECT}
             WHERE o.organization_id = $1
               AND ($2::text IS NULL OR o.deployment = $2)
               AND ($3::text IS NULL OR o.operation = $3)
               AND ($4::timestamptz IS NULL OR (o.created_at, o.id) < ($4, $5::uuid))
             ORDER BY o.created_at DESC, o.id DESC
             LIMIT $6"
        ))
        .bind(organization_id.0)
        .bind(query.deployment.as_deref())
        .bind(query.operation.map(|operation| operation.as_str()))
        .bind(keyset.map(|keyset| keyset.at))
        .bind(keyset.map(|keyset| keyset.id))
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

    /// One override on the caller's connection, so a mutation can read the
    /// row and version its own transaction wrote.
    pub async fn get(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<PriceOverrideView>, sqlx::Error> {
        let row = sqlx::query_as::<_, OverrideRow>(&format!(
            "{OVERRIDE_SELECT} WHERE o.organization_id = $1 AND o.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Families of every release of the organization that prices `operation`
    /// on a deployment named `deployment`. Empty when none does.
    pub async fn declaring_families(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        deployment: &str,
        operation: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT p.family
             FROM gateway.catalog_deployment d
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             JOIN gateway.catalog_price pr
               ON pr.organization_id = d.organization_id AND pr.release_id = d.release_id
              AND pr.deployment_id = d.id AND pr.operation = $3
             WHERE d.organization_id = $1 AND d.name = $2
             ORDER BY p.family",
        )
        .bind(organization_id.0)
        .bind(deployment)
        .bind(operation)
        .fetch_all(&mut *conn)
        .await
    }

    pub async fn insert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        request: &NewPriceOverride,
        actor: UserAccountId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.catalog_price_override
                 (id, organization_id, deployment, operation, input_minor_per_token,
                  output_minor_per_token, cached_input_minor_per_token,
                  reasoning_minor_per_token, other_minor_per_token, currency, confidence,
                  version, reason, effective_from, effective_until, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(&request.deployment)
        .bind(request.operation.as_str())
        .bind(request.input_minor_per_token)
        .bind(request.output_minor_per_token)
        .bind(request.cached_input_minor_per_token)
        .bind(request.reasoning_minor_per_token)
        .bind(request.other_minor_per_token)
        .bind(request.currency.as_str())
        .bind(match request.confidence {
            crate::domain::accounting::PricingConfidence::Known => "known",
            crate::domain::accounting::PricingConfidence::Conservative => "conservative",
        })
        .bind(price_version(id))
        .bind(&request.reason)
        .bind(request.effective_from)
        .bind(request.effective_until)
        .bind(actor.0)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Revoke an override that is not yet revoked. Returns false when it
    /// already was.
    pub async fn revoke(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        reason: &str,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.catalog_price_override
             SET revoked_at = CURRENT_TIMESTAMP, revoked_by = $3, revoke_reason = $4
             WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(actor.0)
        .bind(reason)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }
}
