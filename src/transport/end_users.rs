// HTTP transport for an app's end users (D-028), under
// `/api/admin/v1/organizations/{org}/service-accounts/{service_account_id}`:
// end-user settings, plans, end users, and their credit grants and
// reversals. The contract is `openapi/admin-end-users-v1.json`.
//
// Every route requires a session or an agent token and responds with
// `Cache-Control: no-store`. PUT and DELETE require `Idempotency-Key` and
// `If-Match`; the two POSTs take an optional `Idempotency-Key`, derived from
// the body's reference when absent.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    application::{
        admin_identities::MutationOutcome,
        end_users::{
            EndUserAdminError, EndUserAdminService, EndUserWrite, ROUTE_END_USER, ROUTE_END_USERS,
            ROUTE_GRANTS, ROUTE_PLAN, ROUTE_PLANS, ROUTE_REVERSALS, ROUTE_SETTINGS, Subject,
            WriteContext,
        },
    },
    domain::{
        admin::AdminIdempotencyKey,
        auth::AuthCrypto,
        end_user::{
            CreditGrantInput, CreditReversalInput, EndUserInput, EndUserSettingsInput, PlanInput,
        },
        identity::{OrganizationId, ServiceAccountId, UserAccountId},
        pagination::{PageRequest, PaginationError, SignedCursorCodec},
        version::IfMatch,
    },
    persistence::{admin::AdminStore, auth::AuthStore, end_users::EndUserStore},
    transport::admin_identities::{
        body_rejection, no_store, pagination_problem, problem, request_context,
        require_idempotency_key, require_if_match, set_etag,
    },
};

#[derive(Clone)]
pub struct EndUserState {
    auth: AuthStore,
    crypto: Option<AuthCrypto>,
    service: Arc<EndUserAdminService>,
}

pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    store: EndUserStore,
) -> Router {
    let service = Arc::new(EndUserAdminService::new(
        admin,
        store,
        crypto.clone().map(SignedCursorCodec::new),
    ));
    Router::new()
        .route(ROUTE_SETTINGS, get(get_settings).put(put_settings))
        .route(ROUTE_PLANS, get(list_plans))
        .route(ROUTE_PLAN, put(put_plan).delete(delete_plan))
        .route(ROUTE_END_USERS, get(list_end_users))
        .route(ROUTE_END_USER, get(get_end_user).put(put_end_user))
        .route(ROUTE_GRANTS, get(list_grants).post(create_grant))
        .route(ROUTE_REVERSALS, post(create_reversal))
        .with_state(EndUserState {
            auth,
            crypto,
            service,
        })
        .layer(axum::middleware::map_response(no_store))
}

type AccountPath = Path<(OrganizationId, Uuid)>;
type NamedPath = Path<(OrganizationId, Uuid, String)>;

#[derive(Serialize)]
struct List<T> {
    data: Vec<T>,
}

#[derive(Serialize)]
struct EndUserPage<T> {
    data: Vec<T>,
    next_page_token: Option<String>,
}

async fn subject(
    state: &EndUserState,
    headers: &HeaderMap,
    organization_id: OrganizationId,
    service_account_id: Uuid,
    path: &str,
    id: &str,
) -> Result<Subject, Response> {
    let actor = human_actor(state, headers, path, id).await?;
    Ok(Subject {
        actor,
        organization_id,
        service_account_id: ServiceAccountId(service_account_id),
    })
}

/// The headers of a PUT or DELETE: both required.
fn versioned_write(headers: &HeaderMap, path: &str, id: &str) -> Result<WriteContext, Response> {
    let (_, request_id) = request_context(headers);
    let idempotency_key = require_idempotency_key(headers, path, id)?;
    let if_match = require_if_match(headers, path, id)?;
    Ok(WriteContext {
        if_match,
        idempotency_key: Some(idempotency_key),
        request_id,
    })
}

/// The headers of a POST: `Idempotency-Key` optional, no `If-Match`.
fn ledger_write(headers: &HeaderMap, path: &str, id: &str) -> Result<WriteContext, Response> {
    let (_, request_id) = request_context(headers);
    let idempotency_key = if headers.contains_key("idempotency-key") {
        Some(require_idempotency_key(headers, path, id)?)
    } else {
        None::<AdminIdempotencyKey>
    };
    Ok(WriteContext {
        if_match: IfMatch::Any,
        idempotency_key,
        request_id,
    })
}

fn ok<T: Serialize>(result: Result<T, EndUserAdminError>, path: &str, id: &str) -> Response {
    match result {
        Ok(item) => (StatusCode::OK, Json(item)).into_response(),
        Err(error) => end_user_problem(error, path, id),
    }
}

fn written<T: Serialize>(
    result: Result<MutationOutcome<EndUserWrite<T>>, EndUserAdminError>,
    path: &str,
    id: &str,
) -> Response {
    match result {
        Ok(outcome) => {
            let status = if outcome.item.created && !outcome.replayed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let version = outcome.item.version;
            let mut response = (status, Json(outcome.item.item)).into_response();
            if let Some(version) = version {
                set_etag(&mut response, version);
            }
            response
        }
        Err(error) => end_user_problem(error, path, id),
    }
}

async fn get_settings(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account)): AccountPath,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SETTINGS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    match state.service.settings(subject).await {
        Ok(view) => {
            let version = view.version;
            let mut response = (StatusCode::OK, Json(view)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => end_user_problem(error, path, &id),
    }
}

async fn put_settings(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account)): AccountPath,
    body: Result<Json<EndUserSettingsInput>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SETTINGS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match versioned_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    written(
        state.service.put_settings(subject, input, context).await,
        path,
        &id,
    )
}

async fn list_plans(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account)): AccountPath,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PLANS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    ok(
        state.service.plans(subject).await.map(|data| List { data }),
        path,
        &id,
    )
}

async fn put_plan(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, plan)): NamedPath,
    body: Result<Json<PlanInput>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PLAN;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match versioned_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    written(
        state.service.put_plan(subject, &plan, input, context).await,
        path,
        &id,
    )
}

async fn delete_plan(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, plan)): NamedPath,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PLAN;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match versioned_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    match state.service.delete_plan(subject, &plan, context).await {
        Ok(outcome) if outcome.replayed => {
            (StatusCode::OK, Json(serde_json::json!({ "deleted": true }))).into_response()
        }
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => end_user_problem(error, path, &id),
    }
}

async fn list_end_users(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account)): AccountPath,
    page: Result<Query<PageRequest>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_END_USERS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let Ok(Query(page)) = page else {
        return pagination_problem(PaginationError::LimitOutOfBounds, path, &id);
    };
    if let Err(error) = page.validate() {
        return pagination_problem(error, path, &id);
    }
    // `data`, as the contract and the other lists of this lane name it.
    ok(
        state
            .service
            .end_users(subject, &page)
            .await
            .map(|page| EndUserPage {
                data: page.items,
                next_page_token: page.next_page_token,
            }),
        path,
        &id,
    )
}

async fn get_end_user(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, end_user)): NamedPath,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_END_USER;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    match state.service.end_user(subject, &end_user).await {
        Ok(view) => {
            let version = view.version;
            let mut response = (StatusCode::OK, Json(view)).into_response();
            set_etag(&mut response, version);
            response
        }
        Err(error) => end_user_problem(error, path, &id),
    }
}

async fn put_end_user(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, end_user)): NamedPath,
    body: Result<Json<EndUserInput>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_END_USER;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match versioned_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    written(
        state
            .service
            .put_end_user(subject, &end_user, input, context)
            .await,
        path,
        &id,
    )
}

async fn list_grants(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, end_user)): NamedPath,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_GRANTS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    ok(
        state
            .service
            .grants(subject, &end_user)
            .await
            .map(|data| List { data }),
        path,
        &id,
    )
}

async fn create_grant(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, end_user)): NamedPath,
    body: Result<Json<CreditGrantInput>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_GRANTS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match ledger_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    written(
        state
            .service
            .grant(subject, &end_user, input, context)
            .await,
        path,
        &id,
    )
}

async fn create_reversal(
    State(state): State<EndUserState>,
    headers: HeaderMap,
    Path((organization_id, account, end_user, grant_id)): Path<(
        OrganizationId,
        Uuid,
        String,
        Uuid,
    )>,
    body: Result<Json<CreditReversalInput>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_REVERSALS;
    let subject = match subject(&state, &headers, organization_id, account, path, &id).await {
        Ok(subject) => subject,
        Err(response) => return response,
    };
    let context = match ledger_write(&headers, path, &id) {
        Ok(context) => context,
        Err(response) => return response,
    };
    let Json(input) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection, path, &id),
    };
    written(
        state
            .service
            .reverse(subject, &end_user, grant_id, input, context)
            .await,
        path,
        &id,
    )
}

async fn human_actor(
    state: &EndUserState,
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

fn end_user_problem(error: EndUserAdminError, path: &str, id: &str) -> Response {
    let conflict =
        |code, detail: &str| problem(StatusCode::CONFLICT, code, detail.to_owned(), path, id);
    match error {
        EndUserAdminError::AuthenticationUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication_unavailable",
            "authentication service unavailable",
            path,
            id,
        ),
        EndUserAdminError::Forbidden => problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ),
        EndUserAdminError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "not_found",
            "resource does not exist in the requested scope",
            path,
            id,
        ),
        EndUserAdminError::Invalid(code, detail) => {
            problem(StatusCode::UNPROCESSABLE_ENTITY, code, detail, path, id)
        }
        EndUserAdminError::Pagination(error) => pagination_problem(error, path, id),
        EndUserAdminError::VersionConflict(conflict) => problem(
            StatusCode::CONFLICT,
            "version_conflict",
            format!(
                "resource version conflict; current version is {}",
                conflict.current.get()
            ),
            path,
            id,
        ),
        EndUserAdminError::IdempotencyConflict => conflict(
            "idempotency_conflict",
            "Idempotency-Key conflicts with a different request",
        ),
        EndUserAdminError::IdempotencyInProgress => conflict(
            "idempotency_in_progress",
            "idempotent request is already in progress",
        ),
        EndUserAdminError::SettingsMissing => conflict(
            "end_user_settings_missing",
            "set the service account's end-user settings first",
        ),
        EndUserAdminError::CurrencyLocked => conflict(
            "currency_locked",
            "the currency cannot change once a plan or a credit grant exists",
        ),
        EndUserAdminError::PlanInUse => conflict(
            "plan_in_use",
            "the plan is the default plan or an end user is on it",
        ),
        EndUserAdminError::CreditReferenceConflict => conflict(
            "credit_reference_conflict",
            "a different grant already has this reference",
        ),
        EndUserAdminError::ReversalReferenceConflict => conflict(
            "reversal_reference_conflict",
            "a different reversal of this grant already has this reference",
        ),
        EndUserAdminError::Store(_) | EndUserAdminError::Admin(_) => {
            tracing::warn!(error = ?error, "end-user lane failure");
            problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "end-user store unavailable",
                path,
                id,
            )
        }
    }
}
