//! Batches, their files, provider jobs and lines (migration 0089). A batch
//! is created, routed and reserved in full in one transaction, with its
//! provider-job records, before any provider is called.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool, Postgres};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::accounting::{QuotaScope, ReservationId};
use crate::persistence::accounting::{AccountingStore, AccountingStoreError, ReservationRequest};

#[derive(Debug, Error)]
pub enum BatchStoreError {
    #[error("batch database error")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Accounting(#[from] AccountingStoreError),
    #[error("idempotency key reused with a different batch")]
    IdempotencyConflict,
    /// The input file was deleted, or its content expired.
    #[error("the batch's input file is gone")]
    FileGone,
}

#[derive(Clone, Debug, FromRow)]
pub struct BatchFileRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub service_account_id: Uuid,
    pub purpose: String,
    pub filename: String,
    pub bytes: i64,
    pub sha256: Vec<u8>,
    pub object_key: String,
    pub created_at: DateTime<Utc>,
    pub content_purged_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, FromRow)]
pub struct BatchRow {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub team_id: Option<Uuid>,
    pub service_account_id: Uuid,
    pub fingerprint: Vec<u8>,
    pub endpoint: String,
    pub operation: String,
    pub input_file_id: Uuid,
    pub output_file_id: Option<Uuid>,
    pub error_file_id: Option<Uuid>,
    pub completion_window: String,
    pub metadata: Value,
    pub status: String,
    pub total_lines: i32,
    pub completed_lines: i32,
    pub failed_lines: i32,
    pub errors: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub in_progress_at: Option<DateTime<Utc>>,
    pub finalizing_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
    pub cancelling_at: Option<DateTime<Utc>>,
    pub cancelled_at: Option<DateTime<Utc>>,
}

/// A batch's columns. Until it ends, its request counts are its lines'
/// so far (a line names no `id` column, so `id` is the batch's); when it
/// ends, the counts its output files were written with.
pub(crate) const BATCH_COLUMNS: &str = "id,organization_id,team_id,service_account_id,fingerprint,endpoint,operation,input_file_id,output_file_id,error_file_id,completion_window,metadata,status,total_lines,\
CASE WHEN status IN ('validating','in_progress','finalizing','cancelling') THEN (SELECT count(*)::int FROM gateway.batch_line l WHERE l.batch_id = id AND l.state = 'succeeded') ELSE completed_lines END AS completed_lines,\
CASE WHEN status IN ('validating','in_progress','finalizing','cancelling') THEN (SELECT count(*)::int FROM gateway.batch_line l WHERE l.batch_id = id AND l.state NOT IN ('pending','succeeded')) ELSE failed_lines END AS failed_lines,\
errors,created_at,expires_at,in_progress_at,finalizing_at,completed_at,failed_at,expired_at,cancelling_at,cancelled_at";
const FILE_COLUMNS: &str = "id,organization_id,service_account_id,purpose,filename,bytes,sha256,object_key,created_at,content_purged_at";

/// A provider job to write with its batch: the lines one deployment serves.
#[derive(Clone, Debug)]
pub struct NewJob {
    pub id: Uuid,
    pub ordinal: i32,
    /// `native` or `emulated` (D-021).
    pub mode: crate::domain::batch::BatchMode,
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: String,
    pub family: String,
}

/// A line to write with its batch, charged against `reservation`.
#[derive(Clone, Debug)]
pub struct NewLine {
    pub line_number: u32,
    pub custom_id: String,
    pub job_id: Uuid,
    pub(crate) reservation: ReservationRequest,
}

#[derive(Clone, Debug)]
pub struct NewBatch {
    pub id: Uuid,
    pub scope: QuotaScope,
    pub idempotency_key: String,
    pub fingerprint: [u8; 32],
    pub endpoint: &'static str,
    pub operation: &'static str,
    pub input_file_id: Uuid,
    pub metadata: Value,
    pub expires_at: DateTime<Utc>,
    pub jobs: Vec<NewJob>,
    pub lines: Vec<NewLine>,
}

#[derive(Clone, Debug)]
pub enum CreateOutcome {
    Created(BatchRow),
    /// The batch this idempotency key already created.
    Existing(BatchRow),
}

#[derive(Clone)]
pub struct BatchStore {
    pool: PgPool,
}

impl BatchStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Marks a file whose content was never stored (its upload failed) as
    /// deleted; the expiry sweep removes any object left behind.
    pub async fn abandon_file(
        &self,
        organization_id: Uuid,
        id: Uuid,
    ) -> Result<(), BatchStoreError> {
        sqlx::query("UPDATE gateway.batch_file SET deleted_at = CURRENT_TIMESTAMP WHERE organization_id=$1 AND id=$2 AND deleted_at IS NULL")
            .bind(organization_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Up to `limit` files whose content is due to go: past the
    /// organization's trace content period (its tracing policy's, 30 days
    /// without one), or deleted an hour ago or more (an upload that failed).
    /// A file a batch that has not ended still reads is kept.
    pub async fn expired_files(
        &self,
        now: DateTime<Utc>,
        default_days: i32,
        limit: i64,
    ) -> Result<Vec<(Uuid, Uuid, String)>, BatchStoreError> {
        Ok(sqlx::query_as(
            "SELECT f.organization_id, f.id, f.object_key FROM gateway.batch_file f
             LEFT JOIN gateway.organization_trace_policy p ON p.organization_id = f.organization_id
             WHERE f.content_purged_at IS NULL
               AND ((f.deleted_at IS NOT NULL AND f.deleted_at < $1 - interval '1 hour')
                    OR f.created_at < $1 - make_interval(days => coalesce(p.content_days, $2)))
               AND NOT EXISTS (
                   SELECT 1 FROM gateway.batch b
                   WHERE b.organization_id = f.organization_id
                     AND b.status IN ('validating','in_progress','finalizing','cancelling')
                     AND f.id IN (b.input_file_id, b.output_file_id, b.error_file_id))
             ORDER BY f.created_at LIMIT $3",
        )
        .bind(now)
        .bind(default_days)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Removes a file's content with `delete`, unless a batch that has not
    /// ended reads the file, and records that it is gone; the record stays,
    /// as every record does. The file row is locked throughout, as batch
    /// creation locks it, so a batch is never accepted on content being
    /// removed: whichever comes second sees the other. Returns whether the
    /// content went.
    pub async fn purge_file<F, Fut>(
        &self,
        organization_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
        delete: F,
    ) -> Result<bool, BatchStoreError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let mut tx = self.pool.begin().await?;
        let locked: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM gateway.batch_file WHERE organization_id=$1 AND id=$2 AND content_purged_at IS NULL FOR UPDATE",
        )
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            return Ok(false);
        }
        // A statement of its own, after the lock: it sees a batch created
        // by a transaction the lock waited for.
        let in_use: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.batch b
                 WHERE b.organization_id = $1
                   AND b.status IN ('validating','in_progress','finalizing','cancelling')
                   AND $2 IN (b.input_file_id, b.output_file_id, b.error_file_id))",
        )
        .bind(organization_id)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        // Deleted before the row is marked: should the commit fail, the next
        // sweep deletes again, which is harmless.
        if in_use || !delete().await {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE gateway.batch_file SET content_purged_at=$3 WHERE organization_id=$1 AND id=$2",
        )
        .bind(organization_id)
        .bind(id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn insert_file(&self, file: &BatchFileRow) -> Result<(), BatchStoreError> {
        sqlx::query("INSERT INTO gateway.batch_file (id,organization_id,service_account_id,purpose,filename,bytes,sha256,object_key,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(file.id)
            .bind(file.organization_id)
            .bind(file.service_account_id)
            .bind(&file.purpose)
            .bind(&file.filename)
            .bind(file.bytes)
            .bind(&file.sha256)
            .bind(&file.object_key)
            .bind(file.created_at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A file the service account owns, unless deleted.
    pub async fn file(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        id: Uuid,
    ) -> Result<Option<BatchFileRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchFileRow>(&format!(
            "SELECT {FILE_COLUMNS} FROM gateway.batch_file WHERE organization_id=$1 AND service_account_id=$2 AND id=$3 AND deleted_at IS NULL"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn batch_by_key(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Option<BatchRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 AND service_account_id=$2 AND idempotency_key=$3"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Writes the batch, its provider jobs and its lines, and reserves
    /// every line, in one transaction: a batch is either accepted with all
    /// of its holds or rejected with none. A repeated idempotency key
    /// returns the batch it created, or a conflict when the request
    /// differs.
    pub async fn create(
        &self,
        batch: NewBatch,
        now: DateTime<Utc>,
    ) -> Result<CreateOutcome, BatchStoreError> {
        let service_account_id = batch
            .scope
            .service_account_id
            .ok_or(AccountingStoreError::NotClaimable)?
            .0;
        let organization_id = batch.scope.organization_id.0;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "batch:{organization_id}:{service_account_id}:{}",
                batch.idempotency_key
            ))
            .execute(&mut *tx)
            .await?;
        let existing = sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 AND service_account_id=$2 AND idempotency_key=$3"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(&batch.idempotency_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            if row.fingerprint != batch.fingerprint {
                return Err(BatchStoreError::IdempotencyConflict);
            }
            tx.commit().await?;
            return Ok(CreateOutcome::Existing(row));
        }
        // The input stays while the batch is created: content expiry locks
        // the same row, and refuses a file an open batch reads.
        let available: Option<bool> = sqlx::query_scalar(
            "SELECT deleted_at IS NULL AND content_purged_at IS NULL FROM gateway.batch_file
             WHERE organization_id=$1 AND service_account_id=$2 AND id=$3 FOR SHARE",
        )
        .bind(organization_id)
        .bind(service_account_id)
        .bind(batch.input_file_id)
        .fetch_optional(&mut *tx)
        .await?;
        if available != Some(true) {
            return Err(BatchStoreError::FileGone);
        }
        // One admission for the whole batch, before its row exists: the
        // row is what later admissions count.
        if let crate::persistence::rate_limits::Admission::Refused(breach) =
            crate::persistence::rate_limits::RateLimitStore::check_admission(
                &mut tx,
                &batch.scope,
                crate::persistence::rate_limits::TokenDemand::Batch,
            )
            .await?
        {
            return Err(AccountingStoreError::RateLimited(breach).into());
        }
        let total = i32::try_from(batch.lines.len()).map_err(|_| {
            AccountingStoreError::Domain(crate::domain::accounting::AccountingError::Overflow)
        })?;
        let row = sqlx::query_as::<_, BatchRow>(&format!(
            "INSERT INTO gateway.batch (id,organization_id,team_id,service_account_id,idempotency_key,fingerprint,endpoint,operation,input_file_id,completion_window,metadata,status,total_lines,created_at,expires_at,in_progress_at,api_key_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'24h',$10,'in_progress',$11,clock_timestamp(),$13,$12,$14) RETURNING {BATCH_COLUMNS}"
        ))
        .bind(batch.id)
        .bind(organization_id)
        .bind(batch.scope.team_id.map(|t| t.0))
        .bind(service_account_id)
        .bind(&batch.idempotency_key)
        .bind(batch.fingerprint.as_slice())
        .bind(batch.endpoint)
        .bind(batch.operation)
        .bind(batch.input_file_id)
        .bind(&batch.metadata)
        .bind(total)
        .bind(now)
        .bind(batch.expires_at)
        .bind(batch.scope.api_key_id.map(|key| key.0))
        .fetch_one(&mut *tx)
        .await?;
        for job in &batch.jobs {
            sqlx::query("INSERT INTO gateway.batch_job (id,organization_id,batch_id,ordinal,deployment,model,adapter_instance_key,family,mode,state,next_run_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$10,'pending',$9)")
                .bind(job.id)
                .bind(organization_id)
                .bind(batch.id)
                .bind(job.ordinal)
                .bind(&job.deployment)
                .bind(&job.model)
                .bind(&job.adapter_instance_key)
                .bind(&job.family)
                .bind(now)
                .bind(job.mode.as_str())
                .execute(&mut *tx)
                .await?;
        }
        let requests: Vec<ReservationRequest> = batch
            .lines
            .iter()
            .map(|line| line.reservation.clone())
            .collect();
        let reservations: Vec<ReservationId> =
            AccountingStore::reserve_batch_lines(&mut *tx, &batch.scope, batch.id, &requests, now)
                .await?;
        let rows: Vec<(&NewLine, &ReservationId)> =
            batch.lines.iter().zip(reservations.iter()).collect();
        for chunk in rows.chunks(1000) {
            let mut insert = sqlx::QueryBuilder::<Postgres>::new(
                "INSERT INTO gateway.batch_line (organization_id,batch_id,line_number,custom_id,job_id,reservation_id,state) ",
            );
            insert.push_values(chunk, |mut row, (line, reservation)| {
                row.push_bind(organization_id)
                    .push_bind(batch.id)
                    .push_bind(i32::try_from(line.line_number).unwrap_or(i32::MAX))
                    .push_bind(line.custom_id.clone())
                    .push_bind(line.job_id)
                    .push_bind(reservation.0)
                    .push("'pending'");
            });
            insert.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(CreateOutcome::Created(row))
    }

    /// A batch the service account created.
    pub async fn batch(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        id: Uuid,
    ) -> Result<Option<BatchRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 AND service_account_id=$2 AND id=$3"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The service account's batches, newest first, after the batch
    /// `after` names (a cursor), at most `limit` of them.
    pub async fn list(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<BatchRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch b WHERE organization_id=$1 AND service_account_id=$2 \
             AND ($3::uuid IS NULL OR (created_at, id) < (SELECT created_at, id FROM gateway.batch WHERE organization_id=$1 AND service_account_id=$2 AND id=$3)) \
             ORDER BY created_at DESC, id DESC LIMIT $4"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Asks for a batch to stop: it becomes `cancelling`, and every
    /// provider job not yet finished is told on its next run. A batch
    /// already ending or ended is returned as it is.
    pub async fn cancel(
        &self,
        organization_id: Uuid,
        service_account_id: Uuid,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<BatchRow>, BatchStoreError> {
        let mut tx = self.pool.begin().await?;
        let row =
            Self::cancel_in(&mut tx, organization_id, Some(service_account_id), id, now).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// [`Self::cancel`] in `conn`'s transaction: of the service account's
    /// batch, or with `None` of any of the organization's (an
    /// administrator's cancel).
    pub async fn cancel_in(
        conn: &mut sqlx::PgConnection,
        organization_id: Uuid,
        service_account_id: Option<Uuid>,
        id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<Option<BatchRow>, BatchStoreError> {
        let updated = sqlx::query_as::<_, BatchRow>(&format!(
            "UPDATE gateway.batch SET status='cancelling', cancelling_at=$4 WHERE organization_id=$1 AND ($2::uuid IS NULL OR service_account_id=$2) AND id=$3 AND status IN ('validating','in_progress') RETURNING {BATCH_COLUMNS}"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(id)
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(match updated {
            Some(row) => {
                sqlx::query("UPDATE gateway.batch_job SET cancel_requested=true, next_run_at=LEAST(next_run_at,$3) WHERE organization_id=$1 AND batch_id=$2 AND state<>'finished'")
                    .bind(organization_id)
                    .bind(id)
                    .bind(now)
                    .execute(&mut *conn)
                    .await?;
                Some(row)
            }
            None => {
                sqlx::query_as::<_, BatchRow>(&format!(
                    "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 AND ($2::uuid IS NULL OR service_account_id=$2) AND id=$3"
                ))
                .bind(organization_id)
                .bind(service_account_id)
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?
            }
        })
    }
}

/// A provider job leased to one worker run, with what its batch says.
#[derive(Clone, Debug, FromRow)]
pub struct LeasedJob {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub batch_id: Uuid,
    pub ordinal: i32,
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: String,
    pub family: String,
    pub state: String,
    /// `native` or `emulated`.
    pub mode: String,
    pub team_id: Option<Uuid>,
    pub service_account_id: Uuid,
    /// The key the batch was created with (none for a batch from before
    /// keys were recorded): its own rate-limit policy covers the lines.
    pub api_key_id: Option<Uuid>,
    pub provider_status: Option<String>,
    pub provider_input_file_id: Option<String>,
    pub provider_batch_id: Option<String>,
    pub provider_output_file_id: Option<String>,
    pub provider_error_file_id: Option<String>,
    pub cancel_requested: bool,
    pub attempts: i32,
    pub lease_token: Uuid,
    /// The job's normalized results, once collected.
    pub result_object_key: Option<String>,
    pub endpoint: String,
    pub batch_expires_at: DateTime<Utc>,
    pub input_object_key: String,
    /// The staging a Bedrock or Gemini job was sent through (migration
    /// 0106), once it was first sent.
    pub staging_bucket: Option<String>,
    pub staging_prefix: Option<String>,
    pub staging_region: Option<String>,
    pub staging_role_arn: Option<String>,
}

impl LeasedJob {
    /// The staging the job was sent through, if it kept one.
    pub fn staging(&self) -> Option<crate::provider::batch::BatchStaging> {
        Some(crate::provider::batch::BatchStaging {
            bucket: self.staging_bucket.clone()?,
            prefix: self.staging_prefix.clone()?,
            region: self.staging_region.clone(),
            role_arn: self.staging_role_arn.clone(),
        })
    }
}

/// A line of a job, with its reservation's trace.
#[derive(Clone, Debug, FromRow)]
pub struct JobLine {
    pub line_number: i32,
    pub custom_id: String,
    pub reservation_id: Uuid,
    pub trace_id: Uuid,
    pub state: String,
    /// The output the line's hold covers; the provider is held to it.
    pub output_token_cap: Option<i64>,
    /// When an emulated job sent the line, if it did.
    pub sent_at: Option<DateTime<Utc>>,
}

/// A line's state and result, for the batch's output files.
#[derive(Clone, Debug, FromRow)]
pub struct FinishedLine {
    pub line_number: i32,
    pub custom_id: String,
    pub job_id: Uuid,
    pub state: String,
}

/// What a finished batch ends as.
#[derive(Clone, Debug)]
pub struct BatchFinal {
    pub status: &'static str,
    pub completed: i32,
    pub failed: i32,
    pub output: Option<BatchFileRow>,
    pub errors: Option<BatchFileRow>,
}

impl BatchStore {
    /// Leases up to `limit` provider jobs that are due, for `lease`: a
    /// worker that dies holding one loses it when the lease ends, and the
    /// next worker resumes from what the job records.
    pub async fn lease_jobs(
        &self,
        now: DateTime<Utc>,
        lease: chrono::Duration,
        limit: i64,
    ) -> Result<Vec<LeasedJob>, BatchStoreError> {
        let token = Uuid::new_v4();
        Ok(sqlx::query_as::<_, LeasedJob>(
            "WITH due AS (
                 SELECT id FROM gateway.batch_job
                 WHERE state IN ('pending','submitted') AND next_run_at <= $1
                   AND (lease_expires_at IS NULL OR lease_expires_at <= $1)
                 ORDER BY next_run_at LIMIT $3 FOR UPDATE SKIP LOCKED
             ), leased AS (
                 UPDATE gateway.batch_job j SET lease_token=$4, lease_expires_at=$2, attempts=attempts+1
                 FROM due WHERE j.id = due.id
                 RETURNING j.*
             )
             SELECT l.id, l.organization_id, l.batch_id, l.ordinal, l.deployment, l.model,
                    l.adapter_instance_key, l.family, l.state, l.mode, b.team_id,
                    b.service_account_id, b.api_key_id, l.provider_status,
                    l.provider_input_file_id, l.provider_batch_id, l.provider_output_file_id,
                    l.provider_error_file_id, l.cancel_requested, l.attempts, l.lease_token,
                    l.result_object_key, b.endpoint, b.expires_at AS batch_expires_at, f.object_key AS input_object_key,
                    l.staging_bucket, l.staging_prefix, l.staging_region, l.staging_role_arn
             FROM leased l
             JOIN gateway.batch b ON b.organization_id = l.organization_id AND b.id = l.batch_id
             JOIN gateway.batch_file f ON f.organization_id = b.organization_id AND f.id = b.input_file_id",
        )
        .bind(now)
        .bind(now + lease)
        .bind(limit)
        .bind(token)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Whether this job may be sent under the organization's routing policy
    /// (D-023), as the configuration version it serves now says (a pinned
    /// organization keeps its pinned version's policy, as inference does).
    /// A job is sent long after its batch was routed, so the worker checks
    /// again before anything goes out. A job some of whose lines were
    /// already denied stays denied, so a run cut short between settling
    /// them and finishing the job resumes the denial rather than sending.
    pub async fn job_permitted(&self, job: &LeasedJob) -> Result<bool, BatchStoreError> {
        let denied: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.batch_line
                            WHERE organization_id = $1 AND job_id = $2
                              AND error_code = 'routing_policy_denied')",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .fetch_one(&self.pool)
        .await?;
        if denied {
            return Ok(false);
        }
        let served = super::config_versions::ConfigStore::new(self.pool.clone())
            .served_now(crate::domain::identity::OrganizationId(job.organization_id))
            .await
            .map_err(|error| match error {
                super::config_versions::ConfigStoreError::Database(error) => {
                    BatchStoreError::Database(error)
                }
                super::config_versions::ConfigStoreError::Invalid(id) => BatchStoreError::Database(
                    sqlx::Error::Decode(format!("configuration version {id} is not valid").into()),
                ),
            })?;
        let Some(policy) = served.and_then(|version| version.document.routing_policy) else {
            return Ok(true);
        };
        let location: Option<Option<String>> = sqlx::query_scalar(
            "SELECT d.location
             FROM gateway.batch_line l
             JOIN gateway.accounting_reservation r
               ON r.organization_id = l.organization_id AND r.id = l.reservation_id
             JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.catalog_release_id
              AND d.id = r.catalog_deployment_id
             WHERE l.organization_id = $1 AND l.job_id = $2
             LIMIT 1",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(crate::domain::routing_policy::permits(
            policy.providers.as_deref(),
            policy.locations.as_deref(),
            &job.family,
            location.flatten().as_deref(),
        ))
    }

    pub async fn job_lines(&self, job: &LeasedJob) -> Result<Vec<JobLine>, BatchStoreError> {
        Ok(sqlx::query_as::<_, JobLine>(
            "SELECT l.line_number, l.custom_id, l.reservation_id, r.trace_id, l.state, r.output_token_cap,
                    l.sent_at
             FROM gateway.batch_line l
             JOIN gateway.accounting_reservation r
               ON r.organization_id = l.organization_id AND r.id = l.reservation_id
             WHERE l.organization_id = $1 AND l.job_id = $2 ORDER BY l.line_number",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Claims the job's reservations that are still only reserved, before
    /// its lines go to the provider, so recovery treats a hold whose
    /// worker vanished as unknown rather than releasing it.
    pub async fn claim_job_lines(
        &self,
        job: &LeasedJob,
        now: DateTime<Utc>,
    ) -> Result<u64, BatchStoreError> {
        Ok(sqlx::query(
            "UPDATE gateway.accounting_reservation r SET state='claimed', claimed_at=CURRENT_TIMESTAMP
             FROM gateway.batch_line l
             WHERE l.organization_id = $1 AND l.job_id = $2
               AND r.organization_id = l.organization_id AND r.id = l.reservation_id
               AND r.batch_id = l.batch_id AND r.state = 'reserved' AND r.expires_at > $3",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    /// Updates the leased job; a worker whose lease was taken over changes
    /// nothing (returns false).
    /// Keeps `staging` with the job, unless it kept one already: the one it
    /// keeps, which it is then always created and collected through. `None`
    /// when this worker no longer holds the job.
    pub async fn keep_staging(
        &self,
        job: &LeasedJob,
        staging: &crate::provider::batch::BatchStaging,
    ) -> Result<Option<crate::provider::batch::BatchStaging>, BatchStoreError> {
        let kept: Option<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "UPDATE gateway.batch_job SET
                 staging_bucket = COALESCE(staging_bucket, $4),
                 staging_prefix = CASE WHEN staging_bucket IS NULL THEN $5 ELSE staging_prefix END,
                 staging_region = CASE WHEN staging_bucket IS NULL THEN $6 ELSE staging_region END,
                 staging_role_arn = CASE WHEN staging_bucket IS NULL THEN $7 ELSE staging_role_arn END
             WHERE organization_id = $1 AND id = $2 AND lease_token = $3
             RETURNING staging_bucket, staging_prefix, staging_region, staging_role_arn",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .bind(job.lease_token)
        .bind(&staging.bucket)
        .bind(&staging.prefix)
        .bind(&staging.region)
        .bind(&staging.role_arn)
        .fetch_optional(&self.pool)
        .await?;
        Ok(kept.map(
            |(bucket, prefix, region, role_arn)| crate::provider::batch::BatchStaging {
                bucket,
                prefix,
                region,
                role_arn,
            },
        ))
    }

    pub async fn update_job(
        &self,
        job: &LeasedJob,
        change: JobChange<'_>,
    ) -> Result<bool, BatchStoreError> {
        let result = sqlx::query(
            "UPDATE gateway.batch_job SET
                 provider_input_file_id = COALESCE($3, provider_input_file_id),
                 provider_batch_id = COALESCE($4, provider_batch_id),
                 provider_status = COALESCE($5, provider_status),
                 provider_output_file_id = COALESCE($6, provider_output_file_id),
                 provider_error_file_id = COALESCE($7, provider_error_file_id),
                 result_object_key = COALESCE($8, result_object_key),
                 error_code = COALESCE($9, error_code),
                 state = COALESCE($10, state),
                 submitted_at = CASE WHEN $10 = 'submitted' AND submitted_at IS NULL THEN CURRENT_TIMESTAMP ELSE submitted_at END,
                 finished_at = CASE WHEN $10 = 'finished' THEN CURRENT_TIMESTAMP ELSE finished_at END,
                 attempts = CASE WHEN $12 THEN 0 ELSE attempts END,
                 next_run_at = COALESCE($11, next_run_at),
                 lease_token = CASE WHEN $11 IS NOT NULL OR $10 = 'finished' THEN NULL ELSE lease_token END,
                 lease_expires_at = CASE WHEN $11 IS NOT NULL OR $10 = 'finished' THEN NULL ELSE lease_expires_at END
             WHERE organization_id = $1 AND id = $2 AND lease_token = $13",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .bind(change.input_file_id)
        .bind(change.provider_batch_id)
        .bind(change.provider_status)
        .bind(change.output_file_id)
        .bind(change.error_file_id)
        .bind(change.result_object_key)
        .bind(change.error_code)
        .bind(change.state)
        .bind(change.next_run_at)
        .bind(change.progressed)
        .bind(job.lease_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn finish_line(
        &self,
        organization_id: Uuid,
        batch_id: Uuid,
        line_number: i32,
        state: &str,
        status_code: Option<i32>,
        error_code: Option<&str>,
    ) -> Result<(), BatchStoreError> {
        sqlx::query(
            "UPDATE gateway.batch_line SET state=$4, status_code=$5, error_code=$6, finished_at=CURRENT_TIMESTAMP
             WHERE organization_id=$1 AND batch_id=$2 AND line_number=$3 AND state='pending'",
        )
        .bind(organization_id)
        .bind(batch_id)
        .bind(line_number)
        .bind(state)
        .bind(status_code)
        .bind(error_code)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Leases batches whose every job has finished and that have not ended
    /// yet, so one worker writes each one's output files.
    pub async fn lease_finished_batches(
        &self,
        now: DateTime<Utc>,
        lease: chrono::Duration,
        limit: i64,
    ) -> Result<Vec<(BatchRow, Uuid)>, BatchStoreError> {
        let token = Uuid::new_v4();
        let rows = sqlx::query_as::<_, BatchRow>(&format!(
            "UPDATE gateway.batch SET lease_token=$3, lease_expires_at=$2,
                 status = CASE WHEN status='in_progress' THEN 'finalizing' ELSE status END,
                 finalizing_at = COALESCE(finalizing_at, $1)
             WHERE id IN (
                 SELECT b.id FROM gateway.batch b
                 WHERE b.status IN ('in_progress','finalizing','cancelling')
                   AND (b.lease_expires_at IS NULL OR b.lease_expires_at <= $1)
                   AND NOT EXISTS (SELECT 1 FROM gateway.batch_job j
                                   WHERE j.organization_id = b.organization_id AND j.batch_id = b.id
                                     AND j.state <> 'finished')
                 ORDER BY b.created_at LIMIT $4 FOR UPDATE SKIP LOCKED)
             RETURNING {BATCH_COLUMNS}"
        ))
        .bind(now)
        .bind(now + lease)
        .bind(token)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|row| (row, token)).collect())
    }

    pub async fn batch_lines(
        &self,
        batch: &BatchRow,
    ) -> Result<Vec<FinishedLine>, BatchStoreError> {
        Ok(sqlx::query_as::<_, FinishedLine>(
            "SELECT line_number, custom_id, job_id, state FROM gateway.batch_line
             WHERE organization_id = $1 AND batch_id = $2 ORDER BY line_number",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// The jobs of a batch: id, result object and provider status.
    pub async fn batch_jobs(
        &self,
        batch: &BatchRow,
    ) -> Result<Vec<(Uuid, Option<String>, Option<String>)>, BatchStoreError> {
        Ok(sqlx::query_as(
            "SELECT id, result_object_key, provider_status FROM gateway.batch_job
             WHERE organization_id = $1 AND batch_id = $2 ORDER BY ordinal",
        )
        .bind(batch.organization_id)
        .bind(batch.id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Jobs of ended batches whose saved results are still stored:
    /// organization, job and object. Their content is in the batch's output
    /// files, and nothing reads them again.
    pub async fn ended_job_results(
        &self,
        limit: i64,
    ) -> Result<Vec<(Uuid, Uuid, String)>, BatchStoreError> {
        Ok(sqlx::query_as(
            "SELECT j.organization_id, j.id, j.result_object_key FROM gateway.batch_job j
             JOIN gateway.batch b ON b.organization_id = j.organization_id AND b.id = j.batch_id
             WHERE j.result_object_key IS NOT NULL
               AND b.status IN ('completed', 'failed', 'expired', 'cancelled')
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Records that a job's saved results were deleted.
    pub async fn clear_job_result(
        &self,
        organization_id: Uuid,
        job_id: Uuid,
        key: &str,
    ) -> Result<(), BatchStoreError> {
        sqlx::query(
            "UPDATE gateway.batch_job SET result_object_key = NULL
             WHERE organization_id = $1 AND id = $2 AND result_object_key = $3",
        )
        .bind(organization_id)
        .bind(job_id)
        .bind(key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ends a leased batch: its output files, counts and final status.
    pub async fn finalize(
        &self,
        batch: &BatchRow,
        token: Uuid,
        end: BatchFinal,
        now: DateTime<Utc>,
    ) -> Result<bool, BatchStoreError> {
        let mut tx = self.pool.begin().await?;
        for file in [&end.output, &end.errors].into_iter().flatten() {
            sqlx::query("INSERT INTO gateway.batch_file (id,organization_id,service_account_id,purpose,filename,bytes,sha256,object_key,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (id) DO NOTHING")
                .bind(file.id)
                .bind(file.organization_id)
                .bind(file.service_account_id)
                .bind(&file.purpose)
                .bind(&file.filename)
                .bind(file.bytes)
                .bind(&file.sha256)
                .bind(&file.object_key)
                .bind(file.created_at)
                .execute(&mut *tx)
                .await?;
        }
        let column = match end.status {
            "completed" => "completed_at",
            "failed" => "failed_at",
            "expired" => "expired_at",
            _ => "cancelled_at",
        };
        let updated = sqlx::query(&format!(
            "UPDATE gateway.batch SET status=$3, {column}=$4, completed_lines=$5, failed_lines=$6,
                 output_file_id=$7, error_file_id=$8, lease_token=NULL, lease_expires_at=NULL
             WHERE organization_id=$1 AND id=$2 AND lease_token=$9
               AND status IN ('in_progress','finalizing','cancelling')"
        ))
        .bind(batch.organization_id)
        .bind(batch.id)
        .bind(end.status)
        .bind(now)
        .bind(end.completed)
        .bind(end.failed)
        .bind(end.output.as_ref().map(|f| f.id))
        .bind(end.errors.as_ref().map(|f| f.id))
        .bind(token)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }
}

/// One provider job of a batch, as the batch's detail shows it.
#[derive(Clone, Debug, FromRow, serde::Serialize)]
pub struct JobSummary {
    pub ordinal: i32,
    pub deployment: String,
    pub model: String,
    pub family: String,
    pub mode: String,
    pub state: String,
    pub provider_status: Option<String>,
    pub provider_batch_id: Option<String>,
    pub error_code: Option<String>,
    pub lines: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub pending: i64,
    pub unknown: i64,
    pub submitted_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// What a batch has cost so far and still holds, in one currency: amounts
/// in different currencies are never summed.
#[derive(Clone, Debug, FromRow, serde::Serialize)]
pub struct BatchCost {
    pub currency: String,
    pub consumed_minor: i64,
    pub reserved_minor: i64,
}

/// What [`BatchStore::send_group`] found.
#[derive(Debug, PartialEq)]
pub enum SendGroup {
    /// These lines are marked sent; none means none is left to send.
    Sent(Vec<i32>),
    /// The rate limits leave no room now.
    NoRoom,
    /// The batch was cancelled since the job was leased.
    Cancelled,
    /// The batch's window has ended.
    Expired,
    /// Another worker holds the job.
    Lost,
}

/// A rate-limit policy covering an emulated job's batch.
#[derive(FromRow)]
struct LimitRow {
    scope_kind: String,
    requests_per_minute: Option<i32>,
    max_concurrent: Option<i32>,
}

impl BatchStore {
    /// Takes the next group of an emulated job's lines to send: up to
    /// `wanted` that are pending and unsent, in line order, as many as the
    /// rate limits covering its batch (organization, team, service
    /// account and the key it was created with) leave room for, marked sent before any request goes out,
    /// so none is ever sent twice. One transaction that first locks the
    /// scope's rate-limit policies, as ordinary admission does and in its
    /// order, so two workers on one organization's jobs count each other's
    /// lines rather than both taking the last room. The job's cancellation
    /// and its batch's expiry are read as they are now, not as leased.
    pub async fn send_group(
        &self,
        job: &LeasedJob,
        wanted: u32,
    ) -> Result<SendGroup, BatchStoreError> {
        let mut tx = self.pool.begin().await?;
        let policies = sqlx::query_as::<_, LimitRow>(
            "SELECT scope_kind, requests_per_minute, max_concurrent
             FROM gateway.lock_rate_limit_policies($1, $2, $3, $4)",
        )
        .bind(job.organization_id)
        .bind(job.team_id)
        .bind(job.service_account_id)
        .bind(job.api_key_id)
        .fetch_all(&mut *tx)
        .await?;
        let current: Option<(bool, bool)> = sqlx::query_as(
            "SELECT j.cancel_requested, b.expires_at <= clock_timestamp()
             FROM gateway.batch_job j
             JOIN gateway.batch b ON b.organization_id = j.organization_id AND b.id = j.batch_id
             WHERE j.organization_id = $1 AND j.id = $2 AND j.lease_token = $3
             -- No other worker can lease the job until the group is marked.
             FOR SHARE OF j",
        )
        .bind(job.organization_id)
        .bind(job.id)
        .bind(job.lease_token)
        .fetch_optional(&mut *tx)
        .await?;
        let group = match current {
            None => SendGroup::Lost,
            Some((true, _)) => SendGroup::Cancelled,
            Some((_, true)) => SendGroup::Expired,
            Some(_) => {
                let room = Self::headroom(&mut tx, job, &policies, wanted).await?;
                if room == 0 {
                    SendGroup::NoRoom
                } else {
                    SendGroup::Sent(
                        sqlx::query_scalar(
                            "WITH picked AS MATERIALIZED (
                                 SELECT line_number FROM gateway.batch_line
                                 WHERE organization_id = $1 AND batch_id = $2 AND job_id = $3
                                   AND state = 'pending' AND sent_at IS NULL
                                 ORDER BY line_number LIMIT $4 FOR UPDATE
                             )
                             UPDATE gateway.batch_line l SET sent_at = clock_timestamp()
                             FROM picked
                             WHERE l.organization_id = $1 AND l.batch_id = $2
                               AND l.line_number = picked.line_number
                             RETURNING l.line_number",
                        )
                        .bind(job.organization_id)
                        .bind(job.batch_id)
                        .bind(job.id)
                        .bind(i64::from(room))
                        .fetch_all(&mut *tx)
                        .await?,
                    )
                }
            }
        };
        tx.commit().await?;
        Ok(group)
    }

    /// Clears a line's sent mark: the provider refused it before doing any
    /// work, so it is sent again later. Only while this worker still holds
    /// the job, as for marking.
    pub async fn unsend_line(&self, job: &LeasedJob, line: i32) -> Result<(), BatchStoreError> {
        sqlx::query(
            "UPDATE gateway.batch_line SET sent_at = NULL
             WHERE organization_id = $1 AND batch_id = $2 AND line_number = $3
               AND state = 'pending' AND job_id = $4
               AND EXISTS (SELECT 1 FROM gateway.batch_job
                           WHERE organization_id = $1 AND id = $4 AND lease_token = $5)",
        )
        .bind(job.organization_id)
        .bind(job.batch_id)
        .bind(line)
        .bind(job.id)
        .bind(job.lease_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks a line that will never be sent as if it were, once its end is
    /// saved, so no later run sends it.
    pub async fn mark_sent(&self, job: &LeasedJob, line: i32) -> Result<(), BatchStoreError> {
        sqlx::query(
            "UPDATE gateway.batch_line SET sent_at = COALESCE(sent_at, clock_timestamp())
             WHERE organization_id = $1 AND batch_id = $2 AND line_number = $3 AND job_id = $4",
        )
        .bind(job.organization_id)
        .bind(job.batch_id)
        .bind(line)
        .bind(job.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// How many of `wanted` lines an emulated job may send now within
    /// `policies`. Admissions in the last minute count ordinary requests,
    /// batches and emulated lines sent; in flight count ordinary requests
    /// and emulated lines sent but not ended. Ordinary admission does not
    /// count emulated lines, so an emulated batch only takes the headroom
    /// the limits leave. Lines count no tokens toward a tokens-per-minute
    /// limit, as the batch did not when it was created.
    async fn headroom(
        conn: &mut sqlx::PgConnection,
        job: &LeasedJob,
        policies: &[LimitRow],
        wanted: u32,
    ) -> Result<u32, BatchStoreError> {
        let mut headroom = i64::from(wanted);
        for policy in policies {
            let filter = |alias: &str| match policy.scope_kind.as_str() {
                "organization" => format!("{alias}organization_id = $1"),
                "team" => format!("{alias}organization_id = $1 AND {alias}team_id = $2"),
                "api-key" => format!("{alias}organization_id = $1 AND {alias}api_key_id = $4"),
                _ => format!("{alias}organization_id = $1 AND {alias}service_account_id = $3"),
            };
            if let Some(limit) = policy.requests_per_minute {
                let admitted: i64 = sqlx::query_scalar(&format!(
                    "SELECT
                         (SELECT count(*) FROM gateway.accounting_reservation
                          WHERE {r} AND batch_id IS NULL
                            AND created_at > clock_timestamp() - interval '60 seconds')
                       + (SELECT count(*) FROM gateway.batch
                          WHERE {r} AND created_at > clock_timestamp() - interval '60 seconds')
                       + (SELECT count(*) FROM gateway.batch_line l
                          JOIN gateway.batch b ON b.organization_id = l.organization_id
                                              AND b.id = l.batch_id
                          WHERE {b} AND l.organization_id = $1
                            AND l.sent_at > clock_timestamp() - interval '60 seconds')",
                    r = filter(""),
                    b = filter("b."),
                ))
                .bind(job.organization_id)
                .bind(job.team_id)
                .bind(job.service_account_id)
                .bind(job.api_key_id)
                .fetch_one(&mut *conn)
                .await?;
                headroom = headroom.min(i64::from(limit) - admitted);
            }
            if let Some(limit) = policy.max_concurrent {
                let in_flight: i64 = sqlx::query_scalar(&format!(
                    "SELECT
                         (SELECT count(*) FROM gateway.accounting_reservation
                          WHERE {r} AND batch_id IS NULL
                            AND state IN ('reserved', 'claimed') AND expires_at > clock_timestamp())
                       + (SELECT count(*) FROM gateway.batch_line l
                          JOIN gateway.batch b ON b.organization_id = l.organization_id
                                              AND b.id = l.batch_id
                          WHERE {b} AND l.organization_id = $1
                            AND l.sent_at IS NOT NULL AND l.state = 'pending')",
                    r = filter(""),
                    b = filter("b."),
                ))
                .bind(job.organization_id)
                .bind(job.team_id)
                .bind(job.service_account_id)
                .bind(job.api_key_id)
                .fetch_one(&mut *conn)
                .await?;
                headroom = headroom.min(i64::from(limit) - in_flight);
            }
        }
        Ok(u32::try_from(headroom.max(0)).unwrap_or(0))
    }

    /// Whether the organization allows emulated batches, and how many lines
    /// of one emulated job may be in flight.
    pub async fn emulation(&self, organization_id: Uuid) -> Result<(bool, u32), BatchStoreError> {
        let row: Option<(bool, i32)> = sqlx::query_as(
            "SELECT emulated_allowed, emulated_concurrency
             FROM gateway.organization_batch_settings WHERE organization_id = $1",
        )
        .bind(organization_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map_or(
            (false, crate::domain::batch::DEFAULT_EMULATED_CONCURRENCY),
            |(allowed, concurrency)| (allowed, u32::try_from(concurrency).unwrap_or(1)),
        ))
    }

    /// The organization's batches, newest first, after the batch `after`
    /// names, optionally of one service account or in one status.
    pub async fn admin_list(
        &self,
        organization_id: Uuid,
        service_account_id: Option<Uuid>,
        status: Option<&str>,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<BatchRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 \
             AND ($2::uuid IS NULL OR service_account_id = $2) \
             AND ($3::text IS NULL OR status = $3) \
             AND ($4::uuid IS NULL OR (created_at, id) < (SELECT created_at, id FROM gateway.batch WHERE organization_id=$1 AND id=$4)) \
             ORDER BY created_at DESC, id DESC LIMIT $5"
        ))
        .bind(organization_id)
        .bind(service_account_id)
        .bind(status)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Any batch of the organization.
    pub async fn admin_batch(
        &self,
        organization_id: Uuid,
        id: Uuid,
    ) -> Result<Option<BatchRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchRow>(&format!(
            "SELECT {BATCH_COLUMNS} FROM gateway.batch WHERE organization_id=$1 AND id=$2"
        ))
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// A file of the organization, whoever owns it.
    pub async fn admin_file(
        &self,
        organization_id: Uuid,
        id: Uuid,
    ) -> Result<Option<BatchFileRow>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchFileRow>(&format!(
            "SELECT {FILE_COLUMNS} FROM gateway.batch_file WHERE organization_id=$1 AND id=$2 AND deleted_at IS NULL"
        ))
        .bind(organization_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The batch's provider jobs with their lines' outcomes so far.
    pub async fn job_summaries(
        &self,
        organization_id: Uuid,
        batch_id: Uuid,
    ) -> Result<Vec<JobSummary>, BatchStoreError> {
        Ok(sqlx::query_as::<_, JobSummary>(
            "SELECT j.ordinal, j.deployment, j.model, j.family, j.mode, j.state,
                    j.provider_status, j.provider_batch_id, j.error_code,
                    count(l.line_number) AS lines,
                    count(l.line_number) FILTER (WHERE l.state = 'succeeded') AS succeeded,
                    count(l.line_number) FILTER (WHERE l.state IN ('failed', 'expired', 'cancelled')) AS failed,
                    count(l.line_number) FILTER (WHERE l.state = 'pending') AS pending,
                    count(l.line_number) FILTER (WHERE l.state = 'unknown') AS unknown,
                    j.submitted_at, j.finished_at
             FROM gateway.batch_job j
             LEFT JOIN gateway.batch_line l
               ON l.organization_id = j.organization_id AND l.job_id = j.id
             WHERE j.organization_id = $1 AND j.batch_id = $2
             GROUP BY j.id ORDER BY j.ordinal",
        )
        .bind(organization_id)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// What the batch has cost so far and still holds, per currency. A
    /// reservation holds its amount while any of its allocations is held,
    /// whatever its state: one whose outcome is unknown keeps its budget
    /// until reconciliation settles it. Each budget allocation holds the
    /// whole amount, so a reservation counts once, not once per budget. A
    /// reservation with no allocations (no budget applies) holds until it
    /// is settled, released or expired.
    pub async fn costs(
        &self,
        organization_id: Uuid,
        batch_id: Uuid,
    ) -> Result<Vec<BatchCost>, BatchStoreError> {
        Ok(sqlx::query_as::<_, BatchCost>(
            "SELECT r.currency::text AS currency,
                    coalesce(sum(u.amount_minor), 0)::bigint AS consumed_minor,
                    coalesce(sum(r.reserved_minor) FILTER (
                        WHERE a.held OR (a.allocations = 0
                                         AND r.state IN ('reserved', 'claimed', 'unknown'))
                    ), 0)::bigint AS reserved_minor
             FROM gateway.accounting_reservation r
             LEFT JOIN LATERAL (
                 SELECT sum(amount_minor) AS amount_minor FROM gateway.accounting_usage
                 WHERE organization_id = r.organization_id AND reservation_id = r.id
             ) u ON true
             CROSS JOIN LATERAL (
                 SELECT count(*) AS allocations, coalesce(bool_or(state = 'held'), false) AS held
                 FROM gateway.accounting_allocation
                 WHERE organization_id = r.organization_id AND reservation_id = r.id
             ) a
             WHERE r.organization_id = $1 AND r.batch_id = $2
             GROUP BY r.currency ORDER BY r.currency",
        )
        .bind(organization_id)
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await?)
    }
}

/// What a worker run changes on its job; `None` leaves a field as it is.
#[derive(Clone, Debug, Default)]
pub struct JobChange<'a> {
    pub input_file_id: Option<&'a str>,
    pub provider_batch_id: Option<&'a str>,
    pub provider_status: Option<&'a str>,
    pub output_file_id: Option<&'a str>,
    pub error_file_id: Option<&'a str>,
    pub result_object_key: Option<&'a str>,
    pub error_code: Option<&'a str>,
    /// `submitted` or `finished`.
    pub state: Option<&'static str>,
    /// When to run next; setting it ends the lease.
    pub next_run_at: Option<DateTime<Utc>>,
    /// The run moved the job on: its retry count starts again.
    pub progressed: bool,
}
