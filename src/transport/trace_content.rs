// HTTP transport for reading a traced request's content:
// `GET /api/admin/v1/organizations/{org}/traces/{id}/content`.
//
// Requires a session or an agent token and responds with
// `Cache-Control: no-store`. Problems share the admin lanes'
// `application/problem+json` shape.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use uuid::Uuid;

use crate::{
    application::{
        trace_content::{ROUTE_TRACE_CONTENT, TraceContentError, TraceContentService},
        trace_objects::TraceObjects,
    },
    domain::{auth::AuthCrypto, credentials::CredentialCipher, identity::OrganizationId},
    persistence::{admin::AdminStore, auth::AuthStore, trace_content::TraceContentStore},
    transport::admin_identities::{no_store, problem, request_context},
};

#[derive(Clone)]
pub struct TraceContentState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<TraceContentService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: TraceContentStore,
    objects: Option<Arc<dyn TraceObjects>>,
    cipher: Option<CredentialCipher>,
) -> Router {
    Router::new()
        .route(ROUTE_TRACE_CONTENT, get(read))
        .with_state(TraceContentState {
            service: Arc::new(TraceContentService::new(admin, store, objects, cipher)),
            auth,
            crypto,
        })
        .layer(axum::middleware::map_response(no_store))
}

async fn read(
    State(state): State<TraceContentState>,
    headers: HeaderMap,
    Path((organization_id, trace_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, request_id) = request_context(&headers);
    let path = ROUTE_TRACE_CONTENT;
    let actor = match crate::transport::admin_auth::admin_actor(
        &state.auth,
        state.crypto.as_ref(),
        &headers,
    )
    .await
    {
        Ok(Some(actor)) => actor,
        Ok(None) => {
            return problem(
                StatusCode::UNAUTHORIZED,
                "authentication_required",
                "a session or an agent token is required",
                path,
                &id,
            );
        }
        Err(_) => {
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication_unavailable",
                "authentication service unavailable",
                path,
                &id,
            );
        }
    };
    match state
        .service
        .read(actor, organization_id, trace_id, request_id)
        .await
    {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(TraceContentError::Forbidden) => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            &id,
        ),
        Err(TraceContentError::NotFound) => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            &id,
        ),
        Err(TraceContentError::Unavailable) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "trace_content_unavailable",
            "the trace's content is stored but cannot be read now",
            path,
            &id,
        ),
        Err(error @ (TraceContentError::Store(_) | TraceContentError::Admin(_))) => {
            tracing::warn!(error = ?error, "trace content lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "trace content store unavailable",
                path,
                &id,
            )
        }
    }
}
