//! Read-only admin queries over immutable catalog releases.
//!
//! Every query is bound to one organization and one release; rows of another
//! tenant or release are unreachable by construction. Each collection pages
//! on a key that is unique within a release, so ordering is total and stable.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::domain::{
    catalog::{
        CapabilityManifest, CatalogManifest, CatalogOperation, DeploymentManifest, PriceManifest,
        ProviderManifest, ReleaseManifest, RouteManifest,
    },
    catalog_admin::{
        CapabilityView, CatalogReleaseSummary, DeploymentOperationView, DeploymentQuery,
        DeploymentView, PriceView, ProviderAccountView, RouteQuery, RouteView,
    },
    identity::OrganizationId,
};
use crate::persistence::reporting::TimeKeyset;

#[derive(Debug)]
pub enum CatalogAdminStoreError {
    Database(sqlx::Error),
    /// A page keyset could not be parsed.
    InvalidState,
}

impl From<sqlx::Error> for CatalogAdminStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(FromRow)]
struct ManifestCapabilityRow {
    deployment_id: Uuid,
    operation: String,
    max_input_bytes: i64,
    max_output_tokens: i64,
    max_tokens_per_byte: i32,
    tokenizer: String,
    reservation_ttl_seconds: i64,
    chat_input_count_mode: String,
    reasoning_allowance_tokens: i32,
    tools: bool,
}

#[derive(FromRow)]
struct ManifestPriceRow {
    deployment: String,
    operation: String,
    input_minor_per_token: i64,
    output_minor_per_token: i64,
    cached_input_minor_per_token: i64,
    reasoning_minor_per_token: i64,
    other_minor_per_token: i64,
    currency: String,
    source: String,
    source_version: String,
    confidence: String,
}

impl ManifestPriceRow {
    fn manifest(self) -> PriceManifest {
        PriceManifest {
            deployment: self.deployment,
            operation: self.operation,
            input_minor_per_token: self.input_minor_per_token,
            output_minor_per_token: self.output_minor_per_token,
            cached_input_minor_per_token: self.cached_input_minor_per_token,
            reasoning_minor_per_token: self.reasoning_minor_per_token,
            other_minor_per_token: self.other_minor_per_token,
            currency: self.currency,
            source: self.source,
            source_version: self.source_version,
            confidence: self.confidence,
        }
    }
}

/// `(alias, operation, deployment)` keyset for routes: an alias may route
/// an operation to several deployments (D-023).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteKeyset {
    pub alias: String,
    pub operation: String,
    /// `None` in a token issued before routes had several deployments: the
    /// page resumes after every deployment of its alias and operation.
    pub deployment: Option<String>,
}

impl RouteKeyset {
    pub fn of(route: &RouteView) -> Self {
        Self {
            alias: route.alias.clone(),
            operation: route.operation.clone(),
            deployment: Some(route.deployment_name.clone()),
        }
    }

    pub fn to_claim(&self) -> String {
        match &self.deployment {
            Some(deployment) => serde_json::json!([self.alias, self.operation, deployment]),
            None => serde_json::json!([self.alias, self.operation]),
        }
        .to_string()
    }

    pub fn parse(claim: &str) -> Result<Self, CatalogAdminStoreError> {
        let parts: Vec<String> =
            serde_json::from_str(claim).map_err(|_| CatalogAdminStoreError::InvalidState)?;
        match <[String; 3]>::try_from(parts) {
            Ok([alias, operation, deployment]) => Ok(Self {
                alias,
                operation,
                deployment: Some(deployment),
            }),
            Err(parts) => {
                let [alias, operation]: [String; 2] = parts
                    .try_into()
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?;
                Ok(Self {
                    alias,
                    operation,
                    deployment: None,
                })
            }
        }
    }
}

#[derive(Clone)]
pub struct CatalogAdminStore {
    pool: PgPool,
}

#[derive(FromRow)]
struct ReleaseRow {
    id: Uuid,
    version: String,
    digest: Vec<u8>,
    created_at: DateTime<Utc>,
    sealed_at: Option<DateTime<Utc>>,
    activated_at: Option<DateTime<Utc>>,
}

impl ReleaseRow {
    fn summary(self) -> CatalogReleaseSummary {
        CatalogReleaseSummary {
            id: self.id,
            version: self.version,
            digest: hex(&self.digest),
            created_at: self.created_at,
            sealed_at: self.sealed_at,
            active: self.activated_at.is_some(),
            activated_at: self.activated_at,
        }
    }
}

#[derive(FromRow)]
struct ProviderRow {
    id: Uuid,
    release_id: Uuid,
    adapter_instance_key: String,
    family: String,
    deployment_count: i64,
}

#[derive(FromRow)]
struct DeploymentRow {
    id: Uuid,
    release_id: Uuid,
    provider_account_id: Uuid,
    adapter_instance_key: String,
    family: String,
    name: String,
    model: String,
    location: Option<String>,
}

#[derive(FromRow)]
struct CapabilityRow {
    deployment_id: Uuid,
    operation: String,
    id: Uuid,
    max_input_bytes: i64,
    max_output_tokens: i64,
    max_tokens_per_byte: i32,
    tokenizer: String,
    reservation_ttl_seconds: i64,
    chat_input_count_mode: String,
    reasoning_allowance_tokens: i32,
    tools: bool,
}

#[derive(FromRow)]
struct PriceRow {
    deployment_id: Uuid,
    operation: String,
    id: Uuid,
    currency: String,
    input_minor_per_token: i64,
    output_minor_per_token: i64,
    cached_input_minor_per_token: i64,
    reasoning_minor_per_token: i64,
    other_minor_per_token: i64,
    source: String,
    source_version: String,
    confidence: String,
}

#[derive(FromRow)]
struct RouteRow {
    id: Uuid,
    release_id: Uuid,
    alias: String,
    operation: String,
    deployment_id: Uuid,
    deployment_name: String,
    model: String,
    family: String,
    capability_id: Uuid,
    price_id: Uuid,
    weight: i32,
    priority: i32,
}

impl CatalogAdminStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn active_release(
        &self,
        organization_id: OrganizationId,
    ) -> Result<Option<Uuid>, CatalogAdminStoreError> {
        Ok(sqlx::query_scalar(
            "SELECT release_id FROM gateway.catalog_active_release WHERE organization_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The release's catalog as a manifest, `None` if the organization has
    /// no such release. A release keeps no item order, so each list is
    /// sorted by its key: the manifest describes the same catalog, though its
    /// digest differs from the release's when the release was created from
    /// another order.
    pub async fn release_manifest(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<Option<CatalogManifest>, CatalogAdminStoreError> {
        // A sealed release never changes, so the reads need no snapshot.
        let Some(version) = sqlx::query_scalar::<_, String>(
            "SELECT version FROM gateway.catalog_release WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let providers = sqlx::query_as::<_, (String, String)>(
            "SELECT adapter_instance_key, family FROM gateway.catalog_provider_account
             WHERE organization_id = $1 AND release_id = $2
             ORDER BY adapter_instance_key",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|(adapter_instance_key, family)| ProviderManifest {
            adapter_instance_key,
            family,
        })
        .collect();
        let mut capabilities: HashMap<Uuid, Vec<CapabilityManifest>> = HashMap::new();
        for row in sqlx::query_as::<_, ManifestCapabilityRow>(
            "SELECT deployment_id, operation, max_input_bytes, max_output_tokens,
                    max_tokens_per_byte, tokenizer, reservation_ttl_seconds,
                    chat_input_count_mode, reasoning_allowance_tokens, tools
             FROM gateway.catalog_capability
             WHERE organization_id = $1 AND release_id = $2
             ORDER BY deployment_id, operation",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&self.pool)
        .await?
        {
            let capability = CapabilityManifest {
                operation: row.operation,
                max_input_bytes: u64::try_from(row.max_input_bytes)
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?,
                max_output_tokens: u32::try_from(row.max_output_tokens)
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?,
                max_tokens_per_byte: u32::try_from(row.max_tokens_per_byte)
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?,
                tokenizer: row.tokenizer,
                reservation_ttl_seconds: u64::try_from(row.reservation_ttl_seconds)
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?,
                chat_input_count_mode: row.chat_input_count_mode,
                reasoning_allowance_tokens: u32::try_from(row.reasoning_allowance_tokens)
                    .map_err(|_| CatalogAdminStoreError::InvalidState)?,
                tools: row.tools,
            };
            capabilities
                .entry(row.deployment_id)
                .or_default()
                .push(capability);
        }
        let deployments = sqlx::query_as::<_, (Uuid, String, String, Option<String>, String)>(
            "SELECT id, name, model, location, adapter_instance_key
             FROM gateway.catalog_deployment
             WHERE organization_id = $1 AND release_id = $2
             ORDER BY name",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(
            |(id, name, model, location, adapter_instance_key)| DeploymentManifest {
                name,
                model,
                location,
                adapter_instance_key,
                capabilities: capabilities.remove(&id).unwrap_or_default(),
            },
        )
        .collect();
        let prices = sqlx::query_as::<_, ManifestPriceRow>(
            "SELECT d.name AS deployment, p.operation, p.input_minor_per_token,
                    p.output_minor_per_token, p.cached_input_minor_per_token,
                    p.reasoning_minor_per_token, p.other_minor_per_token, p.currency,
                    p.source, p.source_version, p.confidence
             FROM gateway.catalog_price p
             JOIN gateway.catalog_deployment d
               ON d.organization_id = p.organization_id AND d.release_id = p.release_id
              AND d.id = p.deployment_id
             WHERE p.organization_id = $1 AND p.release_id = $2
             ORDER BY d.name, p.operation",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(ManifestPriceRow::manifest)
        .collect();
        let routes = sqlx::query_as::<_, (String, String, String, i32, i32)>(
            "SELECT r.alias, r.operation, d.name, r.weight, r.priority
             FROM gateway.catalog_route r
             JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.release_id
              AND d.id = r.deployment_id
             WHERE r.organization_id = $1 AND r.release_id = $2
             ORDER BY r.alias, r.operation, r.priority, d.name",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(
            |(alias, operation, deployment, weight, priority)| RouteManifest {
                alias,
                operation,
                deployment,
                // The columns' checks keep both positive.
                weight: u32::try_from(weight).unwrap_or(1),
                priority: u32::try_from(priority).unwrap_or(1),
            },
        )
        .collect();
        Ok(Some(CatalogManifest {
            release: ReleaseManifest { version },
            providers,
            deployments,
            prices,
            routes,
        }))
    }

    pub async fn release_exists(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<bool, CatalogAdminStoreError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway.catalog_release
                            WHERE organization_id = $1 AND id = $2)",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The version of the organization's activation pointer, which
    /// `If-Match` on activation compares; 0 before any API or CLI switch.
    pub async fn activation_version(
        &self,
        organization_id: OrganizationId,
    ) -> Result<u64, CatalogAdminStoreError> {
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT version FROM gateway.admin_resource_version
             WHERE organization_id = $1 AND resource_type = 'catalog_activation'
               AND resource_id = $1",
        )
        .bind(organization_id.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(version.map_or(0, |version| u64::try_from(version).unwrap_or(0)))
    }

    /// Releases newest first.
    pub async fn list_releases(
        &self,
        organization_id: OrganizationId,
        page_size: u32,
        keyset: Option<TimeKeyset>,
    ) -> Result<(Vec<CatalogReleaseSummary>, bool), CatalogAdminStoreError> {
        let rows = sqlx::query_as::<_, ReleaseRow>(
            "SELECT r.id, r.version, r.digest, r.created_at, s.sealed_at, a.activated_at
             FROM gateway.catalog_release r
             LEFT JOIN gateway.catalog_release_seal s
               ON s.organization_id = r.organization_id AND s.release_id = r.id
             LEFT JOIN gateway.catalog_active_release a
               ON a.organization_id = r.organization_id AND a.release_id = r.id
             WHERE r.organization_id = $1
               AND ($2::timestamptz IS NULL OR (r.created_at, r.id) < ($2, $3::uuid))
             ORDER BY r.created_at DESC, r.id DESC
             LIMIT $4",
        )
        .bind(organization_id.0)
        .bind(keyset.map(|keyset| keyset.at))
        .bind(keyset.map(|keyset| keyset.id))
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let items = rows.into_iter().map(ReleaseRow::summary).collect();
        Ok(truncate(items, page_size))
    }

    pub async fn release(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<Option<CatalogReleaseSummary>, CatalogAdminStoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::release_in(&mut conn, organization_id, release_id).await
    }

    /// [`Self::release`] on the caller's connection, so a mutation can read
    /// the release as its own transaction leaves it.
    pub async fn release_in(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<Option<CatalogReleaseSummary>, CatalogAdminStoreError> {
        let row = sqlx::query_as::<_, ReleaseRow>(
            "SELECT r.id, r.version, r.digest, r.created_at, s.sealed_at, a.activated_at
             FROM gateway.catalog_release r
             LEFT JOIN gateway.catalog_release_seal s
               ON s.organization_id = r.organization_id AND s.release_id = r.id
             LEFT JOIN gateway.catalog_active_release a
               ON a.organization_id = r.organization_id AND a.release_id = r.id
             WHERE r.organization_id = $1 AND r.id = $2",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(row.map(ReleaseRow::summary))
    }

    /// The release already holding `digest`, if any. Digests are unique per
    /// organization, so an identical manifest is never stored twice.
    pub async fn release_by_digest(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        digest: &[u8; 32],
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT id FROM gateway.catalog_release WHERE organization_id = $1 AND digest = $2",
        )
        .bind(organization_id.0)
        .bind(digest.to_vec())
        .fetch_optional(&mut *conn)
        .await
    }

    /// Provider accounts ordered by adapter instance key.
    pub async fn list_provider_accounts(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        page_size: u32,
        after: Option<&str>,
    ) -> Result<(Vec<ProviderAccountView>, bool), CatalogAdminStoreError> {
        let rows = sqlx::query_as::<_, ProviderRow>(
            "SELECT p.id, p.release_id, p.adapter_instance_key, p.family,
                    (SELECT count(*) FROM gateway.catalog_deployment d
                     WHERE d.organization_id = p.organization_id AND d.release_id = p.release_id
                       AND d.provider_account_id = p.id)::bigint AS deployment_count
             FROM gateway.catalog_provider_account p
             WHERE p.organization_id = $1 AND p.release_id = $2
               AND ($3::text IS NULL OR p.adapter_instance_key > $3)
             ORDER BY p.adapter_instance_key
             LIMIT $4",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(after)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let items = rows
            .into_iter()
            .map(|row| ProviderAccountView {
                id: row.id,
                release_id: row.release_id,
                adapter_instance_key: row.adapter_instance_key,
                family: row.family,
                deployment_count: row.deployment_count,
            })
            .collect();
        Ok(truncate(items, page_size))
    }

    /// Deployments ordered by name, each with its capabilities and prices
    /// grouped per operation.
    pub async fn list_deployments(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        query: &DeploymentQuery,
        page_size: u32,
        after: Option<&str>,
    ) -> Result<(Vec<DeploymentView>, bool), CatalogAdminStoreError> {
        let rows = sqlx::query_as::<_, DeploymentRow>(
            "SELECT d.id, d.release_id, d.provider_account_id, d.adapter_instance_key, p.family,
                    d.name, d.model, d.location
             FROM gateway.catalog_deployment d
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             WHERE d.organization_id = $1 AND d.release_id = $2
               AND ($3::uuid IS NULL OR d.provider_account_id = $3)
               AND ($4::text IS NULL OR d.model = $4)
               AND ($5::text IS NULL OR d.name > $5)
             ORDER BY d.name
             LIMIT $6",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(query.provider_account_id)
        .bind(query.model.as_deref())
        .bind(after)
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let (rows, has_more) = truncate(rows, page_size);
        let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
        let capabilities = sqlx::query_as::<_, CapabilityRow>(
            "SELECT deployment_id, operation, id, max_input_bytes, max_output_tokens,
                    max_tokens_per_byte, tokenizer, reservation_ttl_seconds, chat_input_count_mode,
                    reasoning_allowance_tokens, tools
             FROM gateway.catalog_capability
             WHERE organization_id = $1 AND release_id = $2 AND deployment_id = ANY($3)",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let prices = sqlx::query_as::<_, PriceRow>(
            "SELECT deployment_id, operation, id, btrim(currency) AS currency,
                    input_minor_per_token, output_minor_per_token, cached_input_minor_per_token,
                    reasoning_minor_per_token, other_minor_per_token, source, source_version,
                    confidence
             FROM gateway.catalog_price
             WHERE organization_id = $1 AND release_id = $2 AND deployment_id = ANY($3)",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;

        let mut operations: HashMap<(Uuid, String), DeploymentOperationView> = HashMap::new();
        for row in capabilities {
            operations
                .entry((row.deployment_id, row.operation.clone()))
                .or_insert_with(|| empty_operation(&row.operation))
                .capability = Some(CapabilityView {
                id: row.id,
                max_input_bytes: row.max_input_bytes,
                max_output_tokens: row.max_output_tokens,
                max_tokens_per_byte: row.max_tokens_per_byte,
                tokenizer: row.tokenizer,
                reservation_ttl_seconds: row.reservation_ttl_seconds,
                chat_input_count_mode: row.chat_input_count_mode,
                reasoning_allowance_tokens: row.reasoning_allowance_tokens,
                tools: row.tools,
            });
        }
        for row in prices {
            operations
                .entry((row.deployment_id, row.operation.clone()))
                .or_insert_with(|| empty_operation(&row.operation))
                .price = Some(PriceView {
                id: row.id,
                currency: row.currency,
                input_minor_per_token: row.input_minor_per_token,
                output_minor_per_token: row.output_minor_per_token,
                cached_input_minor_per_token: row.cached_input_minor_per_token,
                reasoning_minor_per_token: row.reasoning_minor_per_token,
                other_minor_per_token: row.other_minor_per_token,
                source: row.source,
                source_version: row.source_version,
                confidence: row.confidence,
            });
        }
        let items = rows
            .into_iter()
            .map(|row| {
                let ops: Vec<DeploymentOperationView> = [
                    CatalogOperation::Chat,
                    CatalogOperation::ChatStream,
                    CatalogOperation::Embeddings,
                ]
                .into_iter()
                .filter_map(|operation| operations.remove(&(row.id, operation.as_str().to_owned())))
                .collect();
                DeploymentView {
                    id: row.id,
                    release_id: row.release_id,
                    provider_account_id: row.provider_account_id,
                    adapter_instance_key: row.adapter_instance_key,
                    family: row.family,
                    name: row.name,
                    model: row.model,
                    location: row.location,
                    operations: ops,
                }
            })
            .collect();
        Ok((items, has_more))
    }

    /// Routes ordered by `(alias, operation)`.
    pub async fn list_routes(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        query: &RouteQuery,
        page_size: u32,
        keyset: Option<RouteKeyset>,
    ) -> Result<(Vec<RouteView>, bool), CatalogAdminStoreError> {
        let rows = sqlx::query_as::<_, RouteRow>(
            "SELECT r.id, r.release_id, r.alias, r.operation, r.deployment_id,
                    d.name AS deployment_name, d.model, p.family, r.capability_id, r.price_id,
                    r.weight, r.priority
             FROM gateway.catalog_route r
             JOIN gateway.catalog_deployment d
               ON d.organization_id = r.organization_id AND d.release_id = r.release_id
              AND d.id = r.deployment_id
             JOIN gateway.catalog_provider_account p
               ON p.organization_id = d.organization_id AND p.release_id = d.release_id
              AND p.id = d.provider_account_id
             WHERE r.organization_id = $1 AND r.release_id = $2
               AND ($3::text IS NULL OR r.alias = $3)
               AND ($4::text IS NULL OR r.operation = $4)
               AND ($5::text IS NULL OR (r.alias, r.operation) > ($5, $6::text)
                    OR ($7::text IS NOT NULL AND r.alias = $5 AND r.operation = $6
                        AND d.name > $7))
             ORDER BY r.alias, r.operation, d.name
             LIMIT $8",
        )
        .bind(organization_id.0)
        .bind(release_id)
        .bind(query.alias.as_deref())
        .bind(query.operation.map(CatalogOperation::as_str))
        .bind(keyset.as_ref().map(|keyset| keyset.alias.as_str()))
        .bind(keyset.as_ref().map(|keyset| keyset.operation.as_str()))
        .bind(
            keyset
                .as_ref()
                .and_then(|keyset| keyset.deployment.as_deref()),
        )
        .bind(i64::from(page_size) + 1)
        .fetch_all(&self.pool)
        .await?;
        let items = rows
            .into_iter()
            .map(|row| RouteView {
                id: row.id,
                release_id: row.release_id,
                alias: row.alias,
                operation: row.operation,
                deployment_id: row.deployment_id,
                deployment_name: row.deployment_name,
                model: row.model,
                family: row.family,
                capability_id: row.capability_id,
                price_id: row.price_id,
                weight: u32::try_from(row.weight).unwrap_or(1),
                priority: u32::try_from(row.priority).unwrap_or(1),
            })
            .collect();
        Ok(truncate(items, page_size))
    }
}

fn empty_operation(operation: &str) -> DeploymentOperationView {
    DeploymentOperationView {
        operation: operation.to_owned(),
        capability: None,
        price: None,
    }
}

fn truncate<T>(mut items: Vec<T>, page_size: u32) -> (Vec<T>, bool) {
    let has_more = items.len() > page_size as usize;
    items.truncate(page_size as usize);
    (items, has_more)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_keyset_round_trips_and_digest_is_hex() {
        let keyset = RouteKeyset {
            alias: "a|b\"c".into(),
            operation: "chat".into(),
            deployment: Some("chat-b".into()),
        };
        assert_eq!(RouteKeyset::parse(&keyset.to_claim()).unwrap(), keyset);
        let earlier = RouteKeyset::parse(r#"["a","chat"]"#).unwrap();
        assert_eq!(
            (earlier.alias.as_str(), earlier.operation.as_str()),
            ("a", "chat")
        );
        assert_eq!(earlier.deployment, None);
        assert!(RouteKeyset::parse("not json").is_err());
        assert_eq!(hex(&[0, 171, 255]), "00abff");
    }
}
