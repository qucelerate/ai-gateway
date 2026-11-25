#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for cloud onboarding: the instructions for one
//! organization, driven through the real router and RBAC.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use std::sync::Arc;

use gateway::{
    OrganizationId, TeamId, UserAccountId,
    domain::onboarding::OnboardingIdentities,
    domain::{auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper},
    persistence::{
        AdminStore, AuthStore, IdentityStore, cloud_principals::CloudPrincipalStore,
        provider_credentials::ProviderCredentialStore,
    },
    transport::onboarding::router,
};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SESSION_COOKIE: &str = "__Host-gateway_session";
const BROWSER_COOKIE: &str = "__Host-gateway_oidc_browser";
const TEST_PEPPER: &[u8] = b"onboarding-integration-pepper";
const HMAC_KEY: &[u8] = b"onboarding-integration-hmac-k-0032";

fn test_database_url() -> Option<String> {
    match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => Some(value),
        Err(_)
            if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => None,
    }
}

async fn connect_test_pool(database_url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
    panic!("GATEWAY_TEST_DATABASE_URL remained unreachable: {last_error:?}");
}

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = connect_test_pool(database_url).await;
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[19_u8; 32]).unwrap();
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            identities(),
            ProviderCredentialStore::new(pool.clone()),
            Some(Arc::new(FakeProbe)),
        );
        Self {
            pool,
            identity,
            crypto,
            router,
        }
    }

    async fn org(&self, slug: &str) -> OrganizationId {
        let suffix = Uuid::new_v4().simple().to_string();
        self.identity
            .create_organization(&format!("{slug}-{suffix}"), slug)
            .await
            .unwrap()
            .id
    }

    /// A user holding exactly one admin role binding: org-wide when `team` is
    /// `None`, otherwise `team_admin` of that team.
    async fn user(&self, org: OrganizationId, team: Option<TeamId>, role: &str) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(role))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, MembershipRole::Member)
            .await
            .unwrap();
        if let Some(team) = team {
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team.map(|team| team.0))
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> (String, String) {
        let browser = self.crypto.opaque_token().as_str().to_owned();
        let session = self
            .crypto
            .new_session(user, &browser, StdDuration::from_secs(3600));
        sqlx::query(
            "INSERT INTO gateway.session
                 (id, token_digest, user_id, browser_binding_digest, expires_at)
             VALUES ($1, $2, $3, $4, to_timestamp($5))",
        )
        .bind(session.id)
        .bind(&session.token_digest)
        .bind(user.0)
        .bind(&session.browser_binding_digest)
        .bind(session.expires_at as i64)
        .execute(&self.pool)
        .await
        .unwrap();
        (session.token.clone(), browser)
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        session: Option<&(String, String)>,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        if let Some(session) = session {
            builder = builder.header(
                COOKIE,
                format!(
                    "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                    session.0, session.1
                ),
            );
        }
        let body = match body {
            Some(body) => Body::from(serde_json::to_vec(body).unwrap()),
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body, headers)
    }

    async fn get(&self, uri: &str, session: &(String, String)) -> (StatusCode, Value, HeaderMap) {
        self.send(Method::GET, uri, Some(session), &[], None).await
    }
}

/// Answers by the credential, without a cloud: a role ARN ending in
/// `/Refused` is refused, an API key `slow` times out, a Bedrock role lists
/// two models in us-east-2, anything else lists one.
struct FakeProbe;

#[async_trait::async_trait]
impl gateway::domain::onboarding::CredentialProbe for FakeProbe {
    async fn probe(
        &self,
        _organization: OrganizationId,
        _adapter_instance_key: &str,
        _family: gateway::domain::catalog::ProviderFamily,
        secret: &gateway::domain::credentials::ProviderSecret,
    ) -> Result<gateway::domain::onboarding::Probe, gateway::domain::inference::ProviderError> {
        use gateway::domain::{
            credentials::ProviderSecret,
            discovery::DiscoveredModel,
            inference::{ProviderError, ProviderErrorCategory},
            onboarding::Probe,
        };
        match secret {
            ProviderSecret::AwsAssumeRole { role_arn } if role_arn.ends_with("/Refused") => {
                Err(ProviderError::new(ProviderErrorCategory::Authentication))
            }
            ProviderSecret::AwsAssumeRole { .. } => Ok(Probe::Listed {
                models: vec![
                    DiscoveredModel::new("us.amazon.nova-micro-v1:0", Some("Nova Micro")).unwrap(),
                    DiscoveredModel::new("anthropic.claude-sonnet", None).unwrap(),
                ],
                region: Some("us-east-2".into()),
            }),
            ProviderSecret::ApiKey { api_key } if api_key.as_str() == "slow" => {
                Err(ProviderError::new(ProviderErrorCategory::Timeout))
            }
            _ => Ok(Probe::Listed {
                models: vec![DiscoveredModel::new("model-a", None).unwrap()],
                region: None,
            }),
        }
    }
}

fn identities() -> OnboardingIdentities {
    OnboardingIdentities {
        openai_compatible_private_endpoints: vec!["http://models.internal:8080".into()],
        aws_principal_arn: Some("arn:aws:iam::123456789012:user/example-gateway".into()),
        aws_template_url: Some(
            "https://bucket.s3.us-east-2.amazonaws.com/templates/aws-bedrock-role-v1.yaml"
                .parse()
                .unwrap(),
        ),
        azure_client_id: Some("00000000-0000-4000-8000-000000000020".into()),
        azure_consent_redirect: Some(
            "https://gateway.example/api/admin/v1/onboarding/azure/consent"
                .parse()
                .unwrap(),
        ),
        google_service_account: None,
    }
}

fn uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/onboarding{path}", org.0)
}

#[tokio::test]
async fn onboarding_is_rendered_for_the_organization_and_every_org_wide_role() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("onboarding").await;
    let other = harness.org("onboarding-other").await;
    let external_id = format!("qucelerate-{}", org.0);

    for role in [
        "organization_owner",
        "organization_admin",
        "operator",
        "auditor",
        "read_only_member",
    ] {
        let session = harness.session(harness.user(org, None, role).await).await;
        let (status, summary, headers) = harness.get(&uri(org, ""), &session).await;
        assert_eq!(status, StatusCode::OK, "{role} {summary}");
        assert_eq!(headers["cache-control"], "no-store");
        assert_eq!(summary["aws"]["external_id"], external_id.as_str());
        assert_eq!(summary["aws"]["role_name"], "QucelerateGateway");
        assert_eq!(
            summary["azure"]["client_id"],
            "00000000-0000-4000-8000-000000000020"
        );
        // Not configured: not offered.
        assert!(summary["google"].is_null());
    }

    let owner = harness
        .session(harness.user(org, None, "organization_owner").await)
        .await;
    let (status, aws, _) = harness
        .get(
            &uri(org, "/aws?account_id=123456789012&region=us-west-2"),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{aws}");
    assert_eq!(
        aws["role_arn"],
        "arn:aws:iam::123456789012:role/QucelerateGateway"
    );
    assert!(aws["launch_url"].as_str().unwrap().contains(&external_id));
    assert!(aws["terraform"].as_str().unwrap().contains(&external_id));
    let (status, problem, _) = harness
        .get(&uri(org, "/aws?account_id=123&region=us-west-2"), &owner)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem["code"], "invalid_onboarding_input");
    let (status, _, _) = harness.get(&uri(org, "/aws"), &owner).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, foundry, _) = harness
        .get(&uri(org, "/azure?family=azure-ai-foundry"), &owner)
        .await;
    assert_eq!(status, StatusCode::OK, "{foundry}");
    assert_eq!(foundry["role"], "Cognitive Services User");
    let (status, _, _) = harness.get(&uri(org, "/azure?family=openai"), &owner).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // A malformed query is the documented problem, not a plain-text 400.
    let (status, problem, _) = harness
        .get(&uri(org, "/azure?resource_id=a&resource_id=b"), &owner)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    assert_eq!(problem["code"], "invalid_onboarding_input");
    // GovCloud has no one-click link: its console cannot read the template.
    let (status, gov, _) = harness
        .get(
            &uri(org, "/aws?account_id=123456789012&region=us-gov-west-1"),
            &owner,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{gov}");
    assert!(gov["launch_url"].is_null());
    let (status, azure, _) = harness.get(&uri(org, "/azure"), &owner).await;
    assert_eq!(status, StatusCode::OK, "{azure}");
    assert!(
        azure["consent_url"]
            .as_str()
            .unwrap()
            .contains(&org.0.to_string())
    );
    let (status, problem, _) = harness
        .get(&uri(org, "/google?project_id=acme-ai-123"), &owner)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(problem["code"], "onboarding_unavailable");

    // Another organization's instructions need a role there.
    let (status, _, _) = harness.get(&uri(other, ""), &owner).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let team = harness
        .identity
        .create_team(org, "onboarding-team")
        .await
        .unwrap()
        .id;
    let team_admin = harness
        .session(harness.user(org, Some(team), "team_admin").await)
        .await;
    let (status, _, _) = harness.get(&uri(org, ""), &team_admin).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = harness
        .send(Method::GET, &uri(org, ""), None, &[], None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn verification_signs_in_only_for_owners_and_approved_principals() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let org = harness.org("verify").await;
    let owner = harness
        .session(harness.user(org, None, "organization_owner").await)
        .await;
    let verify = |body: Value| {
        let harness = &harness;
        let owner = owner.clone();
        async move {
            harness
                .send(
                    Method::POST,
                    &uri(org, "/verify"),
                    Some(&owner),
                    &[],
                    Some(&body),
                )
                .await
        }
    };

    // A role the tenant created: signed in, models listed, sorted.
    let (status, body, headers) = verify(serde_json::json!({
        "adapter_instance_key": "tenant-account",
        "family": "bedrock",
        "credential": {"role_arn": "arn:aws:iam::123456789012:role/QucelerateGateway"}
    }))
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body["status"], "connected");
    assert_eq!(body["model_count"], 2);
    assert_eq!(body["models"][0]["model"], "anthropic.claude-sonnet");
    assert_eq!(body["region"], "us-east-2");

    // A refusal says what to check, without the provider's response.
    let (_, body, _) = verify(serde_json::json!({
        "adapter_instance_key": "tenant-account",
        "family": "bedrock",
        "credential": {"role_arn": "arn:aws:iam::123456789012:role/Refused"}
    }))
    .await;
    assert_eq!(body["status"], "failed");
    assert_eq!(body["reason"], "authentication");
    assert!(body["message"].as_str().unwrap().contains("external ID"));
    let (_, body, _) =
        verify(serde_json::json!({"adapter_instance_key": "tenant-account", "family": "openai", "credential": {"api_key": "slow"}})).await;
    assert_eq!(body["reason"], "timeout");
    // The secret is never echoed.
    assert!(!body.to_string().contains("slow"));

    // A Google service account is contacted only once approved.
    let account = "tenant-sa@acme-ai-123.iam.gserviceaccount.com";
    let google = serde_json::json!({"adapter_instance_key": "tenant-account", "family": "gemini", "credential": {"service_account": account}});
    let (_, body, _) = verify(google.clone()).await;
    assert_eq!(body["status"], "awaiting_approval", "{body}");
    assert!(body["models"].is_null());
    CloudPrincipalStore::new(harness.pool.clone())
        .allow(
            org,
            &gateway::domain::credentials::CloudPrincipal::parse("google_service_account", account)
                .unwrap(),
        )
        .await
        .unwrap();
    let (_, body, _) = verify(google).await;
    assert_eq!(body["status"], "connected", "{body}");
    // Approval is per organization.
    let other = harness.org("verify-other").await;
    let other_owner = harness
        .session(harness.user(other, None, "organization_owner").await)
        .await;
    let (_, body, _) = harness
        .send(
            Method::POST,
            &uri(other, "/verify"),
            Some(&other_owner),
            &[],
            Some(&serde_json::json!({"adapter_instance_key": "tenant-account", "family": "gemini", "credential": {"service_account": account}})),
        )
        .await;
    assert_eq!(body["status"], "awaiting_approval");

    // Invalid input.
    for invalid in [
        serde_json::json!({"adapter_instance_key": "tenant-account", "family": "nope", "credential": {"api_key": "k"}}),
        serde_json::json!({"adapter_instance_key": "tenant-account", "family": "openai", "credential": {"role_arn": "arn:aws:iam::123456789012:role/x"}}),
        serde_json::json!({"adapter_instance_key": "tenant-account", "family": "openai", "credential": {"api_key": "k", "endpoint_url": "x"}}),
        serde_json::json!({"adapter_instance_key": "tenant-account", "credential": {"api_key": "k"}}),
        serde_json::json!({"family": "openai", "credential": {"api_key": "k"}}),
        serde_json::json!({"adapter_instance_key": "bad\nkey", "family": "openai", "credential": {"api_key": "k"}}),
    ] {
        let (status, body, _) = verify(invalid.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid} {body}");
        assert_eq!(body["code"], "invalid_credential");
    }

    // Owners and admins only: it signs in with a secret.
    for (role, allowed) in [
        ("organization_admin", true),
        ("operator", false),
        ("auditor", false),
        ("read_only_member", false),
    ] {
        let session = harness.session(harness.user(org, None, role).await).await;
        let (status, _, _) = harness
            .send(
                Method::POST,
                &uri(org, "/verify"),
                Some(&session),
                &[],
                Some(&serde_json::json!({"adapter_instance_key": "tenant-account", "family": "openai", "credential": {"api_key": "k"}})),
            )
            .await;
        assert_eq!(status == StatusCode::OK, allowed, "{role} {status}");
    }
}
