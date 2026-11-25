//! The organization's tracing policy row and its version.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, UserAccountId},
    trace_policy::{TraceMode, TracePolicyError, TracePolicyInput, TracePolicyView},
};

#[derive(Clone)]
pub struct TracePolicyStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct PolicyRow {
    mode: String,
    team_ids: Vec<Uuid>,
    service_account_ids: Vec<Uuid>,
    content: bool,
    content_days: i32,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
}

impl From<PolicyRow> for TracePolicyView {
    fn from(row: PolicyRow) -> Self {
        Self {
            // The column's check allows only the modes there are.
            mode: TraceMode::parse(&row.mode).unwrap_or_default(),
            team_ids: row.team_ids,
            service_account_ids: row.service_account_ids,
            content: row.content,
            content_retention_days: u32::try_from(row.content_days).unwrap_or(1),
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: Some(row.updated_at),
        }
    }
}

const POLICY_SELECT: &str = "SELECT p.mode, p.team_ids, p.service_account_ids, p.content,
        p.content_days, v.version, p.updated_at
    FROM gateway.organization_trace_policy p
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = p.organization_id
     AND v.resource_type = 'trace_policy' AND v.resource_id = p.id
    WHERE p.organization_id = $1";

impl TracePolicyStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The organization's policy; one never set traces nothing.
    pub async fn policy(
        &self,
        organization_id: OrganizationId,
    ) -> Result<TracePolicyView, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map_or_else(TracePolicyView::unset, Into::into))
    }

    /// The policy on the caller's connection, so a mutation reads what its
    /// own transaction wrote.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<Option<TracePolicyView>, sqlx::Error> {
        let row = sqlx::query_as::<_, PolicyRow>(POLICY_SELECT)
            .bind(organization_id.0)
            .fetch_optional(&mut *conn)
            .await?;
        Ok(row.map(Into::into))
    }

    /// Whether every listed team and service account is the organization's.
    /// Each row is locked against deletion until the caller commits.
    pub async fn check_selection(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        input: &TracePolicyInput,
    ) -> Result<Result<(), TracePolicyError>, sqlx::Error> {
        let teams: Vec<Uuid> = input.team_ids.iter().map(|id| id.0).collect();
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT id FROM gateway.team
             WHERE organization_id = $1 AND id = ANY($2) FOR KEY SHARE) t",
        )
        .bind(organization_id.0)
        .bind(&teams)
        .fetch_one(&mut *conn)
        .await?;
        if usize::try_from(found).ok() != Some(teams.len()) {
            return Ok(Err(TracePolicyError::UnknownTeam));
        }
        let accounts: Vec<Uuid> = input.service_account_ids.iter().map(|id| id.0).collect();
        let found: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT id FROM gateway.service_account
             WHERE organization_id = $1 AND id = ANY($2) FOR KEY SHARE) s",
        )
        .bind(organization_id.0)
        .bind(&accounts)
        .fetch_one(&mut *conn)
        .await?;
        if usize::try_from(found).ok() != Some(accounts.len()) {
            return Ok(Err(TracePolicyError::UnknownServiceAccount));
        }
        Ok(Ok(()))
    }

    /// Insert or replace the policy. Returns true when a row was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        input: &TracePolicyInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        let teams: Vec<Uuid> = input.team_ids.iter().map(|id| id.0).collect();
        let accounts: Vec<Uuid> = input.service_account_ids.iter().map(|id| id.0).collect();
        sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.organization_trace_policy
                 (organization_id, id, mode, team_ids, service_account_ids, content,
                  content_days, updated_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (organization_id) DO UPDATE
             SET mode = EXCLUDED.mode,
                 team_ids = EXCLUDED.team_ids,
                 service_account_ids = EXCLUDED.service_account_ids,
                 content = EXCLUDED.content,
                 content_days = EXCLUDED.content_days,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(input.mode.as_str())
        .bind(&teams)
        .bind(&accounts)
        .bind(input.content)
        .bind(i32::try_from(input.content_retention_days).unwrap_or(i32::MAX))
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await
    }
}
