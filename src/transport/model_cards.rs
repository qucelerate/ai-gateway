// HTTP transport for model cards:
// `GET /api/admin/v1/organizations/{org}/catalog/models/{model}/card`, the
// organization's note on it (`GET`/`PUT .../card/note`), and the card facts
// of every model the active release serves (`GET .../catalog/model-cards`).
//
// Setting a note requires `Idempotency-Key` and `If-Match` (`*` for any
// state).
//
// Requires a session or an agent token, and responds with
// `Cache-Control: no-store`. `{model}` is the catalog model identifier,
// percent-encoded (Bedrock and Vertex identifiers carry `:` and `/`).
// Problems share the admin lanes' `application/problem+json` shape.

use std::sync::Arc;

use crate::{
    application::model_cards::{
        HubSource, ModelCardError, ModelCardService, ROUTE_MODEL_CARD, ROUTE_MODEL_CARD_IMPORT,
        ROUTE_MODEL_CARD_NOTE, ROUTE_MODEL_CARDS,
    },
    domain::{auth::AuthCrypto, identity::OrganizationId, model_card_files::CardLibrary},
    persistence::{
        admin::AdminStore, auth::AuthStore, model_cards::ModelCardStore, reporting::ReportingStore,
        trace_policy::TracePolicyStore,
    },
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};
use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};

#[derive(Clone)]
struct ModelCardState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ModelCardService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: ModelCardStore,
    reporting: ReportingStore,
    trace_policy: TracePolicyStore,
    library: Arc<CardLibrary>,
    hub: Option<HubSource>,
) -> Router {
    Router::new()
        .route(ROUTE_MODEL_CARD, get(model_card))
        .route(ROUTE_MODEL_CARD_NOTE, get(get_note).put(set_note))
        .route(ROUTE_MODEL_CARDS, get(list_cards))
        .route(
            ROUTE_MODEL_CARD_IMPORT,
            get(get_import).put(set_import).delete(remove_import),
        )
        .with_state(ModelCardState {
            auth,
            crypto,
            service: Arc::new(
                ModelCardService::new(admin, store, reporting, trace_policy, library).with_hub(hub),
            ),
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn actor(
    state: &ModelCardState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<crate::domain::identity::UserAccountId, Response> {
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

#[derive(serde::Serialize)]
struct CardList {
    /// The active release; `null` when the organization has none.
    release_id: Option<uuid::Uuid>,
    items: Vec<crate::domain::model_card::ModelCardSummary>,
}

async fn list_cards(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_CARDS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.summaries(actor, organization_id).await {
        Ok((release_id, items)) => {
            (StatusCode::OK, Json(CardList { release_id, items })).into_response()
        }
        Err(error) => card_problem(error, path, &id),
    }
}

async fn get_note(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_CARD_NOTE;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.note(actor, organization_id, &model).await {
        Ok(note) => {
            let version = note.version;
            let mut response = (StatusCode::OK, Json(note)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => card_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteInput {
    note: String,
}

async fn set_note(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
    body: Result<Json<NoteInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_CARD_NOTE;
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
        .set_note(
            actor,
            organization_id,
            &model,
            &input.note,
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok((note, created, replayed)) => {
            let status = if created && !replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = note.version;
            let mut response = (status, Json(note)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => card_problem(error, path, &id),
    }
}

async fn get_import(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_CARD_IMPORT;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.import(actor, organization_id, &model).await {
        Ok(import) => {
            let version = import.version;
            let mut response = (StatusCode::OK, Json(import)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => card_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportInput {
    /// The Hub repository, `owner/name`.
    repository: String,
    /// Whether the repository is the model provider's own.
    provider_repository: bool,
}

async fn set_import(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
    body: Result<Json<ImportInput>, JsonRejection>,
) -> Response {
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => {
            let (id, _) = request_context(&headers);
            return body_rejection(rejection, ROUTE_MODEL_CARD_IMPORT, &id);
        }
    };
    mutate_import(
        state,
        headers,
        organization_id,
        model,
        Some((input.repository, input.provider_repository)),
    )
    .await
}

async fn remove_import(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
) -> Response {
    mutate_import(state, headers, organization_id, model, None).await
}

async fn mutate_import(
    state: ModelCardState,
    headers: HeaderMap,
    organization_id: OrganizationId,
    model: String,
    repository: Option<(String, bool)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_CARD_IMPORT;
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
        .set_import(
            actor,
            organization_id,
            &model,
            repository.as_ref().map(|(r, p)| (r.as_str(), *p)),
            if_match,
            idempotency_key,
            request_id,
        )
        .await
    {
        Ok((Some(import), created, replayed)) => {
            let status = if created && !replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = import.version;
            let mut response = (status, Json(import)).into_response();
            set_etag(&mut response, version);
            response
        }
        Ok((None, _, _)) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => card_problem(error, path, &id),
    }
}

async fn model_card(
    State(state): State<ModelCardState>,
    headers: HeaderMap,
    Path((organization_id, model)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_CARD;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .card(actor, organization_id, &model, request_id)
        .await
    {
        Ok(card) => (StatusCode::OK, Json(card)).into_response(),
        Err(error) => card_problem(error, path, &id),
    }
}

fn card_problem(error: ModelCardError, path: &str, id: &str) -> Response {
    match error {
        ModelCardError::InvalidModel => problem(
            StatusCode::BAD_REQUEST,
            "invalid_model",
            "the model identifier is empty, too long, or has control characters",
            path,
            id,
        ),
        ModelCardError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        ModelCardError::NotFound if path == ROUTE_MODEL_CARD_IMPORT => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "no card is imported for this model",
            path,
            id,
        ),
        ModelCardError::NotFound if path == ROUTE_MODEL_CARD_NOTE => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "no note is set on this model",
            path,
            id,
        ),
        ModelCardError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "the active catalog release does not serve this model",
            path,
            id,
        ),
        ModelCardError::InvalidRepository => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_repository",
            "repository must be a Hugging Face model repository id, owner/name",
            path,
            id,
        ),
        ModelCardError::ImportDisabled => problem(
            StatusCode::CONFLICT,
            "import_disabled",
            "Hugging Face imports are off: the operator sets GATEWAY_MODEL_CARD_HUB_URL to turn them on",
            path,
            id,
        ),
        ModelCardError::RepositoryNotFound => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "repository_not_found",
            "the Hub has no public model repository by that id",
            path,
            id,
        ),
        ModelCardError::HubUnavailable => problem(
            StatusCode::BAD_GATEWAY,
            "hub_unavailable",
            "the Hugging Face Hub could not be read",
            path,
            id,
        ),
        ModelCardError::ImportUnusable(why) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "import_unusable",
            format!("the Hub's model card cannot be imported: {why}"),
            path,
            id,
        ),
        ModelCardError::InvalidNote => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_note",
            "a note holds at most 4000 characters, without control characters other than newlines and tabs",
            path,
            id,
        ),
        ModelCardError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        ModelCardError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
            path,
            id,
        ),
        ModelCardError::IdempotencyInProgress => problem(
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress",
            path,
            id,
        ),
        ModelCardError::Store(_) | ModelCardError::Reporting(_) | ModelCardError::Admin(_) => {
            tracing::warn!(error = ?error, "model card lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "model card unavailable",
                path,
                id,
            )
        }
    }
}
