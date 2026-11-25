// Catalog sync admin application service. Model discovery: request a sync of
// one provider account, list and read sync jobs, list discovered models, and
// report drift between the active release and what its accounts list. Price
// sync: list price sources and the prices they publish, ask for a source to be
// fetched now, map a deployment to the published item that prices it, and
// report drift between the prices the active release bills and the prices
// sources publish.
//
// Reads need `catalog_read`; a sync or fetch request and mapping writes need
// `catalog_write`. Writes run through the admin mutation coordinator, so they
// are idempotent and audited in the same transaction as the change. Nothing
// here changes a catalog release or price.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditStatus, AuditTargetResource},
    credentials::validate_adapter_instance_key,
    discovery::{
        DeployableModelQuery, DeployableModelView, DiscoveredModelQuery, DiscoveredModelView,
        DiscoveryDrift, DiscoveryQueryError, SyncJobRequest, SyncJobView,
    },
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    pricing::{
        PriceDrift, PriceMappingError, PriceMappingInput, PriceMappingView, PriceObservationQuery,
        PriceObservationView, PriceQueryError, PriceSourceView, valid_name,
    },
    version::{IfMatch, ResourceVersion, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    catalog_sync::{CatalogSyncStore, DeployableKeyset, ModelKeyset},
    price_mappings::PriceMappingStore,
    price_sync::{FetchRequest, ObservationKeyset, PriceSyncStore},
};

pub const ROUTE_SYNC_JOBS: &str = "/api/admin/v1/organizations/{org}/catalog/sync-jobs";
pub const ROUTE_SYNC_JOB: &str = "/api/admin/v1/organizations/{org}/catalog/sync-jobs/{job_id}";
pub const ROUTE_DISCOVERED_MODELS: &str =
    "/api/admin/v1/organizations/{org}/catalog/discovered-models";
pub const ROUTE_DEPLOYABLE_MODELS: &str =
    "/api/admin/v1/organizations/{org}/catalog/deployable-models";
pub const ROUTE_DISCOVERY_DRIFT: &str = "/api/admin/v1/organizations/{org}/catalog/discovery-drift";
pub const ROUTE_PRICE_SOURCES: &str = "/api/admin/v1/organizations/{org}/catalog/price-sources";
pub const ROUTE_PRICE_SOURCE_FETCH: &str =
    "/api/admin/v1/organizations/{org}/catalog/price-sources/{source}/fetch";
pub const ROUTE_PRICE_DRIFT: &str = "/api/admin/v1/organizations/{org}/catalog/price-drift";
pub const ROUTE_PRICE_OBSERVATIONS: &str =
    "/api/admin/v1/organizations/{org}/catalog/price-observations";
pub const ROUTE_PRICE_MAPPINGS: &str = "/api/admin/v1/organizations/{org}/catalog/price-mappings";
pub const ROUTE_PRICE_MAPPING: &str =
    "/api/admin/v1/organizations/{org}/catalog/price-mappings/{deployment}/{source}";

const JOB_COLLECTION: &str = "catalog_sync_jobs";
const MODEL_COLLECTION: &str = "catalog_discovered_models";
const ORDER_CREATED_DESC: &str = "created_at_desc,id_desc";
const ORDER_MODEL: &str = "adapter_instance_key,provider_family,model";
const DEPLOYABLE_COLLECTION: &str = "catalog_deployable_models";
const ORDER_DEPLOYABLE: &str = "adapter_instance_key,provider_family,format,model,version";
const OBSERVATION_COLLECTION: &str = "catalog_price_observations";
const ORDER_OBSERVATION: &str = "source,ordinal";
const MAPPING_COLLECTION: &str = "catalog_price_mappings";
const ORDER_MAPPING: &str = "deployment,source";

#[derive(Debug)]
pub enum CatalogSyncError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Invalid(DiscoveryQueryError),
    InvalidPriceQuery(PriceQueryError),
    InvalidMapping(PriceMappingError),
    /// The active release declares no provider account with the key.
    UnknownAccount,
    /// No price source with the id is configured.
    UnknownPriceSource,
    /// Neither the active release nor an account's automatic offer has a
    /// deployment with the name in the price source's provider family.
    UnknownDeployment,
    VersionConflict(VersionConflict),
    /// The page token's source has published new prices since it was
    /// issued.
    StalePage,
    /// The account already has a queued or running job.
    InProgress,
    /// The price source was fetched too recently; retry after this many
    /// seconds.
    FetchTooSoon(u64),
    Pagination(PaginationError),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for CatalogSyncError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

/// A mapping as written, and whether the write created it.
#[derive(Clone, Debug)]
pub struct PriceMappingWrite {
    pub mapping: PriceMappingView,
    pub created: bool,
}

#[derive(Clone)]
pub struct CatalogSyncService {
    admin: AdminStore,
    store: CatalogSyncStore,
    prices: PriceSyncStore,
    mappings: PriceMappingStore,
    codec: Option<SignedCursorCodec>,
}

/// Every price source; there are at most a few per provider family.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PriceSourceList {
    pub items: Vec<PriceSourceView>,
}

impl CatalogSyncService {
    pub fn new(
        admin: AdminStore,
        store: CatalogSyncStore,
        prices: PriceSyncStore,
        codec: Option<SignedCursorCodec>,
    ) -> Self {
        Self {
            admin,
            store,
            mappings: prices.mappings(),
            prices,
            codec,
        }
    }

    /// Queues a sync of one provider account of the active release. The
    /// worker runs it on its next tick.
    pub async fn request(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        input: SyncJobRequest,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<SyncJobView>, CatalogSyncError> {
        // A malformed key cannot name an account of any release.
        input
            .validate()
            .map_err(|_| CatalogSyncError::UnknownAccount)?;
        let target_id = deterministic_target_id(&[
            "catalog_sync_job.create",
            &organization_id.0.to_string(),
            &actor.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_SYNC_JOBS.into(),
            method: "POST".into(),
            operation: operation("catalog_sync_job.create"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::CatalogSyncJob,
            audit_action: audit_action("catalog_sync_job.requested"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "adapter_instance_key": input.adapter_instance_key }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(CatalogSyncError::NotFound)?;
                let item = serde_json::from_value(snapshot).map_err(|_| {
                    CatalogSyncError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                let family = CatalogSyncStore::active_account_family(
                    tx,
                    organization_id,
                    &input.adapter_instance_key,
                )
                .await?
                .ok_or(CatalogSyncError::UnknownAccount)?;
                let job = CatalogSyncStore::insert_manual(
                    tx,
                    organization_id,
                    target_id,
                    &input.adapter_instance_key,
                    &family,
                    actor,
                )
                .await?
                .ok_or(CatalogSyncError::InProgress)?;
                let next = mutation.next_version().get();
                let snapshot = serde_json::to_value(&job).map_err(|_| {
                    CatalogSyncError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Pending),
                            version: Some(next),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: job,
                    replayed: false,
                })
            }
        }
    }

    /// The organization's sync jobs, newest first.
    pub async fn list_jobs(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: Option<String>,
        page: &PageRequest,
    ) -> Result<Page<SyncJobView>, CatalogSyncError> {
        if let Some(key) = adapter_instance_key.as_deref() {
            validate_adapter_instance_key(key)
                .map_err(|_| CatalogSyncError::Invalid(DiscoveryQueryError::AdapterInstanceKey))?;
        }
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        // JSON keeps a key containing separators from matching another filter.
        let filters = serde_json::json!([adapter_instance_key]).to_string();
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        JOB_COLLECTION,
                        &filters,
                        ORDER_CREATED_DESC,
                    )
                    .map_err(CatalogSyncError::Pagination)?;
                // Only a validly signed token gets here, so a bad keyset is
                // a server-side inconsistency.
                Some(decode_job_keyset(&claims.last_keyset).ok_or_else(|| {
                    CatalogSyncError::Store("sync job page keyset is invalid".into())
                })?)
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list(
                organization_id,
                adapter_instance_key.as_deref(),
                page.page_size,
                after,
            )
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode_token(
                codec,
                CursorClaims::new(
                    actor,
                    scope,
                    JOB_COLLECTION,
                    filters,
                    ORDER_CREATED_DESC,
                    encode_job_keyset(last.created_at, last.id),
                    None,
                ),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn get_job(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        job_id: Uuid,
    ) -> Result<SyncJobView, CatalogSyncError> {
        self.authorize_read(actor, organization_id).await?;
        self.store
            .get(organization_id, job_id)
            .await?
            .ok_or(CatalogSyncError::NotFound)
    }

    /// Models the organization's provider accounts have listed, by account
    /// key, family and model.
    pub async fn list_models(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: DiscoveredModelQuery,
        page: &PageRequest,
    ) -> Result<Page<DiscoveredModelView>, CatalogSyncError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        let filters = query.canonical();
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        MODEL_COLLECTION,
                        &filters,
                        ORDER_MODEL,
                    )
                    .map_err(CatalogSyncError::Pagination)?;
                Some(
                    serde_json::from_str::<ModelKeyset>(&claims.last_keyset).map_err(|_| {
                        CatalogSyncError::Store("discovered model page keyset is invalid".into())
                    })?,
                )
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list_models(organization_id, &query, page.page_size, after.as_ref())
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let keyset = serde_json::json!([
                    last.adapter_instance_key,
                    last.provider_family,
                    last.model
                ])
                .to_string();
                Some(encode_token(
                    codec,
                    CursorClaims::new(
                        actor,
                        scope,
                        MODEL_COLLECTION,
                        filters,
                        ORDER_MODEL,
                        keyset,
                        None,
                    ),
                )?)
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Models the organization's provider accounts may deploy (D-018), by
    /// account key, family, format, model and version.
    pub async fn list_deployable(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: DeployableModelQuery,
        page: &PageRequest,
    ) -> Result<Page<DeployableModelView>, CatalogSyncError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        let filters = query.canonical();
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        DEPLOYABLE_COLLECTION,
                        &filters,
                        ORDER_DEPLOYABLE,
                    )
                    .map_err(CatalogSyncError::Pagination)?;
                Some(
                    serde_json::from_str::<DeployableKeyset>(&claims.last_keyset).map_err(
                        |_| {
                            CatalogSyncError::Store(
                                "deployable model page keyset is invalid".into(),
                            )
                        },
                    )?,
                )
            }
            None => None,
        };
        let (items, has_more) = self
            .store
            .list_deployable(organization_id, &query, page.page_size, after.as_ref())
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => {
                let keyset = serde_json::json!([
                    last.adapter_instance_key,
                    last.provider_family,
                    last.format,
                    last.model,
                    last.version
                ])
                .to_string();
                Some(encode_token(
                    codec,
                    CursorClaims::new(
                        actor,
                        scope,
                        DEPLOYABLE_COLLECTION,
                        filters,
                        ORDER_DEPLOYABLE,
                        keyset,
                        None,
                    ),
                )?)
            }
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Drift for every provider account of the active release.
    pub async fn drift(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<DiscoveryDrift, CatalogSyncError> {
        self.authorize_read(actor, organization_id).await?;
        self.store
            .drift(organization_id)
            .await?
            .ok_or(CatalogSyncError::NotFound)
    }

    /// The gateway's price sources and their current snapshots. Sources are
    /// gateway-wide, not per organization.
    pub async fn price_sources(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<PriceSourceList, CatalogSyncError> {
        self.authorize_read(actor, organization_id).await?;
        Ok(PriceSourceList {
            items: self.prices.sources().await?,
        })
    }

    /// Makes a price source due now, so the worker fetches it within a
    /// minute instead of at its next daily fetch. Sources are gateway-wide;
    /// a source fetched in the last few minutes is not fetched again.
    pub async fn request_price_fetch(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        source: String,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PriceSourceView>, CatalogSyncError> {
        if !valid_name(&source) {
            return Err(CatalogSyncError::NotFound);
        }
        let target_id = deterministic_target_id(&[
            "price_source_fetch.create",
            &organization_id.0.to_string(),
            &actor.0.to_string(),
            idempotency_key.as_str(),
        ]);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_SOURCE_FETCH.into(),
            method: "POST".into(),
            operation: operation("price_source_fetch.create"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::PriceSourceFetch,
            audit_action: audit_action("price_source.fetch_requested"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({ "source": source }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(CatalogSyncError::NotFound)?;
                let item = serde_json::from_value(snapshot).map_err(|_| {
                    CatalogSyncError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item,
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                let item = match PriceSyncStore::request_fetch(tx, &source, Utc::now()).await? {
                    None => return Err(CatalogSyncError::NotFound),
                    Some(FetchRequest::TooSoon(seconds)) => {
                        return Err(CatalogSyncError::FetchTooSoon(seconds));
                    }
                    Some(FetchRequest::Due(item)) => item,
                };
                let next = mutation.next_version().get();
                let snapshot = serde_json::to_value(&item).map_err(|_| {
                    CatalogSyncError::Store("response snapshot is not serializable".into())
                })?;
                mutation
                    .commit(
                        None,
                        Some(AuditSnapshot {
                            status: Some(AuditStatus::Pending),
                            version: Some(next),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item,
                    replayed: false,
                })
            }
        }
    }

    /// Price drift for every price of the active release.
    pub async fn price_drift(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<PriceDrift, CatalogSyncError> {
        self.authorize_read(actor, organization_id).await?;
        self.prices
            .drift(organization_id)
            .await?
            .ok_or(CatalogSyncError::NotFound)
    }

    /// Prices the gateway's sources currently publish, by source and
    /// position, for choosing the item a deployment maps to.
    pub async fn price_observations(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: PriceObservationQuery,
        page: &PageRequest,
    ) -> Result<Page<PriceObservationView>, CatalogSyncError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        let filters = query.canonical();
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(
                        token,
                        actor,
                        scope,
                        OBSERVATION_COLLECTION,
                        &filters,
                        ORDER_OBSERVATION,
                    )
                    .map_err(CatalogSyncError::Pagination)?;
                Some(
                    serde_json::from_str::<ObservationKeyset>(&claims.last_keyset).map_err(
                        |_| CatalogSyncError::Store("observation page keyset is invalid".into()),
                    )?,
                )
            }
            None => None,
        };
        let (items, next) = self
            .prices
            .observations(&query, page.page_size, after.as_ref())
            .await?
            .ok_or(CatalogSyncError::StalePage)?;
        let next_page_token = match next {
            Some(keyset) => Some(encode_token(
                codec,
                CursorClaims::new(
                    actor,
                    scope,
                    OBSERVATION_COLLECTION,
                    filters,
                    ORDER_OBSERVATION,
                    serde_json::json!(keyset).to_string(),
                    None,
                ),
            )?),
            None => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// The organization's price mappings, by deployment and source.
    pub async fn list_mappings(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<PriceMappingView>, CatalogSyncError> {
        let scope = self.authorize_read(actor, organization_id).await?;
        let codec = self.codec()?;
        let after = match page.page_token.as_deref() {
            Some(token) => {
                let claims = codec
                    .decode_and_validate(token, actor, scope, MAPPING_COLLECTION, "", ORDER_MAPPING)
                    .map_err(CatalogSyncError::Pagination)?;
                Some(
                    serde_json::from_str::<(String, String)>(&claims.last_keyset).map_err(
                        |_| CatalogSyncError::Store("price mapping page keyset is invalid".into()),
                    )?,
                )
            }
            None => None,
        };
        let (items, has_more) = self
            .mappings
            .list(organization_id, page.page_size, after.as_ref())
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode_token(
                codec,
                CursorClaims::new(
                    actor,
                    scope,
                    MAPPING_COLLECTION,
                    "",
                    ORDER_MAPPING,
                    serde_json::json!([last.deployment, last.source]).to_string(),
                    None,
                ),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Maps a deployment of the active release to the item that prices it
    /// at `source`, replacing any earlier mapping there. The item need not be
    /// published yet; drift reports no observation until it is.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_mapping(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        deployment: String,
        source: String,
        input: PriceMappingInput,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<PriceMappingWrite>, CatalogSyncError> {
        let input = input.validate().map_err(CatalogSyncError::InvalidMapping)?;
        if !valid_name(&deployment) {
            return Err(CatalogSyncError::UnknownDeployment);
        }
        if !valid_name(&source) {
            return Err(CatalogSyncError::UnknownPriceSource);
        }
        let target_id = mapping_id(organization_id, &deployment, &source);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_MAPPING.into(),
            method: "PUT".into(),
            operation: operation("price_mapping.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::PriceMapping,
            audit_action: audit_action("price_mapping.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "deployment": deployment,
                "source": source,
                "item": input.item,
                "region": input.region,
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            // The recorded response, even if the mapping has since changed.
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(CatalogSyncError::NotFound)?;
                let mapping = serde_json::from_value(snapshot).map_err(|_| {
                    CatalogSyncError::Store("recorded response snapshot is invalid".into())
                })?;
                Ok(MutationOutcome {
                    item: PriceMappingWrite {
                        mapping,
                        created: false,
                    },
                    replayed: true,
                })
            }
            MutationStart::New(mut mutation) => {
                // Errors from here on drop the mutation, which releases the
                // idempotency claim.
                let tx = mutation.transaction();
                let family = PriceMappingStore::source_family(tx, &source)
                    .await?
                    .ok_or(CatalogSyncError::UnknownPriceSource)?;
                let deployed =
                    PriceMappingStore::deployment_family(tx, organization_id, &deployment).await?;
                let offered = match deployed {
                    Some(_) => None,
                    None => {
                        PriceMappingStore::offered_deployment_family(
                            tx,
                            organization_id,
                            &deployment,
                        )
                        .await?
                    }
                };
                if deployed.or(offered) != Some(family) {
                    return Err(CatalogSyncError::UnknownDeployment);
                }
                let before = PriceMappingStore::view(tx, organization_id, target_id).await?;
                let created = PriceMappingStore::upsert(
                    tx,
                    organization_id,
                    target_id,
                    &deployment,
                    &source,
                    &input,
                    actor,
                )
                .await?;
                let stored = PriceMappingStore::view(tx, organization_id, target_id)
                    .await?
                    .ok_or(CatalogSyncError::NotFound)?;
                let next = mutation.next_version();
                // The version this commit writes, which is also the ETag.
                let mapping = PriceMappingView {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&mapping).map_err(|_| {
                    CatalogSyncError::Store("response snapshot is not serializable".into())
                })?;
                let before = before.map(|_| AuditSnapshot {
                    enabled: Some(true),
                    version: previous(next),
                    ..Default::default()
                });
                mutation
                    .commit(
                        before,
                        Some(AuditSnapshot {
                            enabled: Some(true),
                            version: Some(next.get()),
                            ..Default::default()
                        }),
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: Some(snapshot),
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: PriceMappingWrite { mapping, created },
                    replayed: false,
                })
            }
        }
    }

    pub async fn delete_mapping(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        deployment: String,
        source: String,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<()>, CatalogSyncError> {
        let target_id = mapping_id(organization_id, &deployment, &source);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_PRICE_MAPPING.into(),
            method: "DELETE".into(),
            operation: operation("price_mapping.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::PriceMapping,
            audit_action: audit_action("price_mapping.deleted"),
            if_match,
            request_id,
            body: serde_json::json!({ "deployment": deployment, "source": source }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(_) => Ok(MutationOutcome {
                item: (),
                replayed: true,
            }),
            MutationStart::New(mut mutation) => {
                if !PriceMappingStore::delete(mutation.transaction(), organization_id, target_id)
                    .await?
                {
                    return Err(CatalogSyncError::NotFound);
                }
                let next = mutation.next_version();
                mutation
                    .commit(
                        previous(next).map(|version| AuditSnapshot {
                            enabled: Some(true),
                            version: Some(version),
                            ..Default::default()
                        }),
                        None,
                        Some(IdempotencyResultMetadata {
                            resource_id: target_id,
                            version: Some(next.get()),
                            secret_unavailable_on_replay: false,
                            snapshot: None,
                        }),
                    )
                    .await
                    .map_err(map_admin_error)?;
                Ok(MutationOutcome {
                    item: (),
                    replayed: false,
                })
            }
        }
    }

    async fn authorize_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<AdminScope, CatalogSyncError> {
        let scope = AdminScope::Organization(organization_id);
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(AdminPermission::CatalogRead, scope) {
            return Err(CatalogSyncError::Forbidden);
        }
        Ok(scope)
    }

    fn codec(&self) -> Result<&SignedCursorCodec, CatalogSyncError> {
        self.codec
            .as_ref()
            .ok_or(CatalogSyncError::AuthenticationUnavailable)
    }
}

fn encode_token(
    codec: &SignedCursorCodec,
    claims: Result<CursorClaims, PaginationError>,
) -> Result<String, CatalogSyncError> {
    claims
        .and_then(|claims| codec.encode(&claims))
        .map_err(CatalogSyncError::Pagination)
}

fn encode_job_keyset(created_at: DateTime<Utc>, id: Uuid) -> String {
    format!("{}|{id}", created_at.to_rfc3339())
}

fn decode_job_keyset(value: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let (created_at, id) = value.split_once('|')?;
    Some((
        DateTime::parse_from_rfc3339(created_at)
            .ok()?
            .with_timezone(&Utc),
        Uuid::parse_str(id).ok()?,
    ))
}

/// Stable per (organization, deployment, source), so set and delete address
/// the same versioned resource across re-creation.
fn mapping_id(organization_id: OrganizationId, deployment: &str, source: &str) -> Uuid {
    deterministic_target_id(&[
        "price_mapping",
        &organization_id.0.to_string(),
        deployment,
        source,
    ])
}

fn previous(next: ResourceVersion) -> Option<u64> {
    next.get().checked_sub(1).filter(|version| *version > 0)
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> CatalogSyncError {
    match error {
        AdminStoreError::Authorization(_) => CatalogSyncError::Forbidden,
        AdminStoreError::ResourceNotFound => CatalogSyncError::NotFound,
        AdminStoreError::VersionConflict(conflict) => CatalogSyncError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => CatalogSyncError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => CatalogSyncError::IdempotencyInProgress,
        other => CatalogSyncError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_keyset_round_trips() {
        let created_at = Utc::now();
        let id = Uuid::new_v4();
        assert_eq!(
            decode_job_keyset(&encode_job_keyset(created_at, id)),
            Some((created_at, id))
        );
        assert_eq!(decode_job_keyset("not-a-keyset"), None);
    }
}
