// HTTP transport for master model records (Phase 13, D-033):
// `GET .../catalog/model-masters` (every record with its linked deployments
// in the active release, and the deployments with none),
// `GET`/`PUT`/`DELETE .../catalog/model-masters/{master}` (one record; only
// organization records, `org/<slug>`, are written),
// `GET .../catalog/model-masters/{master}/route-proposal`, and
// `GET`/`PUT`/`DELETE .../catalog/model-master-links/{family}/{model}`.
//
// Writes require `Idempotency-Key` and `If-Match` (`"v0"` when new, the
// current version after, or `*`). `{master}` and `{model}` are
// percent-encoded (`org%2Fqwen`, `us.amazon.nova-2-lite-v1%3A0`). Requires a
// session or an agent token, and responds with `Cache-Control: no-store`.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};

use crate::{
    application::model_masters::{
        ModelMasterError, ModelMasterService, ROUTE_MODEL_MASTER, ROUTE_MODEL_MASTER_LINK,
        ROUTE_MODEL_MASTERS, ROUTE_ROUTE_PROPOSAL,
    },
    domain::{
        auth::AuthCrypto, identity::OrganizationId, model_card_files::CardLibrary,
        model_master::Modality,
    },
    persistence::{
        admin::AdminStore, auth::AuthStore, catalog_admin::CatalogAdminStore,
        model_cards::ModelCardStore, model_masters::ModelMasterStore,
        routing_policy::RoutingPolicyStore,
    },
    transport::admin_identities::{
        body_rejection, no_store, problem, request_context, require_idempotency_key,
        require_if_match, set_etag,
    },
};

#[derive(Clone)]
struct MasterState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<ModelMasterService>,
}

#[allow(clippy::too_many_arguments)]
pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    cards: ModelCardStore,
    masters: ModelMasterStore,
    catalog: CatalogAdminStore,
    routing_policy: RoutingPolicyStore,
    library: Arc<CardLibrary>,
) -> Router {
    Router::new()
        .route(ROUTE_MODEL_MASTERS, get(list_masters))
        .route(
            ROUTE_MODEL_MASTER,
            get(get_master).put(put_master).delete(delete_master),
        )
        .route(ROUTE_ROUTE_PROPOSAL, get(route_proposal))
        .route(
            ROUTE_MODEL_MASTER_LINK,
            get(get_link).put(set_link).delete(clear_link),
        )
        .with_state(MasterState {
            auth,
            crypto,
            service: Arc::new(ModelMasterService::new(
                admin,
                cards,
                masters,
                catalog,
                routing_policy,
                library,
            )),
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn actor(
    state: &MasterState,
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

/// Everything a write needs from its headers.
struct Write {
    actor: crate::domain::identity::UserAccountId,
    idempotency_key: crate::domain::admin::AdminIdempotencyKey,
    if_match: crate::domain::version::IfMatch,
    request_id: uuid::Uuid,
}

async fn write(
    state: &MasterState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
    request_id: uuid::Uuid,
) -> Result<Write, Response> {
    let actor = actor(state, headers, path, id).await?;
    let idempotency_key = require_idempotency_key(headers, path, id)?;
    let if_match = require_if_match(headers, path, id)?;
    Ok(Write {
        actor,
        idempotency_key,
        if_match,
        request_id,
    })
}

async fn list_masters(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_MASTERS;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.list(actor, organization_id).await {
        Ok(list) => (StatusCode::OK, Json(list)).into_response(),
        Err(error) => master_problem(error, path, &id),
    }
}

async fn get_master(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, master)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state.service.master(actor, organization_id, &master).await {
        Ok(view) => {
            let version = view.version;
            let mut response = (StatusCode::OK, Json(view)).into_response();
            if let Some(version) = version {
                set_etag(&mut response, version);
            }
            response
        }
        Err(error) => master_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordInput {
    name: String,
    #[serde(default)]
    modality: Modality,
}

async fn put_master(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, master)): Path<(OrganizationId, String)>,
    body: Result<Json<RecordInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER;
    let write = match write(&state, &headers, path, &id, request_id).await {
        Ok(write) => write,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .put_record(
            write.actor,
            organization_id,
            &master,
            &input.name,
            input.modality,
            write.if_match,
            write.idempotency_key,
            write.request_id,
        )
        .await
    {
        Ok((record, created, replayed)) => {
            let status = if created && !replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = record.version;
            let mut response = (status, Json(record)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => master_problem(error, path, &id),
    }
}

async fn delete_master(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, master)): Path<(OrganizationId, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER;
    let write = match write(&state, &headers, path, &id, request_id).await {
        Ok(write) => write,
        Err(response) => return response,
    };
    match state
        .service
        .delete_record(
            write.actor,
            organization_id,
            &master,
            write.if_match,
            write.idempotency_key,
            write.request_id,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => master_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalQuery {
    /// Comma-separated; `chat` when absent.
    #[serde(default)]
    operations: Option<String>,
    #[serde(default)]
    tools: Option<bool>,
    #[serde(default)]
    alias: Option<String>,
}

async fn route_proposal(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, master)): Path<(OrganizationId, String)>,
    query: Result<Query<ProposalQuery>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ROUTE_PROPOSAL;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Query(query)) = query else {
        return problem(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "the query takes operations (comma-separated), tools (true or false) and alias",
            path,
            &id,
        );
    };
    let operations: Vec<String> = query
        .operations
        .as_deref()
        .unwrap_or("chat")
        .split(',')
        .map(|operation| operation.trim().to_owned())
        .collect();
    match state
        .service
        .propose(
            actor,
            organization_id,
            &master,
            &operations,
            query.tools.unwrap_or(false),
            query.alias.as_deref(),
        )
        .await
    {
        Ok(proposal) => (StatusCode::OK, Json(proposal)).into_response(),
        Err(error) => master_problem(error, path, &id),
    }
}

async fn get_link(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, family, model)): Path<(OrganizationId, String, String)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER_LINK;
    let actor = match actor(&state, &headers, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .service
        .link(actor, organization_id, &family, &model)
        .await
    {
        Ok(link) => {
            let version = link.version;
            let mut response = (StatusCode::OK, Json(link)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => master_problem(error, path, &id),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkInput {
    /// A card's id or `org/<slug>`; `null` links to none. Required: a body
    /// that leaves it out is refused rather than read as a link to none.
    #[serde(deserialize_with = "present")]
    master: Option<String>,
}

/// A field that must be present, though it may be `null`: without
/// `#[serde(default)]`, a field using `deserialize_with` is required.
fn present<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<String> as serde::Deserialize>::deserialize(deserializer)
}

async fn set_link(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, family, model)): Path<(OrganizationId, String, String)>,
    body: Result<Json<LinkInput>, JsonRejection>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER_LINK;
    let write = match write(&state, &headers, path, &id, request_id).await {
        Ok(write) => write,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    match state
        .service
        .set_link(
            write.actor,
            organization_id,
            &family,
            &model,
            input.master.as_deref(),
            write.if_match,
            write.idempotency_key,
            write.request_id,
        )
        .await
    {
        Ok((link, created, replayed)) => {
            let status = if created && !replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = link.version;
            let mut response = (status, Json(link)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => master_problem(error, path, &id),
    }
}

async fn clear_link(
    State(state): State<MasterState>,
    headers: HeaderMap,
    Path((organization_id, family, model)): Path<(OrganizationId, String, String)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_MODEL_MASTER_LINK;
    let write = match write(&state, &headers, path, &id, request_id).await {
        Ok(write) => write,
        Err(response) => return response,
    };
    match state
        .service
        .clear_link(
            write.actor,
            organization_id,
            &family,
            &model,
            write.if_match,
            write.idempotency_key,
            write.request_id,
        )
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => master_problem(error, path, &id),
    }
}

fn master_problem(error: ModelMasterError, path: &str, id: &str) -> Response {
    let (status, code, detail): (StatusCode, &str, String) = match error {
        ModelMasterError::Forbidden => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied".into(),
        ),
        ModelMasterError::InvalidMaster if path == ROUTE_MODEL_MASTER => (
            StatusCode::BAD_REQUEST,
            "invalid_master",
            "a master is <vendor>/<model>; only organization records, org/<slug> (1-64 lowercase letters, digits, '-', '.' and '_'), are written here".into(),
        ),
        ModelMasterError::InvalidMaster => (
            StatusCode::BAD_REQUEST,
            "invalid_master",
            "a master is a card's id or org/<slug>, <vendor>/<model> in lowercase letters, digits, '-', '.' and '_'".into(),
        ),
        ModelMasterError::InvalidName => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_name",
            "a name is 1 to 120 characters without control characters".into(),
        ),
        ModelMasterError::InvalidFamily => (
            StatusCode::BAD_REQUEST,
            "invalid_family",
            "unknown provider family".into(),
        ),
        ModelMasterError::InvalidModel => (
            StatusCode::BAD_REQUEST,
            "invalid_model",
            "the model identifier is empty, too long, or has control characters".into(),
        ),
        ModelMasterError::InvalidOperation => (
            StatusCode::BAD_REQUEST,
            "invalid_operation",
            "operations lists one or more of chat, embeddings, chat_batch and embeddings_batch (streaming takes the chat route)".into(),
        ),
        ModelMasterError::InvalidAlias => (
            StatusCode::BAD_REQUEST,
            "invalid_alias",
            "an alias is 1 to 128 bytes, not only whitespace".into(),
        ),
        ModelMasterError::UnknownMaster => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_master",
            "no model card or organization record has that id".into(),
        ),
        ModelMasterError::ModalityMismatch(modality) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "modality_mismatch",
            format!(
                "the master is a {} model, and the deployments of this model serve no {} operation",
                modality.as_str(),
                modality.as_str()
            ),
        ),
        ModelMasterError::DeploymentNotFound => (
            StatusCode::NOT_FOUND,
            "deployment_not_found",
            "the active catalog release has no deployment of this model on this provider family"
                .into(),
        ),
        ModelMasterError::MasterInUse(links) => (
            StatusCode::CONFLICT,
            "master_in_use",
            format!(
                "{links} link(s) name this record; clear or change them first (GET .../catalog/model-masters lists every link)"
            ),
        ),
        ModelMasterError::NoActiveRelease => (
            StatusCode::NOT_FOUND,
            "no_active_release",
            "the organization has no active catalog release".into(),
        ),
        ModelMasterError::NotFound if path == ROUTE_MODEL_MASTER_LINK => (
            StatusCode::NOT_FOUND,
            "not_found",
            "no link is set for this model on this provider family".into(),
        ),
        ModelMasterError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "no model card or organization record has that id".into(),
        ),
        ModelMasterError::VersionConflict(conflict) => (
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
        ),
        ModelMasterError::IdempotencyConflict => (
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request".into(),
        ),
        ModelMasterError::IdempotencyInProgress => (
            StatusCode::CONFLICT,
            "idempotency_in_progress",
            "idempotent request is already in progress".into(),
        ),
        ModelMasterError::Store(_) | ModelMasterError::Admin(_) => {
            tracing::warn!(error = ?error, "model master lane failure");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "model masters unavailable".into(),
            )
        }
    };
    problem(status, code, detail, path, id)
}

#[cfg(test)]
mod link_input {
    use super::LinkInput;

    #[test]
    fn master_must_be_present_but_may_be_null() {
        assert!(serde_json::from_str::<LinkInput>("{}").is_err());
        assert_eq!(
            serde_json::from_str::<LinkInput>(r#"{"master":null}"#)
                .unwrap()
                .master,
            None
        );
        assert_eq!(
            serde_json::from_str::<LinkInput>(r#"{"master":"org/x"}"#)
                .unwrap()
                .master
                .as_deref(),
            Some("org/x")
        );
    }
}
