//! Webhook endpoints and deliveries.
//!
//! [`WebhookFanout`] is the outbox sink: inside the dispatch transaction it
//! records one pending delivery per enabled endpoint subscribed to the event,
//! so fan-out commits exactly once with the event's dispatch mark. Endpoints
//! created after the event occurred never receive it; subscriptions and the
//! enabled flag are otherwise read as they stand at dispatch. The
//! delivery worker then claims due deliveries under a lease, POSTs them
//! outside any transaction, and records the outcome.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::notifications::{NewNotification, NotificationKind};
use crate::domain::{
    credentials::SealedCredential,
    identity::{OrganizationId, UserAccountId},
    outbox::OutboxEventType,
    webhook::{
        DeliveryStatus, ValidEndpoint, ValidUpdate, WebhookDeliveryView, WebhookEndpointView,
    },
};
use crate::persistence::{
    notifications::NotificationChange,
    outbox::{OutboxEvent, OutboxSink, SinkError},
};

const WEBHOOKS_LINK: &str = "/admin/policies/webhooks";

/// Serializes raising and resolving an endpoint's `webhook.failing` until
/// the caller's transaction ends, so two outcomes recorded at once for the
/// same endpoint apply in commit order: a success committing after a
/// parked delivery always sees, and resolves, the notification it raised.
async fn lock_endpoint_notification(
    conn: &mut PgConnection,
    endpoint_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('webhook.failing:' || $1, 0))")
        .bind(endpoint_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// The notification a parked delivery raises. It names the endpoint by host
/// only: a webhook URL's path can carry a secret (chat services put their
/// token there), and notifications are shown to every member who can read
/// webhooks and sent to webhooks themselves.
fn failing_notification(
    delivery: &ClaimedDelivery,
    status_code: Option<u16>,
    error: &str,
) -> NewNotification {
    let host = url::Url::parse(&delivery.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "an endpoint".into());
    let last = match status_code {
        Some(code) => format!("HTTP {code}"),
        None => error.to_string(),
    };
    NewNotification::new(
        NotificationKind::WebhookFailing,
        delivery.endpoint_id.to_string(),
        format!("Webhook deliveries to {host} are failing"),
        format!(
            "A {} delivery to {host} failed every retry and was parked (last: {last}). \
             Fix the endpoint, then redeliver what was parked.",
            delivery.event_type
        ),
    )
    .with_link(WEBHOOKS_LINK)
    .with_data(serde_json::json!({
        "endpoint_id": delivery.endpoint_id,
        "host": host,
        "event_type": delivery.event_type,
        "delivery_id": delivery.id,
        "last_status_code": status_code,
    }))
}

#[derive(Clone)]
pub struct WebhookStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct EndpointRow {
    id: Uuid,
    url: String,
    description: Option<String>,
    event_types: Vec<String>,
    enabled: bool,
    version: Option<i64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<EndpointRow> for WebhookEndpointView {
    fn from(row: EndpointRow) -> Self {
        Self {
            id: row.id,
            url: row.url,
            description: row.description,
            event_types: event_types(&row.event_types),
            enabled: row.enabled,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(FromRow)]
struct DeliveryRow {
    id: Uuid,
    event_id: Uuid,
    event_type: String,
    status: String,
    attempts: i32,
    next_attempt_at: DateTime<Utc>,
    last_attempt_at: Option<DateTime<Utc>>,
    last_status_code: Option<i32>,
    last_error: Option<String>,
    delivered_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl DeliveryRow {
    fn into_view(self) -> Option<WebhookDeliveryView> {
        let status = DeliveryStatus::parse(&self.status)?;
        Some(WebhookDeliveryView {
            id: self.id,
            event_id: self.event_id,
            event_type: OutboxEventType::parse(&self.event_type)?,
            status,
            attempts: self.attempts,
            next_attempt_at: (status == DeliveryStatus::Pending).then_some(self.next_attempt_at),
            last_attempt_at: self.last_attempt_at,
            last_status_code: self.last_status_code,
            last_error: self.last_error,
            delivered_at: self.delivered_at,
            created_at: self.created_at,
        })
    }
}

/// A delivery claimed for one attempt, with what is needed to send it.
#[derive(Clone, FromRow)]
pub struct ClaimedDelivery {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub endpoint_id: Uuid,
    pub url: String,
    pub secret_key_id: String,
    pub secret_ciphertext: Vec<u8>,
    pub event_id: Uuid,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    /// Attempts including this one, across every run; also fences the
    /// outcome write.
    pub attempts: i32,
    /// This attempt's number within the current run, which a redelivery
    /// restarts; backoff and parking count from it.
    pub run_attempt: i32,
}

impl std::fmt::Debug for ClaimedDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedDelivery")
            .field("id", &self.id)
            .field("endpoint_id", &self.endpoint_id)
            .field("event_id", &self.event_id)
            .field("attempts", &self.attempts)
            .field("run_attempt", &self.run_attempt)
            .finish_non_exhaustive()
    }
}

const ENDPOINT_SELECT: &str = "SELECT e.id, e.url, e.description, e.event_types, e.enabled,
        v.version, e.created_at, e.updated_at
    FROM gateway.webhook_endpoint e
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = e.organization_id
     AND v.resource_type = 'webhook_endpoint' AND v.resource_id = e.id";

const DELIVERY_COLUMNS: &str = "id, event_id, event_type, status, attempts, next_attempt_at,
    last_attempt_at, last_status_code, last_error, delivered_at, created_at";

impl WebhookStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One page of endpoints ordered by id, after the `after` keyset. The
    /// flag reports whether more rows follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        after: Option<Uuid>,
    ) -> Result<(Vec<WebhookEndpointView>, bool), sqlx::Error> {
        let rows = sqlx::query_as::<_, EndpointRow>(&format!(
            "{ENDPOINT_SELECT}
             WHERE e.organization_id = $1 AND ($2::uuid IS NULL OR e.id > $2)
             ORDER BY e.id
             LIMIT $3"
        ))
        .bind(organization_id.0)
        .bind(after)
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

    pub async fn get(
        &self,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<WebhookEndpointView>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::view(&mut conn, organization_id, id).await
    }

    /// One endpoint on the caller's connection, so a mutation can read the
    /// row and version its own transaction wrote.
    pub async fn view(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<Option<WebhookEndpointView>, sqlx::Error> {
        let row = sqlx::query_as::<_, EndpointRow>(&format!(
            "{ENDPOINT_SELECT} WHERE e.organization_id = $1 AND e.id = $2"
        ))
        .bind(organization_id.0)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn count(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM gateway.webhook_endpoint WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_one(&mut *conn)
        .await
    }

    pub async fn insert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        endpoint: &ValidEndpoint,
        secret: &SealedCredential,
        actor: UserAccountId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.webhook_endpoint
                 (id, organization_id, url, description, event_types, enabled,
                  secret_key_id, secret_ciphertext, created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(organization_id.0)
        .bind(&endpoint.url)
        .bind(&endpoint.description)
        .bind(event_type_names(&endpoint.event_types))
        .bind(endpoint.enabled)
        .bind(&secret.key_id)
        .bind(&secret.ciphertext)
        .bind(actor.0)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Applies the fields present in `update`. Returns false when the
    /// endpoint does not exist.
    pub async fn update(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        update: &ValidUpdate,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway.webhook_endpoint SET
                 url = COALESCE($3, url),
                 description = CASE WHEN $4 THEN $5 ELSE description END,
                 event_types = COALESCE($6, event_types),
                 enabled = COALESCE($7, enabled),
                 updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(&update.url)
        .bind(update.description.is_some())
        .bind(update.description.clone().flatten())
        .bind(update.event_types.as_deref().map(event_type_names))
        .bind(update.enabled)
        .execute(&mut *conn)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn set_secret(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        secret: &SealedCredential,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway.webhook_endpoint
             SET secret_key_id = $3, secret_ciphertext = $4, updated_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(&secret.key_id)
        .bind(&secret.ciphertext)
        .execute(&mut *conn)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes the endpoint and, by cascade, its deliveries.
    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.webhook_endpoint WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(id)
        .execute(&mut *conn)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// One page of an endpoint's deliveries, newest first, after the
    /// `(created_at, id)` keyset. `None` when the endpoint does not exist.
    pub async fn list_deliveries(
        &self,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        page_size: u32,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<Option<(Vec<WebhookDeliveryView>, bool)>, sqlx::Error> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.webhook_endpoint
                            WHERE organization_id = $1 AND id = $2)",
        )
        .bind(organization_id.0)
        .bind(endpoint_id)
        .fetch_one(&self.pool)
        .await?;
        if !exists {
            return Ok(None);
        }
        let rows = sqlx::query_as::<_, DeliveryRow>(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM gateway.webhook_delivery
             WHERE organization_id = $1 AND endpoint_id = $2
               AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4))
             ORDER BY created_at DESC, id DESC
             LIMIT $5"
        ))
        .bind(organization_id.0)
        .bind(endpoint_id)
        .bind(after.map(|(created_at, _)| created_at))
        .bind(after.map(|(_, id)| id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        Ok(Some((
            rows.into_iter()
                .take(page_size as usize)
                .filter_map(DeliveryRow::into_view)
                .collect(),
            has_more,
        )))
    }

    /// Returns a parked delivery to the queue, due now, for a fresh run of
    /// attempts; its last outcome stays until the next attempt replaces it.
    /// `None` when the delivery is not parked. `attempts` is not reset, so an
    /// attempt that outlived its lease can never match the fence of a later
    /// one.
    pub async fn redeliver(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        endpoint_id: Uuid,
        id: Uuid,
    ) -> Result<Option<WebhookDeliveryView>, sqlx::Error> {
        let row = sqlx::query_as::<_, DeliveryRow>(&format!(
            "UPDATE gateway.webhook_delivery
             SET status = 'pending', attempts_before_run = attempts,
                 next_attempt_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND endpoint_id = $2 AND id = $3 AND status = 'parked'
             RETURNING {DELIVERY_COLUMNS}"
        ))
        .bind(organization_id.0)
        .bind(endpoint_id)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.and_then(DeliveryRow::into_view))
    }

    /// Claims up to `max` due deliveries to enabled endpoints of
    /// organizations that are not suspended, for one attempt each. A claimed delivery is not due again until `lease_until`, so a
    /// worker that stops mid-attempt only delays it.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        max: i64,
    ) -> Result<Vec<ClaimedDelivery>, sqlx::Error> {
        sqlx::query_as(
            "WITH due AS (
                 SELECT d.id FROM gateway.webhook_delivery d
                 JOIN gateway.webhook_endpoint e
                   ON e.organization_id = d.organization_id AND e.id = d.endpoint_id
                 -- A suspended (or deleted) organization's deliveries wait.
                 JOIN gateway.organization o
                   ON o.id = d.organization_id AND o.suspended_at IS NULL
                 WHERE d.status = 'pending' AND d.next_attempt_at <= $1 AND e.enabled
                 ORDER BY d.next_attempt_at
                 LIMIT $3
                 FOR UPDATE OF d SKIP LOCKED
             )
             UPDATE gateway.webhook_delivery d
             SET attempts = d.attempts + 1, next_attempt_at = $2, last_attempt_at = $1
             FROM due, gateway.webhook_endpoint e
             WHERE d.id = due.id
               AND e.organization_id = d.organization_id AND e.id = d.endpoint_id
             RETURNING d.id, d.organization_id, d.endpoint_id, e.url, e.secret_key_id,
                       e.secret_ciphertext, d.event_id, d.event_type, d.payload,
                       d.occurred_at, d.attempts,
                       d.attempts - d.attempts_before_run AS run_attempt",
        )
        .bind(now)
        .bind(lease_until)
        .bind(max)
        .fetch_all(&self.pool)
        .await
    }

    /// Records a successful attempt, and resolves the endpoint's
    /// `webhook.failing` notification. Ignored if the delivery has since
    /// been claimed again or finished.
    pub async fn record_delivered(
        &self,
        delivery: &ClaimedDelivery,
        now: DateTime<Utc>,
        status_code: u16,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE gateway.webhook_delivery
             SET status = 'delivered', delivered_at = $3, last_status_code = $4,
                 last_error = NULL
             WHERE id = $1 AND attempts = $2 AND status = 'pending'",
        )
        .bind(delivery.id)
        .bind(delivery.attempts)
        .bind(now)
        .bind(i32::from(status_code))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 {
            lock_endpoint_notification(&mut tx, delivery.endpoint_id).await?;
            NotificationChange::Resolve(
                OrganizationId(delivery.organization_id),
                NotificationKind::WebhookFailing,
                delivery.endpoint_id.to_string(),
            )
            .apply_best_effort(&mut tx, now)
            .await?;
        }
        tx.commit().await
    }

    /// Records a failed attempt: retried at `retry_at`, or parked when there
    /// is none. Parking raises the endpoint's `webhook.failing`
    /// notification.
    pub async fn record_failed(
        &self,
        delivery: &ClaimedDelivery,
        status_code: Option<u16>,
        error: &str,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE gateway.webhook_delivery
             SET status = CASE WHEN $3::timestamptz IS NULL THEN 'parked' ELSE 'pending' END,
                 next_attempt_at = COALESCE($3, next_attempt_at),
                 last_status_code = $4, last_error = $5
             WHERE id = $1 AND attempts = $2 AND status = 'pending'",
        )
        .bind(delivery.id)
        .bind(delivery.attempts)
        .bind(retry_at)
        .bind(status_code.map(i32::from))
        .bind(error)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated > 0 && retry_at.is_none() {
            lock_endpoint_notification(&mut tx, delivery.endpoint_id).await?;
            NotificationChange::Raise(
                OrganizationId(delivery.organization_id),
                failing_notification(delivery, status_code, error),
            )
            .apply_best_effort(&mut tx, Utc::now())
            .await?;
        }
        tx.commit().await
    }

    /// Deletes up to `max_rows` delivered or parked deliveries created and
    /// last attempted before `cutoff`, so a redelivered one is kept for the
    /// full retention after its last attempt. Pending deliveries are kept
    /// however old they are.
    pub async fn sweep_finished(
        &self,
        cutoff: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.webhook_delivery
             WHERE id IN (
                 SELECT id FROM gateway.webhook_delivery
                 WHERE status <> 'pending' AND created_at < $1
                   AND (last_attempt_at IS NULL OR last_attempt_at < $1)
                 ORDER BY created_at
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

/// Outbox sink that turns each event into deliveries for the organization's
/// subscribed, enabled endpoints. An endpoint added later does not receive
/// events dispatched before it existed.
pub struct WebhookFanout;

#[async_trait]
impl OutboxSink for WebhookFanout {
    async fn deliver(&self, conn: &mut PgConnection, event: &OutboxEvent) -> Result<(), SinkError> {
        let payload = serde_json::to_value(&event.payload)
            .map_err(|_| SinkError::new("event payload could not be encoded"))?;
        sqlx::query(
            "INSERT INTO gateway.webhook_delivery
                 (id, organization_id, endpoint_id, event_id, event_type, payload, occurred_at)
             SELECT gen_random_uuid(), e.organization_id, e.id, $2, $3, $4, $5
             FROM gateway.webhook_endpoint e
             WHERE e.organization_id = $1 AND e.enabled AND $3 = ANY (e.event_types)
               AND e.created_at <= $5
             ON CONFLICT (endpoint_id, event_id) DO NOTHING",
        )
        .bind(event.organization_id.0)
        .bind(event.id)
        .bind(event.event_type().as_str())
        .bind(payload)
        .bind(event.occurred_at)
        .execute(conn)
        .await
        .map_err(|_| SinkError::new("webhook fan-out failed"))?;
        Ok(())
    }
}

fn event_type_names(event_types: &[OutboxEventType]) -> Vec<&'static str> {
    event_types.iter().map(|value| value.as_str()).collect()
}

fn event_types(names: &[String]) -> Vec<OutboxEventType> {
    names
        .iter()
        .filter_map(|name| OutboxEventType::parse(name))
        .collect()
}
