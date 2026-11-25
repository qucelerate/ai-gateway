//! An organization's routing policy (D-023, Phase 9). Writing it publishes a
//! configuration version (the `routing_policy_publish` trigger), which is
//! where routing reads it from.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, UserAccountId},
    routing_policy::{RoutingPolicyInput, RoutingPolicyView},
};

#[derive(Clone)]
pub struct RoutingPolicyStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct PolicyRow {
    providers: Option<Vec<String>>,
    locations: Option<Vec<String>>,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
}

const POLICY_SELECT: &str = "SELECT p.providers, p.locations, v.version, p.updated_at
    FROM gateway.routing_policy p
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = p.organization_id
     AND v.resource_type = 'routing_policy' AND v.resource_id = p.id
    WHERE p.organization_id = $1";

impl From<PolicyRow> for RoutingPolicyView {
    fn from(row: PolicyRow) -> Self {
        Self {
            providers: row.providers,
            locations: row.locations,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: Some(row.updated_at),
        }
    }
}

impl RoutingPolicyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's policy; one never set permits any deployment.
    pub async fn policy(
        &self,
        organization_id: OrganizationId,
    ) -> Result<RoutingPolicyView, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map_or_else(RoutingPolicyView::unset, Into::into))
    }

    /// The policy on the caller's connection, so a mutation reads what its
    /// own transaction wrote.
    pub async fn policy_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<RoutingPolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row.map(Into::into))
    }

    /// Insert or replace the policy (already normalized). Returns true when
    /// a row was created.
    pub async fn upsert_policy(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        input: &RoutingPolicyInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.routing_policy
                 (organization_id, id, providers, locations, updated_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (organization_id) DO UPDATE
             SET providers = EXCLUDED.providers,
                 locations = EXCLUDED.locations,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(input.providers.as_deref())
        .bind(input.locations.as_deref())
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }
}
