//! Email-first sign-in and the email domains that route it. See
//! `openapi/admin-email-domains-v1.json`.
//!
//! `POST /sign-in/start` takes the email address a person enters and says
//! where to go next: the identity provider of the organization that
//! verified the address's domain (with the address as `login_hint`), the
//! password step when local accounts are on, or the gateway's own provider.
//! Owners claim domains under `/organizations/{org}/domains` and prove them
//! with a DNS TXT record.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    application::AppState,
    domain::{
        email_domains::{normalize_domain, record_name, record_value, records_prove},
        identity::OrganizationId,
        identity_provider::{ProviderBinding, SignInPurpose},
        password::{email_domain, normalize_email},
    },
    persistence::email_domains::{DomainError, StoredDomain},
    transport::{
        admin_identities::{problem, request_context},
        http::{authenticated_session, begin_login, set_cookie},
        local_accounts::{
            invalid, invalid_body, local_enabled, not_found, origin_guard, owner, unauthenticated,
            unavailable,
        },
    },
};

pub const ROUTE_SIGN_IN_START: &str = "/api/admin/v1/sign-in/start";
pub const ROUTE_DOMAINS: &str = "/api/admin/v1/organizations/{organization_id}/domains";
pub const ROUTE_DOMAIN: &str = "/api/admin/v1/organizations/{organization_id}/domains/{domain_id}";
pub const ROUTE_DOMAIN_VERIFY: &str =
    "/api/admin/v1/organizations/{organization_id}/domains/{domain_id}/verify";

/// Routes outside any organization, for the router's session-scope check.
pub const UNSCOPED_PATHS: &[&str] = &[ROUTE_SIGN_IN_START];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartBody {
    email: String,
}

/// Where sign-in goes next.
#[derive(Debug, Serialize)]
#[serde(tag = "next", rename_all = "snake_case")]
enum Next {
    /// Send the browser to `authorization_url`. `password`: a password is
    /// also accepted for this address's organization, so the page may offer
    /// it instead.
    Redirect {
        authorization_url: String,
        password: bool,
    },
    /// Ask for the password.
    Password,
}

/// `POST /sign-in/start`.
pub async fn sign_in_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<StartBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_SIGN_IN_START;
    if let Err(response) = origin_guard(&state, &headers, path, &id) {
        return response;
    }
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    let Some(email) = normalize_email(&body.email) else {
        return invalid(path, &id, "the email address is not valid");
    };
    let domain = email_domain(&email).unwrap_or_default();
    let owner = match state.email_domains().owner(domain).await {
        Ok(owner) => owner,
        Err(_) => return unavailable(path, &id),
    };
    let local = local_enabled(&state);
    // An organization's own provider works also without the process-wide
    // one (its callback and session secrets are there for local accounts).
    if let Some(owner) = owner.filter(|_| state.auth_crypto().is_some()) {
        let configuration = match state
            .identity_providers()
            .active_for_organization(owner.organization_id)
            .await
        {
            Ok(configuration) => configuration,
            Err(_) => return unavailable(path, &id),
        };
        if let Some(configuration) = configuration {
            let password = local
                && match state
                    .auth()
                    .password_allowed_in(owner.organization_id)
                    .await
                {
                    Ok(allowed) => allowed,
                    Err(_) => return unavailable(path, &id),
                };
            let binding = ProviderBinding {
                organization_id: owner.organization_id,
                configuration_id: configuration.id,
                purpose: SignInPurpose::SignIn,
                started_by: None,
                expected_version: None,
            };
            return redirect(
                &state,
                &headers,
                Some((configuration, binding)),
                &email,
                password,
            )
            .await;
        }
    }
    // The gateway's own provider before a password: people who sign in with
    // it rarely have one, and the answer says when a password would also do.
    if state.oidc().enabled {
        return redirect(&state, &headers, None, &email, local).await;
    }
    if local {
        return Json(Next::Password).into_response();
    }
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "sign_in_unavailable",
        "this gateway has no way to sign in configured; ask its operator",
        path,
        &id,
    )
}

async fn redirect(
    state: &AppState,
    headers: &HeaderMap,
    provider: Option<(
        crate::persistence::identity_providers::StoredConfiguration,
        ProviderBinding,
    )>,
    email: &str,
    password: bool,
) -> Response {
    match begin_login(state, headers, None, provider, Some(email)).await {
        Ok((authorization_url, cookies)) => {
            let mut response = Json(Next::Redirect {
                authorization_url,
                password,
            })
            .into_response();
            for cookie in cookies {
                set_cookie(&mut response, &cookie);
            }
            response
        }
        // The provider cannot be reached (discovery failed, say): a password
        // still works where one is allowed.
        Err(response) if password && response.status().is_server_error() => {
            tracing::warn!(
                status = %response.status(),
                "starting the organization's sign-in failed; offering the password"
            );
            Json(Next::Password).into_response()
        }
        Err(response) => response,
    }
}

/// A domain as the API shows it, with the record that proves it.
#[derive(Debug, Serialize)]
struct DomainView<'a> {
    #[serde(flatten)]
    stored: &'a StoredDomain,
    status: &'static str,
    record: Record,
}

#[derive(Debug, Serialize)]
struct Record {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    value: String,
}

fn view(stored: &StoredDomain) -> serde_json::Value {
    serde_json::to_value(DomainView {
        stored,
        status: if stored.verified_at.is_some() {
            "verified"
        } else {
            "pending"
        },
        record: Record {
            kind: "TXT",
            name: record_name(&stored.domain),
            value: record_value(&stored.verification_token),
        },
    })
    .unwrap_or_default()
}

/// `GET /organizations/{org}/domains`: any member may read them.
pub async fn list_domains(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DOMAINS;
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
    match state.email_domains().list(organization_id).await {
        Ok(domains) => Json(serde_json::json!({
            "items": domains.iter().map(view).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(_) => unavailable(path, &id),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddBody {
    domain: String,
}

/// `POST /organizations/{org}/domains`: an owner adds a domain to verify.
pub async fn add_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<AddBody>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DOMAINS;
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let Ok(Json(body)) = body else {
        return invalid_body(path, &id);
    };
    // The detail below names the limit.
    const _: () = assert!(crate::domain::email_domains::MAX_DOMAIN_LEN == 231);
    let Some(domain) = normalize_domain(&body.domain) else {
        return invalid(
            path,
            &id,
            "the domain is not valid; enter a name such as example.com, of at most 231 characters",
        );
    };
    match state
        .email_domains()
        .add(organization_id, actor, &domain)
        .await
    {
        Ok(stored) => (StatusCode::CREATED, Json(view(&stored))).into_response(),
        Err(error) => domain_problem(error, path, &id),
    }
}

/// `POST /organizations/{org}/domains/{id}/verify`: looks the TXT record up
/// now. Answers the domain, verified or still pending.
pub async fn verify_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((organization_id, domain_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DOMAIN_VERIFY;
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let stored = match state.email_domains().get(organization_id, domain_id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return domain_problem(DomainError::NotFound, path, &id),
        Err(_) => return unavailable(path, &id),
    };
    let proven = if stored.verified_at.is_some() {
        true
    } else {
        match state.txt_lookup().txt(&record_name(&stored.domain)).await {
            Ok(records) => records_prove(&records, &stored.verification_token),
            Err(_) => {
                return problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dns_unavailable",
                    "the DNS lookup failed; try again in a moment",
                    path,
                    &id,
                );
            }
        }
    };
    match state
        .email_domains()
        .record_check(organization_id, actor, domain_id, proven)
        .await
    {
        Ok(stored) => Json(view(&stored)).into_response(),
        Err(error) => domain_problem(error, path, &id),
    }
}

/// `DELETE /organizations/{org}/domains/{id}`.
pub async fn remove_domain(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((organization_id, domain_id)): Path<(OrganizationId, Uuid)>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_DOMAIN;
    let actor = match owner(&state, &headers, organization_id, path, &id).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    match state
        .email_domains()
        .remove(organization_id, actor, domain_id)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => domain_problem(error, path, &id),
    }
}

fn domain_problem(error: DomainError, path: &str, id: &str) -> Response {
    match error {
        DomainError::Exists => problem(
            StatusCode::CONFLICT,
            "domain_exists",
            "the organization lists this domain already",
            path,
            id,
        ),
        DomainError::Claimed => problem(
            StatusCode::CONFLICT,
            "domain_claimed",
            "another organization on this gateway has verified this domain",
            path,
            id,
        ),
        DomainError::NotFound => problem(
            StatusCode::NOT_FOUND,
            "domain_not_found",
            "no such domain in this organization",
            path,
            id,
        ),
        DomainError::Store(_) | DomainError::Audit => unavailable(path, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-email-domains-v1.json"))
                .unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_SIGN_IN_START,
            ROUTE_DOMAINS,
            ROUTE_DOMAIN,
            ROUTE_DOMAIN_VERIFY,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }

    #[test]
    fn next_steps_serialize_with_their_tag() {
        assert_eq!(
            serde_json::to_value(Next::Password).unwrap(),
            serde_json::json!({ "next": "password" })
        );
        assert_eq!(
            serde_json::to_value(Next::Redirect {
                authorization_url: "https://idp.example/authorize".into(),
                password: true,
            })
            .unwrap(),
            serde_json::json!({
                "next": "redirect",
                "authorization_url": "https://idp.example/authorize",
                "password": true,
            })
        );
    }
}
