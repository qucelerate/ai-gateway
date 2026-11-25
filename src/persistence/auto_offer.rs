//! Automatic model offers: opt-ins, and the runs that turn an opted-in
//! account's listing into a release.
//!
//! A run holds the catalog lock (the organization row, then the catalog's
//! advisory lock) for its whole transaction, as every release write does,
//! then locks the opt-in. It reads the active release, the account's
//! listing and the published prices, plans the next release, and when the
//! catalog changes creates and activates it as the system, bumping the
//! activation pointer's version so an admin's conditional activation made
//! against the previous one fails rather than undoing it. Both writes are
//! audited in the same transaction.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    audit::{
        AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot, AuditTargetResource,
    },
    auto_offer::{
        AutoOfferInput, AutoOfferView, Blocked, ERROR_ACCOUNT_NOT_DECLARED,
        ERROR_INCOMPATIBLE_PRICE_OVERRIDE, ERROR_INVALID_RELEASE, ERROR_NO_ACTIVE_RELEASE,
        ListedModel, OfferStatus, OfferedModelView, PlanInput, PreviousOffer, deployment_name,
        distinct_deployment_name, plan, price_list_items,
    },
    catalog::{CatalogOperation, ProviderFamily},
    discovery::ModelAvailability,
    identity::{OrganizationId, UserAccountId},
    public_prices::AWS_PRICE_LIST_SOURCE,
};
use crate::persistence::{
    admin::{AdminStoreError, insert_audit_on},
    catalog::{CatalogStoreError, activate_release, insert_release, lock_catalog},
    catalog_admin::CatalogAdminStore,
    price_sync::PriceSyncStore,
};

#[derive(Clone)]
pub struct AutoOfferStore {
    pool: PgPool,
}

/// What one run did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    /// The account is not opted in.
    NotEnabled,
    /// The active release already offers what the account lists.
    Unchanged,
    Activated {
        release_id: Uuid,
        added: u32,
        updated: u32,
        removed: u32,
    },
    /// Recorded on the opt-in with this code; the catalog is unchanged.
    Failed(&'static str),
}

#[derive(FromRow)]
struct OfferRow {
    adapter_instance_key: String,
    location: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    applied_at: Option<DateTime<Utc>>,
    applied_release_id: Option<Uuid>,
    error_code: Option<String>,
    version: Option<i64>,
}

#[derive(FromRow)]
struct ModelRow {
    model: String,
    deployment: String,
    status: String,
    price_source: Option<String>,
    price_item: Option<String>,
    updated_at: DateTime<Utc>,
}

const OFFER_SELECT: &str = "
    SELECT o.adapter_instance_key, o.location, o.created_at, o.updated_at, o.applied_at,
           o.applied_release_id, o.error_code, v.version
    FROM gateway.catalog_auto_offer o
    LEFT JOIN gateway.admin_resource_version v
      ON v.organization_id = o.organization_id AND v.resource_type = 'catalog_auto_offer'
     AND v.resource_id = o.id
    WHERE o.organization_id = $1 AND o.adapter_instance_key = $2";

fn decode(message: &'static str) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

fn action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant valid")
}

impl AutoOfferStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn view(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<Option<AutoOfferView>, sqlx::Error> {
        let mut conn = self.pool.acquire().await?;
        Self::view_in(&mut conn, organization_id, adapter_instance_key).await
    }

    pub async fn view_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<Option<AutoOfferView>, sqlx::Error> {
        let Some(row) = sqlx::query_as::<_, OfferRow>(OFFER_SELECT)
            .bind(organization_id.0)
            .bind(adapter_instance_key)
            .fetch_optional(&mut *conn)
            .await?
        else {
            return Ok(None);
        };
        let models = sqlx::query_as::<_, ModelRow>(
            "SELECT model, deployment, status, price_source, price_item, updated_at
             FROM gateway.catalog_auto_offered_model
             WHERE organization_id = $1 AND adapter_instance_key = $2
             ORDER BY model",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|row| {
            Ok(OfferedModelView {
                status: OfferStatus::from_str(&row.status).map_err(|_| decode("offer status"))?,
                model: row.model,
                deployment: row.deployment,
                price_source: row.price_source,
                price_item: row.price_item,
                updated_at: row.updated_at,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        Ok(Some(AutoOfferView {
            adapter_instance_key: row.adapter_instance_key,
            location: row.location,
            created_at: row.created_at,
            updated_at: row.updated_at,
            applied_at: row.applied_at,
            applied_release_id: row.applied_release_id,
            error_code: row.error_code,
            version: row.version.and_then(|v| u64::try_from(v).ok()).unwrap_or(1),
            models,
        }))
    }

    /// Opts the account in, or changes its location. Setting it again
    /// offers withdrawn models once more, including those the next run
    /// would have withdrawn. Returns whether it was created.
    pub async fn upsert(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        id: Uuid,
        adapter_instance_key: &str,
        input: &AutoOfferInput,
        actor: UserAccountId,
    ) -> Result<bool, sqlx::Error> {
        let created = sqlx::query_scalar::<_, bool>(
            "INSERT INTO gateway.catalog_auto_offer
                 (organization_id, adapter_instance_key, id, location, created_by, updated_by)
             VALUES ($1, $2, $3, $4, $5, $5)
             ON CONFLICT (organization_id, adapter_instance_key) DO UPDATE
             SET location = EXCLUDED.location,
                 updated_by = EXCLUDED.updated_by,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING (xmax = 0)",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(id)
        .bind(input.location.as_deref())
        .bind(actor.0)
        .fetch_one(&mut *conn)
        .await?;
        // Saving makes the active release the starting point: withdrawn
        // models, and offered ones the active release has dropped since
        // the last run, are offered again; offered deployments still in it
        // stay the opt-in's.
        sqlx::query(
            "DELETE FROM gateway.catalog_auto_offered_model m
             WHERE m.organization_id = $1 AND m.adapter_instance_key = $2
               AND (m.status = 'withdrawn'
                    OR (m.status = 'offered' AND NOT EXISTS (
                        SELECT 1 FROM gateway.catalog_active_release a
                        JOIN gateway.catalog_deployment d
                          ON d.organization_id = a.organization_id
                         AND d.release_id = a.release_id
                        WHERE a.organization_id = m.organization_id
                          AND d.name = m.deployment)))",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "UPDATE gateway.catalog_auto_offer
             SET applied_release_id = (SELECT release_id FROM gateway.catalog_active_release
                                       WHERE organization_id = $1)
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .execute(&mut *conn)
        .await?;
        Ok(created)
    }

    /// Opts the account out. Offered deployments stay in the catalog as
    /// they are. Returns whether there was an opt-in.
    pub async fn delete(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "DELETE FROM gateway.catalog_auto_offer
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .execute(&mut *conn)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn enabled(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM gateway.catalog_auto_offer
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }

    /// Plans the account's offer from its latest listing and, when the
    /// catalog changes, creates and activates the release. `unchecked` are
    /// the listed models whose entitlement the listing could not check where
    /// a call could subscribe the account: those without evidence that the
    /// account may call them are deferred.
    pub async fn apply(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: ProviderFamily,
        unchecked: &[String],
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        lock_catalog(&mut tx, organization_id).await?;
        let Some((location, applied_release_id)) =
            sqlx::query_as::<_, (Option<String>, Option<Uuid>)>(
                "SELECT location, applied_release_id FROM gateway.catalog_auto_offer
                 WHERE organization_id = $1 AND adapter_instance_key = $2 FOR UPDATE",
            )
            .bind(organization_id.0)
            .bind(adapter_instance_key)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return Ok(ApplyOutcome::NotEnabled);
        };
        let active = sqlx::query_scalar::<_, Uuid>(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(active) = active else {
            return self
                .fail(
                    tx,
                    organization_id,
                    adapter_instance_key,
                    ERROR_NO_ACTIVE_RELEASE,
                    now,
                )
                .await;
        };
        // A sealed release never changes, so it may be read outside the
        // transaction.
        let manifest = CatalogAdminStore::new(self.pool.clone())
            .release_manifest(organization_id, active)
            .await
            .map_err(|_| decode("active release manifest"))?
            .ok_or_else(|| decode("active release manifest"))?;
        if !manifest
            .providers
            .iter()
            .any(|p| p.adapter_instance_key == adapter_instance_key && p.family == family.as_str())
        {
            return self
                .fail(
                    tx,
                    organization_id,
                    adapter_instance_key,
                    ERROR_ACCOUNT_NOT_DECLARED,
                    now,
                )
                .await;
        }
        let listed = sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<Vec<String>>,
                String,
                bool,
                Option<String>,
                Option<String>,
                bool,
            ),
        >(
            // The reason the account's evidence refuses the model, if it does.
            // A refusal counts only once another model of the same account
            // has been called successfully since the refusals began: then the
            // account's credentials work, and the refusal is the model's.
            // Until then a refusal may be a broken or replaced key, which
            // must not remove offers. Later refusals keep `refused_since`, so
            // a confirmed refusal stays in force until the model succeeds.
            // Then the reason a check found something the tenant must do
            // first: the check answered, so the credentials work.
            "SELECT m.model, m.display_name, m.operations, m.availability,
                    m.missing_since IS NOT NULL,
                    CASE WHEN e.state = 'refused' AND EXISTS (
                        SELECT 1 FROM gateway.catalog_model_evidence o
                        WHERE o.organization_id = e.organization_id
                          AND o.adapter_instance_key = e.adapter_instance_key
                          AND o.family = e.family
                          AND o.model <> e.model
                          AND o.verified_at >= e.refused_since
                    ) THEN e.reason END,
                    CASE WHEN e.state = 'requires_action' THEN e.reason END,
                    COALESCE(e.state IN ('entitled', 'verified'), false)
             FROM gateway.catalog_discovered_model m
             LEFT JOIN gateway.catalog_model_evidence e
               ON e.organization_id = m.organization_id
              AND e.adapter_instance_key = m.adapter_instance_key
              AND e.family = m.family
              AND e.model = m.model
             WHERE m.organization_id = $1 AND m.adapter_instance_key = $2 AND m.family = $3",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(family.as_str())
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(
            |(
                model,
                display_name,
                operations,
                availability,
                missing,
                refused,
                needs_action,
                may,
            )| {
                ListedModel {
                    unchecked: !may && unchecked.contains(&model),
                    model,
                    display_name,
                    operations: operations.map(|operations| {
                        operations
                            .iter()
                            .filter_map(|operation| CatalogOperation::from_str(operation).ok())
                            .collect()
                    }),
                    availability: availability.parse().unwrap_or(ModelAvailability::Unknown),
                    missing,
                    blocked: refused
                        .and_then(|reason| reason.parse().ok())
                        .map(Blocked::Refused)
                        .or_else(|| {
                            needs_action
                                .and_then(|reason| reason.parse().ok())
                                .map(Blocked::RequiresAction)
                        }),
                }
            },
        )
        .collect::<Vec<_>>();
        let previous =
            sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
                "SELECT model, deployment, status, price_source, price_item
             FROM gateway.catalog_auto_offered_model
             WHERE organization_id = $1 AND adapter_instance_key = $2",
            )
            .bind(organization_id.0)
            .bind(adapter_instance_key)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(|(model, deployment, status, price_source, price_item)| {
                Ok(PreviousOffer {
                    model,
                    deployment,
                    status: OfferStatus::from_str(&status).map_err(|_| decode("offer status"))?,
                    price_source,
                    price_item,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let models: Vec<&str> = listed.iter().map(|m| m.model.as_str()).collect();
        // Every name a listed model may be offered under, so mappings of
        // any of them are read.
        let deployments: Vec<String> = listed
            .iter()
            .flat_map(|m| {
                [
                    deployment_name(adapter_instance_key, &m.model),
                    distinct_deployment_name(adapter_instance_key, &m.model),
                ]
            })
            .chain(previous.iter().map(|p| p.deployment.clone()))
            .collect();
        let deployment_refs: Vec<&str> = deployments.iter().map(String::as_str).collect();
        // The price list items named after listed models.
        let named: Vec<String> = listed
            .iter()
            .filter_map(|m| price_list_items(family, &listed, &m.model))
            .flat_map(|(standard, long_context)| [standard, long_context])
            .collect();
        let named: Vec<(&str, &str)> = named
            .iter()
            .map(|item| (AWS_PRICE_LIST_SOURCE, item.as_str()))
            .collect();
        let (sources, mappings) = PriceSyncStore::published(
            &mut tx,
            organization_id,
            family,
            &models,
            &deployment_refs,
            &named,
        )
        .await?;
        let planned = match plan(&PlanInput {
            adapter_instance_key,
            family,
            location: location.as_deref(),
            active: &manifest,
            listed: &listed,
            previous: &previous,
            catalog_changed: applied_release_id != Some(active),
            sources: &sources,
            mappings: &mappings,
            now,
        }) {
            Ok(planned) => planned,
            Err(code) => {
                return self
                    .fail(tx, organization_id, adapter_instance_key, code, now)
                    .await;
            }
        };

        let mut outcome = ApplyOutcome::Unchanged;
        let mut release_after = active;
        if let Some(next) = &planned.manifest {
            let release_id = match activate_system_release(&mut tx, organization_id, next).await? {
                SystemRelease::Activated(release_id) => release_id,
                SystemRelease::Invalid => {
                    return self
                        .fail(
                            tx,
                            organization_id,
                            adapter_instance_key,
                            ERROR_INVALID_RELEASE,
                            now,
                        )
                        .await;
                }
                SystemRelease::IncompatiblePriceOverride => {
                    // The release just inserted rolls back with the run.
                    tx.rollback().await?;
                    let tx = self.pool.begin().await?;
                    return self
                        .fail(
                            tx,
                            organization_id,
                            adapter_instance_key,
                            ERROR_INCOMPATIBLE_PRICE_OVERRIDE,
                            now,
                        )
                        .await;
                }
            };
            release_after = release_id;
            outcome = ApplyOutcome::Activated {
                release_id,
                added: planned.added,
                updated: planned.updated,
                removed: planned.removed,
            };
        }

        sqlx::query(
            "DELETE FROM gateway.catalog_auto_offered_model
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .execute(&mut *tx)
        .await?;
        let column = |f: fn(&crate::domain::auto_offer::PlannedModel) -> Option<&str>| {
            planned.models.iter().map(f).collect::<Vec<_>>()
        };
        sqlx::query(
            "INSERT INTO gateway.catalog_auto_offered_model
                 (organization_id, adapter_instance_key, model, deployment, status,
                  price_source, price_item, updated_at)
             SELECT $1, $2, m.model, m.deployment, m.status, m.price_source, m.price_item, $8
             FROM UNNEST($3::text[], $4::text[], $5::text[], $6::text[], $7::text[])
                 AS m (model, deployment, status, price_source, price_item)",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(column(|m| Some(m.model.as_str())))
        .bind(column(|m| Some(m.deployment.as_str())))
        .bind(column(|m| Some(m.status.as_str())))
        .bind(column(|m| m.price_source.as_deref()))
        .bind(column(|m| m.price_item.as_deref()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE gateway.catalog_auto_offer
             SET applied_at = $3, applied_release_id = $4, error_code = NULL
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(now)
        .bind(release_after)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// Records why a run changed nothing, on `tx`, and commits it.
    async fn fail(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        code: &'static str,
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome, sqlx::Error> {
        sqlx::query(
            "UPDATE gateway.catalog_auto_offer SET applied_at = $3, error_code = $4
             WHERE organization_id = $1 AND adapter_instance_key = $2",
        )
        .bind(organization_id.0)
        .bind(adapter_instance_key)
        .bind(now)
        .bind(code)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ApplyOutcome::Failed(code))
    }
}

/// What creating and activating a system release came to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemRelease {
    Activated(Uuid),
    /// The manifest does not validate.
    Invalid,
    /// An effective price override would no longer match; the caller rolls
    /// back, since the release may already be inserted.
    IncompatiblePriceOverride,
}

/// Creates `manifest` as a release (or reuses an identical one) and
/// activates it on `tx`, which holds the catalog lock, audited as the
/// system's. The activation version moves on, so an admin's conditional
/// activation against the old pointer is refused rather than undoing it.
pub(crate) async fn activate_system_release(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    organization_id: OrganizationId,
    manifest: &crate::domain::catalog::CatalogManifest,
) -> Result<SystemRelease, sqlx::Error> {
    let Ok(digest) = manifest.digest() else {
        return Ok(SystemRelease::Invalid);
    };
    let request_id = Uuid::new_v4();
    let release_id = match CatalogAdminStore::release_by_digest(tx, organization_id, &digest.0)
        .await
        .map_err(|_| decode("release by digest"))?
    {
        Some(existing) => existing,
        None => {
            let release_id = Uuid::new_v4();
            match insert_release(tx, organization_id, release_id, manifest, digest).await {
                Ok(()) => {}
                Err(CatalogStoreError::Database(error)) => return Err(error),
                Err(_) => return Ok(SystemRelease::Invalid),
            }
            sqlx::query(
                "INSERT INTO gateway.admin_resource_version
                     (organization_id, resource_type, resource_id)
                 VALUES ($1, 'catalog_release', $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(organization_id.0)
            .bind(release_id)
            .execute(&mut **tx)
            .await?;
            audit(
                tx,
                organization_id,
                AuditTargetResource::CatalogRelease,
                "catalog_release.created",
                release_id,
                request_id,
                None,
                Some(1),
            )
            .await?;
            release_id
        }
    };
    match activate_release(tx, organization_id, release_id).await {
        Ok(()) => {}
        Err(CatalogStoreError::IncompatiblePriceOverride(_)) => {
            return Ok(SystemRelease::IncompatiblePriceOverride);
        }
        Err(CatalogStoreError::Database(error)) => return Err(error),
        Err(_) => return Err(decode("release activation")),
    }
    let version = sqlx::query_scalar::<_, i64>(
        "INSERT INTO gateway.admin_resource_version
             (organization_id, resource_type, resource_id)
         VALUES ($1, 'catalog_activation', $1)
         ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
         SET version = gateway.admin_resource_version.version + 1,
             updated_at = CURRENT_TIMESTAMP
         RETURNING version",
    )
    .bind(organization_id.0)
    .fetch_one(&mut **tx)
    .await?;
    let version = u64::try_from(version).unwrap_or(1);
    audit(
        tx,
        organization_id,
        AuditTargetResource::CatalogActivation,
        "catalog_release.activated",
        release_id,
        request_id,
        version.checked_sub(1).filter(|v| *v > 0),
        Some(version),
    )
    .await?;
    Ok(SystemRelease::Activated(release_id))
}

/// A system catalog write's audit event, on the run's transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn audit(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    target: AuditTargetResource,
    name: &str,
    target_id: Uuid,
    request_id: Uuid,
    before: Option<u64>,
    after: Option<u64>,
) -> Result<(), sqlx::Error> {
    let snapshot = |version: Option<u64>| {
        version.map(|version| AuditSnapshot {
            version: Some(version),
            ..Default::default()
        })
    };
    insert_audit_on(
        conn,
        &AdminAuditEvent::new(
            AuditActor::System,
            organization_id,
            None,
            target,
            action(name),
            target_id,
            AuditOutcome::Succeeded,
            request_id,
            snapshot(before),
            snapshot(after),
        ),
    )
    .await
    .map_err(|error| match error {
        AdminStoreError::Database(error) => error,
        _ => decode("audit event"),
    })
}
