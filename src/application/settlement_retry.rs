//! Automatic settlement retry.
//!
//! A finalization (settlement, or input-count drift reconciliation) that hits
//! a transient database failure is retried with the same finalization id.
//! The store records that id in the finalization's own transaction, so a
//! retry after a lost commit reply replays the recorded outcome instead of
//! writing usage twice.
//!
//! A few quick attempts run inside the request. If they all fail the caller
//! still gets the error, since a settlement must be final before the gateway
//! reports success, and retries continue in the background for up to 15
//! minutes. A settlement that lands after its reservation expired still
//! settles and resolves the reconciliation item recovery opened. Background
//! retries do not survive a restart; those reservations are reconciled as
//! before.

use std::{
    future::Future,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use tokio::time::Instant;

use crate::domain::accounting::ReservationId;
use crate::metrics::{SETTLEMENT_RETRIES, global as metrics};
use crate::persistence::AccountingStoreError;

/// Pauses between the in-request attempts.
const IN_REQUEST_DELAYS: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(400)];
const FIRST_BACKGROUND_DELAY: Duration = Duration::from_secs(1);
const MAX_BACKGROUND_DELAY: Duration = Duration::from_secs(60);
/// How long background retries continue after the in-request attempts.
const BACKGROUND_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Bounds memory during a long outage; beyond it, reservations are left to
/// recovery and reconciliation.
const MAX_BACKGROUND_RETRIES: usize = 10_000;

static BACKGROUND_RETRIES: AtomicUsize = AtomicUsize::new(0);

/// Settlements this process is retrying in the background now.
pub(crate) fn pending() -> usize {
    BACKGROUND_RETRIES.load(Ordering::Relaxed)
}

/// Runs `attempt`, retrying transient failures. Every call of `attempt` must
/// reuse one finalization id.
pub(crate) async fn finalize<F, Fut>(
    reservation_id: ReservationId,
    attempt: F,
) -> Result<(), AccountingStoreError>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AccountingStoreError>> + Send + 'static,
{
    let mut error = match attempt().await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    for delay in IN_REQUEST_DELAYS {
        if !is_transient(&error) {
            return Err(error);
        }
        tokio::time::sleep(delay).await;
        error = match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
    }
    if is_transient(&error) {
        continue_in_background(reservation_id, attempt);
    }
    Err(error)
}

/// Whether a failure may pass on its own: lost connections, pool
/// exhaustion, serialization failures and deadlocks, resource limits,
/// server shutdown or statement cancellation, and a connection that a
/// restarting server answered with something other than the protocol (a
/// PostgreSQL still starting up answers TLS negotiation with a bare error
/// byte, which the driver reports as a protocol error), or whose TLS setup
/// failed. Anything the store decided, such as a conflict or a bound
/// breach, is final.
pub(crate) fn is_transient(error: &AccountingStoreError) -> bool {
    let AccountingStoreError::Database(error) = error else {
        return false;
    };
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::Protocol(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(error) => error.code().is_some_and(|code| {
            ["08", "40", "53", "57", "58"]
                .iter()
                .any(|class| code.starts_with(class))
        }),
        _ => false,
    }
}

fn continue_in_background<F, Fut>(reservation_id: ReservationId, attempt: F)
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AccountingStoreError>> + Send + 'static,
{
    if BACKGROUND_RETRIES.fetch_add(1, Ordering::Relaxed) >= MAX_BACKGROUND_RETRIES {
        BACKGROUND_RETRIES.fetch_sub(1, Ordering::Relaxed);
        metrics().inc(&SETTLEMENT_RETRIES, &["not_scheduled"]);
        tracing::warn!(
            reservation_id = %reservation_id.0,
            "settlement retry not scheduled; too many pending"
        );
        return;
    }
    metrics().inc(&SETTLEMENT_RETRIES, &["scheduled"]);
    tokio::spawn(async move {
        let _slot = Slot;
        let deadline = Instant::now() + BACKGROUND_WINDOW;
        let mut delay = FIRST_BACKGROUND_DELAY;
        loop {
            tokio::time::sleep(delay).await;
            match attempt().await {
                Ok(()) => {
                    metrics().inc(&SETTLEMENT_RETRIES, &["succeeded"]);
                    tracing::info!(reservation_id = %reservation_id.0, "settlement retry succeeded");
                    return;
                }
                Err(error) if is_transient(&error) && Instant::now() + delay < deadline => {
                    delay = (delay * 2).min(MAX_BACKGROUND_DELAY);
                }
                Err(error) => {
                    metrics().inc(&SETTLEMENT_RETRIES, &["abandoned"]);
                    tracing::warn!(
                        reservation_id = %reservation_id.0,
                        error = %error,
                        "settlement retry abandoned; the reservation will be reconciled"
                    );
                    return;
                }
            }
        }
    });
}

/// Releases a background retry slot when its task ends, however it ends.
struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        BACKGROUND_RETRIES.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use uuid::Uuid;

    use super::*;

    /// An attempt that fails with `errors` in turn, then succeeds, and
    /// counts its calls.
    fn scripted(
        errors: Vec<fn() -> AccountingStoreError>,
    ) -> (
        Arc<Mutex<usize>>,
        impl Fn() -> std::future::Ready<Result<(), AccountingStoreError>> + Send + Sync + 'static,
    ) {
        let calls = Arc::new(Mutex::new(0_usize));
        let counter = calls.clone();
        let attempt = move || {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            std::future::ready(match errors.get(*calls - 1) {
                Some(error) => Err(error()),
                None => Ok(()),
            })
        };
        (calls, attempt)
    }

    fn transient() -> AccountingStoreError {
        AccountingStoreError::Database(sqlx::Error::PoolTimedOut)
    }

    fn reservation() -> ReservationId {
        ReservationId(Uuid::new_v4())
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_are_retried_within_the_request() {
        let (calls, attempt) = scripted(vec![transient, transient]);
        finalize(reservation(), attempt).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn decided_failures_are_not_retried() {
        let (calls, attempt) = scripted(vec![|| AccountingStoreError::BoundExceeded]);
        assert!(matches!(
            finalize(reservation(), attempt).await,
            Err(AccountingStoreError::BoundExceeded)
        ));
        tokio::time::sleep(BACKGROUND_WINDOW * 2).await;
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_attempts_report_the_error_then_retry_in_the_background() {
        let (calls, attempt) = scripted(vec![transient; 5]);
        assert!(matches!(
            finalize(reservation(), attempt).await,
            Err(AccountingStoreError::Database(sqlx::Error::PoolTimedOut))
        ));
        assert_eq!(*calls.lock().unwrap(), 3);
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Two more failures in the background, then success.
        assert_eq!(*calls.lock().unwrap(), 6);
        tokio::time::sleep(BACKGROUND_WINDOW).await;
        assert_eq!(*calls.lock().unwrap(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn background_retries_stop_after_the_window() {
        let (calls, attempt) = scripted(vec![transient; 1_000]);
        assert!(finalize(reservation(), attempt).await.is_err());
        tokio::time::sleep(BACKGROUND_WINDOW * 2).await;
        let settled = *calls.lock().unwrap();
        // 3 in the request, then 1s, 2s, ... 32s, and 60s apart until 15
        // minutes have passed.
        assert!((15..=25).contains(&settled), "{settled}");
        tokio::time::sleep(BACKGROUND_WINDOW).await;
        assert_eq!(*calls.lock().unwrap(), settled);
    }

    #[test]
    fn only_database_failures_that_may_pass_are_transient() {
        assert!(is_transient(&transient()));
        assert!(is_transient(&AccountingStoreError::Database(
            sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
        )));
        // What a restarting PostgreSQL answers a new connection with.
        assert!(is_transient(&AccountingStoreError::Database(
            sqlx::Error::Protocol(
                "unexpected response from SSLRequest: 0x00 (sqlx_postgres::connection::tls:95)"
                    .into()
            )
        )));
        assert!(is_transient(&AccountingStoreError::Database(
            sqlx::Error::WorkerCrashed
        )));
        assert!(!is_transient(&AccountingStoreError::Database(
            sqlx::Error::RowNotFound
        )));
        assert!(!is_transient(&AccountingStoreError::Database(
            sqlx::Error::PoolClosed
        )));
        assert!(!is_transient(&AccountingStoreError::SettlementConflict));
        assert!(!is_transient(&AccountingStoreError::NotSettleable));
    }
}
