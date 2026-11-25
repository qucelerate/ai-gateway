//! Notifications: raising and resolving them (for the code that notices
//! what they report), and each member's list, unread count and read state.
//!
//! Raising is idempotent per subject. An open condition raised again keeps
//! its id, creation time and everyone's read state and takes the new text;
//! an event raised again for the same subject is kept as it was.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{
    domain::{
        identity::{OrganizationId, UserAccountId},
        notifications::{
            MarkReadTarget, NewNotification, NotificationCreated, NotificationFilter,
            NotificationKind, NotificationView, ReadFilter, Severity, permission_name,
        },
        outbox::OutboxPayload,
    },
    persistence::outbox,
};

/// Events, and conditions once resolved, are kept this long.
pub const RETENTION_DAYS: i64 = 90;

/// A notification to raise or resolve with a recorded result, in the same
/// transaction, so notifications follow the order results were recorded in:
/// an older result can never overwrite what a newer one notified.
#[derive(Clone, Debug)]
pub enum NotificationChange {
    Raise(OrganizationId, NewNotification),
    RaiseForCatalogOrganizations(NewNotification),
    Resolve(OrganizationId, NotificationKind, String),
    ResolveEverywhere(NotificationKind, String),
}

impl NotificationChange {
    /// Applies the change on the caller's transaction, under a savepoint: a
    /// change that fails is rolled back and logged, and the caller's own
    /// writes stand. Only an error that leaves the transaction unusable (the
    /// savepoint itself failing) is returned.
    pub async fn apply_best_effort(
        &self,
        conn: &mut PgConnection,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SAVEPOINT notification_change")
            .execute(&mut *conn)
            .await?;
        let applied = match self {
            Self::Raise(organization_id, notification) => {
                NotificationStore::raise_on(conn, *organization_id, notification.clone(), now)
                    .await
                    .map(|_| ())
            }
            Self::RaiseForCatalogOrganizations(notification) => {
                NotificationStore::raise_for_catalog_organizations_on(
                    conn,
                    notification.clone(),
                    now,
                )
                .await
                .map(|_| ())
            }
            Self::Resolve(organization_id, kind, subject) => {
                NotificationStore::resolve_on(conn, *organization_id, *kind, subject, now)
                    .await
                    .map(|_| ())
            }
            Self::ResolveEverywhere(kind, subject) => {
                NotificationStore::resolve_everywhere_on(conn, *kind, subject, now)
                    .await
                    .map(|_| ())
            }
        };
        match applied {
            Ok(()) => {
                sqlx::query("RELEASE SAVEPOINT notification_change")
                    .execute(&mut *conn)
                    .await?;
            }
            Err(error) => {
                tracing::warn!(change = ?self, error = %error, "notification change failed");
                sqlx::query("ROLLBACK TO SAVEPOINT notification_change")
                    .execute(&mut *conn)
                    .await?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct NotificationStore {
    pool: PgPool,
}

/// `true` when the member has read `n` (a notification row aliased `n`).
const READ: &str = "(EXISTS (SELECT 1 FROM gateway.notification_read r
                            WHERE r.notification_id = n.id AND r.user_account_id = $2)
         OR EXISTS (SELECT 1 FROM gateway.notification_read_mark m
                    WHERE m.organization_id = n.organization_id AND m.user_account_id = $2
                      AND (n.created_at, n.id) <= (m.through_created_at, m.through_id)))";

impl NotificationStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Raises `notification` in the organization, on the caller's
    /// transaction. Returns its id. A notification newly written (a
    /// condition opened, or an event recorded the first time) is also put
    /// on the outbox as `notification.created`, in the same transaction, for
    /// the organization's webhooks.
    pub async fn raise_on(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        notification: NewNotification,
        now: DateTime<Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let n = notification.bounded();
        let conflict = if n.kind.resolvable() {
            "ON CONFLICT (organization_id, kind, subject) WHERE resolvable AND resolved_at IS NULL
             DO UPDATE SET title = EXCLUDED.title, body = EXCLUDED.body, link = EXCLUDED.link,
                           data = EXCLUDED.data, updated_at = EXCLUDED.updated_at"
        } else {
            // A no-op update, so the kept event's id is returned.
            "ON CONFLICT (organization_id, kind, subject) WHERE NOT resolvable
             DO UPDATE SET kind = gateway.notification.kind"
        };
        let permission = permission_name(n.kind.permission());
        // `xmax = 0` only for a row this statement inserted.
        let (id, inserted, created_at) =
            sqlx::query_as::<_, (Uuid, bool, DateTime<Utc>)>(&format!(
                "INSERT INTO gateway.notification
                     (id, organization_id, kind, subject, severity, resolvable, title, body, link,
                      required_permission, data, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12)
                 {conflict}
                 RETURNING id, (xmax = 0), created_at"
            ))
            .bind(Uuid::new_v4())
            .bind(organization_id.0)
            .bind(n.kind.as_str())
            .bind(&n.subject)
            .bind(n.kind.severity().as_str())
            .bind(n.kind.resolvable())
            .bind(&n.title)
            .bind(&n.body)
            .bind(&n.link)
            .bind(&permission)
            .bind(&n.data)
            .bind(now)
            .fetch_one(&mut *conn)
            .await?;
        if inserted {
            let payload = OutboxPayload::NotificationCreated(NotificationCreated {
                notification_id: id,
                kind: n.kind.as_str().into(),
                severity: n.kind.severity(),
                subject: n.subject,
                title: n.title,
                body: n.body,
                link: n.link,
                data: n.data,
                required_permission: permission,
                created_at,
            });
            outbox::emit(conn, organization_id, &payload)
                .await
                .map_err(|error| match error {
                    outbox::OutboxError::Database(error) => error,
                    outbox::OutboxError::Encode(error) => sqlx::Error::Encode(Box::new(error)),
                })?;
        }
        Ok(id)
    }

    /// Raises `notification` in one organization.
    pub async fn raise(
        &self,
        organization_id: OrganizationId,
        notification: NewNotification,
        now: DateTime<Utc>,
    ) -> Result<Uuid, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let id = Self::raise_on(&mut tx, organization_id, notification, now).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Raises `notification` in every organization that uses the catalog
    /// (it has an active release) and is neither suspended nor deleted: for
    /// what is the same for all, such as a price source. Returns how many
    /// organizations it was raised in.
    pub async fn raise_for_catalog_organizations(
        &self,
        notification: NewNotification,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let raised = Self::raise_for_catalog_organizations_on(&mut tx, notification, now).await?;
        tx.commit().await?;
        Ok(raised)
    }

    /// [`Self::raise_for_catalog_organizations`], on the caller's
    /// transaction.
    pub async fn raise_for_catalog_organizations_on(
        conn: &mut PgConnection,
        notification: NewNotification,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let organizations = sqlx::query_scalar::<_, Uuid>(
            "SELECT o.id FROM gateway.organization o
             JOIN gateway.catalog_active_release a ON a.organization_id = o.id
             WHERE o.suspended_at IS NULL AND o.deleted_at IS NULL",
        )
        .fetch_all(&mut *conn)
        .await?;
        for organization in &organizations {
            Self::raise_on(
                &mut *conn,
                OrganizationId(*organization),
                notification.clone(),
                now,
            )
            .await?;
        }
        Ok(organizations.len() as u64)
    }

    /// Resolves the open condition of `kind` about `subject`, on the
    /// caller's transaction. `false` when none was open.
    pub async fn resolve_on(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        kind: NotificationKind,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway.notification SET resolved_at = $4, updated_at = $4
             WHERE organization_id = $1 AND kind = $2 AND subject = $3
               AND resolvable AND resolved_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(kind.as_str())
        .bind(subject)
        .bind(now)
        .execute(conn)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn resolve(
        &self,
        organization_id: OrganizationId,
        kind: NotificationKind,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::resolve_on(&mut conn, organization_id, kind, subject, now).await
    }

    /// Resolves the open conditions of `kind` about `subject` in every
    /// organization. Returns how many were resolved.
    pub async fn resolve_everywhere(
        &self,
        kind: NotificationKind,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::resolve_everywhere_on(&mut conn, kind, subject, now).await
    }

    /// [`Self::resolve_everywhere`], on the caller's transaction.
    pub async fn resolve_everywhere_on(
        conn: &mut PgConnection,
        kind: NotificationKind,
        subject: &str,
        now: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway.notification SET resolved_at = $3, updated_at = $3
             WHERE kind = $1 AND subject = $2 AND resolvable AND resolved_at IS NULL",
        )
        .bind(kind.as_str())
        .bind(subject)
        .bind(now)
        .execute(conn)
        .await?;
        Ok(result.rows_affected())
    }

    /// One page of the notifications `member` may see, newest first, after
    /// the keyset `after`. The second value says whether more follow.
    pub async fn list(
        &self,
        organization_id: OrganizationId,
        member: UserAccountId,
        permissions: &[String],
        filter: &NotificationFilter,
        page_size: u32,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<(Vec<NotificationView>, bool), sqlx::Error> {
        let read = match filter.read {
            ReadFilter::All => "",
            ReadFilter::Unread => " AND NOT ",
            ReadFilter::Read => " AND ",
        };
        let read_clause = if read.is_empty() {
            String::new()
        } else {
            format!("{read}{READ}")
        };
        let rows = sqlx::query(&format!(
            "SELECT n.id, n.kind, n.severity, n.subject, n.title, n.body, n.link, n.data,
                    n.created_at, n.updated_at, n.resolved_at, {READ} AS read
             FROM gateway.notification n
             WHERE n.organization_id = $1 AND n.required_permission = ANY($3)
               AND ($4::text IS NULL OR n.kind = $4)
               AND ($5::text IS NULL OR n.severity = $5)
               AND ($6::timestamptz IS NULL OR (n.created_at, n.id) < ($6, $7))
               {read_clause}
             ORDER BY n.created_at DESC, n.id DESC
             LIMIT $8"
        ))
        .bind(organization_id.0)
        .bind(member.0)
        .bind(permissions)
        .bind(filter.kind.map(NotificationKind::as_str))
        .bind(filter.severity.map(Severity::as_str))
        .bind(after.map(|(at, _)| at))
        .bind(after.map(|(_, id)| id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > page_size as usize;
        let items = rows
            .iter()
            .take(page_size as usize)
            .map(view)
            .collect::<Result<_, _>>()?;
        Ok((items, has_more))
    }

    /// Unread notifications `member` may see that are not resolved.
    pub async fn unread_count(
        &self,
        organization_id: OrganizationId,
        member: UserAccountId,
        permissions: &[String],
    ) -> Result<u64, sqlx::Error> {
        let count = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT count(*) FROM gateway.notification n
             WHERE n.organization_id = $1 AND n.required_permission = ANY($3)
               AND n.resolved_at IS NULL AND NOT {READ}"
        ))
        .bind(organization_id.0)
        .bind(member.0)
        .bind(permissions)
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Marks notifications `member` may see as read. `false` when `through`
    /// names none of them.
    pub async fn mark_read(
        &self,
        organization_id: OrganizationId,
        member: UserAccountId,
        permissions: &[String],
        target: &MarkReadTarget,
        now: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        match target {
            MarkReadTarget::Through(id) => {
                let Some((created_at,)) = sqlx::query_as::<_, (DateTime<Utc>,)>(
                    "SELECT created_at FROM gateway.notification
                     WHERE organization_id = $1 AND id = $2 AND required_permission = ANY($3)",
                )
                .bind(organization_id.0)
                .bind(id)
                .bind(permissions)
                .fetch_optional(&self.pool)
                .await?
                else {
                    return Ok(false);
                };
                // The mark only moves forward.
                sqlx::query(
                    "INSERT INTO gateway.notification_read_mark
                         (organization_id, user_account_id, through_created_at, through_id,
                          updated_at)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (organization_id, user_account_id) DO UPDATE
                     SET through_created_at = EXCLUDED.through_created_at,
                         through_id = EXCLUDED.through_id, updated_at = EXCLUDED.updated_at
                     WHERE (gateway.notification_read_mark.through_created_at,
                            gateway.notification_read_mark.through_id)
                         < (EXCLUDED.through_created_at, EXCLUDED.through_id)",
                )
                .bind(organization_id.0)
                .bind(member.0)
                .bind(created_at)
                .bind(id)
                .bind(now)
                .execute(&self.pool)
                .await?;
                Ok(true)
            }
            MarkReadTarget::Ids(ids) => {
                // Ids the member may not see, or of another organization, are
                // skipped.
                sqlx::query(
                    "INSERT INTO gateway.notification_read (notification_id, user_account_id,
                                                            read_at)
                     SELECT n.id, $2, $5 FROM gateway.notification n
                     WHERE n.organization_id = $1 AND n.id = ANY($4)
                       AND n.required_permission = ANY($3)
                     ON CONFLICT DO NOTHING",
                )
                .bind(organization_id.0)
                .bind(member.0)
                .bind(permissions)
                .bind(ids)
                .bind(now)
                .execute(&self.pool)
                .await?;
                Ok(true)
            }
        }
    }

    /// Deletes up to `limit` events created, and conditions resolved, before
    /// `cutoff`.
    pub async fn sweep(&self, cutoff: DateTime<Utc>, limit: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.notification WHERE id IN (
                 SELECT id FROM gateway.notification
                 WHERE (resolvable AND resolved_at < $1) OR (NOT resolvable AND created_at < $1)
                 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn view(row: &PgRow) -> Result<NotificationView, sqlx::Error> {
    let severity: String = row.try_get("severity")?;
    Ok(NotificationView {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        severity: Severity::parse(&severity)
            .ok_or_else(|| sqlx::Error::Decode("notification severity".into()))?,
        subject: row.try_get("subject")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        link: row.try_get("link")?,
        data: row.try_get("data")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        resolved_at: row.try_get("resolved_at")?,
        read: row.try_get("read")?,
    })
}
