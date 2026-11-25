//! Reads for model cards (Phase 8): one catalog model's deployments, prices,
//! availability evidence, request outcomes and timings, and the activations
//! that changed it. Read-only; usage totals come from the usage report's own
//! query ([`super::reporting::ReportingStore::usage_totals`]), so a card and
//! the report agree.

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::domain::{
    identity::{OrganizationId, TeamId},
    model_card::{
        CardAvailability, CardDeployment, CardImport, CardListing, CardNote, CardOperation,
        CardPrice, CardRoute, CountBy, FinishReasonCount, GovernanceActor, RequestStats,
        is_safety_finish_reason,
    },
};

#[derive(Clone)]
pub struct ModelCardStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct OperationRow {
    deployment: String,
    model: String,
    adapter_instance_key: String,
    family: String,
    location: Option<String>,
    operation: String,
    max_input_bytes: i64,
    max_output_tokens: i64,
    tools: bool,
    /// `None`: the release prices no such operation (no route can use it).
    currency: Option<String>,
    /// The release price's currency, override or not.
    release_currency: Option<String>,
    overridden: bool,
    input: Option<i64>,
    output: Option<i64>,
    cached_input: Option<i64>,
    reasoning: Option<i64>,
    other: Option<i64>,
    source: Option<String>,
    source_version: Option<String>,
    confidence: Option<String>,
}

#[derive(FromRow)]
struct RouteRow {
    deployment: String,
    alias: String,
    operation: String,
}

#[derive(FromRow)]
struct EvidenceRow {
    model: String,
    adapter_instance_key: String,
    family: String,
    deployment: Option<String>,
    state: String,
    reason: Option<String>,
    source: String,
    observed_at: DateTime<Utc>,
    verified_at: Option<DateTime<Utc>>,
}

/// What a provider listing says of one account's deployment of a model.
#[derive(Clone, FromRow)]
pub struct ListingRow {
    pub adapter_instance_key: String,
    pub family: String,
    /// The catalog model identifier (the listing's name for it).
    pub model: String,
    pub format: Option<String>,
    pub deployment_model: Option<String>,
    pub version: Option<String>,
    pub sku: Option<String>,
    pub lifecycle_status: Option<String>,
    pub retires_at: Option<DateTime<Utc>>,
    pub listed_at: DateTime<Utc>,
    pub content_filter: Option<String>,
    pub content_filter_policy:
        Option<sqlx::types::Json<crate::domain::discovery::ContentFilterPolicy>>,
}

impl ListingRow {
    pub fn listing(&self) -> CardListing {
        CardListing {
            format: self.format.clone(),
            model: self.deployment_model.clone(),
            version: self.version.clone(),
            sku: self.sku.clone(),
            lifecycle_status: self.lifecycle_status.clone(),
            retires_at: self.retires_at,
            listed_at: self.listed_at,
            content_filter: self.content_filter.clone(),
            content_filter_policy: self
                .content_filter_policy
                .as_ref()
                .map(|policy| policy.0.clone()),
        }
    }
}

/// A deployment of the active release, for the Models table's card facts.
#[derive(Clone, FromRow)]
pub struct ReleaseDeploymentRow {
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: String,
    pub family: String,
}

/// Whether a stored credential names a cloud principal (a Google service
/// account, so Agent Platform) or not (a key); `recorded` false: not yet
/// worked out.
#[derive(Clone, FromRow)]
pub struct CredentialKindRow {
    pub adapter_instance_key: String,
    pub has_principal: bool,
    pub recorded: bool,
}

#[derive(FromRow)]
struct NoteRow {
    note: String,
    version: Option<i64>,
    updated_at: DateTime<Utc>,
    updated_by: Option<Uuid>,
    updated_by_name: Option<String>,
}

#[derive(FromRow)]
struct ImportRow {
    model: String,
    repository: String,
    provider_repository: bool,
    card: serde_json::Value,
    version: Option<i64>,
    imported_at: DateTime<Utc>,
    imported_by: Option<Uuid>,
    imported_by_name: Option<String>,
}

impl ImportRow {
    /// `None` for a stored card this version cannot read.
    fn into_import(self) -> Option<CardImport> {
        Some(CardImport {
            card: serde_json::from_value(self.card).ok()?,
            model: self.model,
            repository: self.repository,
            provider_repository: self.provider_repository,
            version: self
                .version
                .and_then(|v| u64::try_from(v).ok())
                .unwrap_or(1),
            imported_at: self.imported_at,
            imported_by: self.imported_by.map(|id| GovernanceActor {
                actor_type: "user".into(),
                id: Some(id),
                name: self.imported_by_name,
            }),
        })
    }
}

#[derive(FromRow)]
struct StatsRow {
    request_count: i64,
    failed_count: i64,
    timed_request_count: i64,
    timed_since: Option<DateTime<Utc>>,
    latency_p50_ms: Option<i32>,
    latency_p95_ms: Option<i32>,
    first_event_p50_ms: Option<i32>,
    first_event_p95_ms: Option<i32>,
}

#[derive(FromRow)]
struct CountRow {
    key: String,
    count: i64,
}

/// One activation of a release, oldest first, with the model's deployments
/// in it (`deployment` is `None` when the release has none).
#[derive(FromRow)]
pub struct ActivationRow {
    pub activated_at: DateTime<Utc>,
    pub release_id: Uuid,
    pub release_version: String,
    pub actor_type: String,
    pub actor_id: Option<Uuid>,
    pub actor_name: Option<String>,
    pub deployment: Option<String>,
    pub fingerprint: Option<String>,
}

impl ActivationRow {
    pub fn actor(&self) -> GovernanceActor {
        GovernanceActor {
            actor_type: self.actor_type.clone(),
            id: self.actor_id,
            name: self.actor_name.clone(),
        }
    }
}

impl ModelCardStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn active_release(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&self.pool)
        .await
    }

    /// The release's deployments of `model`, each with its operations, the
    /// price billed now (an effective override replaces the release's), the
    /// aliases routed to it and its latest availability evidence. Empty when
    /// the release does not serve the model.
    pub async fn deployments(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        model: &str,
    ) -> Result<Vec<CardDeployment>, sqlx::Error> {
        Ok(self
            .deployments_of(organization_id, release_id, Some(model))
            .await?
            .into_iter()
            .map(|(_, deployment)| deployment)
            .collect())
    }

    /// As [`Self::deployments`], for every model the release serves
    /// (`model` None), each with its model identifier, ordered by
    /// deployment name.
    pub async fn deployments_of(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        model: Option<&str>,
    ) -> Result<Vec<(String, CardDeployment)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // One snapshot, so deployments, routes and evidence agree.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let operations = sqlx::query_as::<_, OperationRow>(
            "SELECT d.name AS deployment, d.model, d.adapter_instance_key, p.family, d.location,
                    c.operation, c.max_input_bytes, c.max_output_tokens, c.tools,
                    btrim(COALESCE(o.currency, pr.currency)) AS currency,
                btrim(pr.currency) AS release_currency,
                    o.id IS NOT NULL AS overridden,
                    COALESCE(o.input_minor_per_token, pr.input_minor_per_token) AS input,
                    COALESCE(o.output_minor_per_token, pr.output_minor_per_token) AS output,
                    COALESCE(o.cached_input_minor_per_token, pr.cached_input_minor_per_token)
                        AS cached_input,
                    COALESCE(o.reasoning_minor_per_token, pr.reasoning_minor_per_token)
                        AS reasoning,
                    COALESCE(o.other_minor_per_token, pr.other_minor_per_token) AS other,
                    CASE WHEN o.id IS NULL THEN pr.source END AS source,
                    CASE WHEN o.id IS NULL THEN pr.source_version ELSE o.version END
                        AS source_version,
                    COALESCE(o.confidence, pr.confidence) AS confidence
             FROM gateway.catalog_deployment d
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             JOIN gateway.catalog_capability c
               ON c.organization_id = d.organization_id AND c.release_id = d.release_id
              AND c.deployment_id = d.id
             LEFT JOIN gateway.catalog_price pr
               ON pr.organization_id = d.organization_id AND pr.release_id = d.release_id
              AND pr.deployment_id = d.id AND pr.operation = c.operation
             -- An override replaces a release price; it does not price an
             -- operation the release leaves unpriced.
             LEFT JOIN LATERAL (
                 SELECT po.* FROM gateway.catalog_price_override po
                 WHERE pr.id IS NOT NULL
                   AND po.organization_id = d.organization_id AND po.deployment = d.name
                   AND po.operation = pr.operation AND po.revoked_at IS NULL
                   AND po.effective_from <= now()
                   AND (po.effective_until IS NULL OR po.effective_until > now())
                 ORDER BY po.effective_from DESC, po.created_at DESC, po.id DESC
                 LIMIT 1) o ON true
             WHERE d.organization_id = $1 AND d.release_id = $2
               AND ($3::text IS NULL OR d.model = $3)
             ORDER BY d.name, c.operation",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(model)
        .fetch_all(&mut *tx)
        .await?;
        let routes = sqlx::query_as::<_, RouteRow>(
            "SELECT d.name AS deployment, r.alias, r.operation
             FROM gateway.catalog_route r
             JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.release_id
              AND d.id = r.deployment_id
             WHERE r.organization_id = $1 AND r.release_id = $2
               AND ($3::text IS NULL OR d.model = $3)
             ORDER BY r.alias, r.operation",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(model)
        .fetch_all(&mut *tx)
        .await?;
        let evidence = sqlx::query_as::<_, EvidenceRow>(
            "SELECT model, adapter_instance_key, family, deployment, state, reason, source,
                    observed_at, verified_at
             FROM gateway.catalog_model_evidence
             WHERE organization_id = $1 AND ($2::text IS NULL OR model = $2)",
        )
        .bind(organization_id.0)
        .bind(model)
        .fetch_all(&mut *tx)
        .await?;
        let mut models: Vec<String> = operations.iter().map(|row| row.model.clone()).collect();
        models.sort();
        models.dedup();
        let listings = Self::listings_in(&mut tx, organization_id, &models).await?;
        tx.commit().await?;

        let mut deployments: Vec<(String, CardDeployment)> = Vec::new();
        for row in operations {
            if deployments
                .last()
                .is_none_or(|(_, d)| d.deployment != row.deployment)
            {
                // Evidence names the deployment where the account has
                // deployments (Azure), and otherwise only the model.
                let availability = evidence
                    .iter()
                    .filter(|e| e.model == row.model)
                    .filter(|e| e.adapter_instance_key == row.adapter_instance_key)
                    .filter(|e| e.family == row.family)
                    .filter(|e| e.deployment.as_deref().is_none_or(|d| d == row.deployment))
                    .max_by_key(|e| e.observed_at)
                    .map(|e| CardAvailability {
                        state: e.state.clone(),
                        reason: e.reason.clone(),
                        source: e.source.clone(),
                        observed_at: e.observed_at,
                        verified_at: e.verified_at,
                    });
                deployments.push((
                    row.model.clone(),
                    CardDeployment {
                        routes: routes
                            .iter()
                            .filter(|r| r.deployment == row.deployment)
                            .map(|r| CardRoute {
                                alias: r.alias.clone(),
                                operation: r.operation.clone(),
                            })
                            .collect(),
                        deployment: row.deployment.clone(),
                        adapter_instance_key: row.adapter_instance_key.clone(),
                        provider_family: row.family.clone(),
                        location: row.location.clone(),
                        operations: Vec::new(),
                        availability,
                        provider_listing: listings
                            .iter()
                            .find(|l| {
                                l.adapter_instance_key == row.adapter_instance_key
                                    && l.family == row.family
                                    && l.model == row.model
                            })
                            .map(ListingRow::listing),
                    },
                ));
            }
            let (_, deployment) = deployments.last_mut().expect("pushed above");
            deployment.operations.push(CardOperation {
                operation: row.operation,
                max_input_bytes: row.max_input_bytes,
                max_output_tokens: row.max_output_tokens,
                tools: row.tools,
                price: match (row.currency, row.confidence) {
                    (Some(currency), Some(confidence)) => Some(CardPrice {
                        currency,
                        release_currency: row.release_currency,
                        input_minor_per_token: row.input.unwrap_or_default(),
                        output_minor_per_token: row.output.unwrap_or_default(),
                        cached_input_minor_per_token: row.cached_input.unwrap_or_default(),
                        reasoning_minor_per_token: row.reasoning.unwrap_or_default(),
                        other_minor_per_token: row.other.unwrap_or_default(),
                        billed_by: if row.overridden {
                            "override"
                        } else {
                            "release"
                        },
                        source: row.source,
                        source_version: row.source_version,
                        confidence,
                    }),
                    _ => None,
                },
            });
        }
        Ok(deployments)
    }

    /// What listings of the accounts' own deployments (Azure) say of each
    /// of `models`, with the lifecycle status and retirement date the
    /// account's model catalog gives the model version a deployment serves.
    /// Listings of models (Bedrock, Gemini) record no deployment and are
    /// left out; so are deployments the latest listing no longer reports.
    async fn listings_in(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        models: &[String],
    ) -> Result<Vec<ListingRow>, sqlx::Error> {
        sqlx::query_as::<_, ListingRow>(
            "SELECT m.adapter_instance_key, m.family, m.model,
                    m.deployment_format AS format, m.deployment_model,
                    m.deployment_version AS version, m.deployment_sku AS sku,
                    d.lifecycle_status, d.retires_at, m.last_seen_at AS listed_at,
                    m.deployment_content_filter AS content_filter,
                    m.deployment_content_filter_policy AS content_filter_policy
             FROM gateway.catalog_discovered_model m
             LEFT JOIN gateway.catalog_deployable_model d
               ON d.organization_id = m.organization_id
              AND d.adapter_instance_key = m.adapter_instance_key
              AND d.family = m.family
              AND lower(d.format) = lower(m.deployment_format)
              AND d.model = m.deployment_model
              AND d.version = m.deployment_version
             WHERE m.organization_id = $1 AND m.model = ANY($2)
               AND m.missing_since IS NULL
               AND (m.deployment_format IS NOT NULL OR m.deployment_model IS NOT NULL)
             ORDER BY m.adapter_instance_key, m.family, m.model",
        )
        .bind(organization_id.0)
        .bind(models)
        .fetch_all(&mut *conn)
        .await
    }

    /// The release's deployments, with what listings say of them, for the
    /// Models table's card facts.
    pub async fn release_deployments(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<(Vec<ReleaseDeploymentRow>, Vec<ListingRow>), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let rows = sqlx::query_as::<_, ReleaseDeploymentRow>(
            "SELECT d.name AS deployment, d.model, d.adapter_instance_key, p.family
             FROM gateway.catalog_deployment d
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             WHERE d.organization_id = $1 AND d.release_id = $2
             ORDER BY d.model, d.name",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut models: Vec<String> = rows.iter().map(|row| row.model.clone()).collect();
        models.sort();
        models.dedup();
        let listings = Self::listings_in(&mut tx, organization_id, &models).await?;
        tx.commit().await?;
        Ok((rows, listings))
    }

    /// For each of `keys` with a stored credential, whether it names a
    /// cloud principal. Keys without one are the process's own adapters.
    pub async fn credential_kinds(
        &self,
        organization_id: OrganizationId,
        family: &str,
        keys: &[String],
    ) -> Result<Vec<CredentialKindRow>, sqlx::Error> {
        sqlx::query_as::<_, CredentialKindRow>(
            "SELECT adapter_instance_key, principal_digest IS NOT NULL AS has_principal,
                    principal_recorded AS recorded
             FROM gateway.provider_credential
             WHERE organization_id = $1 AND family = $2 AND adapter_instance_key = ANY($3)",
        )
        .bind(organization_id.0)
        .bind(family)
        .bind(keys)
        .fetch_all(&self.pool)
        .await
    }

    /// The organization's note on `model`, if one was ever set.
    pub async fn note(
        &self,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Option<CardNote>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::note_in(&mut conn, organization_id, model).await
    }

    pub async fn note_in(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Option<CardNote>, sqlx::Error> {
        let row = sqlx::query_as::<_, NoteRow>(
            "SELECT n.note, v.version, n.updated_at, n.updated_by,
                    u.display_name AS updated_by_name
             FROM gateway.model_card_note n
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = n.organization_id
              AND v.resource_type = 'model_card_note' AND v.resource_id = n.id
             LEFT JOIN gateway.user_account u ON u.id = n.updated_by
             WHERE n.organization_id = $1 AND n.model = $2",
        )
        .bind(organization_id.0)
        .bind(model)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(|row| CardNote {
            note: row.note,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            updated_at: row.updated_at,
            updated_by: row.updated_by.map(|id| GovernanceActor {
                actor_type: "user".into(),
                id: Some(id),
                name: row.updated_by_name,
            }),
        }))
    }

    /// Sets the note (already normalized) and keeps the text as a revision.
    /// Returns true when the note was created.
    pub async fn set_note(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        model: &str,
        note: &str,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let created = sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.model_card_note (organization_id, id, model, note, updated_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (organization_id, model) DO UPDATE
             SET note = EXCLUDED.note, updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(id)
        .bind(model)
        .bind(note)
        .bind(actor)
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO gateway.model_card_note_revision
                 (id, organization_id, note_id, note, created_by)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(id)
        .bind(note)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Ok(created)
    }

    /// The organization's evaluations of `model` (latest run per set).
    pub async fn organization_evaluations(
        &self,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Vec<crate::domain::model_evaluation::OrganizationEvaluation>, sqlx::Error> {
        crate::persistence::model_evaluations::ModelEvaluationStore::for_model(
            &self.pool,
            organization_id,
            model,
        )
        .await
    }

    /// The organization's imported card for `model`, unless removed.
    pub async fn import(
        &self,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Option<CardImport>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::import_in(&mut conn, organization_id, model, false).await
    }

    /// The organization's imported card for `model`; with `deleted`, also a
    /// removed one (to replace it).
    pub async fn import_in(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        model: &str,
        deleted: bool,
    ) -> Result<Option<CardImport>, sqlx::Error> {
        let row = sqlx::query_as::<_, ImportRow>(
            "SELECT i.model, i.repository, i.provider_repository, i.card, v.version,
                    i.imported_at, i.imported_by, u.display_name AS imported_by_name
             FROM gateway.model_card_import i
             LEFT JOIN gateway.admin_resource_version v
               ON v.organization_id = i.organization_id
              AND v.resource_type = 'model_card_import' AND v.resource_id = i.id
             LEFT JOIN gateway.user_account u ON u.id = i.imported_by
             WHERE i.organization_id = $1 AND i.model = $2
               AND ($3 OR i.deleted_at IS NULL)",
        )
        .bind(organization_id.0)
        .bind(model)
        .bind(deleted)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.and_then(ImportRow::into_import))
    }

    /// The organization's imported cards (not removed) for `models`.
    pub async fn imports(
        &self,
        organization_id: OrganizationId,
        models: &[String],
    ) -> Result<Vec<CardImport>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ImportRow>(
            "SELECT i.model, i.repository, i.provider_repository, i.card,
                    NULL::bigint AS version, i.imported_at, i.imported_by,
                    NULL::text AS imported_by_name
             FROM gateway.model_card_import i
             WHERE i.organization_id = $1 AND i.model = ANY($2) AND i.deleted_at IS NULL",
        )
        .bind(organization_id.0)
        .bind(models)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(ImportRow::into_import)
            .collect())
    }

    /// Stores an import (replacing one, removed or not) and keeps it as a
    /// revision; with `card` None, removes it (soft). Returns whether a
    /// row was created.
    pub async fn set_import(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        model: &str,
        import: Option<(&str, bool, &serde_json::Value)>,
        actor: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let created = match import {
            Some((repository, provider_repository, card)) => {
                sqlx::query_scalar::<_, bool>(
                    "INSERT INTO gateway.model_card_import
                         (organization_id, id, model, repository, provider_repository, card,
                          imported_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (organization_id, model) DO UPDATE
                     SET repository = EXCLUDED.repository,
                         provider_repository = EXCLUDED.provider_repository,
                         card = EXCLUDED.card, imported_by = EXCLUDED.imported_by,
                         imported_at = CURRENT_TIMESTAMP, deleted_at = NULL
                     RETURNING (xmax = 0)",
                )
                .bind(organization_id.0)
                .bind(id)
                .bind(model)
                .bind(repository)
                .bind(provider_repository)
                .bind(card)
                .bind(actor)
                .fetch_one(&mut *conn)
                .await?
            }
            None => {
                sqlx::query(
                    "UPDATE gateway.model_card_import SET deleted_at = CURRENT_TIMESTAMP
                     WHERE organization_id = $1 AND model = $2 AND deleted_at IS NULL",
                )
                .bind(organization_id.0)
                .bind(model)
                .execute(&mut *conn)
                .await?;
                false
            }
        };
        sqlx::query(
            "INSERT INTO gateway.model_card_import_revision
                 (id, organization_id, import_id, repository, provider_repository, card,
                  created_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(organization_id.0)
        .bind(id)
        .bind(import.map(|import| import.0))
        .bind(import.map(|import| import.1))
        .bind(import.map(|import| import.2))
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        Ok(created)
    }

    /// The name a provider listing gives `model`, if any listing does.
    pub async fn display_name(
        &self,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT display_name FROM gateway.catalog_discovered_model
             WHERE organization_id = $1 AND model = $2 AND display_name IS NOT NULL
             ORDER BY missing_since IS NOT NULL, last_seen_at DESC
             LIMIT 1",
        )
        .bind(organization_id.0)
        .bind(model)
        .fetch_optional(&self.pool)
        .await
    }

    /// How `model`'s requests recorded in `[from, to)` went, for the teams
    /// in `authorized_teams` (`None`: the whole organization).
    pub async fn request_stats(
        &self,
        organization_id: OrganizationId,
        authorized_teams: Option<&[TeamId]>,
        model: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<RequestStats, sqlx::Error> {
        const SCOPE: &str = "FROM gateway.inference_trace t
             WHERE t.organization_id = $1 AND t.model = $2
               AND t.recorded_at >= $3 AND t.recorded_at < $4
               AND ($5::uuid[] IS NULL OR t.team_id = ANY($5))";
        let teams: Option<Vec<Uuid>> =
            authorized_teams.map(|teams| teams.iter().map(|team| team.0).collect());
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let stats = sqlx::query_as::<_, StatsRow>(&format!(
            "SELECT count(*)::bigint AS request_count,
                    count(*) FILTER (WHERE t.provider_error_category IS NOT NULL)::bigint
                        AS failed_count,
                    count(t.provider_latency_ms)::bigint AS timed_request_count,
                    min(t.recorded_at) FILTER (WHERE t.provider_latency_ms IS NOT NULL)
                        AS timed_since,
                    percentile_disc(0.5) WITHIN GROUP (ORDER BY t.provider_latency_ms)
                        AS latency_p50_ms,
                    percentile_disc(0.95) WITHIN GROUP (ORDER BY t.provider_latency_ms)
                        AS latency_p95_ms,
                    percentile_disc(0.5) WITHIN GROUP (ORDER BY t.first_event_ms)
                        AS first_event_p50_ms,
                    percentile_disc(0.95) WITHIN GROUP (ORDER BY t.first_event_ms)
                        AS first_event_p95_ms
             {SCOPE}"
        ))
        .bind(organization_id.0)
        .bind(model)
        .bind(from)
        .bind(to)
        .bind(&teams)
        .fetch_one(&mut *tx)
        .await?;
        let errors = sqlx::query_as::<_, CountRow>(&format!(
            "SELECT t.provider_error_category AS key, count(*)::bigint AS count
             {SCOPE} AND t.provider_error_category IS NOT NULL
             GROUP BY 1 ORDER BY 2 DESC, 1"
        ))
        .bind(organization_id.0)
        .bind(model)
        .bind(from)
        .bind(to)
        .bind(&teams)
        .fetch_all(&mut *tx)
        .await?;
        let reasons = sqlx::query_as::<_, CountRow>(&format!(
            "SELECT t.finish_reason AS key, count(*)::bigint AS count
             {SCOPE} AND t.finish_reason IS NOT NULL
             GROUP BY 1 ORDER BY 2 DESC, 1"
        ))
        .bind(organization_id.0)
        .bind(model)
        .bind(from)
        .bind(to)
        .bind(&teams)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RequestStats {
            days: (to - from).num_days(),
            from,
            to,
            request_count: stats.request_count,
            failed_count: stats.failed_count,
            errors: errors
                .into_iter()
                .map(|row| CountBy {
                    key: row.key,
                    count: row.count,
                })
                .collect(),
            timed_since: stats.timed_since,
            timed_request_count: stats.timed_request_count,
            latency_p50_ms: stats.latency_p50_ms.map(i64::from),
            latency_p95_ms: stats.latency_p95_ms.map(i64::from),
            first_event_p50_ms: stats.first_event_p50_ms.map(i64::from),
            first_event_p95_ms: stats.first_event_p95_ms.map(i64::from),
            finish_reasons: reasons
                .into_iter()
                .map(|row| FinishReasonCount {
                    safety: is_safety_finish_reason(&row.key),
                    reason: row.key,
                    count: row.count,
                })
                .collect(),
        })
    }

    /// The organization's latest `limit` release activations, oldest first,
    /// each with who activated it and a fingerprint of every deployment of
    /// `model` it holds (account, provider family, location, operations,
    /// limits and prices).
    /// Comparing consecutive activations shows when the model was added,
    /// changed or removed.
    pub async fn activations(
        &self,
        organization_id: OrganizationId,
        model: &str,
        limit: i64,
    ) -> Result<Vec<ActivationRow>, sqlx::Error> {
        sqlx::query_as::<_, ActivationRow>(
            "WITH activations AS (
                 SELECT ae.id, ae.occurred_at, ae.entity_id AS release_id, ae.actor_type,
                        COALESCE(ae.actor_user_id, ae.actor_service_account_id) AS actor_id,
                        COALESCE(u.display_name, sa.name) AS actor_name
                 FROM gateway.audit_event ae
                 LEFT JOIN gateway.user_account u ON u.id = ae.actor_user_id
                 LEFT JOIN gateway.service_account sa
                   ON sa.id = ae.actor_service_account_id
                  AND sa.organization_id = ae.organization_id
                 WHERE ae.organization_id = $1 AND ae.action = 'catalog_release.activated'
                   AND ae.outcome = 'succeeded'
                 ORDER BY ae.occurred_at DESC, ae.id DESC
                 LIMIT $3
             ), fingerprints AS (
                 SELECT d.release_id, d.name,
                        md5(d.adapter_instance_key || '|' || p.family || '|' ||
                            COALESCE(d.location, '') || '|' ||
                            string_agg(
                                concat_ws(':', c.operation, c.max_input_bytes,
                                          c.max_output_tokens, btrim(pr.currency),
                                          pr.input_minor_per_token, pr.output_minor_per_token,
                                          pr.cached_input_minor_per_token,
                                          pr.reasoning_minor_per_token,
                                          pr.other_minor_per_token),
                                ',' ORDER BY c.operation)) AS fingerprint
                 FROM gateway.catalog_deployment d
                 JOIN gateway.catalog_provider_account p
                   ON p.organization_id = d.organization_id AND p.release_id = d.release_id
                  AND p.id = d.provider_account_id
                 JOIN gateway.catalog_capability c
                   ON c.organization_id = d.organization_id AND c.release_id = d.release_id
                  AND c.deployment_id = d.id
                 LEFT JOIN gateway.catalog_price pr
                   ON pr.organization_id = d.organization_id AND pr.release_id = d.release_id
                  AND pr.deployment_id = d.id AND pr.operation = c.operation
                 WHERE d.organization_id = $1 AND d.model = $2
                   AND d.release_id IN (SELECT release_id FROM activations)
                 GROUP BY d.release_id, d.name, d.adapter_instance_key, p.family, d.location
             )
             SELECT a.occurred_at AS activated_at, a.release_id, r.version AS release_version,
                    a.actor_type, a.actor_id, a.actor_name,
                    f.name AS deployment, f.fingerprint
             FROM activations a
             JOIN gateway.catalog_release r
               ON r.organization_id = $1 AND r.id = a.release_id
             LEFT JOIN fingerprints f ON f.release_id = a.release_id
             ORDER BY a.occurred_at, a.id, f.name",
        )
        .bind(organization_id.0)
        .bind(model)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Whether a release created before `release_id` has a deployment of
    /// `model`. Releases are never deleted, so when none does, nothing before
    /// that release could have served the model, whatever activations went
    /// unread (past the limit, purged by audit retention, or made without an
    /// audit event by `gateway catalog apply`); releases that never served
    /// it, activated or not, change nothing.
    pub async fn older_release_serves(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        model: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM gateway.catalog_release older
                 JOIN gateway.catalog_release r ON r.organization_id = older.organization_id
                 JOIN gateway.catalog_deployment d
                   ON d.organization_id = older.organization_id AND d.release_id = older.id
                 WHERE r.organization_id = $1 AND r.id = $2 AND d.model = $3
                   AND older.created_at < r.created_at)",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(model)
        .fetch_one(&self.pool)
        .await
    }
}
