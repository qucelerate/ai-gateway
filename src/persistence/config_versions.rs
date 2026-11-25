//! Configuration versions and their feed (migration 0062).
//!
//! A version is an organization's routing configuration: its active catalog
//! release's routes and its live price overrides, published by database
//! triggers in the same transaction as each change. API-serving processes
//! load the version each organization serves and route from memory; see
//! `application::config_cache`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    config::InstanceConfig,
    domain::{catalog::CatalogOperation, identity::OrganizationId},
};

use super::catalog::RouteRow;

/// The only document schema this build reads.
const DOCUMENT_SCHEMA: u32 = 1;

#[derive(Debug, Error)]
pub enum ConfigStoreError {
    #[error("configuration database error")]
    Database(#[from] sqlx::Error),
    /// The stored digest does not match the document, or the document is not
    /// one this build can read.
    #[error("configuration version {0} is not valid")]
    Invalid(Uuid),
}

/// A feed position and the entry an instance applied there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedPosition {
    pub sequence: i64,
    pub entry_id: Uuid,
}

/// The version an organization serves, checked against its digest.
#[derive(Clone, Debug)]
pub struct LoadedVersion {
    pub organization_id: OrganizationId,
    pub version_id: Uuid,
    pub digest: [u8; 32],
    pub document: ConfigDocument,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ConfigDocument {
    pub schema: u32,
    pub release: Option<DocumentRelease>,
    pub routes: Vec<DocumentRoute>,
    pub overrides: Vec<DocumentOverride>,
    /// The organization's routing policy (migration 0101); absent when it
    /// restricts nothing.
    #[serde(default)]
    pub routing_policy: Option<DocumentRoutingPolicy>,
}

/// The provider families and deployment locations an organization's
/// requests may be sent to; a list absent permits any.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DocumentRoutingPolicy {
    #[serde(default)]
    pub providers: Option<Vec<String>>,
    #[serde(default)]
    pub locations: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocumentRelease {
    pub id: Uuid,
    /// Hex SHA-256 of the release manifest.
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocumentRoute {
    pub route_id: Uuid,
    pub alias: String,
    pub operation: String,
    pub deployment_id: Uuid,
    pub deployment: String,
    pub model: String,
    pub adapter_instance_key: String,
    pub family: String,
    pub price: DocumentPrice,
    pub capability: DocumentCapability,
    /// Absent (1) unless set, so documents published before weighted routes
    /// parse and keep their digest (D-023).
    #[serde(default = "one")]
    pub weight: u32,
    #[serde(default = "one")]
    pub priority: u32,
    /// The deployment's location; present only while the organization's
    /// routing policy restricts locations.
    #[serde(default)]
    pub location: Option<String>,
}

fn one() -> u32 {
    1
}

/// One deployment an alias may reach, with its weight and priority, priced
/// and limited as [`ConfigDocument::candidates`] says.
#[derive(Debug)]
pub(crate) struct RouteCandidate {
    pub(crate) row: RouteRow,
    pub(crate) weight: u32,
    pub(crate) priority: u32,
    /// Whether the organization's routing policy permits this deployment;
    /// inference considers only those that are (D-023).
    pub(crate) permitted: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocumentPrice {
    pub id: Uuid,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub currency: String,
    pub source: String,
    pub source_version: String,
    pub confidence: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocumentCapability {
    pub max_input_bytes: i64,
    pub max_output_tokens: i64,
    pub max_tokens_per_byte: i32,
    pub tokenizer: String,
    pub reservation_ttl_seconds: i64,
    pub chat_input_count_mode: String,
    /// Absent (zero) unless set, so documents published before it parse.
    #[serde(default)]
    pub reasoning_allowance_tokens: i64,
    /// Absent (false) unless set (D-032).
    #[serde(default)]
    pub tools: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DocumentOverride {
    pub id: Uuid,
    pub deployment: String,
    pub operation: String,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub currency: String,
    pub confidence: String,
    pub version: String,
    pub effective_from: DateTime<Utc>,
    pub effective_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ConfigDocument {
    /// The route for `alias` and `operation` at `now`: of its deployments,
    /// the first of the lowest priority with the highest weight. Inference
    /// picks among [`Self::candidates`] by weight and health instead; this
    /// is for tools and tests. Only deployments the routing policy permits.
    pub(crate) fn resolve(
        &self,
        alias: &str,
        operation: &str,
        now: DateTime<Utc>,
    ) -> Option<RouteRow> {
        self.candidates(alias, operation, now)
            .into_iter()
            .filter(|candidate| candidate.permitted)
            .min_by_key(|candidate| (candidate.priority, std::cmp::Reverse(candidate.weight)))
            .map(|candidate| candidate.row)
    }

    /// Every deployment `alias` routes `operation` to at `now`, each priced
    /// by the override in effect then (the latest to start, then the latest
    /// created), as the catalog priced it before versions existed.
    ///
    /// One alias is one contract (D-023): every candidate carries the
    /// route's tightest input and output caps, the largest reasoning
    /// allowance, tokens-per-byte bound and reservation lifetime of its
    /// deployments, so a request never fits one deployment and not another.
    /// Each keeps its own price and confidence: a reservation's price must
    /// be its deployment's, as the catalog provenance check requires.
    ///
    /// Streaming is a delivery mode of chat (D-020): a streamed request takes
    /// the alias's `chat` route, with its price, limits and overrides. A
    /// `chat_stream` route still wins where a release made before D-020
    /// declares one. Each row names the route's own operation, which the
    /// reservation's catalog provenance must match; the request path still
    /// records that the request streamed.
    pub(crate) fn candidates(
        &self,
        alias: &str,
        operation: &str,
        now: DateTime<Utc>,
    ) -> Vec<RouteCandidate> {
        let Some(release) = self.release.as_ref() else {
            return Vec::new();
        };
        let Some(digest) = decode_hex(&release.digest) else {
            return Vec::new();
        };
        let find = |wanted: &str| -> Vec<&DocumentRoute> {
            self.routes
                .iter()
                .filter(|route| route.alias == alias && route.operation == wanted)
                .collect()
        };
        let mut routes = find(operation);
        if routes.is_empty() && operation == CatalogOperation::ChatStream.as_str() {
            routes = find(CatalogOperation::Chat.as_str());
        }
        let catalog_currency = routes.first().map(|route| route.price.currency.clone());
        let mut candidates: Vec<RouteCandidate> = routes
            .into_iter()
            .map(|route| RouteCandidate {
                row: self.row(release.id, &digest, route, now),
                weight: route.weight,
                priority: route.priority,
                permitted: self.permits(route),
            })
            .collect();
        if candidates.len() > 1 {
            let rows = || candidates.iter().map(|candidate| &candidate.row);
            let max_input_bytes = rows().map(|row| row.max_input_bytes).min();
            let max_output_tokens = rows().map(|row| row.max_output_tokens).min();
            let max_tokens_per_byte = rows().map(|row| row.max_tokens_per_byte).max();
            let reservation_ttl_seconds = rows().map(|row| row.reservation_ttl_seconds).max();
            let reasoning_allowance_tokens = rows().map(|row| row.reasoning_allowance_tokens).max();
            // The alias takes tools only where every deployment does, and
            // only where a conversation's calls can go back to any of them:
            // Gemini's ids carry thought signatures no other provider takes.
            let tools = rows().all(|row| row.tools);
            // A dialect unknown until called (an endpoint's) or of a family
            // the gateway does not know may differ from any other, itself
            // included, so it counts as mixed on a route of several.
            let dialects: Vec<_> = rows()
                .map(|row| crate::domain::catalog::tool_call_ids(&row.family, &row.model))
                .collect();
            let mixed_tool_call_ids = dialects.iter().any(|dialect| {
                matches!(
                    dialect,
                    None | Some(crate::domain::catalog::ToolCallIds::Unknown)
                ) || *dialect != dialects[0]
            });
            // An option any deployment refuses is refused on the route.
            let tool_option_refusals = rows().map(|row| row.tool_option_refusals).fold(
                Default::default(),
                crate::domain::catalog::ToolOptionRefusals::or,
            );
            for candidate in &mut candidates {
                let row = &mut candidate.row;
                row.max_input_bytes = max_input_bytes.unwrap_or(row.max_input_bytes);
                row.max_output_tokens = max_output_tokens.unwrap_or(row.max_output_tokens);
                row.max_tokens_per_byte = max_tokens_per_byte.unwrap_or(row.max_tokens_per_byte);
                row.reservation_ttl_seconds =
                    reservation_ttl_seconds.unwrap_or(row.reservation_ttl_seconds);
                row.reasoning_allowance_tokens =
                    reasoning_allowance_tokens.unwrap_or(row.reasoning_allowance_tokens);
                row.tools = tools;
                row.mixed_tool_call_ids = mixed_tool_call_ids;
                row.tool_option_refusals = tool_option_refusals;
            }
        }
        // The limits above are the configured route's, whatever is left of
        // it below. A price override in another currency would split the
        // route's one currency (D-023): while it is in effect, the route keeps only its
        // deployments in the catalog's currency (or, when every deployment
        // is overridden, in its preferred deployment's), so every request
        // of the alias draws on the same budgets.
        if candidates
            .iter()
            .any(|candidate| candidate.row.currency != candidates[0].row.currency)
        {
            let currency = catalog_currency
                .filter(|currency| {
                    candidates
                        .iter()
                        .any(|candidate| &candidate.row.currency == currency)
                })
                .or_else(|| {
                    candidates
                        .iter()
                        .min_by_key(|candidate| {
                            (candidate.priority, std::cmp::Reverse(candidate.weight))
                        })
                        .map(|candidate| candidate.row.currency.clone())
                })
                .unwrap_or_default();
            tracing::warn!(
                alias,
                operation,
                currency,
                "a price override in another currency takes deployments out of a route"
            );
            candidates.retain(|candidate| candidate.row.currency == currency);
        }
        candidates
    }

    /// Whether the routing policy permits `route`'s deployment.
    fn permits(&self, route: &DocumentRoute) -> bool {
        let Some(policy) = &self.routing_policy else {
            return true;
        };
        crate::domain::routing_policy::permits(
            policy.providers.as_deref(),
            policy.locations.as_deref(),
            &route.family,
            route.location.as_deref(),
        )
    }

    fn row(
        &self,
        release_id: Uuid,
        digest: &[u8],
        route: &DocumentRoute,
        now: DateTime<Utc>,
    ) -> RouteRow {
        let price_override = self
            .overrides
            .iter()
            .filter(|o| {
                o.deployment == route.deployment
                    && o.operation == route.operation
                    && o.effective_from <= now
                    && o.effective_until.is_none_or(|until| until > now)
            })
            .max_by_key(|o| (o.effective_from, o.created_at, o.id));
        let price = &route.price;
        let capability = &route.capability;
        RouteRow {
            release_id,
            route_id: route.route_id,
            release_digest: digest.to_vec(),
            deployment_id: route.deployment_id,
            deployment_name: route.deployment.clone(),
            model: route.model.clone(),
            adapter_instance_key: route.adapter_instance_key.clone(),
            family: route.family.clone(),
            operation: route.operation.clone(),
            price_id: price.id,
            input_minor_per_token: price_override
                .map_or(price.input_minor_per_token, |o| o.input_minor_per_token),
            output_minor_per_token: price_override
                .map_or(price.output_minor_per_token, |o| o.output_minor_per_token),
            cached_input_minor_per_token: price_override
                .map_or(price.cached_input_minor_per_token, |o| {
                    o.cached_input_minor_per_token
                }),
            reasoning_minor_per_token: price_override
                .map_or(price.reasoning_minor_per_token, |o| {
                    o.reasoning_minor_per_token
                }),
            other_minor_per_token: price_override
                .map_or(price.other_minor_per_token, |o| o.other_minor_per_token),
            currency: price_override
                .map_or(&price.currency, |o| &o.currency)
                .clone(),
            source_version: price_override
                .map_or(&price.source_version, |o| &o.version)
                .clone(),
            source: match price_override {
                Some(_) => "override".to_owned(),
                None => price.source.clone(),
            },
            confidence: price_override
                .map_or(&price.confidence, |o| &o.confidence)
                .clone(),
            price_override_id: price_override.map(|o| o.id),
            max_input_bytes: capability.max_input_bytes,
            max_output_tokens: capability.max_output_tokens,
            max_tokens_per_byte: capability.max_tokens_per_byte,
            tokenizer: capability.tokenizer.clone(),
            reservation_ttl_seconds: capability.reservation_ttl_seconds,
            chat_input_count_mode: capability.chat_input_count_mode.clone(),
            reasoning_allowance_tokens: capability.reasoning_allowance_tokens,
            tools: capability.tools,
            mixed_tool_call_ids: false,
            tool_option_refusals: crate::domain::catalog::ToolOptionRefusals::of(
                &route.family,
                &route.model,
            ),
        }
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(value.get(at..at + 2)?, 16).ok())
        .collect()
}

#[derive(FromRow)]
struct VersionRow {
    organization_id: Uuid,
    version_id: Uuid,
    digest: Vec<u8>,
    document: String,
}

impl VersionRow {
    /// Checks the digest over the document's stored text, then parses it.
    fn load(self) -> Result<LoadedVersion, ConfigStoreError> {
        let invalid = || ConfigStoreError::Invalid(self.version_id);
        let digest: [u8; 32] = self.digest.as_slice().try_into().map_err(|_| invalid())?;
        if <[u8; 32]>::from(Sha256::digest(self.document.as_bytes())) != digest {
            return Err(invalid());
        }
        let document: ConfigDocument =
            serde_json::from_str(&self.document).map_err(|_| invalid())?;
        if document.schema != DOCUMENT_SCHEMA {
            return Err(invalid());
        }
        Ok(LoadedVersion {
            organization_id: OrganizationId(self.organization_id),
            version_id: self.version_id,
            digest,
            document,
        })
    }
}

const SERVED: &str = "SELECT s.organization_id, v.id AS version_id, v.digest,
            v.document::TEXT AS document
     FROM gateway.config_served_versions($1) s
     JOIN gateway.config_version v
       ON v.organization_id = s.organization_id AND v.id = s.version_id";

#[derive(Clone)]
pub struct ConfigStore {
    pool: PgPool,
}

impl ConfigStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// One consistent view of the feed and versions for a process's
    /// refresh. Its reads share a snapshot, so a version the sweep deletes
    /// after the head is read cannot be missing from the load at that head:
    /// the sweep deletes only versions no entry at or before a later head
    /// serves.
    pub async fn snapshot(&self) -> Result<ConfigSnapshot, ConfigStoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        Ok(ConfigSnapshot { tx })
    }

    /// The version `organization` serves now, read straight from the
    /// database rather than a process's loaded configuration.
    pub async fn served_now(
        &self,
        organization: OrganizationId,
    ) -> Result<Option<LoadedVersion>, ConfigStoreError> {
        sqlx::query_as::<_, VersionRow>(&format!("{SERVED} WHERE s.organization_id = $2"))
            .bind(i64::MAX)
            .bind(organization.0)
            .fetch_optional(&self.pool)
            .await?
            .map(VersionRow::load)
            .transpose()
    }

    /// Records that this process is alive and what it has applied.
    pub async fn report(
        &self,
        instance: &InstanceConfig,
        profile: &str,
        started_at: DateTime<Utc>,
        applied: Option<FeedPosition>,
        loaded_at: Option<DateTime<Utc>>,
    ) -> Result<(), ConfigStoreError> {
        sqlx::query(
            "INSERT INTO gateway.gateway_instance
                 (instance_id, profile, region, build, applied_sequence, applied_entry_id,
                  loaded_at, started_at, last_seen_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CURRENT_TIMESTAMP)
             ON CONFLICT (instance_id) DO UPDATE SET
                 profile = EXCLUDED.profile, region = EXCLUDED.region,
                 build = EXCLUDED.build, applied_sequence = EXCLUDED.applied_sequence,
                 applied_entry_id = EXCLUDED.applied_entry_id,
                 loaded_at = EXCLUDED.loaded_at, started_at = EXCLUDED.started_at,
                 last_seen_at = CURRENT_TIMESTAMP",
        )
        .bind(&instance.id)
        .bind(profile)
        .bind(&instance.region)
        .bind(&instance.build)
        .bind(applied.map(|position| position.sequence))
        .bind(applied.map(|position| position.entry_id))
        .bind(loaded_at)
        .bind(started_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One page of the organization's versions, newest first, after the
    /// `before` feed position. The flag reports whether more rows follow.
    pub async fn versions(
        &self,
        organization: OrganizationId,
        page_size: u32,
        before: Option<i64>,
    ) -> Result<(Vec<ConfigVersionView>, bool), ConfigStoreError> {
        let rows = sqlx::query_as::<_, VersionViewRow>(
            "SELECT f.sequence, v.id, v.digest, v.release_id, v.created_at,
                    jsonb_array_length(v.document -> 'routes') AS routes,
                    jsonb_array_length(v.document -> 'overrides') AS overrides,
                    v.id = s.served AS served, v.id = s.pinned AS pinned,
                    v.id = s.newest AS newest
             FROM gateway.config_feed f
             JOIN gateway.config_version v
               ON v.organization_id = f.organization_id AND v.id = f.version_id
             CROSS JOIN (
                 SELECT gateway.config_served_version($1, 9223372036854775807) AS served,
                        gateway.config_pinned_version($1) AS pinned,
                        (SELECT n.version_id FROM gateway.config_feed n
                         WHERE n.organization_id = $1 AND n.kind = 'version'
                         ORDER BY n.sequence DESC LIMIT 1) AS newest
             ) s
             WHERE f.organization_id = $1 AND f.kind = 'version'
               AND ($2::bigint IS NULL OR f.sequence < $2)
             ORDER BY f.sequence DESC
             LIMIT $3",
        )
        .bind(organization.0)
        .bind(before)
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

    /// The organization's pin, newest and served versions now.
    pub async fn pin_state(
        conn: &mut PgConnection,
        organization: OrganizationId,
    ) -> Result<PinState, sqlx::Error> {
        let (pinned, served, newest) =
            sqlx::query_as::<_, (Option<Uuid>, Option<Uuid>, Option<Uuid>)>(
                "SELECT gateway.config_pinned_version($1),
                    gateway.config_served_version($1, 9223372036854775807),
                    (SELECT version_id FROM gateway.config_feed
                     WHERE organization_id = $1 AND kind = 'version'
                     ORDER BY sequence DESC LIMIT 1)",
            )
            .bind(organization.0)
            .fetch_one(&mut *conn)
            .await?;
        Ok(PinState {
            pinned,
            served,
            newest,
        })
    }

    /// Locks the organization's row, as an admin mutation does first. Every
    /// writer to the feed holds it before the feed lock: a feed entry takes
    /// a key-share lock on the row, which an admin mutation holding the row
    /// and waiting for the feed lock would otherwise never release.
    pub async fn lock_organization(
        conn: &mut PgConnection,
        organization: OrganizationId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1 FROM gateway.organization WHERE id = $1 FOR UPDATE")
            .bind(organization.0)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    /// Pins the organization to `version`, or unpins it with `None`, by
    /// appending a feed entry. The caller holds the organization's row lock
    /// ([`Self::lock_organization`]) and, through the admin API, the pin's
    /// version row, in that order. Holds the feed lock, as publishers and the
    /// sweep do, so the version cannot be swept while it is being pinned.
    pub async fn set_pin(
        conn: &mut PgConnection,
        organization: OrganizationId,
        version: Option<Uuid>,
    ) -> Result<(), PinError> {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('config:feed', 0))")
            .execute(&mut *conn)
            .await?;
        let pinned = Self::pin_state(conn, organization).await?.pinned;
        match version {
            Some(version) => {
                let exists = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM gateway.config_version
                                    WHERE organization_id = $1 AND id = $2)",
                )
                .bind(organization.0)
                .bind(version)
                .fetch_one(&mut *conn)
                .await?;
                if !exists {
                    return Err(PinError::VersionNotFound);
                }
            }
            None if pinned.is_none() => return Err(PinError::NotPinned),
            None => {}
        }
        sqlx::query(
            "INSERT INTO gateway.config_feed (entry_id, organization_id, kind, version_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(organization.0)
        .bind(if version.is_some() { "pin" } else { "unpin" })
        .bind(version)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Records a pin change made outside the admin API as a new version of
    /// the organization's pin, so an admin client's `If-Match` conflicts.
    pub async fn bump_pin_version(
        conn: &mut PgConnection,
        organization: OrganizationId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO gateway.admin_resource_version
                 (organization_id, resource_type, resource_id)
             VALUES ($1, 'config_pin', $1)
             ON CONFLICT (organization_id, resource_type, resource_id) DO UPDATE
             SET version = gateway.admin_resource_version.version + 1,
                 updated_at = CURRENT_TIMESTAMP",
        )
        .bind(organization.0)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// The pin's admin resource version; 0 before its first change.
    pub async fn pin_version(&self, organization: OrganizationId) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT version FROM gateway.admin_resource_version
             WHERE organization_id = $1 AND resource_type = 'config_pin' AND resource_id = $1",
        )
        .bind(organization.0)
        .fetch_optional(&self.pool)
        .await?
        .and_then(|version| u64::try_from(version).ok())
        .unwrap_or(0))
    }

    /// Every instance seen, with what it serves `organization` at its
    /// applied position.
    pub async fn instances(
        &self,
        organization: OrganizationId,
    ) -> Result<Vec<InstanceView>, ConfigStoreError> {
        Ok(sqlx::query_as::<_, InstanceRow>(
            "SELECT i.instance_id, i.profile, i.region, i.build, i.applied_sequence,
                    i.loaded_at, i.started_at, i.last_seen_at,
                    CASE WHEN i.applied_sequence IS NOT NULL
                         THEN gateway.config_served_version($1, i.applied_sequence) END
                        AS serving,
                    i.last_seen_at < CURRENT_TIMESTAMP - INTERVAL '5 minutes' AS stale
             FROM gateway.gateway_instance i
             WHERE i.profile <> 'worker'
             ORDER BY i.instance_id",
        )
        .bind(organization.0)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Into::into)
        .collect())
    }

    /// Deletes up to `max_rows` versions created before `cutoff`, never an
    /// organization's newest or pinned version. Their feed entries stay,
    /// without the version.
    pub async fn sweep_versions(
        &self,
        cutoff: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Serializes with pinning, so a version is not swept as it is pinned.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('config:feed', 0))")
            .execute(&mut *tx)
            .await?;
        let result = sqlx::query(
            "DELETE FROM gateway.config_version
             WHERE id IN (
                 SELECT v.id FROM gateway.config_version v
                 WHERE v.created_at < $1
                   AND v.id IS DISTINCT FROM gateway.config_pinned_version(v.organization_id)
                   AND v.id IS DISTINCT FROM (
                       SELECT f.version_id FROM gateway.config_feed f
                       WHERE f.organization_id = v.organization_id AND f.kind = 'version'
                       ORDER BY f.sequence DESC LIMIT 1)
                 ORDER BY v.created_at
                 LIMIT $2
             )",
        )
        .bind(cutoff)
        .bind(max_rows)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    /// Deletes up to `max_rows` instance rows last seen before `cutoff`.
    pub async fn sweep_instances(
        &self,
        cutoff: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.gateway_instance
             WHERE instance_id IN (
                 SELECT instance_id FROM gateway.gateway_instance
                 WHERE last_seen_at < $1
                 ORDER BY last_seen_at
                 LIMIT $2
             )",
        )
        .bind(cutoff)
        .bind(max_rows)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Reads in one REPEATABLE READ snapshot; see [`ConfigStore::snapshot`].
pub struct ConfigSnapshot {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
}

impl ConfigSnapshot {
    /// The newest feed entry; `None` while the feed is empty.
    pub async fn head(&mut self) -> Result<Option<FeedPosition>, ConfigStoreError> {
        Ok(sqlx::query_as::<_, (i64, Uuid)>(
            "SELECT sequence, entry_id FROM gateway.config_feed ORDER BY sequence DESC LIMIT 1",
        )
        .fetch_optional(&mut *self.tx)
        .await?
        .map(|(sequence, entry_id)| FeedPosition { sequence, entry_id }))
    }

    /// Whether the entry an instance applied is still at its position. It
    /// is not after a restore or a failover that lost it, even when later
    /// entries have since taken that position.
    pub async fn still_applied(
        &mut self,
        position: FeedPosition,
    ) -> Result<bool, ConfigStoreError> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT entry_id FROM gateway.config_feed WHERE sequence = $1",
        )
        .bind(position.sequence)
        .fetch_optional(&mut *self.tx)
        .await?
            == Some(position.entry_id))
    }

    /// Organizations with feed entries after `after` up to `upto`.
    pub async fn changed(
        &mut self,
        after: i64,
        upto: i64,
    ) -> Result<Vec<OrganizationId>, ConfigStoreError> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT organization_id FROM gateway.config_feed
             WHERE sequence > $1 AND sequence <= $2",
        )
        .bind(after)
        .bind(upto)
        .fetch_all(&mut *self.tx)
        .await?
        .into_iter()
        .map(OrganizationId)
        .collect())
    }

    /// The version each organization serves at `upto`, or only those of
    /// `organizations` when given.
    pub async fn served(
        &mut self,
        upto: i64,
        organizations: Option<&[OrganizationId]>,
    ) -> Result<Vec<LoadedVersion>, ConfigStoreError> {
        let rows = match organizations {
            None => {
                sqlx::query_as::<_, VersionRow>(SERVED)
                    .bind(upto)
                    .fetch_all(&mut *self.tx)
                    .await?
            }
            Some(organizations) => {
                let ids: Vec<Uuid> = organizations.iter().map(|id| id.0).collect();
                sqlx::query_as::<_, VersionRow>(&format!(
                    "{SERVED} WHERE s.organization_id = ANY($2)"
                ))
                .bind(upto)
                .bind(ids)
                .fetch_all(&mut *self.tx)
                .await?
            }
        };
        rows.into_iter().map(VersionRow::load).collect()
    }
}

#[derive(Debug, Error)]
pub enum PinError {
    #[error("configuration database error")]
    Database(#[from] sqlx::Error),
    #[error("the version does not exist in the organization")]
    VersionNotFound,
    #[error("the organization is not pinned")]
    NotPinned,
}

/// What the organization serves now and what it is pinned to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinState {
    pub pinned: Option<Uuid>,
    pub served: Option<Uuid>,
    pub newest: Option<Uuid>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct ConfigVersionView {
    pub id: Uuid,
    /// The feed position it was published at; the list's order.
    pub sequence: i64,
    /// Hex SHA-256 of the document.
    pub digest: String,
    pub release_id: Option<Uuid>,
    pub routes: u32,
    pub overrides: u32,
    /// Served now: the pinned version while pinned, else the newest.
    pub served: bool,
    pub pinned: bool,
    pub newest: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct VersionViewRow {
    sequence: i64,
    id: Uuid,
    digest: Vec<u8>,
    release_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    routes: Option<i32>,
    overrides: Option<i32>,
    served: Option<bool>,
    pinned: Option<bool>,
    newest: Option<bool>,
}

impl From<VersionViewRow> for ConfigVersionView {
    fn from(row: VersionViewRow) -> Self {
        let count = |value: Option<i32>| value.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        Self {
            id: row.id,
            sequence: row.sequence,
            digest: row
                .digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            release_id: row.release_id,
            routes: count(row.routes),
            overrides: count(row.overrides),
            served: row.served.unwrap_or(false),
            pinned: row.pinned.unwrap_or(false),
            newest: row.newest.unwrap_or(false),
            created_at: row.created_at,
        }
    }
}

/// An API-serving process and the version it serves the organization.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct InstanceView {
    pub instance_id: String,
    pub profile: String,
    pub region: Option<String>,
    pub build: String,
    pub applied_sequence: Option<i64>,
    /// The version it serves the organization at its applied position;
    /// `None` before its first load or while the organization has none.
    pub serving_version_id: Option<Uuid>,
    pub loaded_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    /// Unseen for five minutes.
    pub stale: bool,
}

#[derive(FromRow)]
struct InstanceRow {
    instance_id: String,
    profile: String,
    region: Option<String>,
    build: String,
    applied_sequence: Option<i64>,
    loaded_at: Option<DateTime<Utc>>,
    started_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    serving: Option<Uuid>,
    stale: Option<bool>,
}

impl From<InstanceRow> for InstanceView {
    fn from(row: InstanceRow) -> Self {
        Self {
            instance_id: row.instance_id,
            profile: row.profile,
            region: row.region,
            build: row.build,
            applied_sequence: row.applied_sequence,
            serving_version_id: row.serving,
            loaded_at: row.loaded_at,
            started_at: row.started_at,
            last_seen_at: row.last_seen_at,
            stale: row.stale.unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(overrides: Value) -> ConfigDocument {
        serde_json::from_value(json!({
            "schema": 1,
            "release": {"id": Uuid::nil(), "digest": "ab".repeat(32)},
            "routes": [{
                "route_id": Uuid::nil(), "alias": "chat", "operation": "chat",
                "deployment_id": Uuid::nil(), "deployment": "chat-a", "model": "m",
                "adapter_instance_key": "openai-main", "family": "openai",
                "price": {
                    "id": Uuid::nil(), "input_minor_per_token": 3, "output_minor_per_token": 11,
                    "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
                    "other_minor_per_token": 0, "currency": "USD", "source": "catalog",
                    "source_version": "v1", "confidence": "known"
                },
                "capability": {
                    "max_input_bytes": 4096, "max_output_tokens": 64, "max_tokens_per_byte": 1,
                    "tokenizer": "configured", "reservation_ttl_seconds": 60,
                    "chat_input_count_mode": "unsupported"
                }
            }],
            "overrides": overrides
        }))
        .unwrap()
    }

    /// The routing policy (D-023) marks the deployments it does not permit;
    /// `resolve` skips them. A document without one permits every route.
    #[test]
    fn the_routing_policy_marks_what_it_does_not_permit() {
        let mut doc = document(json!([]));
        let now = at("2026-11-14T00:00:00Z");
        assert!(doc.candidates("chat", "chat", now)[0].permitted);
        for (policy, location, permitted) in [
            (json!({"providers": ["openai"]}), None, true),
            (json!({"providers": ["bedrock"]}), None, false),
            (json!({"locations": ["us-east-1"]}), Some("us-east-1"), true),
            (
                json!({"locations": ["us-east-1"]}),
                Some("eu-west-1"),
                false,
            ),
            (json!({"locations": ["us-east-1"]}), None, false),
            (
                json!({"providers": ["openai"], "locations": ["us-east-1"]}),
                Some("us-east-1"),
                true,
            ),
        ] {
            doc.routing_policy = Some(serde_json::from_value(policy.clone()).unwrap());
            doc.routes[0].location = location.map(str::to_owned);
            let candidates = doc.candidates("chat", "chat", now);
            assert_eq!(candidates[0].permitted, permitted, "{policy} {location:?}");
            assert_eq!(
                doc.resolve("chat", "chat", now).is_some(),
                permitted,
                "{policy}"
            );
        }
    }

    fn price_override(id: u128, from: &str, until: Option<&str>, created: &str) -> Value {
        json!({
            "id": Uuid::from_u128(id), "deployment": "chat-a", "operation": "chat",
            "input_minor_per_token": id, "output_minor_per_token": 9,
            "cached_input_minor_per_token": 0, "reasoning_minor_per_token": 0,
            "other_minor_per_token": 0, "currency": "EUR", "confidence": "conservative",
            "version": format!("override:{}", Uuid::from_u128(id)),
            "effective_from": from, "effective_until": until, "created_at": created
        })
    }

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }

    use serde_json::{Value, json};

    #[test]
    fn without_an_override_the_catalog_price_applies() {
        let route = document(json!([]))
            .resolve("chat", "chat", at("2026-10-27T00:00:00Z"))
            .unwrap();
        assert_eq!(
            (
                route.input_minor_per_token,
                route.source.as_str(),
                route.price_override_id
            ),
            (3, "catalog", None)
        );
        assert_eq!(route.release_digest, vec![0xab; 32]);
        assert!(
            document(json!([]))
                .resolve("chat", "embeddings", at("2026-10-27T00:00:00Z"))
                .is_none()
        );
    }

    #[test]
    fn every_deployment_of_a_route_carries_the_routes_one_contract() {
        let mut doc = document(json!([]));
        let mut second = doc.routes[0].clone();
        second.route_id = Uuid::from_u128(2);
        second.deployment = "chat-b".into();
        second.weight = 3;
        second.priority = 2;
        second.capability.max_input_bytes = 1024;
        second.capability.max_output_tokens = 128;
        second.capability.reservation_ttl_seconds = 300;
        second.capability.reasoning_allowance_tokens = 512;
        second.price.confidence = "conservative".into();
        doc.routes.push(second);
        let candidates = doc.candidates("chat", "chat", at("2026-10-27T00:00:00Z"));
        assert_eq!(candidates.len(), 2);
        for candidate in &candidates {
            let row = &candidate.row;
            assert_eq!(
                (
                    row.max_input_bytes,
                    row.max_output_tokens,
                    row.reservation_ttl_seconds,
                    row.reasoning_allowance_tokens,
                    row.confidence.as_str()
                ),
                (
                    1024,
                    64,
                    300,
                    512,
                    if row.deployment_name == "chat-b" {
                        "conservative"
                    } else {
                        "known"
                    }
                ),
                "{}",
                row.deployment_name
            );
        }
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| (
                    candidate.row.deployment_name.as_str(),
                    candidate.weight,
                    candidate.priority
                ))
                .collect::<Vec<_>>(),
            vec![("chat-a", 1, 1), ("chat-b", 3, 2)]
        );
        // Tools resolve the preferred deployment.
        let preferred = doc
            .resolve("chat", "chat", at("2026-10-27T00:00:00Z"))
            .unwrap();
        assert_eq!(preferred.deployment_name, "chat-a");
        // A route of one deployment keeps its own limits.
        let single = document(json!([]))
            .resolve("chat", "chat", at("2026-10-27T00:00:00Z"))
            .unwrap();
        assert_eq!(
            (single.max_input_bytes, single.confidence.as_str()),
            (4096, "known")
        );
    }

    /// A route whose deployments make different tool call ids (D-032) is
    /// marked, so a request with tools is refused before it is reserved.
    #[test]
    fn a_route_mixing_geminis_tool_call_ids_with_others_is_marked() {
        let now = at("2026-11-18T00:00:00Z");
        let with = |family: &str, model: &str| {
            let mut doc = document(json!([]));
            doc.routes[0].capability.tools = true;
            let mut second = doc.routes[0].clone();
            second.route_id = Uuid::from_u128(2);
            second.deployment = "chat-b".into();
            second.family = family.into();
            second.model = model.into();
            doc.routes.push(second);
            doc.candidates("chat", "chat", now)
        };
        for (family, model, mixed) in [
            ("gemini", "gemini-2.5-pro", true),
            ("gemini", "endpoints/123", true),
            ("bedrock", "anthropic.claude-haiku-4-5-20251001-v1:0", false),
            ("anthropic", "claude-haiku-4-5", false),
            // Claude on Agent Platform makes Anthropic's ids.
            ("gemini", "anthropic/claude-haiku-4-5@20251001", false),
        ] {
            let candidates = with(family, model);
            assert_eq!(candidates.len(), 2);
            for candidate in &candidates {
                assert_eq!(candidate.row.mixed_tool_call_ids, mixed, "{family} {model}");
                assert!(candidate.row.tools);
            }
        }
        // Two endpoints, or two deployments of a family the gateway does
        // not know, may make different ids: mixed too.
        let both = |family: &str, first: &str, second: &str| {
            let mut doc = document(json!([]));
            doc.routes[0].family = family.into();
            doc.routes[0].model = first.into();
            let mut other = doc.routes[0].clone();
            other.route_id = Uuid::from_u128(2);
            other.deployment = "chat-b".into();
            other.model = second.into();
            doc.routes.push(other);
            doc.candidates("chat", "chat", now)
        };
        for candidate in both("gemini", "endpoints/1", "endpoints/2") {
            assert!(candidate.row.mixed_tool_call_ids);
        }
        for candidate in both("unknown-family", "a", "b") {
            assert!(candidate.row.mixed_tool_call_ids);
        }
        for candidate in both("gemini", "gemini-2.5-pro", "gemini-2.5-flash") {
            assert!(!candidate.row.mixed_tool_call_ids);
        }
        // One deployment is never mixed, even an endpoint.
        let mut alone = document(json!([]));
        alone.routes[0].family = "gemini".into();
        alone.routes[0].model = "endpoints/1".into();
        assert!(
            !alone.candidates("chat", "chat", now)[0]
                .row
                .mixed_tool_call_ids
        );
        assert!(
            !document(json!([])).candidates("chat", "chat", now)[0]
                .row
                .mixed_tool_call_ids
        );
    }

    /// A route refuses a tool option any deployment it may reach refuses
    /// (D-032): Claude first with Bedrock behind it refuses what Converse
    /// cannot do, on every candidate, the preferred one included.
    #[test]
    fn a_route_refuses_the_tool_options_any_deployment_refuses() {
        use crate::domain::inference::{ChatRequest, ToolChoice};
        let now = at("2026-11-18T00:00:00Z");
        let mut doc = document(json!([]));
        doc.routes[0].family = "anthropic".into();
        doc.routes[0].model = "claude-haiku-4-5".into();
        let alone = doc.candidates("chat", "chat", now);
        assert_eq!(alone[0].row.tool_option_refusals, Default::default());
        let mut failover = doc.routes[0].clone();
        failover.route_id = Uuid::from_u128(2);
        failover.deployment = "chat-b".into();
        failover.family = "bedrock".into();
        failover.model = "anthropic.claude-haiku-4-5-20251001-v1:0".into();
        failover.priority = 2;
        doc.routes.push(failover);
        let candidates = doc.candidates("chat", "chat", now);
        assert_eq!(candidates.len(), 2);
        for candidate in &candidates {
            let refusals = candidate.row.tool_option_refusals;
            let none = ChatRequest {
                tool_choice: Some(ToolChoice::None),
                ..Default::default()
            };
            let serial = ChatRequest {
                parallel_tool_calls: Some(false),
                ..Default::default()
            };
            assert!(
                refusals
                    .refusal(&none)
                    .unwrap()
                    .contains("tool_choice none")
            );
            assert!(
                refusals
                    .refusal(&serial)
                    .unwrap()
                    .contains("parallel_tool_calls false")
            );
            assert!(refusals.refusal(&ChatRequest::default()).is_none());
            assert!(!candidate.row.mixed_tool_call_ids);
        }
        let preferred = doc.resolve("chat", "chat", now).unwrap();
        assert_eq!(preferred.family, "anthropic");
        assert!(preferred.tool_option_refusals.serial_calls.is_some());
    }

    #[test]
    fn an_override_in_another_currency_takes_its_deployment_out_of_a_route() {
        // `chat-a`'s override is in EUR; the catalog prices both in USD.
        let mut doc = document(json!([price_override(
            1,
            "2026-10-01T00:00:00Z",
            Some("2026-10-20T00:00:00Z"),
            "2026-09-30T00:00:00Z"
        )]));
        // chat-a, the overridden one, has the tighter output cap.
        doc.routes[0].capability.max_output_tokens = 32;
        let mut second = doc.routes[0].clone();
        second.route_id = Uuid::from_u128(2);
        second.deployment = "chat-b".into();
        second.capability.max_output_tokens = 128;
        doc.routes.push(second);
        let names = |at_: &str| {
            doc.candidates("chat", "chat", at(at_))
                .into_iter()
                .map(|candidate| {
                    (
                        candidate.row.deployment_name,
                        candidate.row.currency,
                        candidate.row.max_output_tokens,
                    )
                })
                .collect::<Vec<_>>()
        };
        // chat-b serves alone, still within the whole route's limits.
        assert_eq!(
            names("2026-10-10T00:00:00Z"),
            vec![("chat-b".to_owned(), "USD".to_owned(), 32)]
        );
        // Once the override ends, both serve again.
        assert_eq!(names("2026-10-27T00:00:00Z").len(), 2);
    }

    #[test]
    fn a_streamed_request_takes_the_chat_route_and_its_override() {
        let doc = document(json!([price_override(
            1,
            "2026-10-01T00:00:00Z",
            None,
            "2026-09-30T00:00:00Z"
        )]));
        let chat = doc
            .resolve("chat", "chat", at("2026-10-27T00:00:00Z"))
            .unwrap();
        let streamed = doc
            .resolve("chat", "chat_stream", at("2026-10-27T00:00:00Z"))
            .unwrap();
        assert_eq!(streamed.route_id, chat.route_id);
        assert_eq!(streamed.operation, "chat");
        assert_eq!(
            (streamed.input_minor_per_token, streamed.price_override_id),
            (1, Some(Uuid::from_u128(1)))
        );
    }

    #[test]
    fn the_latest_override_in_effect_prices_the_route() {
        let doc = document(json!([
            price_override(1, "2026-10-01T00:00:00Z", None, "2026-09-30T00:00:00Z"),
            // Starts later, so it wins once in effect.
            price_override(
                2,
                "2026-10-10T00:00:00Z",
                Some("2026-10-20T00:00:00Z"),
                "2026-09-01T00:00:00Z"
            ),
            // Same start as 2 but created later: wins the tie.
            price_override(
                3,
                "2026-10-10T00:00:00Z",
                Some("2026-10-15T00:00:00Z"),
                "2026-09-02T00:00:00Z"
            ),
        ]));
        let priced = |now: &str| {
            let route = doc.resolve("chat", "chat", at(now)).unwrap();
            (route.input_minor_per_token, route.price_override_id)
        };
        assert_eq!(
            priced("2026-09-15T00:00:00Z"),
            (3, None),
            "none started yet"
        );
        assert_eq!(
            priced("2026-10-05T00:00:00Z"),
            (1, Some(Uuid::from_u128(1)))
        );
        assert_eq!(
            priced("2026-10-12T00:00:00Z"),
            (3, Some(Uuid::from_u128(3)))
        );
        assert_eq!(
            priced("2026-10-15T00:00:00Z"),
            (2, Some(Uuid::from_u128(2))),
            "3 ended"
        );
        assert_eq!(
            priced("2026-10-25T00:00:00Z"),
            (1, Some(Uuid::from_u128(1)))
        );
        let route = doc
            .resolve("chat", "chat", at("2026-10-05T00:00:00Z"))
            .unwrap();
        assert_eq!(
            (
                route.source.as_str(),
                route.currency.as_str(),
                route.confidence.as_str()
            ),
            ("override", "EUR", "conservative")
        );
        assert_eq!(
            route.source_version,
            format!("override:{}", Uuid::from_u128(1))
        );
    }

    #[test]
    fn a_document_must_match_its_digest_and_schema() {
        let text = r#"{"schema": 1, "release": null, "routes": [], "overrides": []}"#;
        let row = |document: &str, digest: Vec<u8>| VersionRow {
            organization_id: Uuid::nil(),
            version_id: Uuid::nil(),
            digest,
            document: document.to_owned(),
        };
        let digest = |text: &str| Sha256::digest(text.as_bytes()).to_vec();
        assert!(row(text, digest(text)).load().is_ok());
        assert!(row(text, digest("other")).load().is_err());
        let future = r#"{"schema": 2, "release": null, "routes": [], "overrides": []}"#;
        assert!(row(future, digest(future)).load().is_err());
        // No release: nothing routes.
        let loaded = row(text, digest(text)).load().unwrap();
        assert!(
            loaded
                .document
                .resolve("chat", "chat", Utc::now())
                .is_none()
        );
    }
}
