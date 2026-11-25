// HTTP transport for cloud onboarding: what a tenant creates in its own AWS
// account, Azure directory or Google Cloud project so the gateway can reach
// its models, rendered for one organization (`domain::onboarding`).
//
// - `GET .../organizations/{org}/onboarding`: which clouds are offered and
//   the gateway identities they name.
// - `GET .../onboarding/aws?account_id=&region=`,
//   `.../onboarding/azure[?resource_id=]`, `.../onboarding/google?project_id=`:
//   links and scripts.
// - `GET /api/admin/v1/onboarding/azure/consent`: where Microsoft returns an
//   administrator after consent; sends the browser on to the admin UI with
//   the tenant ID. Nothing is stored: the administrator still submits the
//   connection.
//
// - `POST .../organizations/{org}/onboarding/verify`: signs in with a
//   credential the way inference would and lists the models it can call,
//   storing nothing (`credential_write`: owner and admin).
//
// The organization routes need a session or an agent token and `catalog_read` (every
// org-wide role), and respond with `Cache-Control: no-store`. Nothing they
// return is secret: the instructions name the gateway's public identities
// and the organization's external ID, and verification echoes no secret.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header::LOCATION},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::{
    domain::{
        admin::{AdminPermission, AdminScope},
        auth::AuthCrypto,
        catalog::ProviderFamily,
        credentials::{ProviderSecret, ProviderSecretInput},
        identity::{OrganizationId, UserAccountId},
        inference::ProviderErrorCategory,
        onboarding::{AwsBatch, CredentialProbe, OnboardingError, OnboardingIdentities, Probe},
    },
    persistence::{
        admin::AdminStore, auth::AuthStore, provider_credentials::ProviderCredentialStore,
    },
    transport::admin_identities::{no_store, problem, request_context},
};

pub const ROUTE_ONBOARDING: &str = "/api/admin/v1/organizations/{org}/onboarding";
pub const ROUTE_ONBOARDING_AWS: &str = "/api/admin/v1/organizations/{org}/onboarding/aws";
pub const ROUTE_ONBOARDING_AZURE: &str = "/api/admin/v1/organizations/{org}/onboarding/azure";
pub const ROUTE_ONBOARDING_GOOGLE: &str = "/api/admin/v1/organizations/{org}/onboarding/google";
pub const ROUTE_AZURE_CONSENT: &str = "/api/admin/v1/onboarding/azure/consent";
pub const ROUTE_ONBOARDING_VERIFY: &str = "/api/admin/v1/organizations/{org}/onboarding/verify";

/// Most models a verification returns; the count is always complete.
const MAX_VERIFIED_MODELS: usize = 500;

/// Where the consent return sends the browser.
const PROVIDERS_PAGE: &str = "/admin/providers";

#[derive(Clone)]
pub struct OnboardingState {
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    identities: OnboardingIdentities,
    credentials: ProviderCredentialStore,
    probe: Option<Arc<dyn CredentialProbe>>,
}

/// `probe` signs in with credentials to verify them; without it,
/// verification answers 503.
pub fn router(
    auth: AuthStore,
    admin: AdminStore,
    crypto: Option<AuthCrypto>,
    identities: OnboardingIdentities,
    credentials: ProviderCredentialStore,
    probe: Option<Arc<dyn CredentialProbe>>,
) -> Router {
    let state = OnboardingState {
        auth,
        admin,
        crypto,
        identities,
        credentials,
        probe,
    };
    Router::new()
        .route(ROUTE_ONBOARDING, get(summary))
        .route(ROUTE_ONBOARDING_AWS, get(aws))
        .route(ROUTE_ONBOARDING_AZURE, get(azure))
        .route(ROUTE_ONBOARDING_GOOGLE, get(google))
        .route(ROUTE_AZURE_CONSENT, get(azure_consent))
        .route(ROUTE_ONBOARDING_VERIFY, post(verify))
        .with_state(state)
        .layer(axum::middleware::map_response(no_store))
}

#[derive(Deserialize)]
struct AwsQuery {
    account_id: String,
    region: String,
    /// Also let the role accept models' agreements (D-035).
    #[serde(default)]
    accept_agreements: bool,
    /// Also let the role run batch inference jobs through this staging
    /// bucket and batch service role; both or neither.
    batch_bucket: Option<String>,
    batch_role_arn: Option<String>,
}

#[derive(Deserialize)]
struct AzureQuery {
    resource_id: Option<String>,
    /// `azure-openai` (default) or `azure-ai-foundry`: which role to grant.
    family: Option<String>,
    /// Also create and grant a role for batch files and jobs.
    #[serde(default)]
    batch: bool,
}

#[derive(Deserialize)]
struct GoogleQuery {
    project_id: String,
    /// Also grant access to this batch staging bucket.
    batch_bucket: Option<String>,
}

async fn summary(
    State(state): State<OnboardingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
) -> Response {
    let (id, _) = request_context(&headers);
    if let Err(response) = authorize(&state, &headers, organization_id, ROUTE_ONBOARDING, &id).await
    {
        return response;
    }
    (
        StatusCode::OK,
        Json(state.identities.summary(organization_id)),
    )
        .into_response()
}

async fn aws(
    State(state): State<OnboardingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<AwsQuery>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ONBOARDING_AWS;
    if let Err(response) = authorize(&state, &headers, organization_id, path, &id).await {
        return response;
    }
    let Ok(Query(query)) = query else {
        return onboarding_problem(OnboardingError::AwsAccount, path, &id);
    };
    let batch = match (
        given(query.batch_bucket.as_deref()),
        given(query.batch_role_arn.as_deref()),
    ) {
        (None, None) => None,
        (Some(bucket), Some(service_role_arn)) => Some(AwsBatch {
            bucket,
            service_role_arn,
        }),
        _ => return onboarding_problem(OnboardingError::AwsBatch, path, &id),
    };
    match state.identities.aws(
        organization_id,
        query.account_id.trim(),
        query.region.trim(),
        query.accept_agreements,
        batch,
    ) {
        Ok(instructions) => (StatusCode::OK, Json(instructions)).into_response(),
        Err(error) => onboarding_problem(error, path, &id),
    }
}

async fn azure(
    State(state): State<OnboardingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<AzureQuery>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ONBOARDING_AZURE;
    if let Err(response) = authorize(&state, &headers, organization_id, path, &id).await {
        return response;
    }
    let Ok(Query(query)) = query else {
        return onboarding_problem(OnboardingError::AzureResource, path, &id);
    };
    let resource = query
        .resource_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let foundry = match query.family.as_deref() {
        None | Some("azure-openai") => false,
        Some("azure-ai-foundry") => true,
        Some(_) => return onboarding_problem(OnboardingError::AzureResource, path, &id),
    };
    match state
        .identities
        .azure(organization_id, resource, foundry, query.batch)
    {
        Ok(instructions) => (StatusCode::OK, Json(instructions)).into_response(),
        Err(error) => onboarding_problem(error, path, &id),
    }
}

async fn google(
    State(state): State<OnboardingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    query: Result<Query<GoogleQuery>, QueryRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ONBOARDING_GOOGLE;
    if let Err(response) = authorize(&state, &headers, organization_id, path, &id).await {
        return response;
    }
    let Ok(Query(query)) = query else {
        return onboarding_problem(OnboardingError::GoogleProject, path, &id);
    };
    match state.identities.google(
        query.project_id.trim(),
        given(query.batch_bucket.as_deref()),
    ) {
        Ok(instructions) => (StatusCode::OK, Json(instructions)).into_response(),
        Err(error) => onboarding_problem(error, path, &id),
    }
}

/// A credential to check, as `PUT .../catalog/credentials/{key}` takes it,
/// with the family it is for.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyRequest {
    /// The provider account the credential will be stored for: its
    /// configuration (endpoint, region) is the one checked.
    adapter_instance_key: String,
    family: String,
    credential: ProviderSecretInput,
}

/// The outcome of a verification. Failures are outcomes, not errors: the
/// request was valid and the cloud answered.
#[derive(Serialize)]
struct Verified {
    /// `connected`, `awaiting_approval`, `not_configured`, or `failed`.
    status: &'static str,
    /// `failed`: `authentication`, `authorization`, `unavailable`,
    /// `timeout`, `unsupported`, or `provider`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// What to do next, for a person.
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<Vec<VerifiedModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_count: Option<usize>,
    /// Bedrock: the region the gateway calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
}

#[derive(Serialize)]
struct VerifiedModel {
    model: String,
    display_name: Option<String>,
}

impl Verified {
    fn status(status: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            reason: None,
            message: message.into(),
            models: None,
            model_count: None,
            region: None,
        }
    }
}

async fn verify(
    State(state): State<OnboardingState>,
    headers: HeaderMap,
    Path(organization_id): Path<OrganizationId>,
    body: Result<Json<VerifyRequest>, JsonRejection>,
) -> Response {
    let (id, _) = request_context(&headers);
    let path = ROUTE_ONBOARDING_VERIFY;
    // Owners and admins: it signs in with a secret and acts in their cloud.
    if let Err(response) = authorize_for(
        &state,
        &headers,
        organization_id,
        AdminPermission::CredentialWrite,
        path,
        &id,
    )
    .await
    {
        return response;
    }
    let invalid = |detail: &str| {
        problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_credential",
            detail.to_owned(),
            path,
            &id,
        )
    };
    let Ok(Json(request)) = body else {
        return invalid(
            "the body must be {\"adapter_instance_key\": ..., \"family\": ..., \"credential\": {...}}",
        );
    };
    let Ok(family) = request.family.parse::<ProviderFamily>() else {
        return invalid("unknown provider family");
    };
    if crate::domain::credentials::validate_adapter_instance_key(&request.adapter_instance_key)
        .is_err()
    {
        return invalid("invalid adapter_instance_key");
    }
    let secret = match ProviderSecret::for_family(family, request.credential) {
        Ok(secret) => secret,
        Err(error) => return invalid(&error.to_string()),
    };
    // As for inference: an Entra directory or Google service account is
    // acted as only once the operator has bound it to this organization.
    if let Some(principal) = secret.cloud_principal() {
        match state
            .credentials
            .principal_bound(organization_id, &principal)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::OK,
                    Json(Verified::status(
                        "awaiting_approval",
                        format!(
                            "The gateway's operator must approve this {} for your organization \
                             before the gateway signs in to it. Nothing was contacted.",
                            match principal.kind() {
                                "azure_tenant" => "Entra directory",
                                _ => "Google service account",
                            }
                        ),
                    )),
                )
                    .into_response();
            }
            Err(_) => return unavailable(path, &id),
        }
    }
    let Some(probe) = state.probe.as_ref() else {
        return problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "verification_unavailable",
            "credential verification is not available",
            path,
            &id,
        );
    };
    let outcome = match probe
        .probe(
            organization_id,
            &request.adapter_instance_key,
            family,
            &secret,
        )
        .await
    {
        Ok(Probe::Listed { mut models, region }) => {
            let count = models.len();
            models.sort_by(|a, b| a.model.cmp(&b.model));
            models.truncate(MAX_VERIFIED_MODELS);
            Verified {
                status: "connected",
                reason: None,
                message: format!("The gateway signed in and can see {count} models."),
                models: Some(
                    models
                        .into_iter()
                        .map(|model| VerifiedModel {
                            model: model.model,
                            display_name: model.display_name,
                        })
                        .collect(),
                ),
                model_count: Some(count),
                region,
            }
        }
        Ok(Probe::NotConfigured) => Verified::status(
            "not_configured",
            "This gateway is not set up to use this kind of credential for this provider.",
        ),
        Ok(Probe::EndpointNotAllowed) => Verified {
            reason: Some("endpoint_not_allowed"),
            ..Verified::status(
                "failed",
                "The gateway reaches tenants' endpoints only over HTTPS at public addresses. \
                 For an endpoint on a private network or over plain HTTP, such as a model server \
                 in the gateway's own cluster, the gateway's operator must allow its origin \
                 (GATEWAY_OPENAI_COMPATIBLE_PRIVATE_ENDPOINTS). Nothing was sent to it.",
            )
        },
        Ok(Probe::EndpointUnresolved) => Verified {
            reason: Some("endpoint_unresolved"),
            ..Verified::status(
                "failed",
                "The endpoint's host name could not be resolved. Check the URL.",
            )
        },
        Err(error) => {
            let (reason, message) = failure(family, &secret, error.category());
            Verified {
                reason: Some(reason),
                ..Verified::status("failed", message)
            }
        }
    };
    (StatusCode::OK, Json(outcome)).into_response()
}

/// Why a sign-in or listing failed, and what to check, by family and kind of
/// credential. The provider's own response is never echoed.
fn failure(
    family: ProviderFamily,
    secret: &ProviderSecret,
    category: ProviderErrorCategory,
) -> (&'static str, String) {
    use ProviderErrorCategory as C;
    let role = matches!(secret, ProviderSecret::AwsAssumeRole { .. });
    let directory = matches!(secret, ProviderSecret::AzureEntra { .. });
    let account = matches!(secret, ProviderSecret::GoogleServiceAccount { .. });
    match category {
        C::Authentication if role => (
            "authentication",
            "AWS refused to let the gateway assume the role. Check that the role exists, that \
             its trust policy names the gateway's principal and requires your external ID, and \
             that the ARN is right."
                .into(),
        ),
        C::Authentication if directory => (
            "authentication",
            "Microsoft Entra refused the gateway's sign-in to your directory. An administrator \
             of the directory must consent to the gateway's app first."
                .into(),
        ),
        C::Authentication if account => (
            "authentication",
            "Google refused to let the gateway act as the service account. Grant the gateway's \
             service account roles/iam.serviceAccountTokenCreator on it."
                .into(),
        ),
        C::Authentication => (
            "authentication",
            "The provider rejected the credential. Check that it is current and complete.".into(),
        ),
        C::Authorization if role => (
            "authorization",
            "The gateway assumed the role, but it may not list Bedrock models. Give it \
             bedrock:ListFoundationModels and bedrock:ListInferenceProfiles."
                .into(),
        ),
        C::Authorization if directory => (
            "authorization",
            "The gateway signed in, but may not use the resource. Grant its app the Cognitive \
             Services OpenAI User role (Cognitive Services User for AI Foundry) on it."
                .into(),
        ),
        C::Authorization if account => (
            "authorization",
            "The gateway acts as the service account, but it may not use Agent Platform. Grant \
             it roles/aiplatform.user in the project, and enable aiplatform.googleapis.com."
                .into(),
        ),
        C::Authorization => (
            "authorization",
            "The provider accepted the credential but refused the request.".into(),
        ),
        C::UnsupportedFeature if account => (
            "unsupported",
            "The gateway acts as the service account, but found no Gemini model in the project's \
             location to confirm it may call models. Google serves some models, such as Gemini \
             4, only at the global location. Check the location, and that \
             roles/aiplatform.user is granted."
                .into(),
        ),
        C::UnsupportedFeature => (
            "unsupported",
            format!(
                "The credential may work, but {} cannot list models with it; choose models by \
                 name.",
                family.as_str()
            ),
        ),
        C::Timeout => (
            "timeout",
            "The provider did not answer in time. Try again.".into(),
        ),
        C::RateLimited | C::Unavailable => (
            "unavailable",
            "The provider is unavailable right now. Try again shortly.".into(),
        ),
        _ => (
            "provider",
            "The provider answered unexpectedly. Try again, or check the credential.".into(),
        ),
    }
}

/// Microsoft's admin-consent return: `tenant`, `admin_consent` and the
/// `state` we sent (the organization id), or `error`.
#[derive(Deserialize)]
struct ConsentReturn {
    tenant: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Sends the browser on to the providers page with only values that parse:
/// UUIDs for the organization and tenant, and Microsoft's error code reduced
/// to a fixed set of characters.
async fn azure_consent(Query(query): Query<ConsentReturn>) -> Response {
    let mut location = url::form_urlencoded::Serializer::new(String::new());
    location.append_pair("connect", "azure");
    if let Some(organization) = query
        .state
        .as_deref()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
    {
        location.append_pair("organization", &organization.to_string());
    }
    match (
        query.error.as_deref(),
        query
            .tenant
            .as_deref()
            .and_then(|value| uuid::Uuid::parse_str(value).ok()),
    ) {
        (Some(error), _) => {
            let code: String = error
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(64)
                .collect();
            location.append_pair(
                "error",
                if code.is_empty() {
                    "consent_failed"
                } else {
                    &code
                },
            );
        }
        (None, Some(tenant)) => {
            location.append_pair("tenant", &tenant.to_string());
        }
        (None, None) => {
            location.append_pair("error", "consent_failed");
        }
    }
    (
        StatusCode::SEE_OTHER,
        [(LOCATION, format!("{PROVIDERS_PAGE}?{}", location.finish()))],
    )
        .into_response()
}

async fn authorize(
    state: &OnboardingState,
    headers: &HeaderMap,
    organization_id: OrganizationId,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    authorize_for(
        state,
        headers,
        organization_id,
        AdminPermission::CatalogRead,
        path,
        id,
    )
    .await
}

async fn authorize_for(
    state: &OnboardingState,
    headers: &HeaderMap,
    organization_id: OrganizationId,
    permission: AdminPermission,
    path: &str,
    id: &str,
) -> Result<UserAccountId, Response> {
    let session =
        crate::transport::admin_auth::admin_actor(&state.auth, state.crypto.as_ref(), headers)
            .await;
    let actor = match session {
        Ok(Some(actor)) => actor,
        Ok(None) => {
            return Err(problem(
                StatusCode::UNAUTHORIZED,
                "authentication_required",
                "a session or an agent token is required",
                path,
                id,
            ));
        }
        Err(_) => return Err(unavailable(path, id)),
    };
    let context = state
        .admin
        .authorization_context(actor, organization_id)
        .await
        .map_err(|_| unavailable(path, id))?;
    if !context.allows(permission, AdminScope::Organization(organization_id)) {
        return Err(problem(
            StatusCode::FORBIDDEN,
            "forbidden",
            "administrative permission denied",
            path,
            id,
        ));
    }
    Ok(actor)
}

fn unavailable(path: &str, id: &str) -> Response {
    problem(
        StatusCode::SERVICE_UNAVAILABLE,
        "authentication_unavailable",
        "authentication service unavailable",
        path,
        id,
    )
}

/// An optional query value, trimmed; empty is absent.
fn given(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn onboarding_problem(error: OnboardingError, path: &str, id: &str) -> Response {
    match error {
        OnboardingError::Unavailable => problem(
            StatusCode::NOT_FOUND,
            "onboarding_unavailable",
            error.to_string(),
            path,
            id,
        ),
        _ => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_onboarding_input",
            error.to_string(),
            path,
            id,
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-onboarding-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_ONBOARDING,
            ROUTE_ONBOARDING_AWS,
            ROUTE_ONBOARDING_AZURE,
            ROUTE_ONBOARDING_GOOGLE,
            ROUTE_AZURE_CONSENT,
            ROUTE_ONBOARDING_VERIFY,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }

    use axum::body::Body;
    use tower::ServiceExt;

    use super::*;

    async fn consent(query: &str) -> String {
        let router = Router::new().route(ROUTE_AZURE_CONSENT, get(azure_consent));
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("{ROUTE_AZURE_CONSENT}?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        response.headers()[LOCATION].to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn consent_returns_to_the_providers_page_with_only_what_parses() {
        let org = "6b3f0d6e-8a3c-4f63-9a9a-6f1f0c1d2e3f";
        let tenant = "00000000-0000-4000-8000-000000000010";
        assert_eq!(
            consent(&format!("tenant={tenant}&admin_consent=True&state={org}")).await,
            format!("/admin/providers?connect=azure&organization={org}&tenant={tenant}")
        );
        assert_eq!(
            consent(&format!(
                "error=access_denied&error_description=x%3Cscript%3E&state={org}"
            ))
            .await,
            format!("/admin/providers?connect=azure&organization={org}&error=access_denied")
        );
        // Anything that is not a UUID or a plain code is dropped.
        assert_eq!(
            consent("tenant=evil.example&state=javascript:alert(1)").await,
            "/admin/providers?connect=azure&error=consent_failed"
        );
        assert_eq!(
            consent("error=%22%3E%3Cscript%3E").await,
            "/admin/providers?connect=azure&error=script"
        );
    }
}
