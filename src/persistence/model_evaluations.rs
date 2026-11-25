//! Storage for an organization's evaluation sets and runs (Phase 8
//! milestone 4). Only scores are stored: a run keeps each model's counts,
//! never the batch's answers.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::OrganizationId,
    model_card::GovernanceActor,
    model_evaluation::{
        EvaluationItem, EvaluationRun, EvaluationSet, ModelScore, OrganizationEvaluation,
    },
};

#[derive(Clone)]
pub struct ModelEvaluationStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct SetRow {
    id: Uuid,
    name: String,
    description: Option<String>,
    items: sqlx::types::Json<Vec<EvaluationItem>>,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    created_by_name: Option<String>,
    version: Option<i64>,
}

fn actor(id: Option<Uuid>, name: Option<String>) -> Option<GovernanceActor> {
    id.map(|id| GovernanceActor {
        actor_type: "user".into(),
        id: Some(id),
        name,
    })
}

impl From<SetRow> for EvaluationSet {
    fn from(row: SetRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            description: row.description,
            items: row.items.0,
            created_at: row.created_at,
            created_by: actor(row.created_by, row.created_by_name),
            version: row
                .version
                .and_then(|version| u64::try_from(version).ok())
                .unwrap_or(1),
        }
    }
}

#[derive(FromRow)]
struct RunRow {
    id: Uuid,
    set_id: Uuid,
    batch_id: Uuid,
    created_at: DateTime<Utc>,
    created_by: Option<Uuid>,
    created_by_name: Option<String>,
    unmatched_lines: i32,
}

/// What a run reads of its batch.
#[derive(Clone, Debug, FromRow)]
pub struct RunBatch {
    pub status: String,
    pub operation: String,
    pub output_file_id: Option<Uuid>,
    pub object_key: Option<String>,
    pub bytes: Option<i64>,
    pub content_purged_at: Option<DateTime<Utc>>,
    pub error_file_id: Option<Uuid>,
    pub error_object_key: Option<String>,
    pub error_bytes: Option<i64>,
    pub error_purged_at: Option<DateTime<Utc>>,
}

const SET_SELECT: &str = "SELECT s.id, s.name, s.description, s.items, s.created_at,
        s.created_by, u.display_name AS created_by_name, v.version
 FROM gateway.model_evaluation_set s
 LEFT JOIN gateway.user_account u ON u.id = s.created_by
 LEFT JOIN gateway.admin_resource_version v
   ON v.organization_id = s.organization_id
  AND v.resource_type = 'model_evaluation_set' AND v.resource_id = s.id";

const RUN_SELECT: &str = "SELECT r.id, r.set_id, r.batch_id, r.created_at, r.created_by,
        u.display_name AS created_by_name, r.unmatched_lines
 FROM gateway.model_evaluation_run r
 LEFT JOIN gateway.user_account u ON u.id = r.created_by";

impl ModelEvaluationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create_set(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        name: &str,
        description: Option<&str>,
        items: &[EvaluationItem],
        actor: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.model_evaluation_set
                 (id, organization_id, name, description, items, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(name)
        .bind(description)
        .bind(sqlx::types::Json(items))
        .bind(actor)
        .execute(conn)
        .await?;
        Ok(())
    }

    pub async fn set_in(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<EvaluationSet>, sqlx::Error> {
        Ok(sqlx::query_as::<_, SetRow>(&format!(
            "{SET_SELECT} WHERE s.organization_id = $1 AND s.id = $2 AND s.deleted_at IS NULL"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .map(Into::into))
    }

    pub async fn set(
        &self,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<EvaluationSet>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::set_in(&mut conn, organization_id, id).await
    }

    /// Hides set `id` (soft): its items, runs and scores are kept. False
    /// when the organization has no such set, or it is already deleted.
    pub async fn delete_set(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE gateway.model_evaluation_set
             SET deleted_at = CURRENT_TIMESTAMP, deleted_by = $3
             WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(actor)
        .execute(conn)
        .await?
        .rows_affected()
            == 1)
    }

    /// Whether set `id` is there and not deleted, holding it until the
    /// transaction ends so a delete cannot pass a run being recorded.
    pub async fn lock_live_set(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM gateway.model_evaluation_set
             WHERE organization_id = $1 AND id = $2 AND deleted_at IS NULL
             FOR SHARE",
        )
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(conn)
        .await?
        .is_some())
    }

    /// The organization's sets, newest first, at most `limit`.
    pub async fn sets(
        &self,
        organization_id: OrganizationId,
        limit: i64,
    ) -> Result<Vec<EvaluationSet>, sqlx::Error> {
        Ok(sqlx::query_as::<_, SetRow>(&format!(
            "{SET_SELECT} WHERE s.organization_id = $1 AND s.deleted_at IS NULL
             ORDER BY s.created_at DESC, s.id DESC LIMIT $2"
        ))
        .bind(organization_id.0)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
    }

    /// The batch a run reads, with its output file, if the organization has
    /// it.
    pub async fn run_batch(
        &self,
        organization_id: OrganizationId,
        batch_id: Uuid,
    ) -> Result<Option<RunBatch>, sqlx::Error> {
        sqlx::query_as::<_, RunBatch>(
            "SELECT b.status, b.operation, b.output_file_id, f.object_key, f.bytes,
                    f.content_purged_at, b.error_file_id, e.object_key AS error_object_key,
                    e.bytes AS error_bytes, e.content_purged_at AS error_purged_at
             FROM gateway.batch b
             LEFT JOIN gateway.batch_file f
               ON f.organization_id = b.organization_id AND f.id = b.output_file_id
              AND f.deleted_at IS NULL
             LEFT JOIN gateway.batch_file e
               ON e.organization_id = b.organization_id AND e.id = b.error_file_id
              AND e.deleted_at IS NULL
             WHERE b.organization_id = $1 AND b.id = $2",
        )
        .bind(organization_id.0)
        .bind(batch_id)
        .fetch_optional(&self.pool)
        .await
    }

    /// The model each of the batch's lines ran on, by custom_id.
    pub async fn line_models(
        &self,
        organization_id: OrganizationId,
        batch_id: Uuid,
    ) -> Result<HashMap<String, String>, sqlx::Error> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT l.custom_id, j.model
             FROM gateway.batch_line l
             JOIN gateway.batch_job j
               ON j.organization_id = l.organization_id AND j.id = l.job_id
             WHERE l.organization_id = $1 AND l.batch_id = $2",
        )
        .bind(organization_id.0)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_run(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        set_id: Uuid,
        batch_id: Uuid,
        unmatched_lines: u32,
        scores: &[ModelScore],
        actor: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.model_evaluation_run
                 (id, organization_id, set_id, batch_id, unmatched_lines, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(set_id)
        .bind(batch_id)
        .bind(i32::try_from(unmatched_lines).unwrap_or(i32::MAX))
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        let count = |n: u32| i32::try_from(n).unwrap_or(i32::MAX);
        let models: Vec<&str> = scores.iter().map(|s| s.model.as_str()).collect();
        let items: Vec<i32> = scores.iter().map(|s| count(s.items)).collect();
        let answered: Vec<i32> = scores.iter().map(|s| count(s.answered)).collect();
        let correct: Vec<i32> = scores.iter().map(|s| count(s.correct)).collect();
        sqlx::query(
            "INSERT INTO gateway.model_evaluation_result
                 (organization_id, run_id, model, items, answered, correct)
             SELECT $1, $2, s.model, s.items, s.answered, s.correct
             FROM UNNEST($3::text[], $4::int4[], $5::int4[], $6::int4[])
                 AS s (model, items, answered, correct)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(&models)
        .bind(&items)
        .bind(&answered)
        .bind(&correct)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Runs of `set_id` (all with `None`), newest first, with their scores.
    pub async fn runs_in(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        set_id: Uuid,
        run_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<EvaluationRun>, sqlx::Error> {
        let runs = sqlx::query_as::<_, RunRow>(&format!(
            "{RUN_SELECT} WHERE r.organization_id = $1 AND r.set_id = $2
               AND ($3::uuid IS NULL OR r.id = $3)
             ORDER BY r.created_at DESC, r.id DESC LIMIT $4"
        ))
        .bind(organization_id.0)
        .bind(set_id)
        .bind(run_id)
        .bind(limit)
        .fetch_all(&mut *conn)
        .await?;
        let ids: Vec<Uuid> = runs.iter().map(|run| run.id).collect();
        let results: Vec<(Uuid, String, i32, i32, i32)> = sqlx::query_as(
            "SELECT run_id, model, items, answered, correct
             FROM gateway.model_evaluation_result
             WHERE organization_id = $1 AND run_id = ANY($2)
             ORDER BY model",
        )
        .bind(organization_id.0)
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?;
        let count = |n: i32| u32::try_from(n).unwrap_or(0);
        Ok(runs
            .into_iter()
            .map(|run| EvaluationRun {
                results: results
                    .iter()
                    .filter(|result| result.0 == run.id)
                    .map(|result| ModelScore {
                        model: result.1.clone(),
                        items: count(result.2),
                        answered: count(result.3),
                        correct: count(result.4),
                    })
                    .collect(),
                id: run.id,
                set_id: run.set_id,
                batch_id: run.batch_id,
                created_at: run.created_at,
                created_by: actor(run.created_by, run.created_by_name),
                unmatched_lines: count(run.unmatched_lines),
            })
            .collect())
    }

    pub async fn runs(
        &self,
        organization_id: OrganizationId,
        set_id: Uuid,
        limit: i64,
    ) -> Result<Vec<EvaluationRun>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::runs_in(&mut conn, organization_id, set_id, None, limit).await
    }

    /// `model`'s score in the latest run of each of the organization's sets
    /// that scored it, newest first.
    pub async fn for_model(
        pool: &PgPool,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Vec<OrganizationEvaluation>, sqlx::Error> {
        let rows: Vec<(Uuid, String, Uuid, Uuid, DateTime<Utc>, i32, i32, i32)> = sqlx::query_as(
            "SELECT DISTINCT ON (s.id) s.id, s.name, r.id, r.batch_id, r.created_at,
                    e.items, e.answered, e.correct
             FROM gateway.model_evaluation_result e
             JOIN gateway.model_evaluation_run r
               ON r.organization_id = e.organization_id AND r.id = e.run_id
             JOIN gateway.model_evaluation_set s
               ON s.organization_id = r.organization_id AND s.id = r.set_id
             WHERE e.organization_id = $1 AND e.model = $2 AND s.deleted_at IS NULL
             ORDER BY s.id, r.created_at DESC, r.id DESC",
        )
        .bind(organization_id.0)
        .bind(model)
        .fetch_all(pool)
        .await?;
        let count = |n: i32| u32::try_from(n).unwrap_or(0);
        let mut evaluations: Vec<OrganizationEvaluation> = rows
            .into_iter()
            .map(|row| OrganizationEvaluation {
                set_id: row.0,
                set_name: row.1,
                run_id: row.2,
                batch_id: row.3,
                scored_at: row.4,
                items: count(row.5),
                answered: count(row.6),
                correct: count(row.7),
            })
            .collect();
        evaluations.sort_by_key(|evaluation| std::cmp::Reverse(evaluation.scored_at));
        Ok(evaluations)
    }
}
