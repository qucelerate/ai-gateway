use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder, Row};
use thiserror::Error;
use uuid::Uuid;

use crate::domain::identity::OrganizationId;

#[derive(Debug, Error)]
pub enum AuditStoreError {
    #[error("database operation failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Every list filter is ANDed; a list matches any of its values (empty: no
/// filter).
#[derive(Clone, Debug, Default)]
pub struct AuditListFilters {
    pub actor_id: Option<Uuid>,
    pub actions: Vec<String>,
    pub target_types: Vec<String>,
    pub target_id: Option<Uuid>,
    pub outcomes: Vec<String>,
    pub actor_types: Vec<String>,
    /// Case-insensitive substring of the action or target type.
    pub search: Option<String>,
    pub occurred_after: Option<DateTime<Utc>>,
    pub occurred_before: Option<DateTime<Utc>>,
    /// Leaves out views (`*.read`, `*.list`).
    pub exclude_views: bool,
}

/// A column audit events can be grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditDimension {
    Action,
    Outcome,
    TargetType,
    ActorType,
    /// The acting user or service account.
    Actor,
    /// The UTC day.
    Day,
}

impl AuditDimension {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "action" => Self::Action,
            "outcome" => Self::Outcome,
            "target_type" => Self::TargetType,
            "actor_type" => Self::ActorType,
            "actor" => Self::Actor,
            "day" => Self::Day,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Outcome => "outcome",
            Self::TargetType => "target_type",
            Self::ActorType => "actor_type",
            Self::Actor => "actor",
            Self::Day => "day",
        }
    }

    fn expression(self) -> &'static str {
        match self {
            Self::Action => "ae.action",
            Self::Outcome => "ae.outcome",
            Self::TargetType => "ae.entity_type",
            Self::ActorType => "ae.actor_type",
            Self::Actor => "COALESCE(ae.actor_user_id, ae.actor_service_account_id)::text",
            Self::Day => "to_char(ae.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')",
        }
    }
}

/// One group of a summary: its value per dimension (in the order asked),
/// how many events it has, and when the first and last occurred.
#[derive(Clone, Debug)]
pub struct AuditGroupRecord {
    pub values: Vec<Option<String>>,
    pub count: i64,
    pub first_occurred_at: DateTime<Utc>,
    pub last_occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, FromRow)]
pub struct AuditEventRecord {
    pub id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub organization_id: Uuid,
    pub actor_type: String,
    pub actor_user_id: Option<Uuid>,
    pub actor_service_account_id: Option<Uuid>,
    pub team_id: Option<Uuid>,
    pub action: String,
    pub entity_type: String,
    pub entity_id: Uuid,
    pub outcome: String,
    pub request_id: Option<Uuid>,
    /// The acting person's, agent's or service account's name, if known.
    pub actor_name: Option<String>,
    /// The name of a team or service account target, if it still exists.
    pub target_name: Option<String>,
}

#[derive(Clone)]
pub struct AuditStore {
    pool: PgPool,
}

impl AuditStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(
        &self,
        organization_id: OrganizationId,
        filters: &AuditListFilters,
        page_size: u32,
        upper_watermark: Option<DateTime<Utc>>,
        after: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<(Vec<AuditEventRecord>, bool, DateTime<Utc>), AuditStoreError> {
        let mut transaction = self.pool.begin().await?;
        let upper_watermark = match upper_watermark {
            Some(value) => value,
            None => {
                sqlx::query_scalar::<_, DateTime<Utc>>("SELECT CURRENT_TIMESTAMP")
                    .fetch_one(&mut *transaction)
                    .await?
            }
        };
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT ae.id, ae.occurred_at, ae.organization_id, ae.actor_type, ",
        );
        query.push("ae.actor_user_id, ae.actor_service_account_id, ae.team_id, ae.action, ");
        query.push("ae.entity_type, ae.entity_id, ae.outcome, ae.request_id, ");
        // Names, so a reader sees who and what rather than UUIDs. Only this
        // organization's teams and service accounts are joined.
        query.push("COALESCE(actor_user.display_name, actor_account.name) AS actor_name, ");
        query.push("COALESCE(target_team.name, target_account.name) AS target_name ");
        query.push("FROM gateway.audit_event ae ");
        query
            .push("LEFT JOIN gateway.user_account actor_user ON actor_user.id = ae.actor_user_id ");
        query.push("LEFT JOIN gateway.service_account actor_account ON actor_account.id = ae.actor_service_account_id AND actor_account.organization_id = ae.organization_id ");
        query.push("LEFT JOIN gateway.team target_team ON ae.entity_type = 'team' AND target_team.id = ae.entity_id AND target_team.organization_id = ae.organization_id ");
        query.push("LEFT JOIN gateway.service_account target_account ON ae.entity_type = 'service_account' AND target_account.id = ae.entity_id AND target_account.organization_id = ae.organization_id ");
        query.push("WHERE ae.organization_id = ");
        query.push_bind(organization_id.0);
        query.push(" AND ae.occurred_at <= ");
        query.push_bind(upper_watermark);

        push_filters(&mut query, filters);
        if let Some((occurred_at, id)) = after {
            query.push(" AND (ae.occurred_at < ");
            query.push_bind(occurred_at);
            query.push(" OR (ae.occurred_at = ");
            query.push_bind(occurred_at);
            query.push(" AND ae.id < ");
            query.push_bind(id);
            query.push("))");
        }

        query.push(" ORDER BY ae.occurred_at DESC, ae.id DESC LIMIT ");
        query.push_bind(i64::from(page_size) + 1);
        let mut rows = query
            .build_query_as::<AuditEventRecord>()
            .fetch_all(&mut *transaction)
            .await?;
        let has_more = rows.len() > page_size as usize;
        if has_more {
            rows.pop();
        }
        transaction.commit().await?;
        Ok((rows, has_more, upper_watermark))
    }

    /// Events matching `filters` grouped by `dimensions`, largest groups
    /// first, at most `limit` groups; with the total across every group (from
    /// the same statement, so the two always agree) and whether more groups
    /// exist.
    pub async fn summarize(
        &self,
        organization_id: OrganizationId,
        filters: &AuditListFilters,
        dimensions: &[AuditDimension],
        limit: u32,
    ) -> Result<(Vec<AuditGroupRecord>, i64, bool), AuditStoreError> {
        let mut query = QueryBuilder::<Postgres>::new("SELECT ");
        for (index, dimension) in dimensions.iter().enumerate() {
            query.push(dimension.expression());
            query.push(format!(" AS d{index}, "));
        }
        query.push(
            "count(*) AS events, min(ae.occurred_at) AS first_at, max(ae.occurred_at) AS last_at, ",
        );
        // Window functions run before LIMIT: every group's events.
        query.push("(sum(count(*)) OVER ())::bigint AS total ");
        query.push("FROM gateway.audit_event ae WHERE ae.organization_id = ");
        query.push_bind(organization_id.0);
        push_filters(&mut query, filters);
        if !dimensions.is_empty() {
            query.push(" GROUP BY ");
            let positions: Vec<String> = (1..=dimensions.len()).map(|n| n.to_string()).collect();
            query.push(positions.join(", "));
        }
        // Ties break on the values, so a truncated summary is deterministic.
        query.push(" ORDER BY events DESC");
        for index in 0..dimensions.len() {
            query.push(format!(", d{index} NULLS LAST"));
        }
        query.push(" LIMIT ");
        query.push_bind(i64::from(limit) + 1);
        let rows = query.build().fetch_all(&self.pool).await?;
        let total = match rows.first() {
            Some(row) => row.try_get::<i64, _>("total")?,
            None => 0,
        };
        let mut groups = Vec::with_capacity(rows.len());
        for row in &rows {
            let count: i64 = row.try_get("events")?;
            // An organization with no matching events still yields one
            // ungrouped row of zero, with no times.
            if count == 0 {
                continue;
            }
            let mut values = Vec::with_capacity(dimensions.len());
            for index in 0..dimensions.len() {
                values.push(row.try_get::<Option<String>, _>(format!("d{index}").as_str())?);
            }
            groups.push(AuditGroupRecord {
                values,
                count,
                first_occurred_at: row.try_get("first_at")?,
                last_occurred_at: row.try_get("last_at")?,
            });
        }
        let truncated = groups.len() > limit as usize;
        groups.truncate(limit as usize);
        Ok((groups, total, truncated))
    }
}

fn push_filters(query: &mut QueryBuilder<'_, Postgres>, filters: &AuditListFilters) {
    if let Some(actor_id) = filters.actor_id {
        query.push(" AND (ae.actor_user_id = ");
        query.push_bind(actor_id);
        query.push(" OR ae.actor_service_account_id = ");
        query.push_bind(actor_id);
        query.push(")");
    }
    for (column, values) in [
        ("ae.action", &filters.actions),
        ("ae.entity_type", &filters.target_types),
        ("ae.outcome", &filters.outcomes),
        ("ae.actor_type", &filters.actor_types),
    ] {
        if !values.is_empty() {
            query.push(format!(" AND {column} = ANY("));
            query.push_bind(values.clone());
            query.push(")");
        }
    }
    if let Some(target_id) = filters.target_id {
        query.push(" AND ae.entity_id = ");
        query.push_bind(target_id);
    }
    if let Some(search) = filters.search.as_deref() {
        let pattern = format!("%{}%", escape_like(search));
        query.push(" AND (ae.action ILIKE ");
        query.push_bind(pattern.clone());
        query.push(" ESCAPE '\\' OR ae.entity_type ILIKE ");
        query.push_bind(pattern);
        query.push(" ESCAPE '\\')");
    }
    if filters.exclude_views {
        query.push(r" AND ae.action NOT LIKE '%.read' AND ae.action NOT LIKE '%.list'");
    }
    if let Some(value) = filters.occurred_after {
        query.push(" AND ae.occurred_at >= ");
        query.push_bind(value);
    }
    if let Some(value) = filters.occurred_before {
        query.push(" AND ae.occurred_at <= ");
        query.push_bind(value);
    }
}

/// `value` with LIKE's wildcards and escape character taken literally.
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}
