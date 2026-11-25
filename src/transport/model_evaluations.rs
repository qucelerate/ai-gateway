// HTTP transport for an organization's own evaluations (Phase 8 milestone
// 4): `GET`/`POST .../catalog/evaluation-sets`, `GET`/`DELETE .../{set}`,
// and `GET`/`POST .../{set}/runs`. Creating and deleting need
// `Idempotency-Key`; deleting needs `If-Match` too.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use uuid::Uuid;

use crate::{
    application::{
        model_evaluations::{
            EvaluationError, ModelEvaluationService, ROUTE_EVALUATION_RUNS, ROUTE_EVALUATION_SET,
            ROUTE_EVALUATION_SETS,
        },
        trace_objects::TraceObjects,
    },
    domain::{auth::AuthCrypto, identity::OrganizationId, model_evaluation::EvaluationItem},
    persistence::{admin::AdminStore, auth::AuthStore, model_evaluations::ModelEvaluationStore},
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
struct EvaluationState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ModelEvaluationService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: ModelEvaluationStore,
    objects: Option<Arc<dyn TraceObjects>>,
) -> Router {
    Router::new()
        .route(ROUTE_EVALUATION_SETS, get(list_sets).post(create_set))
        .route(ROUTE_EVALUATION_SET, get(get_set).delete(delete_set))
        .route(ROUTE_EVALUATION_RUNS, get(list_runs).post(create_run))
        .with_state(EvaluationState {
            auth,
            crypto,
            service: Arc::new(ModelEvaluationService::new(admin, store, objects)),
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn actor(
    state: &EvaluationState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<crate::domain::identity::UserAccountId, Response> {
    match crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
        .await
    {
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

#[derive(serde::Serialize)]
struct Items<T> {
    items: Vec<T>,
}

async fn list_sets(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_EVALUATION_SETS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.sets(actor, organization_id).await {
        Ok(items) => Json(Items { items }).into_response(),
        Err(error) => evaluation_problem(error, path, &id),
    }
}

async fn get_set(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path((organization_id, set)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_EVALUATION_SET;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.set(actor, organization_id, set).await {
        Ok(set) => {
            let version = set.version;
            let mut response = Json(set).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => evaluation_problem(error, path, &id),
    }
}

async fn delete_set(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path((organization_id, set)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_EVALUATION_SET;
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
        .delete_set(
            actor,
            organization_id,
            set,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => evaluation_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetInput {
    name: String,
    #[serde(default)]
    description: Option<String>,
    items: Vec<EvaluationItem>,
}

async fn create_set(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<SetInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_EVALUATION_SETS;
    let actor = match actor(&state, &headers, path, &id).await {
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
        .service
        .create_set(
            actor,
            organization_id,
            &input.name,
            input.description.as_deref(),
            &input.items,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok((set, replayed)) => {
            let version = set.version;
            let mut response = (
                if replayed {
                    StatusCode::OK
                } else {
                    StatusCode::CREATED
                },
                Json(set),
            )
                .into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => evaluation_problem(error, path, &id),
    }
}

async fn list_runs(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path((organization_id, set)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_EVALUATION_RUNS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.runs(actor, organization_id, set).await {
        Ok(items) => Json(Items { items }).into_response(),
        Err(error) => evaluation_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    /// The batch's id as the batches APIs give it (`batch_<uuid>`), or the
    /// bare UUID.
    batch_id: String,
}

fn run_batch_id(value: &str) -> Option<Uuid> {
    crate::domain::batch::parse_batch_ref(value)
}

async fn create_run(
    State(state): State<EvaluationState>,
    headers: HeaderMap,
    Path((organization_id, set)): Path<(OrganizationId, Uuid)>,
    body: Result<Json<RunInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_EVALUATION_RUNS;
    let actor = match actor(&state, &headers, path, &id).await {
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
    let Some(batch_id) = run_batch_id(&input.batch_id) else {
        return problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch_not_found",
            "batch_id is not a batch id",
            path,
            &id,
        );
    };
    match state
        .service
        .create_run(
            actor,
            organization_id,
            set,
            batch_id,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok((run, replayed)) => (
            if replayed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            Json(run),
        )
            .into_response(),
        Err(error) => evaluation_problem(error, path, &id),
    }
}

fn evaluation_problem(error: EvaluationError, path: &str, id: &str) -> Response {
    match error {
        EvaluationError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the caller may not do this in this organization's catalog",
            path,
            id,
        ),
        EvaluationError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such evaluation set",
            path,
            id,
        ),
        EvaluationError::Invalid(why) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_evaluation_set",
            why,
            path,
            id,
        ),
        EvaluationError::BatchNotFound => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch_not_found",
            "the organization has no such batch",
            path,
            id,
        ),
        EvaluationError::BatchUnusable(why) => {
            problem(StatusCode::CONFLICT, "batch_unusable", why, path, id)
        }
        EvaluationError::StoreUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "file_store_unavailable",
            "the batch file store is not configured or could not be read",
            path,
            id,
        ),
        EvaluationError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        EvaluationError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        EvaluationError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        EvaluationError::Store(_) | EvaluationError::Admin(_) => {
            tracing::warn!(error = ?error, "model evaluation lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "model evaluations unavailable",
                path,
                id,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_names_its_batch_either_way() {
        let id = Uuid::new_v4();
        assert_eq!(run_batch_id(&format!("batch_{id}")), Some(id));
        assert_eq!(run_batch_id(&id.to_string()), Some(id));
        assert_eq!(run_batch_id(&format!("file-{id}")), None);
        assert_eq!(run_batch_id("batch_"), None);
    }
}
