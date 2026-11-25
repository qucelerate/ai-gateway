use std::time::Instant;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{COOKIE, ORIGIN, SET_COOKIE},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::{
    application::{
        AppState, audit::AuditApplicationService, identity_provider::ROUTE_IDENTITY_PROVIDER_TEST,
    },
    domain::{
        auth::{LoginFailure, OidcBackendError, validate_claims},
        identity::{OrganizationId, TeamId},
        identity_provider::{ProviderBinding, SignInPurpose},
    },
    persistence::identity_providers::StoredConfiguration,
    transport::deployment,
    transport::email_domains as domains,
    transport::local_accounts as local,
};

pub(crate) const SESSION_COOKIE: &str = "__Host-gateway_session";
pub(crate) const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
pub(crate) const CSRF_COOKIE: &str = "__Host-gateway_csrf";

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: &'static str,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    state: String,
    code: String,
}

#[derive(Debug, Deserialize)]
struct ContextRequest {
    organization_id: OrganizationId,
    team_id: Option<TeamId>,
}

#[derive(Debug, Serialize)]
struct MeResponse {
    user_id: crate::domain::identity::UserAccountId,
    display_name: Option<String>,
    email: Option<String>,
    /// Where to fetch the person's photo, if they have one.
    photo_url: Option<&'static str>,
    /// `password` for a local account's session, else `oidc`.
    sign_in_method: &'static str,
    /// Administers the deployment: may manage its organizations.
    deployment_admin: bool,
    /// Signed in through one organization's own provider: the session acts
    /// only there (and cannot create organizations).
    organization_session: bool,
    /// Signed-in people may create organizations
    /// (`GATEWAY_SELF_SERVE_ORGANIZATIONS`).
    self_serve_organizations: bool,
    /// When this session ends, in Unix seconds, so the console can warn
    /// before it does; `None` for an agent token.
    session_expires_at: Option<i64>,
}

const ME_PHOTO_PATH: &str = "/api/admin/v1/me/photo";

pub fn router(state: AppState) -> Router {
    let inference_body = state.inference_max_body_bytes();
    // Each subtree is closed with its own state first, then the closed
    // `Router<()>`s are merged. Organization/access and service-identity lanes
    // are human-session-only; API-key bearer tokens are never accepted here.
    let http = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/admin/v1/oidc/start", get(oidc_start))
        .route("/api/admin/v1/invitations/accept", get(invitation_accept))
        .route("/api/admin/v1/oidc/callback", get(oidc_callback))
        .route("/api/admin/v1/logout", post(logout))
        .route("/api/admin/v1/me", get(me))
        .route(ME_PHOTO_PATH, get(me_photo))
        .route(
            "/api/admin/v1/organizations",
            get(organizations).post(deployment::self_serve),
        )
        .route(
            deployment::ROUTE_ORGANIZATIONS,
            get(deployment::list).post(deployment::create),
        )
        .route(deployment::ROUTE_SUSPEND, post(deployment::suspend))
        .route(deployment::ROUTE_UNSUSPEND, post(deployment::unsuspend))
        .route(deployment::ROUTE_DELETE, post(deployment::delete))
        .route(deployment::ROUTE_RESTORE, post(deployment::restore))
        .route(deployment::ROUTE_AUDIT, get(deployment::audit))
        .route(
            deployment::ROUTE_DELETION_REQUEST,
            get(deployment::get_deletion_request)
                .put(deployment::put_deletion_request)
                .delete(deployment::delete_deletion_request),
        )
        .route("/api/admin/v1/context", post(context))
        .route(ROUTE_IDENTITY_PROVIDER_TEST, post(identity_provider_test))
        .route(local::ROUTE_SIGN_IN_OPTIONS, get(local::sign_in_options))
        .route(domains::ROUTE_SIGN_IN_START, post(domains::sign_in_start))
        .route(
            domains::ROUTE_DOMAINS,
            get(domains::list_domains).post(domains::add_domain),
        )
        .route(domains::ROUTE_DOMAIN, delete(domains::remove_domain))
        .route(domains::ROUTE_DOMAIN_VERIFY, post(domains::verify_domain))
        .route(local::ROUTE_SETUP, post(local::setup))
        .route(local::ROUTE_SETUP_CHECK, post(local::setup_check))
        .route(local::ROUTE_PASSWORD_SIGN_IN, post(local::password_sign_in))
        .route(local::ROUTE_PASSWORD_RESET, post(local::reset_password))
        .route(local::ROUTE_PASSWORD_RESET_CHECK, post(local::reset_check))
        .route(local::ROUTE_ME_PASSWORD, post(local::change_password))
        .route(
            local::ROUTE_INVITATION_INSPECT,
            post(local::invitation_inspect),
        )
        .route(
            local::ROUTE_INVITATION_ACCEPT_PASSWORD,
            post(local::invitation_accept_password),
        )
        .route(
            local::ROUTE_MEMBER_PASSWORD_RESET,
            post(local::member_password_reset),
        )
        .route(
            local::ROUTE_SIGN_IN_POLICY,
            get(local::get_sign_in_policy).put(local::put_sign_in_policy),
        )
        .route(
            crate::transport::inference::CHAT_PATH,
            post(crate::transport::inference::chat).layer(DefaultBodyLimit::max(inference_body)),
        )
        .route(
            crate::transport::inference::STREAM_PATH,
            post(crate::transport::inference::chat_stream)
                .layer(DefaultBodyLimit::max(inference_body)),
        )
        .route(
            crate::transport::inference::EMBEDDINGS_PATH,
            post(crate::transport::inference::embeddings)
                .layer(DefaultBodyLimit::max(inference_body)),
        )
        .merge(crate::transport::batches::routes())
        .with_state(state.clone());

    let access = crate::transport::admin_access::admin_access_router(
        crate::transport::admin_access::AdminAccessState::new_with_audit(
            state.access().clone(),
            state.auth().clone(),
            state.auth_crypto().cloned(),
            AuditApplicationService::new(
                state.admin().clone(),
                state.admin().audit_store(),
                state.auth_crypto().cloned(),
            ),
        ),
    );
    let identities = crate::transport::admin_identities::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.service_identities().clone(),
    );

    let reporting = crate::transport::reporting::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.reporting().clone(),
    );
    let catalog = crate::transport::catalog_admin::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.catalog_admin().clone(),
        state.provider_credentials().clone(),
        state.credential_cipher().cloned(),
    );
    let model_cards = crate::transport::model_cards::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        crate::persistence::model_cards::ModelCardStore::new(state.catalog_admin().pool().clone()),
        state.reporting().clone(),
        state.trace_policy().clone(),
        state.model_cards().clone(),
        state.model_card_hub().cloned(),
    );
    let model_masters = crate::transport::model_masters::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        crate::persistence::model_cards::ModelCardStore::new(state.catalog_admin().pool().clone()),
        crate::persistence::model_masters::ModelMasterStore::new(
            state.catalog_admin().pool().clone(),
        ),
        state.catalog_admin().clone(),
        state.routing_policy().clone(),
        state.model_cards().clone(),
    );
    let model_evaluations = crate::transport::model_evaluations::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        crate::persistence::model_evaluations::ModelEvaluationStore::new(
            state.catalog_admin().pool().clone(),
        ),
        state.trace_objects().cloned(),
    );
    let rate_limits = crate::transport::rate_limits::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.rate_limits().clone(),
    );
    let config_versions = crate::transport::config_versions::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.config_versions().clone(),
    );
    let onboarding = crate::transport::onboarding::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.onboarding().clone(),
        state.provider_credentials().clone(),
        state.verifier().cloned(),
    );
    let budgets = crate::transport::budgets::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.budgets().clone(),
    );
    let end_users = crate::transport::end_users::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.end_users().clone(),
    );
    let webhooks = crate::transport::webhooks::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.webhooks().clone(),
        state.credential_cipher().cloned(),
    );
    let retention = crate::transport::retention::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.retention().clone(),
    );
    let price_adoption = crate::transport::price_adoption::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.price_adoption().clone(),
    );
    let routing_policy = crate::transport::routing_policy::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.routing_policy().clone(),
    );
    let trace_policy = crate::transport::trace_policy::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.trace_policy().clone(),
    );
    let batch_admin = crate::transport::batch_admin::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        crate::persistence::batch_settings::BatchSettingsStore::new(
            state.batches().store().pool().clone(),
        ),
        state.batches().store().clone(),
        state.trace_objects().cloned(),
    );
    let notifications = crate::transport::notifications::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.notifications().clone(),
    );
    let trace_content = crate::transport::trace_content::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.trace_content().clone(),
        state.trace_objects().cloned(),
        state.credential_cipher().cloned(),
    );
    let usage_export = crate::transport::usage_export::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.usage_export().clone(),
        state.export_writer().clone(),
    );
    let agents = crate::transport::agents::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.agents().clone(),
    );
    let model_agreements = crate::transport::model_agreements::router(
        state.auth().clone(),
        state.auth_crypto().cloned(),
        crate::application::model_agreements::ModelAgreementService::new(
            state.catalog_admin().pool().clone(),
            state.admin().clone(),
            state.provider_accounts().cloned(),
        ),
    );
    let auto_offer = crate::transport::auto_offer::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.auto_offer().clone(),
    );
    let catalog_sync = crate::transport::catalog_sync::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.catalog_sync().clone(),
        state.price_sync().clone(),
    );
    let invitations = crate::transport::invitations::router(
        state.auth().clone(),
        state.admin().clone(),
        state.auth_crypto().cloned(),
        state.invitations(),
        crate::application::invitations::InvitationLinks::from_config_or_local(
            state.oidc(),
            state.local_accounts_config().enabled,
        )
        .ok(),
    );
    let identity_provider = crate::transport::identity_provider::router(
        state.auth().clone(),
        state.auth_crypto().cloned(),
        identity_provider_service(&state),
    );
    let routes = Router::new()
        .merge(http)
        .merge(access)
        .merge(identities)
        .merge(reporting)
        .merge(catalog)
        .merge(model_cards)
        .merge(model_masters)
        .merge(model_evaluations)
        .merge(rate_limits)
        .merge(config_versions)
        .merge(budgets)
        .merge(end_users)
        .merge(onboarding)
        .merge(webhooks)
        .merge(catalog_sync)
        .merge(retention)
        .merge(trace_policy)
        .merge(batch_admin)
        .merge(price_adoption)
        .merge(routing_policy)
        .merge(notifications)
        .merge(trace_content)
        .merge(usage_export)
        .merge(auto_offer)
        .merge(model_agreements)
        .merge(agents)
        .merge(invitations)
        .merge(identity_provider);
    let routes = routes.layer(middleware::from_fn_with_state(
        (state.auth().clone(), state.auth_crypto().cloned()),
        keep_sessions_in_their_organization,
    ));
    // MCP tool calls are admin requests sent to these routes in process,
    // counted like any other (an MCP `chat` is an inference request).
    let mcp = crate::transport::mcp::router(
        routes.clone().layer(middleware::from_fn(track)),
        state.auth().clone(),
        state.oidc().origin.clone(),
    );
    let routes = routes.merge(mcp);
    harden(routes, state.oidc().origin.clone())
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn(track))
}

/// The `data-plane` profile's routes: inference (files and batches
/// included), `/healthz` and `/readyz`.
/// No admin API and no sign-in, so it needs no session secrets and its
/// database role no access to admin tables.
pub fn data_plane_router(state: AppState) -> Router {
    let inference_body = state.inference_max_body_bytes();
    let routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            crate::transport::inference::CHAT_PATH,
            post(crate::transport::inference::chat).layer(DefaultBodyLimit::max(inference_body)),
        )
        .route(
            crate::transport::inference::STREAM_PATH,
            post(crate::transport::inference::chat_stream)
                .layer(DefaultBodyLimit::max(inference_body)),
        )
        .route(
            crate::transport::inference::EMBEDDINGS_PATH,
            post(crate::transport::inference::embeddings)
                .layer(DefaultBodyLimit::max(inference_body)),
        )
        .merge(crate::transport::batches::routes())
        .with_state(state);
    harden(routes, None)
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn(track))
}

/// The browser-facing protections every route gets.
fn harden(routes: Router, origin: Option<String>) -> Router {
    routes
        .layer(middleware::from_fn_with_state(
            origin,
            reject_cross_site_admin_writes,
        ))
        .layer(middleware::from_fn(security_headers))
}

/// Headers every response carries: no MIME sniffing, no framing, no
/// referrer. Admin responses, which hold tenant data, are never cached.
async fn security_headers(request: Request, next: Next) -> Response {
    use axum::http::header::{
        CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
        X_FRAME_OPTIONS,
    };
    let admin = request.uri().path().starts_with("/api/admin/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    // The API serves data, never documents to render; even opened directly,
    // a response runs nothing (`sandbox`).
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; sandbox"),
    );
    if admin && !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Refuses a state-changing admin request that carries the session cookie
/// but comes from another site or origin. Admin writes already need custom
/// headers, which a cross-origin page cannot send without a CORS preflight
/// the gateway never grants; this makes the rule explicit rather than a
/// side effect. Requests without a browser's `Origin`/`Sec-Fetch-Site`
/// (scripts, CLIs) are unaffected.
async fn reject_cross_site_admin_writes(
    State(origin): State<Option<String>>,
    request: Request,
    next: Next,
) -> Response {
    let unsafe_method = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let headers = request.headers();
    if unsafe_method
        && request.uri().path().starts_with("/api/admin/")
        && cookie(headers, SESSION_COOKIE).is_some()
    {
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let cross_site = header("sec-fetch-site").is_some_and(|site| site == "cross-site");
        let foreign_origin = match (header("origin"), origin.as_deref()) {
            (Some(sent), Some(expected)) => !origins_match(sent, expected),
            // A configured origin is required to accept a browser origin.
            (Some(_), None) => true,
            (None, _) => false,
        };
        if cross_site || foreign_origin {
            return error(StatusCode::FORBIDDEN, "cross_origin_request_refused");
        }
    }
    next.run(request).await
}

/// Admin routes outside any one organization that a session from an
/// organization's provider may still use: they act only on the session
/// itself, or (`organizations`, `context`) limit themselves to its
/// organization.
const UNSCOPED_ADMIN_PATHS: &[&str] = &[
    "/api/admin/v1/me",
    ME_PHOTO_PATH,
    "/api/admin/v1/organizations",
    "/api/admin/v1/context",
    "/api/admin/v1/logout",
    "/api/admin/v1/oidc/start",
    "/api/admin/v1/oidc/callback",
    "/api/admin/v1/invitations/accept",
    // Microsoft's admin-consent return: reads no session, only redirects.
    crate::transport::onboarding::ROUTE_AZURE_CONSENT,
];

/// Keeps a session from an organization's own provider, and an agent's
/// token, in that organization, for every admin route at once, so no lane
/// can forget to:
/// `/api/admin/v1/organizations/{org}/…` must name it (another answers
/// `404`, as for an organization the caller cannot see), and any other
/// admin route outside [`UNSCOPED_ADMIN_PATHS`] answers `403`. Process-wide
/// sessions, requests without a session and non-admin routes pass through
/// untouched; whether the session is valid is still each route's check.
async fn keep_sessions_in_their_organization(
    State((auth, crypto)): State<(
        crate::persistence::AuthStore,
        Option<crate::domain::auth::AuthCrypto>,
    )>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if !path.starts_with("/api/admin/") {
        return next.run(request).await;
    }
    // An organization id never needs percent-encoding, and the checks below
    // read the raw path while handlers see it decoded: refuse it outright.
    if organization_segment(path).is_some_and(|segment| segment.contains('%')) {
        return error(StatusCode::NOT_FOUND, "not_found");
    }
    // An agent token acts only in its agent's organization, and one limited
    // to a service account only on that account's end users.
    let mut service_account = None;
    let scope = if let Some(token) = crate::transport::admin_auth::agent_token(request.headers()) {
        auth.find_agent_token(token).await.map(|found| {
            found.map(|agent| {
                service_account = agent.service_account_id;
                crate::persistence::auth::SessionScope {
                    user_id: agent.agent_id,
                    organization_id: Some(agent.organization_id),
                    password: false,
                }
            })
        })
    } else {
        let (Some(crypto), Some(token)) =
            (crypto.as_ref(), cookie(request.headers(), SESSION_COOKIE))
        else {
            return next.run(request).await;
        };
        auth.session_scope(&crypto.digest(&token)).await
    };
    let scope = match scope {
        Ok(Some(scope)) => scope,
        Ok(None) => return next.run(request).await,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    // A session limited to one organization reaches only it, before
    // anything about another organization is told.
    if let Some(limited) = scope.organization_id {
        match session_may_reach(path, limited) {
            SessionReach::Allowed => {}
            SessionReach::OtherOrganization => return error(StatusCode::NOT_FOUND, "not_found"),
            SessionReach::OutsideOrganization => {
                return error(StatusCode::FORBIDDEN, "organization_session_scope");
            }
        }
    }
    if let Some(account) = service_account {
        if !service_account_token_may_reach(path, account) {
            return error(StatusCode::FORBIDDEN, "service_account_token_scope");
        }
    }
    // A suspended organization refuses its members; a deleted one is not
    // there. Anyone else learns nothing here (the route answers 404).
    if let Some(organization_id) = path_organization(path) {
        match auth
            .organization_state(organization_id, scope.user_id)
            .await
        {
            Ok(Some((_, true))) => return error(StatusCode::NOT_FOUND, "not_found"),
            Ok(Some((true, false))) => {
                return error(StatusCode::FORBIDDEN, "organization_suspended");
            }
            Ok(_) => {}
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        }
    }
    // A password session cannot act in an organization that requires its
    // own identity provider.
    if let Some(organization_id) = path_organization(path).filter(|_| scope.password) {
        match auth.password_allowed_in(organization_id).await {
            Ok(true) => {}
            Ok(false) => return error(StatusCode::FORBIDDEN, "sso_required"),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        }
    }
    next.run(request).await
}

/// Whether a token limited to `account` may reach `path`: the paths every
/// organization-limited session may reach outside an organization, and
/// `…/service-accounts/{account}/end-user-settings`, `end-user-plans` and
/// `end-users` with what is under them. Routes match whole segments, so
/// whatever follows stays within the account's own routes.
fn service_account_token_may_reach(path: &str, account: Uuid) -> bool {
    if UNSCOPED_ADMIN_PATHS.contains(&path) {
        return true;
    }
    let Some(rest) = path
        .strip_prefix("/api/admin/v1/organizations/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, rest)| rest)
    else {
        return false;
    };
    let mut segments = rest.split('/');
    segments.next() == Some("service-accounts")
        && segments
            .next()
            .filter(|segment| !segment.contains('%'))
            .and_then(|segment| Uuid::parse_str(segment).ok())
            == Some(account)
        && matches!(
            segments.next(),
            Some("end-user-settings" | "end-user-plans" | "end-users")
        )
}

/// The organization an `/api/admin/v1/organizations/{org}/…` path names,
/// if its segment is exactly an id.
fn path_organization(path: &str) -> Option<OrganizationId> {
    let segment = organization_segment(path)?;
    if segment.contains('%') {
        return None;
    }
    Uuid::parse_str(segment).ok().map(OrganizationId)
}

/// The raw `{org}` segment of an `/api/admin/v1/organizations/{org}/…` path.
fn organization_segment(path: &str) -> Option<&str> {
    path.strip_prefix("/api/admin/v1/organizations/")?
        .split('/')
        .next()
}

#[derive(Debug, Eq, PartialEq)]
enum SessionReach {
    Allowed,
    OtherOrganization,
    OutsideOrganization,
}

/// Whether a session limited to `scope` may use the admin route at `path`.
/// The organization segment must parse as exactly `scope`'s id; anything
/// odd (percent-encoded, empty, not an id) is refused rather than guessed.
fn session_may_reach(path: &str, scope: OrganizationId) -> SessionReach {
    if UNSCOPED_ADMIN_PATHS.contains(&path)
        || local::UNSCOPED_PATHS.contains(&path)
        || domains::UNSCOPED_PATHS.contains(&path)
    {
        return SessionReach::Allowed;
    }
    let Some(rest) = path.strip_prefix("/api/admin/v1/organizations/") else {
        return SessionReach::OutsideOrganization;
    };
    let segment = rest.split('/').next().unwrap_or_default();
    match Uuid::parse_str(segment) {
        Ok(id) if !segment.contains('%') && id == scope.0 => SessionReach::Allowed,
        _ => SessionReach::OtherOrganization,
    }
}

/// Records each request under its route template (never the raw path, which
/// carries organization and resource ids), and each inference request under
/// its operation and outcome.
async fn track(request: Request, next: Next) -> Response {
    use crate::metrics::{
        HTTP_DURATION, HTTP_REQUESTS, HTTP_VERSIONS, INFERENCE_REQUESTS, ProblemCode,
        global as metrics,
    };
    use crate::transport::inference::{CHAT_PATH, EMBEDDINGS_PATH, STREAM_PATH};

    let started = Instant::now();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());
    let method = match *request.method() {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    };
    let version = match request.version() {
        axum::http::Version::HTTP_2 => "HTTP/2",
        axum::http::Version::HTTP_3 => "HTTP/3",
        axum::http::Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let response = next.run(request).await;
    let status = match response.status().as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    let registry = metrics();
    registry.inc(&HTTP_REQUESTS, &[&route, method, status]);
    registry.inc(&HTTP_VERSIONS, &[version]);
    registry.observe(&HTTP_DURATION, &[&route], started.elapsed());
    let operation = match route.as_str() {
        CHAT_PATH => Some("chat"),
        STREAM_PATH => Some("chat_stream"),
        EMBEDDINGS_PATH => Some("embeddings"),
        _ => None,
    };
    if let Some(operation) = operation {
        let outcome = match response.extensions().get::<ProblemCode>() {
            Some(ProblemCode(code)) => Some(*code),
            // A started stream counts its own outcome when it ends.
            None if response.status().is_success() && operation == "chat_stream" => None,
            None if response.status().is_success() => Some("ok"),
            // A rejection the handler never saw, such as an oversized body.
            None => Some("rejected"),
        };
        if let Some(outcome) = outcome {
            registry.inc(&INFERENCE_REQUESTS, &[operation, outcome]);
        }
    }
    response
}

async fn healthz(State(_state): State<AppState>) -> impl IntoResponse {
    (StatusCode::OK, Json(HealthResponse { status: "ok" }))
}

/// Ready once configuration is loaded, and only while the configuration feed
/// was read recently, so load balancers move traffic away from a process cut
/// off from the primary.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.config().is_none_or(|config| config.ready()) {
        (StatusCode::OK, Json(HealthResponse { status: "ok" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: "unavailable",
            }),
        )
    }
}

#[derive(Debug, Deserialize)]
struct StartQuery {
    /// The slug of the organization whose own provider to sign in through.
    organization: Option<String>,
}

/// Starts a sign-in. With `?organization={slug}` of an organization whose
/// own provider is enabled, through that provider; otherwise (no
/// organization, an unknown one, or one without a provider) through the
/// process-wide provider, exactly as without it, so the answer does not say
/// which organizations exist.
async fn oidc_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StartQuery>,
) -> Response {
    let organization = query
        .organization
        .as_deref()
        .map(str::trim)
        .filter(|slug| !slug.is_empty() && slug.len() <= 255);
    let provider = match organization {
        Some(slug) => match state.identity_providers().active_for_slug(slug).await {
            Ok(configuration) => configuration.map(|configuration| {
                let binding = ProviderBinding {
                    organization_id: OrganizationId(configuration.organization_id),
                    configuration_id: configuration.id,
                    purpose: SignInPurpose::SignIn,
                    started_by: None,
                    expected_version: None,
                };
                (configuration, binding)
            }),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        },
        None => None,
    };
    match begin_login(&state, &headers, None, provider, None).await {
        Ok((authorization_url, cookies)) => {
            let mut response =
                Json(serde_json::json!({ "authorization_url": authorization_url })).into_response();
            for cookie in cookies {
                set_cookie(&mut response, &cookie);
            }
            response
        }
        Err(response) => response,
    }
}

#[derive(Debug, Deserialize)]
struct InvitationQuery {
    token: String,
    /// `sso`: go straight to the identity provider, even where a password
    /// could accept the invitation.
    #[serde(default)]
    method: Option<String>,
}

/// The invitation link: starts a sign-in carrying the invitation and sends
/// the browser to the identity provider. The callback accepts it. An
/// invitation naming the issuer of its organization's own, enabled provider
/// is accepted through that provider; any other through the process-wide
/// one (whose issuer it must then name, or acceptance fails).
async fn invitation_accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<InvitationQuery>,
) -> Response {
    // With local accounts on, the admin UI's invitation page offers the
    // choice (a password, or the provider), with the token in the fragment
    // so it reaches no server log from there on.
    if state.local_accounts_config().enabled
        && state.auth_crypto().is_some()
        && query.method.as_deref() != Some("sso")
    {
        match state.invitations().pending_target(&query.token).await {
            Ok(Some(_)) => {
                return see_other(&format!("/admin/accept-invitation#token={}", query.token));
            }
            Ok(None) => return error(StatusCode::NOT_FOUND, "invitation_invalid"),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        }
    }
    let (invitation, organization_id, issuer) =
        match state.invitations().pending_target(&query.token).await {
            Ok(Some(target)) => target,
            // Unknown, expired, revoked or used: the same answer for each.
            Ok(None) => return error(StatusCode::NOT_FOUND, "invitation_invalid"),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        };
    let provider = match state
        .identity_providers()
        .active_for_organization(organization_id)
        .await
    {
        Ok(Some(configuration)) if configuration.issuer == issuer => {
            let binding = ProviderBinding {
                organization_id,
                configuration_id: configuration.id,
                purpose: SignInPurpose::SignIn,
                started_by: None,
                expected_version: None,
            };
            Some((configuration, binding))
        }
        Ok(_) => None,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    match begin_login(&state, &headers, Some(invitation), provider, None).await {
        Ok((authorization_url, cookies)) => {
            let mut response = see_other(&authorization_url);
            for cookie in cookies {
                set_cookie(&mut response, &cookie);
            }
            response
        }
        Err(response) => response,
    }
}

/// `POST …/identity-provider/test`: starts the test sign-in that activates
/// the pending configuration. Only the owner who saved it may, from their
/// own session; the transaction is bound to that session, the configuration
/// and the provider's current version. The browser already holds its
/// session cookies, which are left as they are.
async fn identity_provider_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    use crate::transport::admin_identities::{problem, request_context};
    use crate::transport::identity_provider::provider_problem;
    let (id, _) = request_context(&headers);
    let path = ROUTE_IDENTITY_PROVIDER_TEST;
    if !csrf_valid(&state, &headers) {
        return problem(
            StatusCode::FORBIDDEN,
            "csrf_origin_or_token_required",
            "the request must come from the admin UI with its CSRF token",
            path,
            &id,
        );
    }
    let actor = match authenticated_session(&state, &headers).await {
        Ok(Some((actor, _))) => actor,
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
    let start = match identity_provider_service(&state)
        .begin_test(actor, organization_id)
        .await
    {
        Ok(start) => start,
        Err(error) => return provider_problem(error, path, &id),
    };
    let binding = ProviderBinding {
        organization_id,
        configuration_id: start.configuration.id,
        purpose: SignInPurpose::ProviderTest,
        started_by: Some(actor),
        expected_version: Some(start.version),
    };
    match begin_login(
        &state,
        &headers,
        None,
        Some((start.configuration, binding)),
        None,
    )
    .await
    {
        // The browser and CSRF cookies are those of the owner's session:
        // re-setting them would cut its lifetime short.
        Ok((authorization_url, _)) => {
            Json(serde_json::json!({ "authorization_url": authorization_url })).into_response()
        }
        Err(response) => response,
    }
}

pub(crate) fn identity_provider_service(
    state: &AppState,
) -> crate::application::identity_provider::IdentityProviderService {
    crate::application::identity_provider::IdentityProviderService::new(
        state.admin().clone(),
        state.identity_providers().clone(),
        state.credential_cipher().cloned(),
        state.oidc().issuer.clone(),
        state.oidc().redirect_uri.clone(),
    )
    .with_issuer_rule(state.organization_oidc().issuer_rule())
}

/// What a sign-in goes through: an organization's configuration and its
/// client, or (`None`) the process-wide provider.
struct OrganizationProvider {
    configuration: StoredConfiguration,
    binding: ProviderBinding,
    client: std::sync::Arc<dyn crate::domain::auth::OidcBackend>,
}

/// The client of an organization's configuration, or why there is none.
async fn organization_provider(
    state: &AppState,
    configuration: StoredConfiguration,
    binding: ProviderBinding,
) -> Result<OrganizationProvider, (StatusCode, &'static str)> {
    let Some(cipher) = state.credential_cipher() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_encryption_unavailable",
        ));
    };
    match state
        .organization_oidc()
        .client(&configuration, cipher)
        .await
    {
        Ok(client) => Ok(OrganizationProvider {
            configuration,
            binding,
            client,
        }),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_encryption_unavailable",
        )),
    }
}

/// A login transaction and the provider's authorization URL, with the
/// cookies that bind the callback to this browser. `provider` is the
/// organization configuration to go through, and what the transaction is
/// bound to; `None` is the process-wide provider.
/// Starts a login transaction and answers the provider's authorization URL
/// and the cookies to set. `login_hint` (an email address the person
/// entered) is passed on, so the provider can skip asking for it.
pub(crate) async fn begin_login(
    state: &AppState,
    headers: &HeaderMap,
    invitation_id: Option<Uuid>,
    provider: Option<(StoredConfiguration, ProviderBinding)>,
    login_hint: Option<&str>,
) -> Result<(String, Vec<String>), Response> {
    // An organization's own provider works without the process-wide one
    // (a local-accounts-only install connecting its first SSO).
    if !state.oidc().enabled && provider.is_none() {
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "oidc_disabled"));
    }
    let Some(crypto) = state.auth_crypto() else {
        return Err(error(StatusCode::SERVICE_UNAVAILABLE, "oidc_disabled"));
    };
    let organization = match provider {
        Some((configuration, binding)) => {
            match organization_provider(state, configuration, binding).await {
                Ok(provider) => Some(provider),
                Err((status, code)) => return Err(error(status, code)),
            }
        }
        None => None,
    };
    let issuer = match &organization {
        Some(provider) => provider.configuration.issuer.as_str(),
        None => {
            let Some(issuer) = state.oidc().issuer.as_deref() else {
                return Err(error(StatusCode::SERVICE_UNAVAILABLE, "oidc_disabled"));
            };
            issuer
        }
    };
    let backend = match &organization {
        Some(provider) => provider.client.as_ref(),
        None => state.oidc_backend(),
    };
    // Before anything is stored: a provider outage leaves no login
    // transaction behind.
    let endpoint = match backend.authorization_endpoint(issuer).await {
        Ok(endpoint) => endpoint,
        Err(_) => {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "oidc_provider_unavailable",
            ));
        }
    };
    let browser = cookie(headers, BROWSER_COOKIE)
        .unwrap_or_else(|| crypto.opaque_token().as_str().to_owned());
    let Ok((mut transaction, state_token, nonce, verifier)) =
        crypto.new_login_transaction(&browser, std::time::Duration::from_secs(300))
    else {
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth_crypto_failed",
        ));
    };
    transaction.invitation_id = invitation_id;
    transaction.provider = organization.as_ref().map(|provider| provider.binding);
    if state
        .auth()
        .insert_login_transaction(&transaction)
        .await
        .is_err()
    {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "auth_store_unavailable",
        ));
    }
    let redirect = state.oidc().redirect_uri.as_deref().unwrap_or_default();
    let (client, scope) = match &organization {
        Some(provider) => (
            provider.configuration.client_id.as_str(),
            std::borrow::Cow::Owned(crate::domain::identity_provider::scope(
                &provider.configuration.extra_scopes,
            )),
        ),
        None => (
            state.oidc().client_id.as_deref().unwrap_or_default(),
            std::borrow::Cow::Borrowed(oidc_scope(state.oidc().microsoft_graph_photo)),
        ),
    };
    let authorization_url = match build_authorization_url(
        endpoint,
        &scope,
        client,
        redirect,
        state_token.as_str(),
        nonce.as_str(),
        &verifier,
        login_hint,
    ) {
        Ok(url) => url.to_string(),
        Err(_) => {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "oidc_configuration_invalid",
            ));
        }
    };
    let csrf =
        cookie(headers, CSRF_COOKIE).unwrap_or_else(|| crypto.opaque_token().as_str().to_owned());
    Ok((
        authorization_url,
        vec![
            format!(
                "{BROWSER_COOKIE}={browser}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=600"
            ),
            format!("{CSRF_COOKIE}={csrf}; Path=/; Secure; SameSite=Lax; Max-Age=600"),
        ],
    ))
}

/// A `303 See Other` to `location`, which the browser follows with a GET.
pub(crate) fn see_other(location: &str) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    response
}

/// Where a test sign-in sends the owner back: the Organization screen of the
/// organization tested (its id), with the outcome as one code from a closed
/// set.
fn provider_test_result(organization_id: OrganizationId, code: &'static str) -> Response {
    see_other(&format!(
        "/admin/organization?organization={}&sign_in_test={code}",
        organization_id.0
    ))
}

async fn oidc_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if query.code.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "missing_authorization_code");
    }
    // Without the process-wide provider, only an organization's provider
    // (checked once the transaction is claimed) can have started this.
    let Some(crypto) = state.auth_crypto() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "oidc_disabled");
    };
    let Some(browser) = cookie(&headers, BROWSER_COOKIE) else {
        return error(StatusCode::UNAUTHORIZED, "browser_binding_required");
    };
    let digest = crypto.digest(&query.state);
    let binding_digest = crypto.digest(&browser);

    // This claim is the callback security boundary: no verifier decryption
    // and no backend invocation occurs until the database fences the request.
    let Some((transaction, claim_id)) = (match state
        .auth()
        .claim_login_callback(&digest, &binding_digest)
        .await
    {
        Ok(value) => value,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    }) else {
        return error(StatusCode::UNAUTHORIZED, "invalid_or_replayed_state");
    };
    let transaction_id = transaction.id;
    let test = transaction
        .provider
        .filter(|binding| binding.purpose == SignInPurpose::ProviderTest);
    // A test sign-in returns the owner to the admin UI with its outcome; a
    // sign-in answers as it always has.
    let fail = |failure: LoginFailure, status: StatusCode, code: &'static str| {
        let state = state.clone();
        async move {
            let response =
                finish_callback_failure(&state, transaction_id, claim_id, failure, status, code)
                    .await;
            match test {
                Some(binding) if response.status() == status => {
                    provider_test_result(binding.organization_id, code)
                }
                _ => response,
            }
        }
    };

    let verifier = match crypto.decrypt_verifier(&transaction.verifier_ciphertext) {
        Ok(value) => value,
        Err(_) => {
            return fail(
                LoginFailure::ClaimsRejected,
                StatusCode::UNAUTHORIZED,
                "invalid_transaction",
            )
            .await;
        }
    };
    if transaction.provider.is_none() && !state.oidc().enabled {
        return fail(
            LoginFailure::BackendUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "oidc_disabled",
        )
        .await;
    }
    let Some(redirect_uri) = state.oidc().redirect_uri.as_deref() else {
        return fail(
            LoginFailure::BackendUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            "oidc_disabled",
        )
        .await;
    };

    // Which provider the sign-in started with. An organization's is used
    // only while it is still the configuration the state was bound to.
    let organization = match transaction.provider {
        None => None,
        Some(binding) => {
            let current = match state
                .identity_providers()
                .configuration(binding.organization_id, binding.configuration_id)
                .await
            {
                Ok(current) => current,
                Err(_) => {
                    return fail(
                        LoginFailure::IdentityUnavailable,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "auth_store_unavailable",
                    )
                    .await;
                }
            };
            let Some((configuration, pointers)) =
                current.filter(|(_, pointers)| binding.is_current(pointers))
            else {
                return fail(
                    LoginFailure::ClaimsRejected,
                    StatusCode::CONFLICT,
                    "identity_provider_changed",
                )
                .await;
            };
            debug_assert_eq!(pointers.organization_id, binding.organization_id);
            // A test finishes only in the session of the owner who started it.
            if let Some(started_by) = binding.started_by {
                match authenticated_session(&state, &headers).await {
                    Ok(Some((user, _))) if user == started_by => {}
                    Ok(_) => {
                        return fail(
                            LoginFailure::ReplayOrInvalidState,
                            StatusCode::FORBIDDEN,
                            "provider_test_session_required",
                        )
                        .await;
                    }
                    Err(_) => {
                        return fail(
                            LoginFailure::IdentityUnavailable,
                            StatusCode::SERVICE_UNAVAILABLE,
                            "auth_store_unavailable",
                        )
                        .await;
                    }
                }
            }
            match organization_provider(&state, configuration, binding).await {
                Ok(provider) => Some(provider),
                Err((status, code)) => {
                    return fail(LoginFailure::BackendUnavailable, status, code).await;
                }
            }
        }
    };
    let (issuer, audience, client_id, backend) = match &organization {
        // The configured client is the only audience an organization's
        // provider is accepted for.
        Some(provider) => (
            provider.configuration.issuer.as_str(),
            provider.configuration.client_id.as_str(),
            provider.configuration.client_id.as_str(),
            provider.client.as_ref(),
        ),
        None => {
            let (Some(issuer), Some(audience), Some(client_id)) = (
                state.oidc().issuer.as_deref(),
                state.oidc().audience.as_deref(),
                state.oidc().client_id.as_deref(),
            ) else {
                return fail(
                    LoginFailure::BackendUnavailable,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "oidc_disabled",
                )
                .await;
            };
            (issuer, audience, client_id, state.oidc_backend())
        }
    };

    // Persist the exchange fence before entering application-owned network
    // code. A process interruption can therefore never make this reusable.
    match state
        .auth()
        .start_login_exchange(transaction_id, claim_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return fail(
                LoginFailure::ReplayOrInvalidState,
                StatusCode::UNAUTHORIZED,
                "invalid_or_replayed_state",
            )
            .await;
        }
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    }

    let claims = match backend
        .authenticate(
            issuer,
            audience,
            client_id,
            redirect_uri,
            &query.code,
            &verifier,
        )
        .await
    {
        Ok(claims) => claims,
        Err(OidcBackendError::NotDispatched) => {
            match state
                .auth()
                .release_login_callback_claim(transaction_id, claim_id)
                .await
            {
                Ok(true) => {
                    if let Some(binding) = test {
                        return provider_test_result(
                            binding.organization_id,
                            "oidc_backend_unavailable",
                        );
                    }
                    return error(StatusCode::SERVICE_UNAVAILABLE, "oidc_backend_unavailable");
                }
                Ok(false) => return error(StatusCode::UNAUTHORIZED, "invalid_or_replayed_state"),
                Err(_) => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "security_audit_unavailable",
                    );
                }
            }
        }
        Err(OidcBackendError::Unavailable) => {
            return fail(
                LoginFailure::BackendUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "oidc_backend_unavailable",
            )
            .await;
        }
        Err(OidcBackendError::InvalidResponse) => {
            return fail(
                LoginFailure::BackendRejected,
                StatusCode::UNAUTHORIZED,
                "oidc_backend_rejected",
            )
            .await;
        }
    };

    if !crypto.verify_digest(&claims.nonce, &transaction.nonce_digest)
        || validate_claims(&claims, issuer, audience, &claims.nonce, now()).is_err()
    {
        return fail(
            LoginFailure::ClaimsRejected,
            StatusCode::UNAUTHORIZED,
            "claim_validation_failed",
        )
        .await;
    }

    // A test sign-in proves the configuration and activates it. It signs
    // nobody in and provisions nobody.
    if let (Some(binding), Some(started_by), Some(expected_version)) = (
        test,
        test.and_then(|binding| binding.started_by),
        test.and_then(|binding| binding.expected_version),
    ) {
        use crate::application::identity_provider::{CompletedTest, IdentityProviderAdminError};
        let activated = identity_provider_service(&state)
            .activate(CompletedTest {
                actor: started_by,
                organization_id: binding.organization_id,
                configuration_id: binding.configuration_id,
                expected_version,
                login_transaction_id: transaction_id,
                claim_id,
                request_id: Uuid::new_v4(),
            })
            .await;
        return match activated {
            Ok(_) => provider_test_result(binding.organization_id, "activated"),
            Err(error) => {
                let (failure, status, code) = match error {
                    IdentityProviderAdminError::Forbidden => {
                        (LoginFailure::Disabled, StatusCode::FORBIDDEN, "forbidden")
                    }
                    IdentityProviderAdminError::Store(_)
                    | IdentityProviderAdminError::Admin(_)
                    | IdentityProviderAdminError::IdempotencyInProgress => (
                        LoginFailure::IdentityUnavailable,
                        StatusCode::SERVICE_UNAVAILABLE,
                        "auth_store_unavailable",
                    ),
                    _ => (
                        LoginFailure::ClaimsRejected,
                        StatusCode::CONFLICT,
                        "identity_provider_changed",
                    ),
                };
                if let IdentityProviderAdminError::Store(_) | IdentityProviderAdminError::Admin(_) =
                    &error
                {
                    tracing::warn!(error = ?error, "identity provider activation failed");
                }
                fail(failure, status, code).await
            }
        };
    }

    // A sign-in started from an invitation provisions the person first,
    // under the (issuer, subject) the provider just vouched for.
    if let Some(invitation_id) = transaction.invitation_id {
        use crate::persistence::invitations::InvitationError;
        let accepted = state
            .invitations()
            .accept_through(
                invitation_id,
                &claims.issuer,
                &claims.subject,
                organization
                    .as_ref()
                    .map(|provider| provider.binding.configuration_id),
            )
            .await;
        let failure = match accepted {
            Ok(_) => None,
            Err(InvitationError::ProviderChanged) => Some((
                LoginFailure::ClaimsRejected,
                StatusCode::CONFLICT,
                "identity_provider_changed",
            )),
            Err(InvitationError::Invalid | InvitationError::WrongIssuer) => Some((
                LoginFailure::Unprovisioned,
                StatusCode::FORBIDDEN,
                "invitation_invalid",
            )),
            Err(InvitationError::Provision(
                crate::persistence::bootstrap::BootstrapError::UserDisabled,
            )) => Some((
                LoginFailure::Disabled,
                StatusCode::FORBIDDEN,
                "enabled_membership_required",
            )),
            Err(InvitationError::Provision(_)) => Some((
                LoginFailure::IdentityUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "identity_store_unavailable",
            )),
        };
        if let Some((failure, status, code)) = failure {
            return finish_callback_failure(
                &state,
                transaction_id,
                claim_id,
                failure,
                status,
                code,
            )
            .await;
        }
    }

    // This controls the response; the exact predicates are revalidated
    // atomically with session creation by the success mutation below.
    let user_status = match state
        .identity()
        .oidc_user_status(&claims.issuer, &claims.subject)
        .await
    {
        Ok(status) => status,
        Err(_) => {
            return finish_callback_failure(
                &state,
                transaction_id,
                claim_id,
                LoginFailure::IdentityUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "identity_store_unavailable",
            )
            .await;
        }
    };
    let Some((user_id, enabled, has_enabled_membership)) = user_status else {
        return finish_callback_failure(
            &state,
            transaction_id,
            claim_id,
            LoginFailure::Unprovisioned,
            StatusCode::FORBIDDEN,
            "preprovisioned_membership_required",
        )
        .await;
    };
    if !enabled || !has_enabled_membership {
        return finish_callback_failure(
            &state,
            transaction_id,
            claim_id,
            LoginFailure::Disabled,
            StatusCode::FORBIDDEN,
            "enabled_membership_required",
        )
        .await;
    }

    let rotated_browser = crypto.opaque_token().as_str().to_owned();
    let session = crypto.new_session(
        user_id,
        &rotated_browser,
        std::time::Duration::from_secs(state.session_ttl_seconds()),
    );
    // A sign-in through an organization's provider gets a session for that
    // organization only, while that configuration stays active.
    let scope = organization.as_ref().map(|provider| {
        (
            provider.binding.organization_id,
            provider.binding.configuration_id,
        )
    });
    let finished = match scope {
        Some(scope) => {
            state
                .auth()
                .finish_organization_login_success_claimed(
                    transaction_id,
                    claim_id,
                    &claims.issuer,
                    &claims.subject,
                    &session,
                    scope,
                )
                .await
        }
        None => {
            state
                .auth()
                .finish_login_callback_success_claimed(
                    transaction_id,
                    claim_id,
                    &claims.issuer,
                    &claims.subject,
                    &session,
                )
                .await
        }
    };
    match finished {
        Ok(true) => {}
        Ok(false) => return error(StatusCode::FORBIDDEN, "enabled_membership_required"),
        Err(_) => {
            return finish_callback_failure(
                &state,
                transaction_id,
                claim_id,
                LoginFailure::IdentityUnavailable,
                StatusCode::SERVICE_UNAVAILABLE,
                "auth_store_unavailable",
            )
            .await;
        }
    }

    // Display details only: the person is signed in whether or not they
    // could be recorded.
    let _ = state
        .identity()
        .record_oidc_profile(user_id, &claims.profile)
        .await;

    // The provider sent the browser here: send it on to the admin UI, now
    // signed in.
    let mut response = see_other("/admin/");
    set_cookie(
        &mut response,
        &format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
            session.token,
            state.session_ttl_seconds()
        ),
    );
    set_cookie(
        &mut response,
        &format!(
            "{BROWSER_COOKIE}={rotated_browser}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
            state.session_ttl_seconds()
        ),
    );
    let csrf = crypto.opaque_token().as_str().to_owned();
    set_cookie(
        &mut response,
        &format!(
            "{CSRF_COOKIE}={csrf}; Path=/; Secure; SameSite=Lax; Max-Age={}",
            state.session_ttl_seconds()
        ),
    );
    response
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !csrf_valid(&state, &headers) {
        return error(StatusCode::FORBIDDEN, "csrf_origin_or_token_required");
    }
    let Some(crypto) = state.auth_crypto() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "oidc_disabled");
    };
    let token = cookie(&headers, SESSION_COOKIE).unwrap_or_else(|| "no-session".to_owned());
    if state
        .auth()
        .logout_session(&crypto.digest(&token))
        .await
        .is_err()
    {
        return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable");
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie(
        &mut response,
        &format!("{SESSION_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"),
    );
    set_cookie(
        &mut response,
        &format!("{BROWSER_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"),
    );
    set_cookie(
        &mut response,
        &format!("{CSRF_COOKIE}=; Path=/; Secure; SameSite=Lax; Max-Age=0"),
    );
    response
}

async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let organization_session = match authenticated_session(&state, &headers).await {
        Ok(Some((_, scope))) => scope.is_some(),
        Ok(None) => false,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    let user_id = match authenticated_user(&state, &headers).await {
        Ok(Some(user_id)) => user_id,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "authentication_required"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    let profile = match state.identity().user_profile(user_id).await {
        Ok(profile) => profile,
        Err(_) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "identity_store_unavailable",
            );
        }
    };
    let (display_name, email, has_photo) = profile
        .map(|profile| (profile.display_name, profile.email, profile.has_photo))
        .unwrap_or_default();
    let password = match session_is_password(&state, &headers).await {
        Ok(password) => password,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    let session_expires_at = match session_expires_at(&state, &headers).await {
        Ok(expires_at) => expires_at,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    let deployment_admin = match state
        .auth()
        .local_accounts()
        .is_deployment_admin(user_id)
        .await
    {
        Ok(admin) => admin,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    (
        StatusCode::OK,
        Json(MeResponse {
            user_id,
            display_name,
            email,
            photo_url: has_photo.then_some(ME_PHOTO_PATH),
            sign_in_method: if password { "password" } else { "oidc" },
            deployment_admin,
            organization_session,
            self_serve_organizations: state.local_accounts_config().self_serve_organizations,
            session_expires_at,
        }),
    )
        .into_response()
}

/// The signed-in person's photo. Like every admin response it is `no-store`
/// (one URL serves whoever is signed in, so a browser must not reuse it for
/// the next person), `nosniff` and sandboxed (see [`security_headers`]).
async fn me_photo(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user_id = match authenticated_user(&state, &headers).await {
        Ok(Some(user_id)) => user_id,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "authentication_required"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    match state.identity().user_photo(user_id).await {
        Ok(Some((content_type, bytes))) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, content_type)],
            bytes,
        )
            .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "photo_not_found"),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity_store_unavailable",
        ),
    }
}

async fn organizations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (user_id, scope) = match authenticated_session(&state, &headers).await {
        Ok(Some(session)) => session,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "authentication_required"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    let password = match session_is_password(&state, &headers).await {
        Ok(password) => password,
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    match state.identity().organizations_for_user(user_id).await {
        // A session from an organization's provider sees only that one; a
        // password session not those that require their own provider.
        Ok(values) => {
            let mut visible = Vec::with_capacity(values.len());
            for organization in values {
                if scope.is_some_and(|scope| organization.id != scope) {
                    continue;
                }
                if password {
                    match state.auth().password_allowed_in(organization.id).await {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(_) => {
                            return error(
                                StatusCode::SERVICE_UNAVAILABLE,
                                "auth_store_unavailable",
                            );
                        }
                    }
                }
                visible.push(organization);
            }
            (StatusCode::OK, Json(visible)).into_response()
        }
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity_store_unavailable",
        ),
    }
}

async fn context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ContextRequest>,
) -> Response {
    if !csrf_valid(&state, &headers) {
        return error(StatusCode::FORBIDDEN, "csrf_origin_or_token_required");
    }
    let (user_id, scope) = match authenticated_session(&state, &headers).await {
        Ok(Some(session)) => session,
        Ok(None) => return error(StatusCode::UNAUTHORIZED, "authentication_required"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    };
    if scope.is_some_and(|scope| scope != request.organization_id) {
        return error(StatusCode::FORBIDDEN, "organization_or_team_context_denied");
    }
    match session_is_password(&state, &headers).await {
        Ok(false) => {}
        Ok(true) => match state
            .auth()
            .password_allowed_in(request.organization_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => return error(StatusCode::FORBIDDEN, "sso_required"),
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
        },
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    }
    match state
        .identity()
        .human_principal(user_id, request.organization_id, request.team_id)
        .await
    {
        Ok(Some(principal)) => (StatusCode::OK, Json(principal)).into_response(),
        Ok(_) => error(StatusCode::FORBIDDEN, "organization_or_team_context_denied"),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "identity_store_unavailable",
        ),
    }
}

/// Whether the request's session cookie names a password session.
async fn session_expires_at(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<i64>, sqlx::Error> {
    let (Some(crypto), Some(token)) = (state.auth_crypto(), cookie(headers, SESSION_COOKIE)) else {
        return Ok(None);
    };
    state
        .auth()
        .session_expires_at(&crypto.digest(&token))
        .await
}

async fn session_is_password(state: &AppState, headers: &HeaderMap) -> Result<bool, sqlx::Error> {
    let (Some(crypto), Some(token)) = (state.auth_crypto(), cookie(headers, SESSION_COOKIE)) else {
        return Ok(false);
    };
    Ok(state
        .auth()
        .session_scope(&crypto.digest(&token))
        .await?
        .is_some_and(|scope| scope.password))
}

async fn authenticated_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<crate::domain::identity::UserAccountId>, sqlx::Error> {
    Ok(authenticated_session(state, headers)
        .await?
        .map(|(user_id, _)| user_id))
}

/// The signed-in user and, for a session from an organization's provider,
/// the organization it is limited to.
pub(crate) async fn authenticated_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<
    Option<(
        crate::domain::identity::UserAccountId,
        Option<OrganizationId>,
    )>,
    sqlx::Error,
> {
    crate::transport::admin_auth::scoped_admin_actor(state.auth(), state.auth_crypto(), headers)
        .await
}

async fn finish_callback_failure(
    state: &AppState,
    transaction_id: Uuid,
    claim_id: Uuid,
    failure: LoginFailure,
    intended_status: StatusCode,
    intended_code: &'static str,
) -> Response {
    match state
        .auth()
        .finish_login_callback_failure_claimed(transaction_id, claim_id, failure)
        .await
    {
        Ok(true) => error(intended_status, intended_code),
        Ok(false) => error(StatusCode::UNAUTHORIZED, "invalid_or_replayed_state"),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "security_audit_unavailable",
        ),
    }
}

pub(crate) fn csrf_valid(state: &AppState, headers: &HeaderMap) -> bool {
    if !same_origin(headers, state.oidc().origin.as_deref()) {
        return false;
    }
    let Some(crypto) = state.auth_crypto() else {
        return false;
    };
    let (Some(cookie), Some(header)) = (
        cookie(headers, CSRF_COOKIE),
        headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok()),
    ) else {
        return false;
    };
    crypto.verify_digest(header, &crypto.digest(&cookie))
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

/// `openid` identifies the person; `profile` and `email` name them for
/// display; `User.Read` lets the gateway fetch their Microsoft Graph photo.
fn oidc_scope(microsoft_graph_photo: bool) -> &'static str {
    if microsoft_graph_photo {
        "openid profile email User.Read"
    } else {
        "openid profile email"
    }
}

fn build_authorization_url(
    authorization_endpoint: Url,
    scope: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    verifier: &str,
    login_hint: Option<&str>,
) -> Result<Url, url::ParseError> {
    let mut url = authorization_endpoint;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair(
            "code_challenge",
            &crate::domain::auth::AuthCrypto::pkce_challenge(verifier),
        )
        .append_pair("code_challenge_method", "S256");
    if let Some(hint) = login_hint {
        url.query_pairs_mut().append_pair("login_hint", hint);
    }
    Ok(url)
}
pub(crate) fn set_cookie(response: &mut Response, value: &str) {
    response.headers_mut().append(
        SET_COOKIE,
        HeaderValue::from_str(value).expect("cookie value is valid"),
    );
}
pub(crate) fn same_origin(headers: &HeaderMap, configured_origin: Option<&str>) -> bool {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    configured_origin.is_some_and(|expected| origins_match(origin, expected))
}

/// Whether two origins are the same as browsers compare them: scheme, host
/// and port, with default ports and letter case normalized. Anything that
/// does not parse as a URL with a host never matches.
pub(crate) fn origins_match(sent: &str, expected: &str) -> bool {
    let origin = |value: &str| {
        Url::parse(value)
            .ok()
            .map(|url| url.origin())
            .filter(url::Origin::is_tuple)
    };
    matches!((origin(sent), origin(expected)), (Some(a), Some(b)) if a == b)
}
pub(crate) fn error(status: StatusCode, code: &'static str) -> Response {
    (status, Json(ErrorResponse { error: code })).into_response()
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    fn hardened() -> Router {
        let ok = || async { StatusCode::OK };
        harden(
            Router::new()
                .route("/api/admin/v1/thing", get(ok).post(ok))
                .route("/api/inference/v1/thing", post(ok)),
            Some("https://admin.example.test".to_owned()),
        )
    }

    async fn send(method: &str, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        hardened()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn responses_carry_security_headers_and_admin_ones_are_not_cached() {
        let response = send("GET", "/api/admin/v1/thing", &[]).await;
        let headers = response.headers();
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(
            headers["content-security-policy"],
            "default-src 'none'; frame-ancestors 'none'; sandbox"
        );
        assert_eq!(headers["cache-control"], "no-store");
        let response = send("POST", "/api/inference/v1/thing", &[]).await;
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(response.headers().get("cache-control").is_none());
    }

    #[tokio::test]
    async fn cross_site_admin_writes_with_a_session_are_refused() {
        let session = format!("{SESSION_COOKIE}=s; {BROWSER_COOKIE}=b");
        let same = "https://admin.example.test";
        for (method, headers, status) in [
            // Same origin, or no browser headers (scripts): allowed.
            (
                "POST",
                vec![("cookie", session.as_str()), ("origin", same)],
                StatusCode::OK,
            ),
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("origin", "https://admin.example.test/"),
                ],
                StatusCode::OK,
            ),
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("origin", "HTTPS://Admin.Example.Test:443"),
                ],
                StatusCode::OK,
            ),
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("origin", "https://admin.example.test:8443"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                vec![("cookie", session.as_str()), ("origin", "null")],
                StatusCode::FORBIDDEN,
            ),
            ("POST", vec![("cookie", session.as_str())], StatusCode::OK),
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("sec-fetch-site", "same-origin"),
                ],
                StatusCode::OK,
            ),
            // Another origin or site, with the session: refused.
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("origin", "https://evil.example.test"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                vec![
                    ("cookie", session.as_str()),
                    ("sec-fetch-site", "cross-site"),
                ],
                StatusCode::FORBIDDEN,
            ),
            // Reads, and requests without the session cookie, are unaffected.
            (
                "GET",
                vec![
                    ("cookie", session.as_str()),
                    ("origin", "https://evil.example.test"),
                ],
                StatusCode::OK,
            ),
            (
                "POST",
                vec![("origin", "https://evil.example.test")],
                StatusCode::OK,
            ),
        ] {
            let response = send(method, "/api/admin/v1/thing", &headers).await;
            assert_eq!(response.status(), status, "{method} {headers:?}");
        }
        // Inference uses bearer keys, never cookies.
        let response = send(
            "POST",
            "/api/inference/v1/thing",
            &[
                ("cookie", session.as_str()),
                ("origin", "https://evil.example.test"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    struct BlockingCountingBackend {
        entries: Arc<AtomicUsize>,
        release: Arc<Notify>,
        claims: crate::domain::auth::OidcClaims,
    }

    #[async_trait::async_trait]
    impl crate::domain::auth::OidcBackend for BlockingCountingBackend {
        async fn authenticate(
            &self,
            _issuer: &str,
            _audience: &str,
            _client_id: &str,
            _redirect_uri: &str,
            _code: &str,
            _verifier: &str,
        ) -> Result<crate::domain::auth::OidcClaims, OidcBackendError> {
            self.entries.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            Ok(self.claims.clone())
        }
    }

    fn callback_request(state: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri(format!(
                "/api/admin/v1/oidc/callback?state={state}&code=code"
            ))
            .header(COOKIE, format!("{BROWSER_COOKIE}=route-browser"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn concurrent_callback_route_enters_backend_once_and_audits_loser() {
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));

        let pepper = crate::domain::keys::ApiKeyPepper::from_bytes(b"route-test-pepper").unwrap();
        let identity = crate::persistence::IdentityStore::new(pool.clone(), pepper);
        let auth = crate::persistence::AuthStore::new(pool.clone());
        let crypto = crate::domain::auth::AuthCrypto::new(
            b"route-test-session-hmac-key-32-bytes",
            &[4_u8; 32],
        )
        .unwrap();
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let organization = identity
            .create_organization(&format!("route-org-{suffix}"), "Route organization")
            .await
            .unwrap();
        let user = identity
            .create_user(
                organization.id,
                Some("https://route-issuer.example"),
                Some(&format!("route-subject-{suffix}")),
                None,
                None,
            )
            .await
            .unwrap();
        identity
            .add_organization_membership(
                organization.id,
                user.id,
                crate::domain::identity::MembershipRole::Owner,
            )
            .await
            .unwrap();

        let (transaction, state_token, nonce, _) = crypto
            .new_login_transaction("route-browser", std::time::Duration::from_secs(120))
            .unwrap();
        auth.insert_login_transaction(&transaction).await.unwrap();
        let entries = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let backend = Arc::new(BlockingCountingBackend {
            entries: entries.clone(),
            release: release.clone(),
            claims: crate::domain::auth::OidcClaims {
                issuer: "https://route-issuer.example".into(),
                subject: format!("route-subject-{suffix}"),
                audience: vec!["route-client".into()],
                nonce: nonce.as_str().into(),
                expires_at: u64::MAX,
                profile: Default::default(),
            },
        });
        let state = crate::application::AppState::new(
            pool.clone(),
            identity,
            auth,
            Some(crypto),
            crate::config::OidcConfig {
                enabled: true,
                origin: Some("http://localhost".into()),
                issuer: Some("https://route-issuer.example".into()),
                audience: Some("route-client".into()),
                client_id: Some("route-client".into()),
                redirect_uri: Some("http://localhost/callback".into()),
                microsoft_graph_photo: false,
            },
            3600,
            backend,
            Arc::new(crate::provider::UnavailableProviderAdapter),
            crate::application::AccountingPolicy::for_tests(),
        );
        let replays_before: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.security_event
             WHERE user_id IS NULL AND event_type = 'login.replay_or_invalid_state'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut first =
            tokio::spawn(router(state.clone()).oneshot(callback_request(state_token.as_str())));
        for _ in 0..100 {
            if entries.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        if entries.load(Ordering::SeqCst) != 1 {
            release.notify_waiters();
            first.abort();
            panic!("blocking OIDC backend was not entered");
        }
        let loser = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            router(state).oneshot(callback_request(state_token.as_str())),
        )
        .await
        {
            Ok(result) => result.unwrap(),
            Err(_) => {
                // Wake any erroneous second backend waiter before failing.
                release.notify_waiters();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut first).await;
                panic!("concurrent loser entered blocking backend");
            }
        };
        assert_eq!(loser.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(entries.load(Ordering::SeqCst), 1);
        release.notify_waiters();
        let first_response = tokio::time::timeout(std::time::Duration::from_secs(2), &mut first)
            .await
            .expect("successful callback remained blocked")
            .unwrap()
            .unwrap();
        // Signed in: on to the admin UI.
        assert_eq!(first_response.status(), StatusCode::SEE_OTHER);
        assert_eq!(first_response.headers()["location"], "/admin/");

        let sessions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM gateway.session WHERE user_id = $1")
                .bind(user.id.0)
                .fetch_one(&pool)
                .await
                .unwrap();
        let successes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.security_event WHERE user_id = $1 AND event_type = 'login.succeeded'",
        )
        .bind(user.id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        let replays: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gateway.security_event WHERE user_id IS NULL AND event_type = 'login.replay_or_invalid_state'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(successes, 1);
        assert_eq!(replays, replays_before + 1);
        pool.close().await;
    }

    /// Returns the claims of whoever signs in, with the nonce of the login
    /// transaction it is told about.
    struct InvitedBackend {
        claims: std::sync::Mutex<Option<crate::domain::auth::OidcClaims>>,
    }

    #[async_trait::async_trait]
    impl crate::domain::auth::OidcBackend for InvitedBackend {
        async fn authenticate(
            &self,
            _issuer: &str,
            _audience: &str,
            _client_id: &str,
            _redirect_uri: &str,
            _code: &str,
            _verifier: &str,
        ) -> Result<crate::domain::auth::OidcClaims, OidcBackendError> {
            self.claims
                .lock()
                .unwrap()
                .clone()
                .ok_or(OidcBackendError::Unavailable)
        }
    }

    #[tokio::test]
    async fn an_invitation_link_signs_the_invitee_in_and_provisions_them() {
        let database_url = match std::env::var("GATEWAY_TEST_DATABASE_URL") {
            Ok(value) => value,
            Err(_)
                if std::env::var_os("CI").is_some()
                    || std::env::var_os("GITHUB_ACTIONS").is_some() =>
            {
                panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
            }
            Err(_) => return,
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .unwrap();
        crate::persistence::migrations::run(&pool).await.unwrap();
        let issuer = "https://invite-issuer.example";
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let invitations = crate::persistence::invitations::InvitationStore::new(pool.clone());
        let (_, organization, token) = invitations
            .create(
                &format!("invite-route-{}", &suffix[..12]),
                None,
                crate::domain::identity::MembershipRole::Admin,
                issuer,
                chrono::Duration::hours(1),
                None,
            )
            .await
            .unwrap();
        let backend = Arc::new(InvitedBackend {
            claims: std::sync::Mutex::new(None),
        });
        let crypto = crate::domain::auth::AuthCrypto::new(
            b"route-test-session-hmac-key-32-bytes",
            &[4_u8; 32],
        )
        .unwrap();
        let state = crate::application::AppState::new(
            pool.clone(),
            crate::persistence::IdentityStore::new(
                pool.clone(),
                crate::domain::keys::ApiKeyPepper::from_bytes(b"route-test-pepper").unwrap(),
            ),
            crate::persistence::AuthStore::new(pool.clone()),
            Some(crypto),
            crate::config::OidcConfig {
                enabled: true,
                origin: Some("http://localhost".into()),
                issuer: Some(issuer.into()),
                audience: Some("route-client".into()),
                client_id: Some("route-client".into()),
                redirect_uri: Some("http://localhost/callback".into()),
                microsoft_graph_photo: false,
            },
            3600,
            backend.clone(),
            Arc::new(crate::provider::UnavailableProviderAdapter),
            crate::application::AccountingPolicy::for_tests(),
        );
        let get = |uri: String, cookie: Option<String>| {
            let mut request = axum::http::Request::builder().uri(uri);
            if let Some(cookie) = cookie {
                request = request.header(COOKIE, cookie);
            }
            router(state.clone()).oneshot(request.body(Body::empty()).unwrap())
        };

        // A bad or unknown token: nothing started.
        for bad in ["nope".to_owned(), "x".repeat(43)] {
            let response = get(
                format!("/api/admin/v1/invitations/accept?token={bad}"),
                None,
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{bad}");
        }

        // The link sends the browser to the provider, bound to it.
        let response = get(
            format!("/api/admin/v1/invitations/accept?token={}", token.as_str()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let authorize = Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
        assert!(
            authorize
                .as_str()
                .starts_with("https://invite-issuer.example/authorize?")
        );
        let browser = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| value.strip_prefix(&format!("{BROWSER_COOKIE}=")))
            .and_then(|value| value.split(';').next())
            .unwrap()
            .to_owned();
        let param = |name: &str| {
            authorize
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        let (state_token, nonce) = (param("state"), param("nonce"));

        // The provider vouches for a person the gateway has never seen.
        let subject = format!("invitee-{suffix}");
        *backend.claims.lock().unwrap() = Some(crate::domain::auth::OidcClaims {
            issuer: issuer.into(),
            subject: subject.clone(),
            audience: vec!["route-client".into()],
            nonce,
            expires_at: u64::MAX,
            profile: crate::domain::auth::OidcProfile {
                name: Some("Ada Lovelace".into()),
                email: Some("ada@example.com".into()),
                photo: crate::domain::auth::ProfilePhoto::Present {
                    content_type: "image/png".into(),
                    bytes: b"\x89PNG\r\n\x1a\nrest".to_vec(),
                },
            },
        });
        let response = get(
            format!("/api/admin/v1/oidc/callback?state={state_token}&code=code"),
            Some(format!("{BROWSER_COOKIE}={browser}")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/");
        assert!(
            response
                .headers()
                .get_all(SET_COOKIE)
                .iter()
                .any(|value| value.to_str().unwrap().starts_with(SESSION_COOKIE))
        );
        let cookies = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok()?.split(';').next())
            .collect::<Vec<_>>()
            .join("; ");
        let role: String = sqlx::query_scalar(
            "SELECT b.role FROM gateway.admin_role_binding b
             JOIN gateway.user_account u ON u.id = b.user_id
             WHERE b.organization_id = $1 AND u.oidc_issuer = $2 AND u.oidc_subject = $3
               AND b.enabled",
        )
        .bind(organization.0)
        .bind(issuer)
        .bind(&subject)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "organization_admin");

        // The admin UI can show who signed in, and their photo.
        let me = get("/api/admin/v1/me".into(), Some(cookies.clone()))
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
        let me: serde_json::Value =
            serde_json::from_slice(&me.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(me["display_name"], "Ada Lovelace");
        assert_eq!(me["email"], "ada@example.com");
        assert_eq!(me["photo_url"], ME_PHOTO_PATH);
        let photo = get(ME_PHOTO_PATH.into(), Some(cookies)).await.unwrap();
        assert_eq!(photo.status(), StatusCode::OK);
        assert_eq!(photo.headers()["content-type"], "image/png");
        assert_eq!(photo.headers()["x-content-type-options"], "nosniff");
        assert_eq!(photo.headers()["cache-control"], "no-store");
        assert!(
            photo.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .ends_with("; sandbox")
        );
        assert_eq!(
            &photo.into_body().collect().await.unwrap().to_bytes()[..],
            b"\x89PNG\r\n\x1a\nrest"
        );
        // Not without a session.
        let anonymous = get(ME_PHOTO_PATH.into(), None).await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        // The link is spent.
        let response = get(
            format!("/api/admin/v1/invitations/accept?token={}", token.as_str()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        pool.close().await;
    }

    #[tokio::test]
    async fn health_endpoint_is_non_sensitive_and_does_not_need_database() {
        let response = router(AppState::for_test())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn logout_requires_origin_and_csrf_token_before_touching_storage() {
        let response = router(AppState::for_test())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/admin/v1/logout")
                    .header("origin", "https://gateway.example")
                    .header("host", "gateway.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_session_store_failure_is_service_unavailable() {
        let state = AppState::for_test();
        let token = state.auth_crypto().unwrap().opaque_token();
        let browser = state.auth_crypto().unwrap().opaque_token();
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/admin/v1/me")
                    .header(
                        COOKIE,
                        format!(
                            "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                            token.as_str(),
                            browser.as_str()
                        ),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn a_service_account_token_reaches_only_that_accounts_end_users() {
        let account = Uuid::new_v4();
        let org = Uuid::new_v4();
        let base = format!("/api/admin/v1/organizations/{org}/service-accounts/{account}");
        for path in [
            format!("{base}/end-user-settings"),
            format!("{base}/end-user-plans"),
            format!("{base}/end-user-plans/free"),
            format!("{base}/end-users"),
            format!("{base}/end-users/user%2F7/credit-grants"),
            "/api/admin/v1/me".to_owned(),
            "/api/admin/v1/context".to_owned(),
        ] {
            assert!(service_account_token_may_reach(&path, account), "{path}");
        }
        let other = Uuid::new_v4();
        let encoded = account.to_string().replacen('-', "%2D", 1);
        for path in [
            base.clone(),
            format!("{base}/keys"),
            format!("{base}/end-users-export"),
            format!("/api/admin/v1/organizations/{org}/service-accounts/{other}/end-users"),
            format!("/api/admin/v1/organizations/{org}/service-accounts/{encoded}/end-users"),
            format!("/api/admin/v1/organizations/{org}/service-accounts"),
            format!("/api/admin/v1/organizations/{org}/budgets"),
            format!("/api/admin/v1/organizations/{org}/agents"),
            format!("/api/admin/v1/organizations/{org}"),
            "/api/admin/v1/deployment".to_owned(),
        ] {
            assert!(!service_account_token_may_reach(&path, account), "{path}");
        }
    }

    #[test]
    fn organization_sessions_reach_only_their_organization() {
        let scope = OrganizationId(Uuid::new_v4());
        let other = Uuid::new_v4();
        let own = scope.0;
        for (path, reach) in [
            (
                format!("/api/admin/v1/organizations/{own}/members"),
                SessionReach::Allowed,
            ),
            (
                format!("/api/admin/v1/organizations/{own}"),
                SessionReach::Allowed,
            ),
            (
                format!("/api/admin/v1/organizations/{own}/identity-provider/test"),
                SessionReach::Allowed,
            ),
            // The same id in another form parses the same way axum's does.
            (
                format!("/api/admin/v1/organizations/{}/teams", own.simple()),
                SessionReach::Allowed,
            ),
            (
                format!("/api/admin/v1/organizations/{other}/members"),
                SessionReach::OtherOrganization,
            ),
            (
                format!(
                    "/api/admin/v1/organizations/%{}/members",
                    &own.to_string()[1..]
                ),
                SessionReach::OtherOrganization,
            ),
            (
                "/api/admin/v1/organizations//members".to_owned(),
                SessionReach::OtherOrganization,
            ),
            ("/api/admin/v1/me".to_owned(), SessionReach::Allowed),
            (
                "/api/admin/v1/organizations".to_owned(),
                SessionReach::Allowed,
            ),
            ("/api/admin/v1/context".to_owned(), SessionReach::Allowed),
            ("/api/admin/v1/logout".to_owned(), SessionReach::Allowed),
            // Anything else outside an organization, now or added later.
            (
                "/api/admin/v1/some-future-route".to_owned(),
                SessionReach::OutsideOrganization,
            ),
            (
                "/api/admin/v1/me/".to_owned(),
                SessionReach::OutsideOrganization,
            ),
        ] {
            assert_eq!(session_may_reach(&path, scope), reach, "{path}");
        }
    }

    #[test]
    fn csrf_requires_same_origin() {
        let mut headers = HeaderMap::new();
        assert!(!same_origin(&headers, Some("https://gateway.example")));
        headers.insert(ORIGIN, HeaderValue::from_static("https://evil.example"));
        headers.insert("host", HeaderValue::from_static("gateway.example"));
        assert!(!same_origin(&headers, Some("https://gateway.example")));
        headers.insert(ORIGIN, HeaderValue::from_static("https://gateway.example"));
        assert!(same_origin(&headers, Some("https://gateway.example")));
    }

    #[test]
    fn oidc_authorization_requests_openid_scope() {
        let url = build_authorization_url(
            Url::parse("https://login.issuer.example/oauth2/v2/authorize?tenant=t").unwrap(),
            oidc_scope(false),
            "gateway-client",
            "https://gateway.example/callback",
            "state",
            "nonce",
            "verifier",
            None,
        )
        .unwrap();

        assert_eq!(
            url.query_pairs()
                .find_map(|(key, value)| (key == "scope").then(|| value.into_owned())),
            Some("openid profile email".to_owned())
        );
        assert!(!url.query_pairs().any(|(key, _)| key == "login_hint"));
        // An email the person entered goes along as the login hint.
        let hinted = build_authorization_url(
            Url::parse("https://login.issuer.example/authorize").unwrap(),
            oidc_scope(false),
            "gateway-client",
            "https://gateway.example/callback",
            "state",
            "nonce",
            "verifier",
            Some("ada+test@example.com"),
        )
        .unwrap();
        assert_eq!(
            hinted
                .query_pairs()
                .find_map(|(key, value)| (key == "login_hint").then(|| value.into_owned())),
            Some("ada+test@example.com".to_owned())
        );
        assert_eq!(oidc_scope(true), "openid profile email User.Read");
        // The provider's endpoint and its own query are kept.
        assert_eq!(url.path(), "/oauth2/v2/authorize");
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "tenant" && value == "t")
        );
    }

    #[tokio::test]
    async fn merged_admin_routes_are_registered_and_never_fall_through() {
        // The organization/access and service-identity routers must be merged
        // into the primary admin router. With the opaque test pool the auth
        // store is unreachable, so these routes must resolve to an
        // authentication/service failure (401/503), never 404.
        let state = AppState::for_test();
        let org = "00000000-0000-0000-0000-000000000000";
        for path in [
            format!("/api/admin/v1/organizations/{org}/teams"),
            format!("/api/admin/v1/organizations/{org}/members"),
            format!("/api/admin/v1/organizations/{org}/role-bindings"),
            format!("/api/admin/v1/organizations/{org}/service-accounts"),
        ] {
            let response = router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .uri(&path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(
                response.status() != StatusCode::NOT_FOUND,
                "merged admin route {path} must be reachable"
            );
        }
    }
}
