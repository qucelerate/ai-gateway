use crate::domain::{
    accounting::{Currency, PriceSnapshot, PricingConfidence},
    catalog::{CatalogDigest, CatalogManifest, ProviderFamily},
    identity::OrganizationId,
};
use sqlx::{FromRow, PgConnection, PgPool};
use std::collections::HashMap;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum CatalogStoreError {
    #[error("catalog database error")]
    Database(#[from] sqlx::Error),
    #[error("catalog accounting price is invalid")]
    Price(#[from] crate::domain::accounting::AccountingError),
    #[error("catalog graph is inconsistent")]
    Graph,
    /// The release's provider family cannot bill a price that this live
    /// override sets for one of its deployments.
    #[error("price override {0} cannot apply to the release")]
    IncompatiblePriceOverride(Uuid),
}

#[derive(Clone)]
pub struct CatalogStore {
    pool: PgPool,
}

#[derive(Clone, Debug)]
pub struct AppliedCatalogRelease {
    pub id: Uuid,
    pub organization_id: OrganizationId,
    pub version: String,
    pub digest: CatalogDigest,
}

#[derive(Clone, Debug)]
pub struct ResolvedCatalogRoute {
    pub release_id: Uuid,
    pub route_id: Uuid,
    pub digest: CatalogDigest,
    pub deployment_id: Uuid,
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: String,
    pub family: String,
    pub operation: String,
    pub price_id: Uuid,
    pub source: String,
    /// Set when an organization price override replaced the catalog price;
    /// `source` is then `override` and `price` carries the override.
    pub price_override_id: Option<Uuid>,
    pub max_input_bytes: u64,
    pub max_output_tokens: u32,
    pub max_tokens_per_byte: u32,
    pub tokenizer: String,
    pub reservation_ttl_seconds: u64,
    pub chat_input_count_mode: crate::domain::catalog::ChatInputCountMode,
    /// Reasoning tokens reserved beyond the request's output cap.
    pub reasoning_allowance_tokens: u32,
    /// Whether every deployment the route may reach takes tools (D-032).
    pub tools: bool,
    /// Whether the deployments the route may reach make tool call ids of
    /// different dialects, Gemini's and the others' (D-032).
    pub mixed_tool_call_ids: bool,
    /// The tool options a deployment the route may reach refuses (D-032).
    pub tool_option_refusals: crate::domain::catalog::ToolOptionRefusals,
    pub price: PriceSnapshot,
    pub currency: Currency,
}

#[derive(FromRow)]
struct ReleaseRow {
    id: Uuid,
    version: String,
    digest: Vec<u8>,
}

/// One route as the catalog prices it, from a configuration version.
#[derive(Clone, Debug)]
pub(crate) struct RouteRow {
    pub(crate) release_id: Uuid,
    pub(crate) route_id: Uuid,
    pub(crate) release_digest: Vec<u8>,
    pub(crate) deployment_id: Uuid,
    pub(crate) deployment_name: String,
    pub(crate) model: String,
    pub(crate) adapter_instance_key: String,
    pub(crate) family: String,
    pub(crate) operation: String,
    pub(crate) price_id: Uuid,
    pub(crate) input_minor_per_token: i64,
    pub(crate) output_minor_per_token: i64,
    pub(crate) cached_input_minor_per_token: i64,
    pub(crate) reasoning_minor_per_token: i64,
    pub(crate) other_minor_per_token: i64,
    pub(crate) currency: String,
    pub(crate) source_version: String,
    pub(crate) source: String,
    pub(crate) confidence: String,
    pub(crate) price_override_id: Option<Uuid>,
    pub(crate) max_input_bytes: i64,
    pub(crate) max_output_tokens: i64,
    pub(crate) max_tokens_per_byte: i32,
    pub(crate) tokenizer: String,
    pub(crate) reservation_ttl_seconds: i64,
    pub(crate) chat_input_count_mode: String,
    pub(crate) reasoning_allowance_tokens: i64,
    pub(crate) tools: bool,
    /// Whether the route's deployments make tool call ids of different
    /// dialects (D-032).
    pub(crate) mixed_tool_call_ids: bool,
    /// The tool options a deployment the route may reach refuses (D-032).
    pub(crate) tool_option_refusals: crate::domain::catalog::ToolOptionRefusals,
}

impl CatalogStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn apply(
        &self,
        organization_id: OrganizationId,
        manifest: &CatalogManifest,
        digest: CatalogDigest,
    ) -> Result<AppliedCatalogRelease, CatalogStoreError> {
        manifest.validate().map_err(|_| CatalogStoreError::Graph)?;
        if manifest.digest().map_err(|_| CatalogStoreError::Graph)? != digest {
            return Err(CatalogStoreError::Graph);
        }
        let mut tx = self.pool.begin().await?;
        lock_catalog(&mut tx, organization_id).await?;
        let existing = sqlx::query_as::<_, ReleaseRow>(
            "SELECT id, version, digest FROM gateway.catalog_release WHERE organization_id=$1 AND digest=$2",
        )
        .bind(organization_id.0)
        .bind(digest.0.to_vec())
        .fetch_optional(&mut *tx)
        .await?;

        let release = if let Some(row) = existing {
            row
        } else {
            let release_id = Uuid::new_v4();
            insert_release(&mut tx, organization_id, release_id, manifest, digest).await?;
            ReleaseRow {
                id: release_id,
                version: manifest.release.version.clone(),
                digest: digest.0.to_vec(),
            }
        };
        let previous = sqlx::query_scalar::<_, Uuid>(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        activate_release(&mut tx, organization_id, release.id).await?;
        if previous != Some(release.id) {
            // Keep the admin API's `If-Match` on activation honest: a CLI
            // switch is a new activation version, so a stale API client
            // conflicts instead of overwriting it. Re-applying the active
            // release leaves the version alone.
            sqlx::query(
                "INSERT INTO gateway.admin_resource_version
                     (organization_id, resource_type, resource_id)
                 VALUES ($1, 'catalog_activation', $1)
                 ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
                 SET version = gateway.admin_resource_version.version + 1,
                     updated_at = CURRENT_TIMESTAMP",
            )
            .bind(organization_id.0)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(AppliedCatalogRelease {
            id: release.id,
            organization_id,
            version: release.version,
            digest: CatalogDigest(
                release
                    .digest
                    .try_into()
                    .map_err(|_| CatalogStoreError::Graph)?,
            ),
        })
    }

    /// The route the organization serves now for `alias` and `operation`,
    /// read from its served configuration version in the database. Inference
    /// routes from the same versions loaded into memory
    /// (`application::config_cache`); this reads them directly, for tools
    /// and tests.
    pub async fn resolve_active(
        &self,
        organization_id: OrganizationId,
        alias: &str,
        operation: &str,
    ) -> Result<Option<ResolvedCatalogRoute>, CatalogStoreError> {
        let version = super::config_versions::ConfigStore::new(self.pool.clone())
            .served_now(organization_id)
            .await
            .map_err(|error| match error {
                super::config_versions::ConfigStoreError::Database(error) => {
                    CatalogStoreError::Database(error)
                }
                super::config_versions::ConfigStoreError::Invalid(_) => CatalogStoreError::Graph,
            })?;
        version
            .and_then(|version| {
                version
                    .document
                    .resolve(alias, operation, chrono::Utc::now())
            })
            .map(route_from_row)
            .transpose()
    }
}

/// Prices and limits a route. An override that charges a dimension the
/// route's family never reports fails the route instead of falling back to
/// the catalog price.
pub(crate) fn route_from_row(row: RouteRow) -> Result<ResolvedCatalogRoute, CatalogStoreError> {
    let confidence = match row.confidence.as_str() {
        "known" => PricingConfidence::Known,
        "conservative" => PricingConfidence::Conservative,
        _ => return Err(CatalogStoreError::Graph),
    };
    // An override that charges a dimension this family's adapter never
    // reports would be rejected by the reservation guard; refuse the
    // route instead of silently falling back to the catalog price.
    if row.price_override_id.is_some()
        && row
            .family
            .parse::<ProviderFamily>()
            .map_err(|_| CatalogStoreError::Graph)?
            .charges_unreported_usage(
                row.cached_input_minor_per_token,
                row.reasoning_minor_per_token,
                row.other_minor_per_token,
            )
    {
        return Err(CatalogStoreError::Graph);
    }
    let currency = Currency::new(row.currency)?;
    let price = PriceSnapshot::configured_with_breakdown(
        row.input_minor_per_token,
        row.output_minor_per_token,
        row.cached_input_minor_per_token,
        row.reasoning_minor_per_token,
        row.other_minor_per_token,
        row.source_version,
        confidence,
    )?;
    Ok(ResolvedCatalogRoute {
        release_id: row.release_id,
        route_id: row.route_id,
        digest: CatalogDigest(
            row.release_digest
                .try_into()
                .map_err(|_| CatalogStoreError::Graph)?,
        ),
        deployment_id: row.deployment_id,
        deployment: row.deployment_name,
        model: row.model,
        adapter_instance_key: row.adapter_instance_key,
        family: row.family,
        operation: row.operation,
        price_id: row.price_id,
        source: row.source,
        price_override_id: row.price_override_id,
        max_input_bytes: u64::try_from(row.max_input_bytes)
            .map_err(|_| CatalogStoreError::Graph)?,
        max_output_tokens: u32::try_from(row.max_output_tokens)
            .map_err(|_| CatalogStoreError::Graph)?,
        max_tokens_per_byte: u32::try_from(row.max_tokens_per_byte)
            .map_err(|_| CatalogStoreError::Graph)?,
        tokenizer: row.tokenizer,
        reservation_ttl_seconds: u64::try_from(row.reservation_ttl_seconds)
            .map_err(|_| CatalogStoreError::Graph)?,
        chat_input_count_mode: row
            .chat_input_count_mode
            .parse()
            .map_err(|_| CatalogStoreError::Graph)?,
        reasoning_allowance_tokens: u32::try_from(row.reasoning_allowance_tokens)
            .map_err(|_| CatalogStoreError::Graph)?,
        tools: row.tools,
        mixed_tool_call_ids: row.mixed_tool_call_ids,
        tool_option_refusals: row.tool_option_refusals,
        price,
        currency,
    })
}

/// Serialize every release insert and activation by organization so
/// concurrent writers cannot race the active pointer. Held until the caller's
/// transaction ends.
///
/// The organization row is locked first, as the admin mutation coordinator
/// does before any catalog write, so the CLI and the admin API take the two
/// locks in the same order and cannot deadlock.
pub(crate) async fn lock_catalog(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
        .bind(organization_id.0)
        .execute(&mut *conn)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("catalog:organization:{}", organization_id.0))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Insert and seal a complete release graph under `release_id`. The caller
/// holds [`lock_catalog`] and has checked the digest is not yet present.
pub(crate) async fn insert_release(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    release_id: Uuid,
    manifest: &CatalogManifest,
    digest: CatalogDigest,
) -> Result<(), CatalogStoreError> {
    sqlx::query(
        "INSERT INTO gateway.catalog_release (id,organization_id,version,digest) VALUES ($1,$2,$3,$4)",
    )
    .bind(release_id)
    .bind(organization_id.0)
    .bind(&manifest.release.version)
    .bind(digest.0.to_vec())
    .execute(&mut *conn)
    .await?;

    let mut providers = HashMap::new();
    for provider in &manifest.providers {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gateway.catalog_provider_account (id,organization_id,release_id,adapter_instance_key,family) VALUES ($1,$2,$3,$4,$5)")
            .bind(id).bind(organization_id.0).bind(release_id)
            .bind(&provider.adapter_instance_key).bind(&provider.family)
            .execute(&mut *conn).await?;
        providers.insert(provider.adapter_instance_key.clone(), id);
    }
    let mut deployments = HashMap::new();
    for deployment in &manifest.deployments {
        let provider_id = *providers
            .get(&deployment.adapter_instance_key)
            .ok_or(CatalogStoreError::Graph)?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gateway.catalog_deployment (id,organization_id,release_id,provider_account_id,adapter_instance_key,name,model,location) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(id).bind(organization_id.0).bind(release_id).bind(provider_id)
            .bind(&deployment.adapter_instance_key).bind(&deployment.name)
            .bind(&deployment.model).bind(&deployment.location)
            .execute(&mut *conn).await?;
        deployments.insert(deployment.name.clone(), id);
        for capability in &deployment.capabilities {
            let (
                max_input_bytes,
                max_output_tokens,
                max_tokens_per_byte,
                tokenizer,
                reservation_ttl_seconds,
                count_mode,
            ) = capability.limits();
            sqlx::query("INSERT INTO gateway.catalog_capability (id,organization_id,release_id,deployment_id,operation,max_input_bytes,max_output_tokens,max_tokens_per_byte,tokenizer,reservation_ttl_seconds,chat_input_count_mode,reasoning_allowance_tokens,tools) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)")
                .bind(Uuid::new_v4()).bind(organization_id.0).bind(release_id)
                .bind(id).bind(capability.operation())
                .bind(i64::try_from(max_input_bytes).map_err(|_| CatalogStoreError::Graph)?)
                .bind(i64::from(max_output_tokens)).bind(i32::try_from(max_tokens_per_byte).map_err(|_| CatalogStoreError::Graph)?)
                .bind(tokenizer)
                .bind(i64::try_from(reservation_ttl_seconds).map_err(|_| CatalogStoreError::Graph)?)
                .bind(count_mode)
                .bind(i32::try_from(capability.reasoning_allowance_tokens).map_err(|_| CatalogStoreError::Graph)?)
                .bind(capability.tools)
                .execute(&mut *conn).await?;
        }
    }
    let mut prices = HashMap::new();
    for price in &manifest.prices {
        let deployment_id = *deployments
            .get(&price.deployment)
            .ok_or(CatalogStoreError::Graph)?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO gateway.catalog_price (id,organization_id,release_id,deployment_id,operation,input_minor_per_token,output_minor_per_token,cached_input_minor_per_token,reasoning_minor_per_token,other_minor_per_token,currency,source,source_version,confidence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
            .bind(id).bind(organization_id.0).bind(release_id).bind(deployment_id)
            .bind(&price.operation).bind(price.input_minor_per_token).bind(price.output_minor_per_token)
            .bind(price.cached_input_minor_per_token).bind(price.reasoning_minor_per_token)
            .bind(price.other_minor_per_token).bind(&price.currency).bind(&price.source)
            .bind(&price.source_version).bind(&price.confidence)
            .execute(&mut *conn).await?;
        prices.insert((price.deployment.clone(), price.operation.clone()), id);
    }
    for route in &manifest.routes {
        let deployment_id = *deployments
            .get(&route.deployment)
            .ok_or(CatalogStoreError::Graph)?;
        let price_id = *prices
            .get(&(route.deployment.clone(), route.operation.clone()))
            .ok_or(CatalogStoreError::Graph)?;
        let capability_id: Uuid = sqlx::query_scalar("SELECT id FROM gateway.catalog_capability WHERE organization_id=$1 AND release_id=$2 AND deployment_id=$3 AND operation=$4")
            .bind(organization_id.0).bind(release_id).bind(deployment_id).bind(&route.operation)
            .fetch_one(&mut *conn).await?;
        sqlx::query("INSERT INTO gateway.catalog_route (id,organization_id,release_id,alias,operation,deployment_id,capability_id,price_id,weight,priority) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(Uuid::new_v4()).bind(organization_id.0).bind(release_id)
            .bind(&route.alias).bind(&route.operation).bind(deployment_id)
            .bind(capability_id).bind(price_id)
            .bind(i32::try_from(route.weight).map_err(|_| CatalogStoreError::Graph)?)
            .bind(i32::try_from(route.priority).map_err(|_| CatalogStoreError::Graph)?)
            .execute(&mut *conn).await?;
    }
    // Seal the complete graph before publishing its release. A
    // sealed release can be activated again for rollback, but no
    // provider/deployment/capability/price/route rows may be added.
    // This is independent of the active pointer, so switching A → B
    // never reopens A for graph mutations.
    sqlx::query("INSERT INTO gateway.catalog_release_seal (organization_id,release_id) VALUES ($1,$2) ON CONFLICT (organization_id,release_id) DO NOTHING")
        .bind(organization_id.0)
        .bind(release_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Point the organization at `release_id`. A release whose provider family
/// cannot bill a price set by an unrevoked, unexpired override is refused:
/// route resolution would otherwise fail closed on the overridden route.
pub(crate) async fn activate_release(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    release_id: Uuid,
) -> Result<(), CatalogStoreError> {
    let overrides = sqlx::query_as::<_, (Uuid, String, i64, i64, i64)>(
        "SELECT o.id, p.family, o.cached_input_minor_per_token, o.reasoning_minor_per_token,
                o.other_minor_per_token
         FROM gateway.catalog_price_override o
         JOIN gateway.catalog_deployment d
           ON d.organization_id = o.organization_id AND d.release_id = $2
          AND d.name = o.deployment
         JOIN gateway.catalog_provider_account p
           ON p.organization_id = d.organization_id AND p.release_id = d.release_id
          AND p.id = d.provider_account_id
         JOIN gateway.catalog_price pr
           ON pr.organization_id = d.organization_id AND pr.release_id = d.release_id
          AND pr.deployment_id = d.id AND pr.operation = o.operation
         WHERE o.organization_id = $1 AND o.revoked_at IS NULL
           AND (o.effective_until IS NULL OR o.effective_until > now())
         ORDER BY o.created_at, o.id",
    )
    .bind(organization_id.0)
    .bind(release_id)
    .fetch_all(&mut *conn)
    .await?;
    for (id, family, cached_input, reasoning, other) in overrides {
        if family
            .parse::<ProviderFamily>()
            .map_err(|_| CatalogStoreError::Graph)?
            .charges_unreported_usage(cached_input, reasoning, other)
        {
            return Err(CatalogStoreError::IncompatiblePriceOverride(id));
        }
    }
    sqlx::query("INSERT INTO gateway.catalog_active_release (organization_id,release_id) VALUES ($1,$2) ON CONFLICT (organization_id) DO UPDATE SET release_id=EXCLUDED.release_id, activated_at=CURRENT_TIMESTAMP")
        .bind(organization_id.0)
        .bind(release_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
