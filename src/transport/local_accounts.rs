//! HTTP routes of local accounts: first-run setup, password sign-in,
//! changing and resetting a password, accepting an invitation with a
//! password, owners' reset links and the "Require SSO" setting. See
//! `openapi/admin-local-accounts-v1.json`.
//!
//! Routes that work without a session (setup, sign-in, reset, invitation)
//! accept a browser request only from the gateway's own origin (`Origin`
//! must match `GATEWAY_ORIGIN`), so another site cannot sign a visitor in to
//! an account of its choosing. Failed passwords are limited per account
//! (stored backoff) and per client address (in this process).

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, FromRequestParts, Path, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    application::AppState,
    domain::{
        auth::{SecurityEvent, SessionRecord},
        identity::{MembershipRole, OrganizationId, UserAccountId},
        password::{
            PasswordRejection, check_password, hash_password, normalize_email, verify_nothing,
            verify_password,
        },
    },
    persistence::local_accounts::{
        AcceptingAccount, PasswordAcceptError, PasswordAcceptance, PasswordSignIn, ResetRefusal,
        Setup, SetupError,
    },
    transport::{
        admin_identities::{problem, request_context},
        http::{
            BROWSER_COOKIE, CSRF_COOKIE, SESSION_COOKIE, authenticated_session, cookie, csrf_valid,
            same_origin, set_cookie,
        },
    },
};

pub const ROUTE_SIGN_IN_OPTIONS: &str = "/api/admin/v1/sign-in/options";
pub const ROUTE_SETUP: &str = "/api/admin/v1/setup";
pub const ROUTE_SETUP_CHECK: &str = "/api/admin/v1/setup/check";
pub const ROUTE_PASSWORD_SIGN_IN: &str = "/api/admin/v1/password/sign-in";
pub const ROUTE_PASSWORD_RESET: &str = "/api/admin/v1/password/reset";
pub const ROUTE_PASSWORD_RESET_CHECK: &str = "/api/admin/v1/password/reset/check";
pub const ROUTE_ME_PASSWORD: &str = "/api/admin/v1/me/password";
pub const ROUTE_INVITATION_INSPECT: &str = "/api/admin/v1/invitations/inspect";
pub const ROUTE_INVITATION_ACCEPT_PASSWORD: &str = "/api/admin/v1/invitations/accept-password";
pub const ROUTE_MEMBER_PASSWORD_RESET: &str =
    "/api/admin/v1/organizations/{organization_id}/members/{user_id}/password-reset-link";
pub const ROUTE_SIGN_IN_POLICY: &str =
    "/api/admin/v1/organizations/{organization_id}/sign-in-policy";

/// Routes outside any organization, for the router's session-scope check.
pub const UNSCOPED_PATHS: &[&str] = &[
    ROUTE_SIGN_IN_OPTIONS,
    ROUTE_SETUP,
    ROUTE_SETUP_CHECK,
    ROUTE_PASSWORD_SIGN_IN,
    ROUTE_PASSWORD_RESET,
    ROUTE_PASSWORD_RESET_CHECK,
    ROUTE_ME_PASSWORD,
    ROUTE_INVITATION_INSPECT,
    ROUTE_INVITATION_ACCEPT_PASSWORD,
];

/// How long a setup or reset link works.
pub const LINK_VALID_HOURS: i64 = 24;

/// Failed passwords one client address may send per window.
const ADDRESS_FAILURES: u32 = 20;
const ADDRESS_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Addresses tracked at once; the oldest windows are dropped beyond it.
const ADDRESS_CAPACITY: usize = 10_000;

/// Failed password attempts per client address, in this process.
#[derive(Debug, Default)]
pub struct AttemptLimiter {
    windows: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl AttemptLimiter {
    /// How long `address` must wait, if it has failed too often lately.
    /// IPv6 addresses count by their /64 (D-026).
    pub fn retry_after(&self, address: IpAddr) -> Option<Duration> {
        let address = crate::net::abuse_key(address);
        let windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (started, failures) = windows.get(&address)?;
        let elapsed = started.elapsed();
        (failures >= &ADDRESS_FAILURES && elapsed < ADDRESS_WINDOW)
            .then(|| ADDRESS_WINDOW - elapsed)
    }

    /// One more failure from `address`.
    pub fn fail(&self, address: IpAddr) {
        let address = crate::net::abuse_key(address);
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if windows.len() >= ADDRESS_CAPACITY && !windows.contains_key(&address) {
            windows.retain(|_, (started, _)| started.elapsed() < ADDRESS_WINDOW);
            if windows.len() >= ADDRESS_CAPACITY {
                let oldest = windows
                    .iter()
                    .min_by_key(|(_, (started, _))| *started)
                    .map(|(address, _)| *address);
                if let Some(oldest) = oldest {
                    windows.remove(&oldest);
                }
            }
        }
        let entry = windows.entry(address).or_insert((Instant::now(), 0));
        if entry.0.elapsed() >= ADDRESS_WINDOW {
            *entry = (Instant::now(), 0);
        }
        entry.1 = entry.1.saturating_add(1);
    }
}

/// The client's address, canonical (IPv4 mapped into IPv6 is IPv4): the
/// last `X-Forwarded-For` entry when the gateway is configured to trust its
/// proxy and the request came through it (not on the TLS or QUIC listener,
/// which clients reach directly), else the peer. Unknown (no peer, as in
/// tests) is one shared address.
pub struct ClientAddress(pub IpAddr);

impl FromRequestParts<AppState> for ClientAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let proxied = parts
            .extensions
            .get::<crate::listeners::DirectConnection>()
            .is_none();
        let forwarded = (proxied && state.local_accounts_config().trust_forwarded_for)
            .then(|| {
                // The entry the trusted proxy appended; earlier ones are
                // the client's own claims.
                let last = parts
                    .headers
                    .get_all("x-forwarded-for")
                    .iter()
                    .next_back()?;
                crate::net::parse_forwarded_entry(last.to_str().ok()?.rsplit(',').next()?)
            })
            .flatten();
        // A dual-stack listener reports IPv4 peers as `::ffff:a.b.c.d`.
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| crate::net::canonical(address.ip()));
        Ok(Self(
            forwarded
                .or(peer)
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ))
    }
}

#[derive(Debug, Serialize)]
struct SignInOptions {
    /// People may sign in with an email address and a password.
    password: bool,
    /// The gateway's own OpenID Connect provider is configured.
    oidc: bool,
    /// No organization exists yet: first-run setup is waiting (the link is
    /// in the gateway's log).
    setup_required: bool,
}

/// `GET /sign-in/options`: what the sign-in page offers.
pub async fn sign_in_options(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let (id, _) = request_context(&headers);
    let password = local_enabled(&state);
    let setup_required = if password {
        match state.auth().local_accounts().any_organization().await {
            Ok(any) => !any,
            Err(_) => return unavailable(ROUTE_SIGN_IN_OPTIONS, &id),
        }
    } else {
        false
    };
    Json(SignInOptions {
        password,
        oidc: state.oidc().enabled,
        setup_required,
    })
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenBody {
    token: Zeroizing<String>,
}

/// `POST /setup/check`: whether a setup link still works, so the setup page
/// can say so before the form is filled in.
pub async fn setup_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<TokenBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SETUP_CHECK;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    match state
        .auth()
        .local_accounts()
        .setup_link_valid(&body.token)
        .await
    {
        Ok(valid) => Json(serde_json::json!({ "valid": valid })).into_response(),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupBody {
    token: Zeroizing<String>,
    organization_name: String,
    organization_slug: String,
    email: String,
    #[serde(default)]
    display_name: Option<String>,
    password: Zeroizing<String>,
}

/// `POST /setup`: the first organization and its owner, signed in.
pub async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SetupBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SETUP;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let name = body.organization_name.trim();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        return invalid(
            path,
            &id,
            "the organization name must have 1 to 200 characters",
        );
    }
    let slug = body.organization_slug.trim();
    if !crate::application::organization_slug_ok(slug) {
        return invalid(
            path,
            &id,
            "the short name must be 1 to 64 lowercase letters, digits and hyphens, not starting with a hyphen",
        );
    }
    let Some(email) = normalize_email(&body.email) else {
        return invalid(path, &id, "the email address is not valid");
    };
    let display_name = match display_name(body.display_name.as_deref()) {
        Ok(name) => name,
        Err(()) => return invalid(path, &id, "the name must have at most 200 characters"),
    };
    if let Err(rejection) = check_password(&body.password, &email) {
        return weak_password(rejection, path, &id);
    }
    let store = state.auth().local_accounts();
    match store.setup_link_valid(&body.token).await {
        Ok(true) => {}
        Ok(false) => return setup_link_invalid(&store, path, &id).await,
        Err(_) => return unavailable(path, &id),
    }
    let Ok(hash) = hash(body.password.clone()).await else {
        return unavailable(path, &id);
    };
    let Some((session, browser)) = new_session(&state) else {
        return unavailable(path, &id);
    };
    let setup = Setup {
        token: &body.token,
        organization_slug: slug,
        organization_name: name,
        email: &email,
        display_name: display_name.as_deref(),
        password_hash: &hash,
    };
    match store.complete_setup(&setup, &session).await {
        Ok(done) => signed_in(
            &state,
            &session,
            &browser,
            StatusCode::CREATED,
            serde_json::json!({
                "organization_id": done.organization_id,
                "user_id": done.user_id,
            }),
        ),
        Err(SetupError::InvalidLink) => setup_link_invalid(&store, path, &id).await,
        Err(SetupError::AlreadyDone) => problem(
            StatusCode::CONFLICT,
            "setup_already_done",
            "the gateway is already set up; sign in instead",
            path,
            &id,
        ),
        Err(SetupError::Store(error)) => {
            tracing::warn!(error = ?error, "first-run setup failed");
            unavailable(path, &id)
        }
    }
}

async fn setup_link_invalid(
    store: &crate::persistence::local_accounts::LocalAccountStore,
    path: &str,
    id: &str,
) -> Response {
    if store.any_organization().await.unwrap_or(false) {
        return problem(
            StatusCode::CONFLICT,
            "setup_already_done",
            "the gateway is already set up; sign in instead",
            path,
            id,
        );
    }
    problem(
        StatusCode::NOT_FOUND,
        "setup_link_invalid",
        "the setup link is not valid or has expired; restart the gateway or run `gateway setup-link` for a new one",
        path,
        id,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignInBody {
    email: String,
    password: Zeroizing<String>,
}

/// `POST /password/sign-in`.
pub async fn password_sign_in(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    headers: HeaderMap,
    body: Result<Json<SignInBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PASSWORD_SIGN_IN;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    if let Some(wait) = state.sign_in_limiter().retry_after(address) {
        return too_many_attempts(wait, path, &id);
    }
    let store = state.auth().local_accounts();
    let email = normalize_email(&body.email);
    let credential = match &email {
        Some(email) => match store.credential(email).await {
            Ok(credential) => credential,
            Err(_) => return unavailable(path, &id),
        },
        None => None,
    };
    let Some(credential) = credential else {
        // The same work and answer as a wrong password.
        let password = body.password.clone();
        if password_work(move || verify_nothing(&password))
            .await
            .is_err()
        {
            return busy(path, &id);
        }
        state.sign_in_limiter().fail(address);
        return invalid_credentials(path, &id);
    };
    if credential.locked_at(chrono::Utc::now()) {
        let _ = store.record_locked(credential.user_id).await;
        let wait = credential
            .locked_until
            .and_then(|until| (until - chrono::Utc::now()).to_std().ok())
            .unwrap_or(Duration::from_secs(30));
        return too_many_attempts(wait, path, &id);
    }
    let Ok(matches) = verify(credential.password_hash.clone(), body.password.clone()).await else {
        return busy(path, &id);
    };
    if !matches {
        state.sign_in_limiter().fail(address);
        if store.record_failure(credential.user_id).await.is_err() {
            return unavailable(path, &id);
        }
        return invalid_credentials(path, &id);
    }
    let Some((session, browser)) = new_session(&state) else {
        return unavailable(path, &id);
    };
    match store
        .finish_sign_in(credential.user_id, &credential.password_hash, &session)
        .await
    {
        Ok(PasswordSignIn::SignedIn) => signed_in(
            &state,
            &session,
            &browser,
            StatusCode::OK,
            serde_json::json!({ "user_id": credential.user_id }),
        ),
        Ok(PasswordSignIn::NoMembership) => problem(
            StatusCode::FORBIDDEN,
            "enabled_membership_required",
            "this account is disabled or belongs to no organization",
            path,
            &id,
        ),
        Ok(PasswordSignIn::SsoRequired) => problem(
            StatusCode::FORBIDDEN,
            "sso_required",
            "your organization requires you to sign in with its identity provider",
            path,
            &id,
        ),
        // The password changed while it was checked.
        Ok(PasswordSignIn::Stale) => invalid_credentials(path, &id),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePasswordBody {
    current_password: Zeroizing<String>,
    new_password: Zeroizing<String>,
}

/// `POST /me/password`: a signed-in local account changes its password;
/// its other sessions end.
pub async fn change_password(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    headers: HeaderMap,
    body: Result<Json<ChangePasswordBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ME_PASSWORD;
    if !local_enabled(&state) {
        return disabled(path, &id);
    }
    if !csrf_valid(&state, &headers) {
        return problem(
            StatusCode::FORBIDDEN,
            "csrf_origin_or_token_required",
            "the request must come from the admin UI with its CSRF token",
            path,
            &id,
        );
    }
    let user_id = match human_session(&state, &headers).await {
        Ok(Some(user_id)) => user_id,
        Ok(None) => return unauthenticated(path, &id),
        Err(()) => return unavailable(path, &id),
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    if let Some(wait) = state.sign_in_limiter().retry_after(address) {
        return too_many_attempts(wait, path, &id);
    }
    let store = state.auth().local_accounts();
    let credential = match store.credential_of(user_id).await {
        Ok(Some(credential)) => credential,
        Ok(None) => {
            return problem(
                StatusCode::CONFLICT,
                "not_a_local_account",
                "this account signs in with an identity provider, which keeps its password",
                path,
                &id,
            );
        }
        Err(_) => return unavailable(path, &id),
    };
    let Ok(matches) = verify(
        credential.password_hash.clone(),
        body.current_password.clone(),
    )
    .await
    else {
        return busy(path, &id);
    };
    if !matches {
        state.sign_in_limiter().fail(address);
        let _ = store.record_failure(user_id).await;
        return problem(
            StatusCode::FORBIDDEN,
            "current_password_incorrect",
            "the current password is not correct",
            path,
            &id,
        );
    }
    if let Err(rejection) = check_password(&body.new_password, &credential.email) {
        return weak_password(rejection, path, &id);
    }
    let Ok(hash) = hash(body.new_password.clone()).await else {
        return unavailable(path, &id);
    };
    let current = match current_session_id(&state, &headers).await {
        Ok(current) => current,
        Err(()) => return unavailable(path, &id),
    };
    match store
        .change_password(
            user_id,
            &credential.password_hash,
            &hash,
            current,
            SecurityEvent::PasswordChanged,
        )
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // Reset or changed elsewhere since the current password was checked.
        Ok(false) => problem(
            StatusCode::FORBIDDEN,
            "current_password_incorrect",
            "the password changed meanwhile; enter the current one",
            path,
            &id,
        ),
        Err(_) => unavailable(path, &id),
    }
}

/// `POST /password/reset/check`: whether a reset link still works, and the
/// email address it is for.
pub async fn reset_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<TokenBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PASSWORD_RESET_CHECK;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    match state
        .auth()
        .local_accounts()
        .reset_link_target(&body.token)
        .await
    {
        Ok(Some((_, email))) => {
            Json(serde_json::json!({ "valid": true, "email": email })).into_response()
        }
        Ok(None) => Json(serde_json::json!({ "valid": false, "email": null })).into_response(),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetBody {
    token: Zeroizing<String>,
    password: Zeroizing<String>,
}

/// `POST /password/reset`: sets a new password with a reset link. Every
/// session of the account ends; the person then signs in.
pub async fn reset_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ResetBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_PASSWORD_RESET;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let store = state.auth().local_accounts();
    let email = match store.reset_link_target(&body.token).await {
        Ok(Some((_, email))) => email,
        Ok(None) => return reset_link_invalid(path, &id),
        Err(_) => return unavailable(path, &id),
    };
    if let Err(rejection) = check_password(&body.password, &email) {
        return weak_password(rejection, path, &id);
    }
    let Ok(hash) = hash(body.password.clone()).await else {
        return unavailable(path, &id);
    };
    match store.reset_with_link(&body.token, &hash).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => reset_link_invalid(path, &id),
        Err(_) => unavailable(path, &id),
    }
}

fn reset_link_invalid(path: &str, id: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "reset_link_invalid",
        "the reset link is not valid, was used already or has expired; ask for a new one",
        path,
        id,
    )
}

/// `POST /invitations/inspect`: what an invitation offers its holder.
pub async fn invitation_inspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<TokenBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_INVITATION_INSPECT;
    if let Err(response) = origin_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let offer = match state
        .auth()
        .local_accounts()
        .invitation_offer(&body.token)
        .await
    {
        Ok(Some(offer)) => offer,
        Ok(None) => return invitation_invalid(path, &id),
        Err(_) => return unavailable(path, &id),
    };
    let organization_issuer = match state
        .identity_providers()
        .active_for_organization(offer.organization_id)
        .await
    {
        Ok(configuration) => configuration.map(|configuration| configuration.issuer),
        Err(_) => return unavailable(path, &id),
    };
    let sso = organization_issuer.as_deref() == Some(offer.issuer.as_str())
        || (state.oidc().enabled && state.oidc().issuer.as_deref() == Some(offer.issuer.as_str()));
    Json(serde_json::json!({
        "organization_name": offer.organization_name,
        "role": offer.role,
        "password": offer.password && local_enabled(&state),
        "sso": sso,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptBody {
    token: Zeroizing<String>,
    email: String,
    password: Zeroizing<String>,
    #[serde(default)]
    display_name: Option<String>,
}

/// `POST /invitations/accept-password`: accepts an invitation with a local
/// account, new (with this password) or existing (whose password this is),
/// and signs the person in.
pub async fn invitation_accept_password(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    headers: HeaderMap,
    body: Result<Json<AcceptBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_INVITATION_ACCEPT_PASSWORD;
    if let Err(response) = browser_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    if let Some(wait) = state.sign_in_limiter().retry_after(address) {
        return too_many_attempts(wait, path, &id);
    }
    let Some(email) = normalize_email(&body.email) else {
        return invalid(path, &id, "the email address is not valid");
    };
    let display_name = match display_name(body.display_name.as_deref()) {
        Ok(name) => name,
        Err(()) => return invalid(path, &id, "the name must have at most 200 characters"),
    };
    let store = state.auth().local_accounts();
    match store.invitation_offer(&body.token).await {
        Ok(Some(offer)) if offer.password => {}
        Ok(_) => return invitation_invalid(path, &id),
        Err(_) => return unavailable(path, &id),
    }
    let existing = match store.credential(&email).await {
        Ok(existing) => existing,
        Err(_) => return unavailable(path, &id),
    };
    let new_hash;
    let account = match &existing {
        Some(credential) => {
            if credential.locked_at(chrono::Utc::now()) {
                let _ = store.record_locked(credential.user_id).await;
                return too_many_attempts(Duration::from_secs(30), path, &id);
            }
            let Ok(matches) = verify(credential.password_hash.clone(), body.password.clone()).await
            else {
                return busy(path, &id);
            };
            if !matches {
                state.sign_in_limiter().fail(address);
                let _ = store.record_failure(credential.user_id).await;
                return problem(
                    StatusCode::UNAUTHORIZED,
                    "invalid_credentials",
                    "an account with this email address exists; enter its password",
                    path,
                    &id,
                );
            }
            AcceptingAccount::Existing {
                user_id: credential.user_id,
                verified_hash: &credential.password_hash,
            }
        }
        None => {
            if let Err(rejection) = check_password(&body.password, &email) {
                return weak_password(rejection, path, &id);
            }
            new_hash = match hash(body.password.clone()).await {
                Ok(hash) => hash,
                Err(()) => return unavailable(path, &id),
            };
            AcceptingAccount::New {
                password_hash: &new_hash,
            }
        }
    };
    let Some((record, browser)) = new_session(&state) else {
        return unavailable(path, &id);
    };
    let acceptance = PasswordAcceptance {
        invitation_token: &body.token,
        email: &email,
        display_name: display_name.as_deref(),
    };
    match store.accept_invitation(&acceptance, account, &record).await {
        Ok(user_id) => signed_in(
            &state,
            &record,
            &browser,
            StatusCode::OK,
            serde_json::json!({ "user_id": user_id }),
        ),
        Err(PasswordAcceptError::Invalid) => invitation_invalid(path, &id),
        Err(PasswordAcceptError::AccountExists) => problem(
            StatusCode::CONFLICT,
            "account_exists",
            "an account with this email address was just created; enter its password",
            path,
            &id,
        ),
        Err(PasswordAcceptError::StalePassword) => problem(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "the account's password changed meanwhile; enter its new password",
            path,
            &id,
        ),
        Err(PasswordAcceptError::Disabled) => problem(
            StatusCode::FORBIDDEN,
            "enabled_membership_required",
            "this account is disabled",
            path,
            &id,
        ),
        Err(PasswordAcceptError::Store(error)) => {
            tracing::warn!(error = ?error, "accepting an invitation with a password failed");
            unavailable(path, &id)
        }
    }
}

fn invitation_invalid(path: &str, id: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "invitation_invalid",
        "the invitation is not valid, was used already, has expired or must be accepted through your organization's identity provider",
        path,
        id,
    )
}

/// `POST /organizations/{org}/members/{user}/password-reset-link`: an owner
/// makes a single-use link that lets a member with a local account choose a
/// new password. The link is in this response only.
pub async fn member_password_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((organization_id, target)): Path<(OrganizationId, UserAccountId)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_MEMBER_PASSWORD_RESET;
    if !local_enabled(&state) {
        return disabled(path, &id);
    }
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let store = state.auth().local_accounts();
    match store.reset_allowed(actor, organization_id, target).await {
        Ok(Ok(())) => {}
        Ok(Err(ResetRefusal::NotFound)) => {
            return problem(
                StatusCode::NOT_FOUND,
                "local_account_not_found",
                "no member of this organization with a password account has that id",
                path,
                &id,
            );
        }
        Ok(Err(ResetRefusal::OperatorOnly)) => {
            return problem(
                StatusCode::FORBIDDEN,
                "reset_requires_operator",
                "this person also belongs to organizations you do not own, or administers the gateway; the operator can reset their password with `gateway user reset-password`",
                path,
                &id,
            );
        }
        Err(_) => return unavailable(path, &id),
    }
    let Some(origin) = state.oidc().origin.as_deref() else {
        return unavailable(path, &id);
    };
    let valid_for = chrono::Duration::hours(LINK_VALID_HOURS);
    match store
        .create_reset_link(target, valid_for, Some((organization_id, actor)))
        .await
    {
        Ok(token) => {
            let mut response = (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "link": reset_link(origin, token.as_str()),
                    "expires_at": chrono::Utc::now() + valid_for,
                })),
            )
                .into_response();
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(_) => unavailable(path, &id),
    }
}

/// Where a reset link sends the person: the admin UI's reset page, with the
/// token in the fragment so it never reaches a server log.
pub fn reset_link(origin: &str, token: &str) -> String {
    format!(
        "{}/admin/reset-password#token={token}",
        origin.trim_end_matches('/')
    )
}

/// Where the first-run setup link sends the operator.
pub fn setup_link(origin: &str, token: &str) -> String {
    format!("{}/admin/setup#token={token}", origin.trim_end_matches('/'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignInPolicyBody {
    require_sso: bool,
}

/// `GET /organizations/{org}/sign-in-policy`.
pub async fn get_sign_in_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SIGN_IN_POLICY;
    let user_id = match authenticated_session(&state, &headers).await {
        Ok(Some((user_id, _))) => user_id,
        Ok(None) => return unauthenticated(path, &id),
        Err(_) => return unavailable(path, &id),
    };
    match state
        .identity()
        .human_principal(user_id, organization_id, None)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(path, &id),
        Err(_) => return unavailable(path, &id),
    }
    policy_response(&state, organization_id, path, &id).await
}

/// `PUT /organizations/{org}/sign-in-policy`: an owner requires (or stops
/// requiring) the organization's own identity provider.
pub async fn put_sign_in_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<SignInPolicyBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SIGN_IN_POLICY;
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    match state
        .auth()
        .local_accounts()
        .set_require_sso(
            organization_id,
            crate::domain::audit::AuditActor::User(actor),
            body.require_sso,
        )
        .await
    {
        Ok(true) => policy_response(&state, organization_id, path, &id).await,
        Ok(false) => problem(
            StatusCode::CONFLICT,
            "identity_provider_required",
            "set up and test the organization's identity provider before requiring it",
            path,
            &id,
        ),
        Err(_) => unavailable(path, &id),
    }
}

async fn policy_response(
    state: &AppState,
    organization_id: OrganizationId,
    path: &str,
    id: &str,
) -> Response {
    match state
        .auth()
        .local_accounts()
        .sign_in_policy(organization_id)
        .await
    {
        Ok(Some((require_sso, provider_enabled))) => Json(serde_json::json!({
            "require_sso": require_sso,
            "identity_provider_enabled": provider_enabled,
            // Whether members may act here with a password right now.
            "passwords_allowed": !(require_sso && provider_enabled),
            "local_accounts": local_enabled(state),
        }))
        .into_response(),
        Ok(None) => not_found(path, id),
        Err(_) => unavailable(path, id),
    }
}

/// The signed-in owner of `organization_id`, from a browser session (not an
/// agent token), with a valid CSRF token for writes.
pub(crate) async fn owner(
    state: &AppState,
    headers: &HeaderMap,
    organization_id: OrganizationId,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    if !csrf_valid(state, headers) {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "csrf_origin_or_token_required",
            "the request must come from the admin UI with its CSRF token",
            path,
            id,
        ));
    }
    let actor = match human_session(state, headers).await {
        Ok(Some(actor)) => actor,
        Ok(None) => return Err(unauthenticated(path, id)),
        Err(()) => return Err(unavailable(path, id)),
    };
    match state
        .identity()
        .human_principal(actor, organization_id, None)
        .await
    {
        Ok(Some(principal)) if principal.role == MembershipRole::Owner => Ok(actor),
        Ok(Some(_)) => Err(problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only the organization's owners may do this",
            path,
            id,
        )),
        Ok(None) => Err(not_found(path, id)),
        Err(_) => Err(unavailable(path, id)),
    }
}

/// The user of the request's browser session; agent tokens are refused.
pub(crate) async fn human_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<UserAccountId>, ()> {
    if crate::transport::admin_auth::agent_token(headers).is_some() {
        return Ok(None);
    }
    authenticated_session(state, headers)
        .await
        .map(|session| session.map(|(user_id, _)| user_id))
        .map_err(|_| ())
}

async fn current_session_id(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<uuid::Uuid>, ()> {
    let (Some(crypto), Some(token)) = (state.auth_crypto(), cookie(headers, SESSION_COOKIE)) else {
        return Ok(None);
    };
    state
        .auth()
        .local_accounts()
        .session(&crypto.digest(&token))
        .await
        .map(|session| session.map(|(id, _)| id))
        .map_err(|_| ())
}

pub(crate) fn local_enabled(state: &AppState) -> bool {
    state.local_accounts_config().enabled && state.auth_crypto().is_some()
}

/// Local accounts must be on, and a browser request must come from the
/// gateway's own origin.
fn browser_guard(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<(), Response> {
    if !local_enabled(state) {
        return Err(disabled(path, id));
    }
    origin_guard(state, headers, path, id)
}

/// A request with an `Origin` must come from the gateway's own; one without
/// (a script) is let through, as it cannot carry a victim's cookies.
pub(crate) fn origin_guard(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
) -> Result<(), Response> {
    if headers.contains_key(axum::http::header::ORIGIN)
        && !same_origin(headers, state.oidc().origin.as_deref())
    {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "cross_origin_request_refused",
            "the request must come from the gateway's own admin UI",
            path,
            id,
        ));
    }
    Ok(())
}

/// A new session and the browser binding its cookie pairs with. The store
/// that creates the session stores it for the person it signs in (whose id
/// may not exist yet, as in setup), not for the placeholder user here.
/// `None` without session secrets.
fn new_session(state: &AppState) -> Option<(SessionRecord, String)> {
    let crypto = state.auth_crypto()?;
    let browser = crypto.opaque_token().as_str().to_owned();
    let session = crypto.new_session(
        UserAccountId(uuid::Uuid::nil()),
        &browser,
        Duration::from_secs(state.session_ttl_seconds()),
    );
    Some((session, browser))
}

/// `status` with `body`, and the cookies of the new session.
fn signed_in(
    state: &AppState,
    session: &SessionRecord,
    browser: &str,
    status: StatusCode,
    body: serde_json::Value,
) -> Response {
    let ttl = state.session_ttl_seconds();
    let csrf = state
        .auth_crypto()
        .map(|crypto| crypto.opaque_token().as_str().to_owned())
        .unwrap_or_default();
    let mut response = (status, Json(body)).into_response();
    set_cookie(
        &mut response,
        &format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={ttl}",
            session.token
        ),
    );
    set_cookie(
        &mut response,
        &format!(
            "{BROWSER_COOKIE}={browser}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={ttl}"
        ),
    );
    set_cookie(
        &mut response,
        &format!("{CSRF_COOKIE}={csrf}; Path=/; Secure; SameSite=Lax; Max-Age={ttl}"),
    );
    response
}

/// Password hashing work running at once: each Argon2 run holds about
/// 19 MiB, so this bounds the memory sign-in bursts can take.
fn running_password_work() -> &'static tokio::sync::Semaphore {
    static RUNNING: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    RUNNING.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(2, usize::from);
        tokio::sync::Semaphore::new(cores.clamp(2, 8))
    })
}

/// Password work running or waiting to: beyond it, requests are refused
/// at once (`503 sign_in_busy`) rather than queued.
fn admitted_password_work() -> &'static tokio::sync::Semaphore {
    static ADMITTED: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    ADMITTED.get_or_init(|| tokio::sync::Semaphore::new(64))
}

/// Too much password work is running or waiting.
#[derive(Debug)]
struct Busy;

/// Runs `work` (Argon2) off the executor, within the shared limits.
async fn password_work<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Busy> {
    let _admitted = admitted_password_work().try_acquire().map_err(|_| Busy)?;
    let _running = running_password_work().acquire().await.map_err(|_| Busy)?;
    tokio::task::spawn_blocking(work).await.map_err(|_| Busy)
}

pub(crate) async fn hash(password: Zeroizing<String>) -> Result<String, ()> {
    password_work(move || hash_password(&password))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

/// Whether `password` matches `stored`; `Err` when the gateway is too busy
/// to check.
async fn verify(stored: String, password: Zeroizing<String>) -> Result<bool, Busy> {
    password_work(move || verify_password(&stored, &password)).await
}

fn busy(path: &str, id: &str) -> Response {
    let mut response = problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "sign_in_busy",
        "too many sign-ins are being checked; try again in a moment",
        path,
        id,
    );
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

fn display_name(value: Option<&str>) -> Result<Option<String>, ()> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > 200 || value.chars().any(char::is_control) {
        return Err(());
    }
    Ok(Some(value.to_owned()))
}

fn weak_password(rejection: PasswordRejection, path: &str, id: &str) -> Response {
    problem(
        StatusCode::UNPROCESSABLE_ENTITY,
        rejection.code(),
        rejection.detail(),
        path,
        id,
    )
}

fn invalid_credentials(path: &str, id: &str) -> Response {
    problem(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "the email address or password is not correct",
        path,
        id,
    )
}

fn too_many_attempts(wait: Duration, path: &str, id: &str) -> Response {
    let seconds = wait.as_secs().max(1);
    let mut response = problem(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        format!(
            "too many failed attempts; try again in {}",
            wait_text(seconds)
        ),
        path,
        id,
    );
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

fn wait_text(seconds: u64) -> String {
    if seconds < 90 {
        format!("{seconds} seconds")
    } else {
        format!("{} minutes", seconds.div_ceil(60))
    }
}

fn disabled(path: &str, id: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "local_accounts_disabled",
        "this gateway does not offer password accounts",
        path,
        id,
    )
}

pub(crate) fn invalid(path: &str, id: &str, detail: &'static str) -> Response {
    problem(StatusCode::BAD_REQUEST, "invalid_request", detail, path, id)
}

pub(crate) fn invalid_body(path: &str, id: &str) -> Response {
    problem(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "the request body is not valid JSON of the expected shape",
        path,
        id,
    )
}

pub(crate) fn unauthenticated(path: &str, id: &str) -> Response {
    problem(
        StatusCode::UNAUTHORIZED,
        "authentication_required",
        "sign in first",
        path,
        id,
    )
}

pub(crate) fn not_found(path: &str, id: &str) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "not_found",
        "organization not found",
        path,
        id,
    )
}

pub(crate) fn unavailable(path: &str, id: &str) -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "auth_store_unavailable",
        "the sign-in service is unavailable; try again",
        path,
        id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_limited_after_too_many_failures() {
        let limiter = AttemptLimiter::default();
        let address: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        for _ in 0..ADDRESS_FAILURES - 1 {
            limiter.fail(address);
        }
        assert_eq!(limiter.retry_after(address), None);
        limiter.fail(address);
        let wait = limiter.retry_after(address).unwrap();
        assert!(wait <= ADDRESS_WINDOW && wait > ADDRESS_WINDOW - Duration::from_secs(5));
        assert_eq!(limiter.retry_after(other), None);
    }

    #[test]
    fn ipv6_addresses_are_limited_by_their_64() {
        let limiter = AttemptLimiter::default();
        // Walking a /64's addresses does not reset the limit.
        for host in 0..ADDRESS_FAILURES {
            limiter.fail(format!("2001:db8:1:2::{:x}", host + 1).parse().unwrap());
        }
        assert!(
            limiter
                .retry_after("2001:db8:1:2::ffff".parse().unwrap())
                .is_some()
        );
        assert_eq!(
            limiter.retry_after("2001:db8:1:3::1".parse().unwrap()),
            None
        );
        // An IPv4 client of a dual-stack listener is the IPv4 address.
        let mapped: IpAddr = "::ffff:192.0.2.9".parse().unwrap();
        for _ in 0..ADDRESS_FAILURES {
            limiter.fail(mapped);
        }
        assert!(limiter.retry_after("192.0.2.9".parse().unwrap()).is_some());
    }

    #[test]
    fn links_carry_the_token_in_the_fragment() {
        assert_eq!(
            reset_link("http://localhost:8080/", "abc"),
            "http://localhost:8080/admin/reset-password#token=abc"
        );
        assert_eq!(
            setup_link("https://gw.example.com", "abc"),
            "https://gw.example.com/admin/setup#token=abc"
        );
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-local-accounts-v1.json"))
                .unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = UNSCOPED_PATHS.to_vec();
        routes.extend([ROUTE_MEMBER_PASSWORD_RESET, ROUTE_SIGN_IN_POLICY]);
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }

    #[test]
    fn waits_read_as_seconds_then_minutes() {
        assert_eq!(wait_text(30), "30 seconds");
        assert_eq!(wait_text(600), "10 minutes");
    }
}
