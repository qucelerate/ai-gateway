// HTTP transport for batch administration (D-021, Phase 7 milestone 6):
// the organization's batch settings and connection staging, and its
// batches as Usage -> Batches shows them, with cancel and results
// downloads.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Writes require `Idempotency-Key`; setting the
// settings or a staging, and deleting a staging, also `If-Match` (`*` for
// any state). Problems share the admin lanes' `application/problem+json`
// shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    application::{
        batch_admin::{
            BatchAdminError, BatchAdminService, BatchDetail, MAX_PAGE, ROUTE_ADMIN_BATCH,
            ROUTE_ADMIN_BATCH_CANCEL, ROUTE_ADMIN_BATCH_ERRORS, ROUTE_ADMIN_BATCH_OUTPUT,
            ROUTE_ADMIN_BATCHES, ROUTE_BATCH_SETTINGS, ROUTE_BATCH_STAGING, ROUTE_BATCH_STAGINGS,
            ResultFile,
        },
        trace_objects::TraceObjects,
    },
    domain::{
        auth::AuthCrypto,
        batch::{BatchSettingsInput, BatchStagingInput},
        identity::{OrganizationId, UserAccountId},
    },
    persistence::{
        admin::AdminStore, auth::AuthStore, batch_settings::BatchSettingsStore, batches::BatchStore,
    },
    transport::{
        admin_identities::{
            body_rejection, no_store, problem, request_context, require_idempotency_key,
            require_if_match, set_etag,
        },
        batches::{BATCH_PREFIX, batch_id, batch_object, gateway_extension, parse_id},
    },
};

#[derive(Clone)]
pub struct BatchAdminState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<BatchAdminService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    settings: BatchSettingsStore,
    batches: BatchStore,
    objects: Option<Arc<dyn TraceObjects>>,
) -> Router {
    Router::new()
        .route(ROUTE_BATCH_SETTINGS, get(get_settings).put(set_settings))
        .route(ROUTE_BATCH_STAGINGS, get(list_stagings))
        .route(ROUTE_BATCH_STAGING, put(set_staging).delete(delete_staging))
        .route(ROUTE_ADMIN_BATCHES, get(list_batches))
        .route(ROUTE_ADMIN_BATCH, get(get_batch))
        .route(ROUTE_ADMIN_BATCH_CANCEL, post(cancel_batch))
        .route(ROUTE_ADMIN_BATCH_OUTPUT, get(output))
        .route(ROUTE_ADMIN_BATCH_ERRORS, get(errors))
        .with_state(BatchAdminState {
            service: Arc::new(BatchAdminService::new(admin, settings, batches, objects)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn get_settings(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_BATCH_SETTINGS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.settings(actor, organization_id).await {
        Ok(settings) => versioned(StatusCode::OK, settings.version, &settings),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn set_settings(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<BatchSettingsInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_BATCH_SETTINGS;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .service
        .set_settings(
            actor,
            organization_id,
            input,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => versioned(
            created(outcome.item.created && !outcome.replayed),
            outcome.item.settings.version,
            &outcome.item.settings,
        ),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn list_stagings(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_BATCH_STAGINGS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.stagings(actor, organization_id).await {
        Ok(stagings) => (StatusCode::OK, Json(json!({ "data": stagings }))).into_response(),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn set_staging(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path((organization_id, connection)): Path<(OrganizationId, String)>,
    body: Result<Json<BatchStagingInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_BATCH_STAGING;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .service
        .set_staging(
            actor,
            organization_id,
            &connection,
            input,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(outcome) => versioned(
            created(outcome.item.created && !outcome.replayed),
            outcome.item.staging.version,
            &outcome.item.staging,
        ),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn delete_staging(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path((organization_id, connection)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_BATCH_STAGING;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .service
        .delete_staging(
            actor,
            organization_id,
            &connection,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => batch_problem(error, path, &id),
    }
}

#[derive(Debug, Deserialize)]
pub struct BatchListQuery {
    pub service_account_id: Option<Uuid>,
    pub status: Option<String>,
    /// The last batch id of the previous page.
    pub page_token: Option<String>,
    pub page_size: Option<i64>,
}

const STATUSES: [&str; 8] = [
    "validating",
    "in_progress",
    "finalizing",
    "completed",
    "failed",
    "expired",
    "cancelling",
    "cancelled",
];

async fn list_batches(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<BatchListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ADMIN_BATCHES;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(query)) = query else {
        return invalid_query(path, &id);
    };
    let after = match query.page_token.as_deref() {
        None => None,
        Some(after) => match parse_id(after, BATCH_PREFIX) {
            Some(after) => Some(after),
            None => return invalid_query(path, &id),
        },
    };
    if query
        .status
        .as_deref()
        .is_some_and(|status| !STATUSES.contains(&status))
    {
        return invalid_query(path, &id);
    }
    let limit = query.page_size.unwrap_or(25);
    if !(1..=MAX_PAGE).contains(&limit) {
        return invalid_query(path, &id);
    }
    match state
        .service
        .batches(
            actor,
            organization_id,
            query.service_account_id,
            query.status.as_deref(),
            after,
            limit + 1,
        )
        .await
    {
        Ok(mut rows) => {
            let has_more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            let data: Vec<Value> = rows.iter().map(admin_batch).collect();
            let body = json!({
                "items": data,
                "next_page_token": has_more.then(|| rows.last().map(|row| batch_id(row.id))).flatten(),
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn get_batch(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path((organization_id, batch)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ADMIN_BATCH;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Some(batch) = parse_id(&batch, BATCH_PREFIX) else {
        return batch_problem(BatchAdminError::NotFound, path, &id);
    };
    match state.service.batch(actor, organization_id, batch).await {
        Ok(detail) => (StatusCode::OK, Json(detail_body(&detail))).into_response(),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn cancel_batch(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path((organization_id, batch)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_ADMIN_BATCH_CANCEL;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let idempotency_key = match require_idempotency_key(&headers, path, &id) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let Some(batch) = parse_id(&batch, BATCH_PREFIX) else {
        return batch_problem(BatchAdminError::NotFound, path, &id);
    };
    match state
        .service
        .cancel(actor, organization_id, batch, idempotency_key, request_id)
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(detail_body(&outcome.item))).into_response(),
        Err(error) => batch_problem(error, path, &id),
    }
}

async fn output(
    state: State<BatchAdminState>,
    headers: HeaderMap,
    path: Path<(OrganizationId, String)>,
) -> Response {
    results(
        state,
        headers,
        path,
        ResultFile::Output,
        ROUTE_ADMIN_BATCH_OUTPUT,
    )
    .await
}

async fn errors(
    state: State<BatchAdminState>,
    headers: HeaderMap,
    path: Path<(OrganizationId, String)>,
) -> Response {
    results(
        state,
        headers,
        path,
        ResultFile::Errors,
        ROUTE_ADMIN_BATCH_ERRORS,
    )
    .await
}

async fn results(
    State(state): State<BatchAdminState>,
    headers: HeaderMap,
    Path((organization_id, batch)): Path<(OrganizationId, String)>,
    which: ResultFile,
    path: &str,
) -> Response {
    let (id, _) = request_context(&headers);
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Some(batch) = parse_id(&batch, BATCH_PREFIX) else {
        return batch_problem(BatchAdminError::NotFound, path, &id);
    };
    match state
        .service
        .results(actor, organization_id, batch, which)
        .await
    {
        Ok(bytes) => {
            let name = match which {
                ResultFile::Output => "output",
                ResultFile::Errors => "errors",
            };
            let mut response = (StatusCode::OK, bytes).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/jsonl"),
            );
            if let Ok(value) = HeaderValue::from_str(&format!(
                "attachment; filename=\"{}-{name}.jsonl\"",
                batch_id(batch)
            )) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(error) => batch_problem(error, path, &id),
    }
}

/// A batch as the admin list shows it: the inference API's object, with
/// who made it.
fn admin_batch(batch: &crate::persistence::batches::BatchRow) -> Value {
    let mut body = batch_object(batch);
    body["service_account_id"] = json!(batch.service_account_id);
    body["team_id"] = json!(batch.team_id);
    body
}

/// A batch's detail: its jobs, each with how and where it runs, and its
/// cost so far.
fn detail_body(detail: &BatchDetail) -> Value {
    let mut body = admin_batch(&detail.batch);
    let mut extension = gateway_extension(&detail.jobs, &detail.costs);
    if let Some(jobs) = extension["jobs"].as_array_mut() {
        for (job, summary) in jobs.iter_mut().zip(&detail.jobs) {
            job["deployment"] = json!(summary.deployment);
            job["model"] = json!(summary.model);
            job["provider_batch_id"] = json!(summary.provider_batch_id);
            job["error_code"] = json!(summary.error_code);
            job["request_counts"]["pending"] = json!(summary.pending);
            job["request_counts"]["unknown"] = json!(summary.unknown);
            job["submitted_at"] = json!(summary.submitted_at);
            job["finished_at"] = json!(summary.finished_at);
        }
    }
    body["gateway"] = extension;
    body
}

fn created(created: bool) -> StatusCode {
    if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

fn versioned<T: serde::Serialize>(status: StatusCode, version: u64, body: &T) -> Response {
    let mut response = (status, Json(body)).into_response();
    if version > 0 {
        set_etag(&mut response, version);
    }
    response
}

fn invalid_query(path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "status, page_token or page_size is not valid",
        path,
        id,
    )
}

async fn actor(
    state: &BatchAdminState,
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

fn batch_problem(error: BatchAdminError, path: &str, id: &str) -> Response {
    match error {
        BatchAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        BatchAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        BatchAdminError::Invalid(message) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_batch_setting",
            message,
            path,
            id,
        ),
        BatchAdminError::NotStaged => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch_staging_unsupported",
            "the connection's provider runs no batch jobs staged in a bucket",
            path,
            id,
        ),
        BatchAdminError::ResultsUnavailable => problem(
            StatusCode::NOT_FOUND,
            "batch_results_unavailable",
            "the batch has no such results yet, or they were purged",
            path,
            id,
        ),
        BatchAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        BatchAdminError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        BatchAdminError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        BatchAdminError::Store(_) | BatchAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "batch admin lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "batch store unavailable",
                path,
                id,
            )
        }
    }
}
