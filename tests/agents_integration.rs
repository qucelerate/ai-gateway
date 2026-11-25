#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for agents: an organization's non-human administrators
//! and their `gwa_` tokens, driven through the whole admin router (so the
//! organization scoping middleware applies) with RBAC, the role ceiling,
//! idempotency and audit, and created headlessly through the store call
//! `gateway agent create` makes.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use axum::{
    Router,
    body::Body,
    http::{Method, StatusCode, header::AUTHORIZATION, header::COOKIE},
};
use gateway::{
    AdminRole, AppState, OrganizationId, UnavailableOidcBackend, UserAccountId,
    config::OidcConfig,
    domain::{
        agents::AgentTokenInput, auth::AuthCrypto, credentials::CredentialCipher,
        identity::MembershipRole, keys::ApiKeyPepper,
    },
    jobs::egress::DeliveryPolicy,
    persistence::{IdentityStore, agents::AgentStore},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
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
const TEST_PEPPER: &[u8] = b"agent-integration-pepper";
const HMAC_KEY: &[u8] = b"agent-integration-hmac-key-00000032";
const ORIGIN: &str = "https://gateway.agents.test";

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

/// Who makes a request.
enum Caller<'a> {
    Session(&'a (String, String)),
    Token(&'a str),
    /// A session's cookies and a raw `Authorization` header together.
    Both(&'a (String, String), &'a str),
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
        let crypto = AuthCrypto::new(HMAC_KEY, &[37_u8; 32]).unwrap();
        let state = AppState::for_sign_in_tests(
            pool.clone(),
            ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap(),
            crypto.clone(),
            OidcConfig {
                enabled: true,
                origin: Some(ORIGIN.into()),
                issuer: Some("https://login.agents.test/v2.0".into()),
                audience: Some("agents-client".into()),
                client_id: Some("agents-client".into()),
                redirect_uri: Some(format!("{ORIGIN}/api/admin/v1/oidc/callback")),
                microsoft_graph_photo: false,
            },
            Arc::new(UnavailableOidcBackend),
            Some(CredentialCipher::new(&[43_u8; 32]).unwrap()),
            DeliveryPolicy::allow_private_targets_for_tests(),
        );
        Self {
            router: gateway::transport::http::router(state),
            pool,
            identity,
            crypto,
        }
    }

    /// A new organization and its slug.
    async fn org(&self, name: &str) -> (OrganizationId, String) {
        let suffix = Uuid::new_v4().simple().to_string();
        let slug = format!("{name}-{}", &suffix[..12]);
        let org = self
            .identity
            .create_organization(&slug, name)
            .await
            .unwrap()
            .id;
        (org, slug)
    }

    /// A person in `org` with membership `role` and its derived binding.
    async fn person(&self, org: OrganizationId, role: MembershipRole) -> UserAccountId {
        let user = self
            .identity
            .create_user(org, None, None, None, Some(role.as_str()))
            .await
            .unwrap()
            .id;
        self.identity
            .add_organization_membership(org, user, role)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
             SELECT $1, $2, $3, $4, true
             WHERE NOT EXISTS (SELECT 1 FROM gateway.admin_role_binding
                               WHERE organization_id = $2 AND user_id = $3)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(AdminRole::from_membership(role).as_str())
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
        caller: &Caller<'_>,
        key: Option<&str>,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        builder = match caller {
            Caller::Session(session) => builder
                .header(
                    COOKIE,
                    format!(
                        "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                        session.0, session.1
                    ),
                )
                .header("origin", ORIGIN)
                .header("sec-fetch-site", "same-origin"),
            Caller::Token(token) => builder.header(AUTHORIZATION, format!("Bearer {token}")),
            Caller::Both(session, authorization) => builder
                .header(
                    COOKIE,
                    format!(
                        "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={}",
                        session.0, session.1
                    ),
                )
                .header(AUTHORIZATION, *authorization),
        };
        if let Some(key) = key {
            builder = builder.header("idempotency-key", key);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn agents_uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/agents{path}", org.0)
}

async fn audit_actions(pool: &PgPool, org: OrganizationId) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type IN ('agent', 'agent_token')
           AND outcome = 'succeeded'
         ORDER BY occurred_at, id",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn an_owner_creates_an_agent_whose_token_drives_the_admin_api() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("agents").await;
    let (other, _) = harness.org("elsewhere").await;
    let owner = harness.person(org, MembershipRole::Owner).await;
    let session = harness.session(owner).await;
    let as_owner = Caller::Session(&session);

    let (status, agent) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_owner,
            Some("agent-1"),
            Some(&json!({"name": "ci", "role": "admin", "description": "deploys"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    assert_eq!(agent["name"], "ci");
    assert_eq!(agent["role"], "admin");
    assert_eq!(agent["enabled"], true);
    let agent_id = agent["id"].as_str().unwrap().to_owned();

    // Replaying the request returns the same agent; reusing the key for
    // something else conflicts; a second agent may not take the name.
    let (status, replay) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_owner,
            Some("agent-1"),
            Some(&json!({"name": "ci", "role": "admin", "description": "deploys"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["id"], agent_id.as_str());
    let (status, _) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_owner,
            Some("agent-1"),
            Some(&json!({"name": "cd", "role": "admin"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, taken) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_owner,
            Some("agent-2"),
            Some(&json!({"name": "ci", "role": "auditor"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(taken["code"], "agent_name_taken");

    let token_uri = agents_uri(org, &format!("/{agent_id}/tokens"));
    let (status, created) = harness
        .send(
            Method::POST,
            &token_uri,
            &as_owner,
            Some("token-1"),
            Some(&json!({"name": "pipeline", "expires_in_days": 30})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("gwa_"));
    assert_eq!(created["public_prefix"], &token[..12]);
    assert_eq!(created["status"], "active");
    assert_eq!(created["replay"], false);
    // The token is shown once: a replay carries only its metadata.
    let (status, replay) = harness
        .send(
            Method::POST,
            &token_uri,
            &as_owner,
            Some("token-1"),
            Some(&json!({"name": "pipeline", "expires_in_days": 30})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["id"], created["id"]);
    assert_eq!(replay["token"], Value::Null);
    assert_eq!(replay["replay"], true);
    // Only its digest is stored.
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.agent_token
         WHERE agent_user_id = $1 AND encode(token_digest, 'escape') LIKE '%gwa_%'",
    )
    .bind(Uuid::parse_str(&agent_id).unwrap())
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(stored, 0);

    // The token finds its organization and acts in it, on other lanes too.
    let as_agent = Caller::Token(&token);
    let (status, me) = harness
        .send(Method::GET, "/api/admin/v1/me", &as_agent, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user_id"], agent_id.as_str());
    let (status, organizations) = harness
        .send(
            Method::GET,
            "/api/admin/v1/organizations",
            &as_agent,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(organizations.as_array().unwrap().len(), 1);
    assert_eq!(organizations[0]["id"], org.0.to_string());
    let (status, listed) = harness
        .send(Method::GET, &agents_uri(org, ""), &as_agent, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["tokens"][0]["name"], "pipeline");
    assert!(listed["items"][0]["tokens"][0].get("token").is_none());
    let (status, invitations) = harness
        .send(
            Method::GET,
            &format!("/api/admin/v1/organizations/{}/invitations", org.0),
            &as_agent,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{invitations}");
    // Writes need no browser headers: there is no cookie to ride.
    let (status, peer) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_agent,
            Some("agent-3"),
            Some(&json!({"name": "reporter", "role": "auditor"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{peer}");

    // An admin agent may not create an owner, and nothing outside its
    // organization exists for it.
    let (status, denied) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_agent,
            Some("agent-4"),
            Some(&json!({"name": "root", "role": "owner"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "role_escalation_denied");
    for uri in [
        agents_uri(other, ""),
        format!("/api/admin/v1/organizations/{}/invitations", other.0),
    ] {
        let (status, _) = harness.send(Method::GET, &uri, &as_agent, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    // Revoking the token, then disabling the agent, ends its access.
    let token_id = created["id"].as_str().unwrap();
    let (status, revoked) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{agent_id}/tokens/{token_id}")),
            &as_owner,
            Some("revoke-1"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revoked["status"], "revoked");
    let (status, _) = harness
        .send(Method::GET, &agents_uri(org, ""), &as_agent, None, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, second) = harness
        .send(
            Method::POST,
            &token_uri,
            &as_owner,
            Some("token-2"),
            Some(&json!({"name": "pipeline-2"})),
        )
        .await;
    let second = second["token"].as_str().unwrap().to_owned();
    let (status, _) = harness
        .send(
            Method::GET,
            &agents_uri(org, ""),
            &Caller::Token(&second),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, disabled) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{agent_id}")),
            &as_owner,
            Some("disable-1"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disabled["enabled"], false);
    assert!(
        disabled["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .all(|token| token["status"] == "revoked")
    );
    let (status, _) = harness
        .send(
            Method::GET,
            "/api/admin/v1/me",
            &Caller::Token(&second),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, refused) = harness
        .send(
            Method::POST,
            &token_uri,
            &as_owner,
            Some("token-3"),
            Some(&json!({"name": "again"})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(refused["code"], "agent_disabled");

    assert_eq!(
        audit_actions(&harness.pool, org).await,
        [
            "agent.created",
            "agent_token.created",
            "agent.created",
            "agent_token.revoked",
            "agent_token.created",
            "agent.disabled",
        ]
    );
}

#[tokio::test]
async fn members_below_admin_cannot_manage_agents_and_tokens_must_be_well_formed() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("agents-rbac").await;
    let auditor = harness.person(org, MembershipRole::Auditor).await;
    let session = harness.session(auditor).await;
    let as_auditor = Caller::Session(&session);

    let (status, _) = harness
        .send(Method::GET, &agents_uri(org, ""), &as_auditor, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_auditor,
            Some("agent-1"),
            Some(&json!({"name": "ci", "role": "read-only"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let owner = harness.person(org, MembershipRole::Owner).await;
    let session = harness.session(owner).await;
    for body in [
        json!({"name": " ci", "role": "admin"}),
        json!({"name": "ci", "role": "superuser"}),
        json!({"name": "", "role": "admin"}),
    ] {
        let (status, problem) = harness
            .send(
                Method::POST,
                &agents_uri(org, ""),
                &Caller::Session(&session),
                Some(&Uuid::new_v4().to_string()),
                Some(&body),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(problem["code"], "invalid_agent");
    }

    for token in [
        "gwa_".to_owned(),
        format!("gwa_{}", "A".repeat(43)),
        "gw_notanagenttoken".to_owned(),
    ] {
        let (status, _) = harness
            .send(
                Method::GET,
                &agents_uri(org, ""),
                &Caller::Token(&token),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token}");
    }
}

#[tokio::test]
async fn the_cli_bootstraps_an_agent_once_and_mints_a_token_each_run() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let store = AgentStore::new(harness.pool.clone());
    let slug = format!("agents-cli-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let input = AgentTokenInput {
        name: "cli".into(),
        expires_in_days: Some(7),
        service_account_id: None,
    };

    let first = store
        .bootstrap(
            &slug,
            Some("CLI"),
            "operator",
            MembershipRole::Owner,
            &input,
        )
        .await
        .unwrap();
    assert!(first.created_agent);
    let second = store
        .bootstrap(&slug, None, "operator", MembershipRole::Owner, &input)
        .await
        .unwrap();
    assert!(!second.created_agent);
    assert_eq!(second.agent_id, first.agent_id);
    assert_eq!(second.organization_id, first.organization_id);
    assert_ne!(second.token.as_str(), first.token.as_str());

    // The first token owns the organization it was made for.
    let org = first.organization_id;
    let (status, listed) = harness
        .send(
            Method::GET,
            &agents_uri(org, ""),
            &Caller::Token(first.token.as_str()),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"][0]["role"], "owner");
    assert_eq!(listed["items"][0]["tokens"].as_array().unwrap().len(), 2);
    let created_by: Option<Uuid> =
        sqlx::query_scalar("SELECT created_by FROM gateway.agent WHERE user_id = $1")
            .bind(first.agent_id)
            .fetch_one(&harness.pool)
            .await
            .unwrap();
    assert_eq!(created_by, None);
}

#[tokio::test]
async fn cli_agents_are_audited_and_managed_through_the_api() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let store = AgentStore::new(harness.pool.clone());
    let slug = format!(
        "agents-cli-api-{}",
        &Uuid::new_v4().simple().to_string()[..12]
    );
    let input = AgentTokenInput {
        name: "cli".into(),
        expires_in_days: None,
        service_account_id: None,
    };
    let operator = store
        .bootstrap(&slug, None, "operator", MembershipRole::Owner, &input)
        .await
        .unwrap();
    let org = operator.organization_id;
    let worker = store
        .bootstrap(&slug, None, "worker", MembershipRole::Admin, &input)
        .await
        .unwrap();
    // Both events of one run share its transaction's timestamp.
    let mut actions = audit_actions(&harness.pool, org).await;
    actions.sort_unstable();
    assert_eq!(
        actions,
        [
            "agent.created",
            "agent.created",
            "agent_token.created",
            "agent_token.created",
        ]
    );
    let system: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type IN ('agent', 'agent_token')
           AND actor_type = 'system'",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(system, 4);

    // The operator's agent revokes the CLI token and disables the agent the
    // CLI made, through the API.
    let as_operator = Caller::Token(operator.token.as_str());
    let (_, listed) = harness
        .send(Method::GET, &agents_uri(org, ""), &as_operator, None, None)
        .await;
    let worker_view = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["name"] == "worker")
        .unwrap()
        .clone();
    let token_id = worker_view["tokens"][0]["id"].as_str().unwrap();
    let (status, revoked) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{}/tokens/{token_id}", worker.agent_id)),
            &as_operator,
            Some("revoke-cli"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["status"], "revoked");
    let (status, disabled) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{}", worker.agent_id)),
            &as_operator,
            Some("disable-cli"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert_eq!(disabled["enabled"], false);
    // Its membership and role binding are off with it.
    let (membership, bindings): (bool, i64) = sqlx::query_as(
        "SELECT m.enabled,
                (SELECT count(*) FROM gateway.admin_role_binding b
                 WHERE b.organization_id = m.organization_id AND b.user_id = m.user_id
                   AND b.enabled)
         FROM gateway.organization_membership m
         WHERE m.organization_id = $1 AND m.user_id = $2",
    )
    .bind(org.0)
    .bind(worker.agent_id)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(!membership);
    assert_eq!(bindings, 0);

    // The last owner cannot disable itself.
    let (status, refused) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{}", operator.agent_id)),
            &as_operator,
            Some("disable-self"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "last_owner_required");
}

#[tokio::test]
async fn an_agents_extra_role_bindings_count_toward_the_ceiling() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("agents-ceiling").await;
    let owner = harness.person(org, MembershipRole::Owner).await;
    let owner_session = harness.session(owner).await;
    let admin = harness.person(org, MembershipRole::Admin).await;
    let admin_session = harness.session(admin).await;

    let (status, agent) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &Caller::Session(&admin_session),
            Some("agent-1"),
            Some(&json!({"name": "ci", "role": "admin"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let agent_id = Uuid::parse_str(agent["id"].as_str().unwrap()).unwrap();
    // An owner grants the admin agent an owner binding beyond its membership.
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
         VALUES ($1, $2, $3, 'organization_owner', true)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .bind(agent_id)
    .execute(&harness.pool)
    .await
    .unwrap();

    let tokens = agents_uri(org, &format!("/{agent_id}/tokens"));
    let (status, denied) = harness
        .send(
            Method::POST,
            &tokens,
            &Caller::Session(&admin_session),
            Some("token-admin"),
            Some(&json!({"name": "escalate"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "role_escalation_denied");
    let (status, denied) = harness
        .send(
            Method::DELETE,
            &agents_uri(org, &format!("/{agent_id}")),
            &Caller::Session(&admin_session),
            Some("disable-admin"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denied["code"], "role_escalation_denied");
    let (status, _) = harness
        .send(
            Method::POST,
            &tokens,
            &Caller::Session(&owner_session),
            Some("token-owner"),
            Some(&json!({"name": "allowed"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_bearer_credential_never_falls_back_to_session_cookies() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, slug) = harness.org("agents-bearer").await;
    let (other, _) = harness.org("agents-bearer-other").await;
    // A person who owns another organization, and an agent of the first.
    let person = harness.person(other, MembershipRole::Owner).await;
    let session = harness.session(person).await;
    let agent = AgentStore::new(harness.pool.clone())
        .bootstrap(
            &slug,
            None,
            "ci",
            MembershipRole::Auditor,
            &AgentTokenInput {
                name: "cli".into(),
                expires_in_days: None,
                service_account_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(agent.organization_id, org);
    let other_agents = agents_uri(other, "");

    // The cookies alone reach the person's organization.
    let (status, _) = harness
        .send(
            Method::GET,
            &other_agents,
            &Caller::Session(&session),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // With a valid token, oddly spaced, the request is the agent's: the
    // person's organization does not exist for it.
    let spaced = format!("Bearer  {} ", agent.token.as_str());
    let (status, _) = harness
        .send(
            Method::GET,
            &other_agents,
            &Caller::Both(&session, &spaced),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, me) = harness
        .send(
            Method::GET,
            "/api/admin/v1/me",
            &Caller::Both(&session, &spaced),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["user_id"], agent.agent_id.to_string());
    // Any bearer credential that is not a live agent token is refused, never
    // retried as the session.
    for authorization in [
        "Bearer  gwa_wrong",
        "Bearer gw_0123456789abcdef0123",
        "Bearer",
        "bearer\tgwa_",
    ] {
        let (status, _) = harness
            .send(
                Method::GET,
                &other_agents,
                &Caller::Both(&session, authorization),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization}");
    }
}

#[tokio::test]
async fn a_token_limited_to_a_service_account_reaches_only_its_end_users() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let harness = Harness::new(&database_url).await;
    let (org, _) = harness.org("app-agents").await;
    let (other_org, _) = harness.org("app-elsewhere").await;
    let owner = harness.person(org, MembershipRole::Owner).await;
    let other_owner = harness.person(other_org, MembershipRole::Owner).await;
    let session = harness.session(owner).await;
    let as_owner = Caller::Session(&session);
    let app = harness
        .identity
        .create_service_account(org, owner, None, "chat-app")
        .await
        .unwrap();
    let other_app = harness
        .identity
        .create_service_account(org, owner, None, "search-app")
        .await
        .unwrap();
    let foreign_app = harness
        .identity
        .create_service_account(other_org, other_owner, None, "foreign-app")
        .await
        .unwrap();
    let (status, agent) = harness
        .send(
            Method::POST,
            &agents_uri(org, ""),
            &as_owner,
            Some("app-agent"),
            Some(&json!({"name": "chat-backend", "role": "admin"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let tokens = agents_uri(org, &format!("/{}/tokens", agent["id"].as_str().unwrap()));

    // Only a service account of the organization.
    for (key, account) in [("foreign", foreign_app.id.0), ("unknown", Uuid::new_v4())] {
        let (status, problem) = harness
            .send(
                Method::POST,
                &tokens,
                &as_owner,
                Some(key),
                Some(&json!({"name": "backend", "service_account_id": account})),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{problem}");
    }
    let (status, created) = harness
        .send(
            Method::POST,
            &tokens,
            &as_owner,
            Some("scoped"),
            Some(&json!({"name": "backend", "service_account_id": app.id.0})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["service_account_id"], app.id.0.to_string());
    let token = created["token"].as_str().unwrap().to_owned();
    let as_app = Caller::Token(&token);

    // Its own account's end users, plans and settings; nothing else, even
    // what the agent's role allows.
    let base = |account: Uuid| {
        format!(
            "/api/admin/v1/organizations/{}/service-accounts/{account}",
            org.0
        )
    };
    for path in ["/end-users", "/end-user-plans"] {
        let (status, body) = harness
            .send(
                Method::GET,
                &format!("{}{path}", base(app.id.0)),
                &as_app,
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    for uri in [
        format!("{}/end-users", base(other_app.id.0)),
        base(app.id.0),
        format!("/api/admin/v1/organizations/{}/service-accounts", org.0),
        agents_uri(org, ""),
    ] {
        let (status, problem) = harness.send(Method::GET, &uri, &as_app, None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {problem}");
        assert_eq!(problem["error"], "service_account_token_scope", "{uri}");
    }
    let (status, _) = harness
        .send(
            Method::GET,
            &format!("{}/end-users", base(foreign_app.id.0))
                .replace(&org.0.to_string(), &other_org.0.to_string()),
            &as_app,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The same agent's token for the whole organization still reaches it.
    let (status, created) = harness
        .send(
            Method::POST,
            &tokens,
            &as_owner,
            Some("whole"),
            Some(&json!({"name": "ops"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created["service_account_id"].is_null());
    let whole = created["token"].as_str().unwrap().to_owned();
    let (status, _) = harness
        .send(
            Method::GET,
            &format!("{}/end-users", base(other_app.id.0)),
            &Caller::Token(&whole),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}
