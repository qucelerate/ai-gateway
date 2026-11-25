// HTTP transport for data retention and export under
// `/api/admin/v1/organizations/{org}`: read and set the retention policy,
// download usage, traces or audit events as NDJSON or CSV, and list past
// exports.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. Setting the policy requires `Idempotency-Key` and
// `If-Match` (`*` for any state). An export streams in batches; if the store
// fails midway the connection is closed before the body ends, so the client
// sees an incomplete transfer rather than a short file. Problems share the
// admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use chrono::Utc;
use serde::Deserialize;

use crate::{
    application::retention::{
        ROUTE_EXPORT, ROUTE_EXPORTS, ROUTE_RETENTION_POLICY, RetentionError, RetentionService,
        StartedExport,
    },
    domain::{
        auth::AuthCrypto,
        identity::{OrganizationId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        retention::{ExportQuery, ExportStatus, RetentionPolicyInput},
    },
    persistence::{admin::AdminStore, auth::AuthStore, retention::RetentionStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

/// Rows read per export query.
const EXPORT_BATCH: i64 = 1_000;

#[derive(Clone)]
pub struct RetentionState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<RetentionService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: RetentionStore,
) -> Router {
    Router::new()
        .route(ROUTE_RETENTION_POLICY, get(get_policy).put(set_policy))
        .route(ROUTE_EXPORTS, get(list_exports))
        .route(ROUTE_EXPORT, get(export))
        .with_state(RetentionState {
            service: Arc::new(RetentionService::new(
                admin,
                store,
                crypto.clone().map(SignedCursorCodec::new),
            )),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    format: Option<String>,
    from: Option<String>,
    to: Option<String>,
    /// Usage and traces: only this batch's lines.
    batch_id: Option<String>,
    /// Usage and traces: only requests of a master record's deployments.
    model_master: Option<String>,
}

async fn get_policy(
    State(state): State<RetentionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_RETENTION_POLICY;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.policy(actor, organization_id).await {
        Ok(policy) => {
            let version = policy.version;
            let mut response = (StatusCode::OK, Json(policy)).into_response();
            if version > 0 {
                set_etag(&mut response, version);
            }
            response
        }
        Err(error) => retention_problem(error, path, &id),
    }
}

async fn set_policy(
    State(state): State<RetentionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<RetentionPolicyInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_RETENTION_POLICY;
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
        .service
        .set_policy(
            actor,
            organization_id,
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
            let version = outcome.item.policy.version;
            let mut response = (status, Json(outcome.item.policy)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => retention_problem(error, path, &id),
    }
}

async fn export(
    State(state): State<RetentionState>,
    headers: HeaderMap,
    Path((organization_id, dataset)): Path<(OrganizationId, String)>,
    params: Result<Query<ExportParams>, QueryRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_EXPORT;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(params)) = params else {
        return problem(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "unknown or malformed query parameter",
            path,
            &id,
        );
    };
    let query = match ExportQuery::new(
        &dataset,
        params.format.as_deref(),
        params.from.as_deref(),
        params.to.as_deref(),
        Utc::now(),
    )
    .and_then(|query| query.with_batch(params.batch_id.as_deref()))
    .and_then(|query| query.with_model_master(params.model_master.as_deref()))
    {
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
    let started = match state
        .service
        .begin_export(actor, organization_id, &query, request_id)
        .await
    {
        Ok(started) => started,
        Err(error) => return retention_problem(error, path, &id),
    };
    let store = state.service.store().clone();
    let mut outcome = ExportOutcome {
        service: state.service.clone(),
        export: started,
        rows: 0,
        finished: false,
    };
    let content_type = query.format.content_type();
    let file_name = query.file_name();
    let body = async_stream::stream! {
        let preamble = query.format.preamble(query.dataset);
        if !preamble.is_empty() {
            yield Ok::<Bytes, std::io::Error>(Bytes::from(preamble));
        }
        let mut after = None;
        loop {
            match store
                .export_page(organization_id, &query, after, EXPORT_BATCH)
                .await
            {
                Ok(rows) => {
                    let done = rows.len() < EXPORT_BATCH as usize;
                    let mut chunk = String::new();
                    for (at, row_id, values) in &rows {
                        chunk.push_str(&query.format.row(query.dataset, values));
                        after = Some((*at, *row_id));
                    }
                    if !chunk.is_empty() {
                        yield Ok(Bytes::from(chunk));
                    }
                    // Counted once the client has taken the chunk.
                    outcome.rows += rows.len() as u64;
                    if done {
                        outcome.finish(ExportStatus::Completed).await;
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "export query failed; body truncated");
                    outcome.finish(ExportStatus::Failed).await;
                    yield Err(std::io::Error::other("export failed"));
                    break;
                }
            }
        }
    };
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{}\"", file_name)) {
        headers.insert(CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&id) {
        headers.insert("x-request-id", value);
    }
    response
}

/// How a streaming export ends. If the body is dropped before it finishes,
/// usually because the client went away, the export is recorded as
/// abandoned.
struct ExportOutcome {
    service: Arc<RetentionService>,
    export: StartedExport,
    rows: u64,
    finished: bool,
}

impl ExportOutcome {
    /// Records the end in a task of its own, so a client leaving while it
    /// is written cannot cut it short.
    async fn finish(&mut self, status: ExportStatus) {
        self.finished = true;
        let (service, export, rows) = (self.service.clone(), self.export, self.rows);
        let _ =
            tokio::spawn(async move { service.finish_export(export, status, rows).await }).await;
    }
}

impl Drop for ExportOutcome {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let (service, export, rows) = (self.service.clone(), self.export, self.rows);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                service
                    .finish_export(export, ExportStatus::Abandoned, rows)
                    .await;
            });
        }
    }
}

async fn list_exports(
    State(state): State<RetentionState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_EXPORTS;
    let actor = match human_actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(page)) = page else {
        return pagination_problem(PaginationError::LimitOutOfBounds, path, &id);
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    match state.service.exports(actor, organization_id, &page).await {
        Ok(page) => (StatusCode::OK, Json(page)).into_response(),
        Err(error) => retention_problem(error, path, &id),
    }
}

async fn human_actor(
    state: &RetentionState,
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

fn retention_problem(error: RetentionError, path: &str, id: &str) -> Response {
    match error {
        RetentionError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        RetentionError::Pagination(error) => pagination_problem(error, path, id),
        RetentionError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        RetentionError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        RetentionError::Invalid(error) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_retention_policy",
            error.to_string(),
            path,
            id,
        ),
        RetentionError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        RetentionError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        RetentionError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        RetentionError::Store(_) | RetentionError::Admin(_) => {
            tracing::warn!(error = ?error, "retention lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "retention store unavailable",
                path,
                id,
            )
        }
    }
}
