//! Transactional outbox.
//!
//! Accounting emits events on the connection of the transaction that makes
//! the change, so an event exists exactly when its change commits. The
//! dispatcher later hands pending events to a sink. Delivery is at least once
//! and roughly in commit order: `sequence` is assigned at insert, so an event
//! from a transaction that commits late can follow events with a higher
//! sequence. Consumers deduplicate by event id.

use crate::domain::{
    identity::OrganizationId,
    outbox::{OutboxEventType, OutboxPayload},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Acquire, FromRow, PgConnection, PgPool};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Longest `last_error` stored; the column enforces the same limit.
const MAX_ERROR_CHARS: usize = 200;
const BASE_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Error)]
pub enum OutboxError {
    #[error("outbox database error")]
    Database(#[from] sqlx::Error),
    #[error("outbox payload could not be encoded")]
    Encode(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEvent {
    pub id: Uuid,
    pub sequence: i64,
    pub organization_id: OrganizationId,
    pub payload: OutboxPayload,
    pub occurred_at: DateTime<Utc>,
    pub attempts: i32,
}

impl OutboxEvent {
    pub fn event_type(&self) -> OutboxEventType {
        self.payload.event_type()
    }
}

/// Why a sink could not take an event. The reason is stored on the event, so
/// it must be a short operator-facing summary without secrets or payload data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SinkError {
    reason: String,
}

impl SinkError {
    pub fn new(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            reason: reason.chars().take(MAX_ERROR_CHARS).collect(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Consumer of outbox events. `deliver` runs inside a savepoint of the
/// dispatch transaction: database writes it makes on `conn` commit together
/// with the event being marked dispatched, and are rolled back if it fails.
/// The dispatch transaction holds its batch locked while sinks run, so sinks
/// should only write to the database and leave slow work such as network
/// calls to their own queue.
#[async_trait]
pub trait OutboxSink: Send + Sync {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError>;
}

/// Hands each event to every sink in turn, in one savepoint: the event is
/// dispatched only if every sink takes it, and is otherwise retried with
/// all of them, so each sink must tolerate seeing it again.
pub struct Sinks<'a>(pub &'a [&'a dyn OutboxSink]);

#[async_trait]
impl OutboxSink for Sinks<'_> {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        for sink in self.0 {
            sink.deliver(conn, event).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DispatchReport {
    pub dispatched: usize,
    pub failed: usize,
}

/// Records an event on the caller's connection, normally inside the
/// transaction that makes the change the event describes.
pub async fn emit(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    payload: &OutboxPayload,
) -> Result<Uuid, OutboxError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.outbox_event (id, organization_id, event_type, payload)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(organization_id.0)
    .bind(payload.event_type().as_str())
    .bind(serde_json::to_value(payload)?)
    .execute(conn)
    .await?;
    Ok(id)
}

/// Delay before the next attempt once an event has failed `attempts` times.
pub fn backoff(attempts: i32) -> Duration {
    let exponent = u32::try_from(attempts.saturating_sub(1))
        .unwrap_or(0)
        .min(16);
    BASE_BACKOFF.saturating_mul(1 << exponent).min(MAX_BACKOFF)
}

#[derive(Clone)]
pub struct OutboxStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct EventRow {
    id: Uuid,
    sequence: i64,
    organization_id: Uuid,
    event_type: String,
    payload: serde_json::Value,
    occurred_at: DateTime<Utc>,
    attempts: i32,
}

impl OutboxStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Hands up to `max_events` due events to `sink`, oldest first. Events
    /// already claimed by a concurrent dispatcher are skipped. A failed event
    /// is kept and retried after [`backoff`]; events are never dropped.
    pub async fn dispatch_batch(
        &self,
        sink: &dyn OutboxSink,
        now: DateTime<Utc>,
        max_events: i64,
    ) -> Result<DispatchReport, OutboxError> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT id, sequence, organization_id, event_type, payload, occurred_at, attempts
             FROM gateway.outbox_event
             WHERE dispatched_at IS NULL AND next_attempt_at <= $1
             ORDER BY sequence
             LIMIT $2
             FOR UPDATE SKIP LOCKED",
        )
        .bind(now)
        .bind(max_events)
        .fetch_all(&mut *tx)
        .await?;
        let mut report = DispatchReport::default();
        for row in rows {
            let outcome = match decode(&row) {
                Ok(event) => {
                    let mut savepoint = tx.begin().await?;
                    match sink.deliver(&mut savepoint, &event).await {
                        Ok(()) => {
                            savepoint.commit().await?;
                            Ok(())
                        }
                        Err(error) => {
                            savepoint.rollback().await?;
                            Err(error)
                        }
                    }
                }
                Err(error) => Err(error),
            };
            match outcome {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE gateway.outbox_event
                         SET dispatched_at = $2, attempts = attempts + 1, last_error = NULL
                         WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                    report.dispatched += 1;
                }
                Err(error) => {
                    let attempts = row.attempts.saturating_add(1);
                    // Back off from when this delivery failed, not from the
                    // start of the batch, so a slow batch cannot leave the
                    // retry already due.
                    let retry_at = now.max(Utc::now())
                        + chrono::Duration::from_std(backoff(attempts))
                            .unwrap_or(chrono::Duration::hours(1));
                    sqlx::query(
                        "UPDATE gateway.outbox_event
                         SET attempts = $2, next_attempt_at = $3, last_error = $4
                         WHERE id = $1",
                    )
                    .bind(row.id)
                    .bind(attempts)
                    .bind(retry_at)
                    .bind(error.reason())
                    .execute(&mut *tx)
                    .await?;
                    report.failed += 1;
                }
            }
        }
        tx.commit().await?;
        Ok(report)
    }

    /// Deletes up to `max_rows` events dispatched before `cutoff`. Pending
    /// events are kept however old they are.
    pub async fn sweep_dispatched(
        &self,
        cutoff: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, OutboxError> {
        let result = sqlx::query(
            "DELETE FROM gateway.outbox_event
             WHERE id IN (
                 SELECT id FROM gateway.outbox_event
                 WHERE dispatched_at IS NOT NULL AND dispatched_at < $1
                 ORDER BY dispatched_at
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED
             )",
        )
        .bind(cutoff)
        .bind(max_rows)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn decode(row: &EventRow) -> Result<OutboxEvent, SinkError> {
    let event_type = OutboxEventType::parse(&row.event_type)
        .ok_or_else(|| SinkError::new("unknown event type"))?;
    let payload = OutboxPayload::from_stored(event_type, row.payload.clone())
        .map_err(|_| SinkError::new("malformed payload"))?;
    Ok(OutboxEvent {
        id: row.id,
        sequence: row.sequence,
        organization_id: OrganizationId(row.organization_id),
        payload,
        occurred_at: row.occurred_at,
        attempts: row.attempts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_five_seconds_up_to_an_hour() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(10), Duration::from_secs(2560));
        assert_eq!(backoff(11), MAX_BACKOFF);
        assert_eq!(backoff(i32::MAX), MAX_BACKOFF);
        assert_eq!(backoff(0), Duration::from_secs(5));
    }

    #[test]
    fn sink_errors_are_truncated_to_the_stored_limit() {
        let error = SinkError::new("é".repeat(500));
        assert_eq!(error.reason().chars().count(), MAX_ERROR_CHARS);
    }
}
