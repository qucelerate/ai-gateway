#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Batch administration (D-021, Phase 7 milestone 6) through the real
//! router: the organization's batch settings and connection staging (RBAC,
//! validation, versioning, idempotency, audit, soft delete), and its
//! batches as Usage -> Batches shows them, with cancel and results.
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
    http::{HeaderMap, Method, StatusCode, header::COOKIE},
};
use gateway::{
    CatalogManifest, CatalogStore, OrganizationId, UserAccountId,
    agent_tools::{args::Args, client::Client, commands},
    application::trace_objects::{MemoryTraceObjects, TraceObjects},
    domain::{
        agents::AgentTokenInput, auth::AuthCrypto, identity::MembershipRole, keys::ApiKeyPepper,
    },
    persistence::{
        AdminStore, AuthStore, IdentityStore, agents::AgentStore,
        batch_settings::BatchSettingsStore, batches::BatchStore,
    },
    transport::batch_admin::router,
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
const TEST_PEPPER: &[u8] = b"batch-admin-integration-pepper-0";
const HMAC_KEY: &[u8] = b"batch-admin-integration-hmac-k0032";

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

struct Harness {
    pool: PgPool,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
    objects: Arc<dyn TraceObjects>,
}

struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Response {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

impl Harness {
    async fn new(database_url: &str) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(StdDuration::from_secs(5))
            .connect(database_url)
            .await
            .unwrap_or_else(|error| panic!("GATEWAY_TEST_DATABASE_URL is unreachable: {error}"));
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        let identity =
            IdentityStore::new(pool.clone(), ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap());
        let crypto = AuthCrypto::new(HMAC_KEY, &[41_u8; 32]).unwrap();
        let objects: Arc<dyn TraceObjects> = Arc::new(MemoryTraceObjects::default());
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            BatchSettingsStore::new(pool.clone()),
            BatchStore::new(pool.clone()),
            Some(objects.clone()),
        );
        Self {
            pool,
            identity,
            crypto,
            router,
            objects,
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

    /// A user with one admin role binding: org-wide, or `team_admin` of a
    /// new team when `role` is `team_admin`.
    async fn user(&self, org: OrganizationId, role: &str) -> UserAccountId {
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
        let team = if role == "team_admin" {
            let team = self.identity.create_team(org, "team").await.unwrap().id;
            self.identity
                .add_team_membership(org, team, user, MembershipRole::Admin)
                .await
                .unwrap();
            Some(team.0)
        } else {
            None
        };
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(user.0)
        .bind(team)
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
        user
    }

    async fn session(&self, user: UserAccountId) -> String {
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
        format!(
            "{SESSION_COOKIE}={}; {BROWSER_COOKIE}={browser}",
            session.token
        )
    }

    async fn as_role(&self, org: OrganizationId, role: &str) -> String {
        let user = self.user(org, role).await;
        self.session(user).await
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        cookie: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(COOKIE, cookie);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let response = self
            .router
            .clone()
            .oneshot(
                builder
                    .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Response {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn audit_actions(&self, org: OrganizationId, entity_type: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, outcome FROM gateway.audit_event
             WHERE organization_id = $1 AND entity_type = $2 ORDER BY occurred_at, id",
        )
        .bind(org.0)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// A catalog with a Bedrock and an OpenAI connection.
    async fn connections(&self, org: OrganizationId) {
        let manifest: CatalogManifest = serde_json::from_value(json!({
            "release": {"version": "batch-admin"},
            "providers": [{"adapter_instance_key": "bedrock-main", "family": "bedrock"},
                          {"adapter_instance_key": "openai-main", "family": "openai"}],
            "deployments": [], "prices": [], "routes": []
        }))
        .unwrap();
        CatalogStore::new(self.pool.clone())
            .apply(org, &manifest, manifest.digest().unwrap())
            .await
            .unwrap();
    }

    /// A batch of the organization's, `status`, with an output file whose
    /// content is `output`.
    async fn batch(&self, org: OrganizationId, status: &str, output: &[u8]) -> Uuid {
        let owner = self.user(org, "organization_owner").await;
        let account = self
            .identity
            .create_service_account(org, owner, None, "batch-admin")
            .await
            .unwrap()
            .id;
        let input = Uuid::new_v4();
        let out = Uuid::new_v4();
        for (file, purpose) in [(input, "batch"), (out, "batch_output")] {
            let key = format!("batch-files/{}/{file}", org.0);
            sqlx::query(
                "INSERT INTO gateway.batch_file
                     (id, organization_id, service_account_id, purpose, filename, bytes, sha256, object_key)
                 VALUES ($1, $2, $3, $4, 'f.jsonl', 1, $5, $6)",
            )
            .bind(file)
            .bind(org.0)
            .bind(account.0)
            .bind(purpose)
            .bind(vec![0_u8; 32])
            .bind(&key)
            .execute(&self.pool)
            .await
            .unwrap();
            self.objects.put(&key, output.to_vec()).await.unwrap();
        }
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.batch
                 (id, organization_id, service_account_id, idempotency_key, fingerprint, endpoint,
                  operation, input_file_id, output_file_id, completion_window, status, total_lines,
                  expires_at)
             VALUES ($1, $2, $3, $4, $5, '/v1/chat/completions', 'chat_batch', $6, $7, '24h', $8,
                     2, now() + interval '1 day')",
        )
        .bind(id)
        .bind(org.0)
        .bind(account.0)
        .bind(id.to_string())
        .bind(vec![0_u8; 32])
        .bind(input)
        .bind((status == "completed").then_some(out))
        .bind(status)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.batch_job
                 (id, organization_id, batch_id, ordinal, mode, deployment, model,
                  adapter_instance_key, family, state, next_run_at)
             VALUES ($1, $2, $3, 0, 'emulated', 'dep', 'm', 'openai-main', 'openai', $4, 'infinity')",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(id)
        .bind(if status == "completed" { "finished" } else { "pending" })
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }
}

fn admin_uri(org: OrganizationId, path: &str) -> String {
    format!("/api/admin/v1/organizations/{}/{path}", org.0)
}

#[tokio::test]
async fn emulated_batches_are_allowed_by_catalog_writers_and_versioned() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("batch-settings").await;
    let reader = harness.as_role(org, "read_only_member").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let uri = admin_uri(org, "batch-settings");

    // Never set: off.
    let unset = harness.send(Method::GET, &uri, &reader, &[], None).await;
    assert_eq!(unset.status, StatusCode::OK, "{}", unset.body);
    assert_eq!(unset.json()["emulated_allowed"], false);
    assert_eq!(unset.json()["emulated_concurrency"], 4);
    assert_eq!(unset.json()["version"], 0);

    let body = json!({"emulated_allowed": true, "emulated_concurrency": 8});
    let refused = harness
        .send(
            Method::PUT,
            &uri,
            &reader,
            &[("idempotency-key", "s1"), ("if-match", "*")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let invalid = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "s0"), ("if-match", "*")],
            Some(json!({"emulated_allowed": true, "emulated_concurrency": 99})),
        )
        .await;
    assert_eq!(
        invalid.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        invalid.body
    );

    let set = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "s1"), ("if-match", "*")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(set.status, StatusCode::CREATED, "{}", set.body);
    assert_eq!(set.json()["emulated_allowed"], true);
    assert_eq!(set.json()["version"], 1);
    assert_eq!(set.headers["etag"], "\"v1\"");
    // Replayed, not set again.
    let replay = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "s1"), ("if-match", "*")],
            Some(body),
        )
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json()["version"], 1);
    // A stale version is refused.
    let stale = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "s2"), ("if-match", "\"v7\"")],
            Some(json!({"emulated_allowed": false})),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let read = harness.send(Method::GET, &uri, &reader, &[], None).await;
    assert_eq!(read.json()["emulated_concurrency"], 8);
    // The refused attempt is audited too.
    assert_eq!(
        harness.audit_actions(org, "batch_settings").await,
        [
            ("batch_settings.set".to_owned(), "denied".to_owned()),
            ("batch_settings.set".to_owned(), "succeeded".to_owned())
        ]
    );
}

#[tokio::test]
async fn staging_is_set_for_bedrock_connections_by_credential_writers_and_soft_deleted() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("batch-staging").await;
    harness.connections(org).await;
    let reader = harness.as_role(org, "read_only_member").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let uri = admin_uri(org, "batch-staging/bedrock-main");
    let staging = json!({"bucket": "tenant-batches", "prefix": "gw/", "region": "us-east-1",
                         "role_arn": "arn:aws:iam::123456789012:role/gateway-batch"});

    let refused = harness
        .send(
            Method::PUT,
            &uri,
            &reader,
            &[("idempotency-key", "t1"), ("if-match", "*")],
            Some(staging.clone()),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    // S3 staging names its service role.
    let no_role = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "t0"), ("if-match", "*")],
            Some(json!({"bucket": "tenant-batches", "region": "us-east-1"})),
        )
        .await;
    assert_eq!(
        no_role.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        no_role.body
    );
    // A connection whose family stages nothing, and one that is not there.
    let openai = harness
        .send(
            Method::PUT,
            &admin_uri(org, "batch-staging/openai-main"),
            &admin,
            &[("idempotency-key", "t2"), ("if-match", "*")],
            Some(staging.clone()),
        )
        .await;
    assert_eq!(openai.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(openai.json()["code"], "batch_staging_unsupported");
    let missing = harness
        .send(
            Method::PUT,
            &admin_uri(org, "batch-staging/nowhere"),
            &admin,
            &[("idempotency-key", "t3"), ("if-match", "*")],
            Some(staging.clone()),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let set = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "t4"), ("if-match", "*")],
            Some(staging.clone()),
        )
        .await;
    assert_eq!(set.status, StatusCode::CREATED, "{}", set.body);
    assert_eq!(set.json()["kind"], "s3");
    assert_eq!(set.json()["version"], 1);
    let list = harness
        .send(
            Method::GET,
            &admin_uri(org, "batch-staging"),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(list.json()["data"][0]["connection"], "bedrock-main");
    assert_eq!(list.json()["data"][0]["role_arn"], staging["role_arn"]);

    let deleted = harness
        .send(
            Method::DELETE,
            &uri,
            &admin,
            &[("idempotency-key", "t5"), ("if-match", "\"v1\"")],
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
    let list = harness
        .send(
            Method::GET,
            &admin_uri(org, "batch-staging"),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(list.json()["data"], json!([]));
    // The row stays, hidden.
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.batch_staging WHERE organization_id = $1 AND deleted_at IS NOT NULL",
    )
    .bind(org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);
    let again = harness
        .send(
            Method::DELETE,
            &uri,
            &admin,
            &[("idempotency-key", "t6"), ("if-match", "*")],
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    // Set again: revived, its version going on.
    let revived = harness
        .send(
            Method::PUT,
            &uri,
            &admin,
            &[("idempotency-key", "t7"), ("if-match", "*")],
            Some(staging),
        )
        .await;
    assert_eq!(revived.status, StatusCode::OK, "{}", revived.body);
    assert_eq!(revived.json()["version"], 3);
    assert_eq!(
        harness.audit_actions(org, "batch_staging").await,
        [
            ("batch_staging.set".to_owned(), "denied".to_owned()),
            ("batch_staging.set".to_owned(), "succeeded".to_owned()),
            ("batch_staging.delete".to_owned(), "succeeded".to_owned()),
            ("batch_staging.set".to_owned(), "succeeded".to_owned()),
        ]
    );
}

#[tokio::test]
async fn usage_readers_list_batches_and_only_content_readers_download_results() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("batch-list").await;
    let other = harness.org("batch-list-other").await;
    let done = harness
        .batch(org, "completed", b"{\"custom_id\":\"a\"}\n")
        .await;
    let running = harness.batch(org, "in_progress", b"").await;
    let theirs = harness.batch(other, "in_progress", b"").await;
    let owner = harness.as_role(org, "organization_owner").await;
    let reader = harness.as_role(org, "read_only_member").await;

    let list = harness
        .send(
            Method::GET,
            &format!("{}?page_size=1", admin_uri(org, "batches")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    let page = list.json();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], format!("batch_{running}"));
    let next = harness
        .send(
            Method::GET,
            &format!(
                "{}?page_token={}",
                admin_uri(org, "batches"),
                page["next_page_token"].as_str().unwrap()
            ),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(next.json()["items"][0]["id"], format!("batch_{done}"));
    assert_eq!(next.json()["next_page_token"], Value::Null);
    let filtered = harness
        .send(
            Method::GET,
            &format!("{}?status=completed", admin_uri(org, "batches")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(filtered.json()["items"].as_array().unwrap().len(), 1);
    let bad = harness
        .send(
            Method::GET,
            &format!("{}?status=nope", admin_uri(org, "batches")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let detail = harness
        .send(
            Method::GET,
            &admin_uri(org, &format!("batches/batch_{done}")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.body);
    assert_eq!(detail.json()["gateway"]["jobs"][0]["mode"], "emulated");
    assert_eq!(
        detail.json()["gateway"]["jobs"][0]["price_source"],
        "on_demand"
    );
    assert_eq!(detail.json()["gateway"]["jobs"][0]["deployment"], "dep");
    // Another organization's batch is not found.
    let foreign = harness
        .send(
            Method::GET,
            &admin_uri(org, &format!("batches/batch_{theirs}")),
            &owner,
            &[],
            None,
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    // Results carry the model's answers: content readers only.
    let output = admin_uri(org, &format!("batches/batch_{done}/output"));
    let refused = harness.send(Method::GET, &output, &reader, &[], None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let downloaded = harness.send(Method::GET, &output, &owner, &[], None).await;
    assert_eq!(downloaded.status, StatusCode::OK, "{}", downloaded.body);
    assert_eq!(downloaded.headers["content-type"], "application/jsonl");
    assert_eq!(downloaded.body, "{\"custom_id\":\"a\"}\n");
    let none = harness
        .send(
            Method::GET,
            &admin_uri(org, &format!("batches/batch_{done}/errors")),
            &owner,
            &[],
            None,
        )
        .await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
    assert_eq!(none.json()["code"], "batch_results_unavailable");
}

#[tokio::test]
async fn a_page_of_the_largest_size_says_whether_another_follows() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("batch-full-page").await;
    let first = harness.batch(org, "in_progress", b"").await;
    // 100 more of the same, older than it: 101 in all.
    sqlx::query(
        "INSERT INTO gateway.batch
             (id, organization_id, service_account_id, idempotency_key, fingerprint, endpoint,
              operation, input_file_id, completion_window, status, total_lines, expires_at,
              created_at)
         SELECT gen_random_uuid(), organization_id, service_account_id, 'copy-' || n, fingerprint,
                endpoint, operation, input_file_id, completion_window, status, total_lines,
                expires_at, created_at - make_interval(secs => n)
         FROM gateway.batch, generate_series(1, 100) AS n
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(org.0)
    .bind(first)
    .execute(&harness.pool)
    .await
    .unwrap();
    let reader = harness.as_role(org, "read_only_member").await;
    let page = harness
        .send(
            Method::GET,
            &format!("{}?page_size=100", admin_uri(org, "batches")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let page = page.json();
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    let token = page["next_page_token"]
        .as_str()
        .expect("another page follows");
    let last = harness
        .send(
            Method::GET,
            &format!(
                "{}?page_size=100&page_token={token}",
                admin_uri(org, "batches")
            ),
            &reader,
            &[],
            None,
        )
        .await
        .json();
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert_eq!(last["next_page_token"], Value::Null);
    let over = harness
        .send(
            Method::GET,
            &format!("{}?page_size=101", admin_uri(org, "batches")),
            &reader,
            &[],
            None,
        )
        .await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "{}", over.body);
}

#[tokio::test]
async fn gwctl_downloads_a_one_line_output_file_as_it_came() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let slug = format!("batch-gwctl-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let agent = AgentStore::new(harness.pool.clone())
        .bootstrap(
            &slug,
            None,
            "ops",
            MembershipRole::Owner,
            &AgentTokenInput {
                name: "ci".into(),
                expires_in_days: Some(1),
                service_account_id: None,
            },
        )
        .await
        .unwrap();
    // One result is one JSON document too, which must not be reformatted.
    let file = b"{\"custom_id\":\"a\",\"response\":{\"status_code\":200}}\n";
    let batch = harness
        .batch(agent.organization_id, "completed", file)
        .await;
    let client = Client::local(harness.router.clone(), agent.token.as_str().to_owned());
    let org = agent.organization_id.0.to_string();
    let run = |words: Vec<String>| {
        let client = client.clone();
        async move {
            let commands::Plan::Call(call) =
                commands::plan(Args::parse(words).unwrap(), &|_| unreachable!()).unwrap()
            else {
                panic!("not a call");
            };
            client.run(call).await
        }
    };
    let words = |verb: &str| {
        ["batches", verb, &format!("batch_{batch}"), "--org", &org]
            .map(str::to_owned)
            .to_vec()
    };
    let Ok(output) = run(words("output")).await else {
        panic!("the output download failed");
    };
    assert_eq!(
        output,
        Value::String(String::from_utf8(file.to_vec()).unwrap())
    );
    // A JSON read is still parsed.
    let Ok(read) = run(words("get")).await else {
        panic!("the batch read failed");
    };
    assert_eq!(read["id"], format!("batch_{batch}"));
}

#[tokio::test]
async fn an_admin_cancel_is_idempotent_and_audited() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("batch-cancel").await;
    let running = harness.batch(org, "in_progress", b"").await;
    let reader = harness.as_role(org, "read_only_member").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let uri = admin_uri(org, &format!("batches/batch_{running}/cancel"));

    let refused = harness
        .send(
            Method::POST,
            &uri,
            &reader,
            &[("idempotency-key", "c1")],
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let cancelled = harness
        .send(
            Method::POST,
            &uri,
            &admin,
            &[("idempotency-key", "c1")],
            None,
        )
        .await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.body);
    assert_eq!(cancelled.json()["status"], "cancelling");
    let asked: bool = sqlx::query_scalar(
        "SELECT cancel_requested FROM gateway.batch_job WHERE organization_id = $1 AND batch_id = $2",
    )
    .bind(org.0)
    .bind(running)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(asked);
    let replay = harness
        .send(
            Method::POST,
            &uri,
            &admin,
            &[("idempotency-key", "c1")],
            None,
        )
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json()["status"], "cancelling");
    let missing = harness
        .send(
            Method::POST,
            &admin_uri(org, &format!("batches/batch_{}/cancel", Uuid::new_v4())),
            &admin,
            &[("idempotency-key", "c2")],
            None,
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        harness.audit_actions(org, "batch").await,
        [
            ("batch.cancelled".to_owned(), "denied".to_owned()),
            ("batch.cancelled".to_owned(), "succeeded".to_owned())
        ]
    );
}
