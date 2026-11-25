// HTTP transport for the catalog admin lane under
// `/api/admin/v1/organizations/{org}/catalog`: reads of releases, provider
// accounts, deployments, routes, credential metadata, and price overrides;
// a release's manifest, the base for a release that changes it;
// writes that create and activate releases, set or remove provider
// credentials, and create or revoke price overrides.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Release-scoped collections accept `release_id`;
// without it they read the active release. Writes require `Idempotency-Key`;
// activation, credential writes, and override revocation also require
// `If-Match`.
// Problems share the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{BytesRejection, JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    application::{
        catalog_admin::{
            CatalogAdminError, CatalogAdminService, ROUTE_CLOUD_TRUST, ROUTE_DEPLOYMENTS,
            ROUTE_PRICE_OVERRIDES, ROUTE_PROVIDER_ACCOUNTS, ROUTE_RELEASE_MANIFEST, ROUTE_RELEASES,
            ROUTE_ROUTES,
        },
        catalog_write::{
            CatalogWriteError, CatalogWriteService, ROUTE_CREDENTIAL, ROUTE_CREDENTIALS,
            ROUTE_PRICE_OVERRIDE_REVOKE, ROUTE_RELEASE_ACTIVATE,
        },
    },
    domain::{
        auth::AuthCrypto,
        catalog::MAX_MANIFEST_BYTES,
        catalog_admin::{CatalogQueryError, CatalogReleaseSummary, DeploymentQuery, RouteQuery},
        credentials::{CredentialCipher, ProviderSecretInput},
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        price_override::{PriceOverrideInput, PriceOverrideQuery, PriceOverrideRevocation},
    },
    persistence::{
        admin::AdminStore, auth::AuthStore, catalog_admin::CatalogAdminStore,
        price_overrides::PriceOverrideStore, provider_credentials::ProviderCredentialStore,
    },
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseParams {
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderAccountParams {
    release_id: Option<Uuid>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentParams {
    release_id: Option<Uuid>,
    provider_account_id: Option<Uuid>,
    model: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteParams {
    release_id: Option<Uuid>,
    alias: Option<String>,
    operation: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceOverrideParams {
    deployment: Option<String>,
    operation: Option<String>,
    #[serde(rename = "page_size")]
    _page_size: Option<String>,
    #[serde(rename = "page_token")]
    _page_token: Option<String>,
}

#[derive(Serialize)]
struct ActivationResponse {
    release: CatalogReleaseSummary,
    /// Version of the organization's activation pointer; also the `ETag`.
    activation_version: u64,
}

#[derive(Clone)]
pub struct CatalogAdminState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<CatalogAdminService>,
    writes: Arc<CatalogWriteService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: CatalogAdminStore,
    credentials: ProviderCredentialStore,
    cipher: Option<CredentialCipher>,
) -> Router {
    let writes = Arc::new(CatalogWriteService::new(
        admin.clone(),
        store.clone(),
        cipher,
    ));
    let overrides = PriceOverrideStore::new(store.pool().clone());
    let service = Arc::new(CatalogAdminService::new(
        admin,
        store,
        credentials,
        overrides,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(
            ROUTE_RELEASES,
            // A manifest may exceed the lane-wide body limit.
            get(list_releases)
                .post(create_release)
                .layer(DefaultBodyLimit::max(MAX_MANIFEST_BYTES)),
        )
        .route(ROUTE_RELEASE_ACTIVATE, post(activate_release))
        .route(ROUTE_RELEASE_MANIFEST, get(release_manifest))
        .route(ROUTE_PROVIDER_ACCOUNTS, get(list_provider_accounts))
        .route(ROUTE_DEPLOYMENTS, get(list_deployments))
        .route(ROUTE_ROUTES, get(list_routes))
        .route(ROUTE_CREDENTIALS, get(list_credentials))
        .route(ROUTE_CLOUD_TRUST, get(cloud_trust))
        .route(
            ROUTE_CREDENTIAL,
            put(set_credential).delete(delete_credential),
        )
        .route(
            ROUTE_PRICE_OVERRIDES,
            get(list_price_overrides).post(create_price_override),
        )
        .route(ROUTE_PRICE_OVERRIDE_REVOKE, post(revoke_price_override))
        .with_state(CatalogAdminState {
            auth,
            crypto,
            service,
            writes,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn list_releases(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ReleaseParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_RELEASES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    if params.is_err() {
        return invalid_query(path, &id);
    }
    match state
        .service
        .list_releases(actor, organization_id, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn list_provider_accounts(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ProviderAccountParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PROVIDER_ACCOUNTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    match state
        .service
        .list_provider_accounts(actor, organization_id, params.release_id, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn list_deployments(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<DeploymentParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DEPLOYMENTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    let query = match DeploymentQuery::new(params.provider_account_id, params.model) {
        Ok(query) => query,
        Err(error) => return query_problem(error, path, &id),
    };
    match state
        .service
        .list_deployments(actor, organization_id, params.release_id, &query, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn list_routes(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<RouteParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ROUTES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    let query = match RouteQuery::new(params.alias, params.operation) {
        Ok(query) => query,
        Err(error) => return query_problem(error, path, &id),
    };
    match state
        .service
        .list_routes(actor, organization_id, params.release_id, &query, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn create_release(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_RELEASES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    if !is_json(&headers) {
        return problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "request_body_invalid",
            "unsupported media type",
            path,
            &id,
        );
    }
    let body = match body {
        Ok(body) => body,
        Err(rejection) => {
            return problem(
                rejection.status(),
                "request_body_invalid",
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    "request body too large"
                } else {
                    "request body is invalid"
                },
                path,
                &id,
            );
        }
    };
    match state
        .writes
        .create_release(actor, organization_id, &body, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, Json(outcome.item)).into_response()
        }
        Err(error) => write_problem(error, path, &id),
    }
}

async fn activate_release(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path((organization_id, release_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_RELEASE_ACTIVATE;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .writes
        .activate_release(
            actor,
            organization_id,
            release_id,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = ok(ActivationResponse {
                release: outcome.item.release,
                activation_version: version,
            });
            set_etag(&mut response, version);
            response
        }
        Err(error) => write_problem(error, path, &id),
    }
}

async fn release_manifest(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path((organization_id, release_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_RELEASE_MANIFEST;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .release_manifest(actor, organization_id, release_id)
        .await
    {
        Ok(manifest) => ok(manifest),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn cloud_trust(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_CLOUD_TRUST;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.cloud_trust(actor, organization_id).await {
        Ok(trust) => ok(trust),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn list_credentials(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<ReleaseParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_CREDENTIALS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    // Credentials take only paging parameters, like the release list.
    if params.is_err() {
        return invalid_query(path, &id);
    }
    match state
        .service
        .list_credentials(actor, organization_id, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn set_credential(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
    body: Result<Json<ProviderSecretInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_CREDENTIAL;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .writes
        .set_credential(
            actor,
            organization_id,
            &adapter_instance_key,
            input,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let status = if outcome.item.created && !outcome.replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = outcome.item.metadata.version;
            let mut response = (status, Json(outcome.item.metadata)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => write_problem(error, path, &id),
    }
}

async fn delete_credential(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path((organization_id, adapter_instance_key)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_CREDENTIAL;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    match state
        .writes
        .delete_credential(
            actor,
            organization_id,
            &adapter_instance_key,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) if outcome.replayed => ok(serde_json::json!({ "deleted": true })),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => write_problem(error, path, &id),
    }
}

async fn list_price_overrides(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    params: Result<Query<PriceOverrideParams>, QueryRejection>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PRICE_OVERRIDES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let page = match page_request(page, path, &id) {
        Ok(page) => page,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return invalid_query(path, &id);
    };
    let query = match PriceOverrideQuery::new(params.deployment, params.operation) {
        Ok(query) => query,
        Err(error) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                error.to_string(),
                path,
                &id,
            );
        }
    };
    match state
        .service
        .list_price_overrides(actor, organization_id, &query, &page)
        .await
    {
        Ok(page) => ok(page),
        Err(error) => catalog_problem(error, path, &id),
    }
}

async fn create_price_override(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<PriceOverrideInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_OVERRIDES;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .writes
        .create_price_override(actor, organization_id, input, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => {
            let status = if outcome.replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            let version = outcome.item.version;
            let mut response = (status, Json(outcome.item)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => write_problem(error, path, &id),
    }
}

async fn revoke_price_override(
    State(state): State<CatalogAdminState>,
    headers: HeaderMap,
    Path((organization_id, override_id)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<PriceOverrideRevocation>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_PRICE_OVERRIDE_REVOKE;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let if_match = match require_if_match(&headers, path, &id) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Json(revocation) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .writes
        .revoke_price_override(
            actor,
            organization_id,
            override_id,
            revocation,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => {
            let version = outcome.item.version;
            let mut response = ok(outcome.item);
            set_etag(&mut response, version);
            response
        }
        Err(error) => write_problem(error, path, &id),
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

async fn human_actor(
    state: &CatalogAdminState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    let session =
        crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
            .await;
    match session {
        Ok(Some(actor)) => Ok(actor),
        Ok(None) => Err(problem(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "a session or an agent token is required",
            path,
            id,
        )),
        Err(_) => Err(problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        )),
    }
}

fn page_request(
    page: Result<Query<PageRequest>, QueryRejection>,
    path: &str,
    id: &str,
) -> Result<PageRequest, Response> {
    let Ok(Query(page)) = page else {
        return Err(pagination_problem(
            PaginationError::LimitOutOfBounds,
            path,
            id,
        ));
    };
    page.validate()
        .map_err(|error| pagination_problem(error, path, id))?;
    Ok(page)
}

fn ok(body: impl Serialize) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

fn invalid_query(path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "query parameters are missing, unknown, or malformed",
        path,
        id,
    )
}

fn query_problem(error: CatalogQueryError, path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        error.to_string(),
        path,
        id,
    )
}

fn catalog_problem(error: CatalogAdminError, path: &str, id: &str) -> Response {
    match error {
        CatalogAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        CatalogAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        CatalogAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        CatalogAdminError::Pagination(error) => pagination_problem(error, path, id),
        // Only a keyset from a validly signed token reaches the store, so a
        // parse failure there is a server-side inconsistency, not client input.
        CatalogAdminError::Store(_) | CatalogAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "catalog admin lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "catalog store unavailable",
                path,
                id,
            )
        }
    }
}

fn write_problem(error: CatalogWriteError, path: &str, id: &str) -> Response {
    match error {
        CatalogWriteError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        CatalogWriteError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        CatalogWriteError::ProviderAccountNotFound => problem(
            StatusCode::NOT_FOUND,
            "provider_account_not_found",
            "no catalog release of this organization declares the adapter instance key",
            path,
            id,
        ),
        CatalogWriteError::InvalidManifest(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_manifest",
            detail,
            path,
            id,
        ),
        CatalogWriteError::ReleaseExists(release_id) => problem(
            StatusCode::CONFLICT,
            "release_exists",
            format!("a release with this manifest digest already exists: {release_id}"),
            path,
            id,
        ),
        CatalogWriteError::InvalidCredential(detail) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_credential",
            detail,
            path,
            id,
        ),
        CatalogWriteError::InvalidPriceOverride(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_price_override",
            error.to_string(),
            path,
            id,
        ),
        CatalogWriteError::DeploymentNotDeclared => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_deployment",
            "no catalog release of this organization prices the operation on a deployment of this name",
            path,
            id,
        ),
        CatalogWriteError::UnreportedUsagePrice(family) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_price_override",
            format!(
                "the {family} provider family does not report usage for a priced dimension; that price must be zero"
            ),
            path,
            id,
        ),
        CatalogWriteError::AlreadyRevoked => problem(
            StatusCode::CONFLICT,
            "already_revoked",
            "the price override is already revoked",
            path,
            id,
        ),
        CatalogWriteError::IncompatiblePriceOverride(override_id) => problem(
            StatusCode::CONFLICT,
            "price_override_incompatible",
            format!(
                "price override {override_id} sets a price the release's provider family does not report usage for; revoke it before activating"
            ),
            path,
            id,
        ),
        CatalogWriteError::CredentialEncryptionUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_encryption_unavailable",
            "credential storage is not configured on this gateway",
            path,
            id,
        ),
        CatalogWriteError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        CatalogWriteError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        CatalogWriteError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        CatalogWriteError::Store(_) | CatalogWriteError::Admin(_) => {
            tracing::warn!(error = ?error, "catalog write lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "catalog store unavailable",
                path,
                id,
            )
        }
    }
}
