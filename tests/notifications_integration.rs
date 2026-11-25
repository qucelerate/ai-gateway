#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for notifications: raising and resolving them, each
//! member's list, unread count and read state, and who sees what, through
//! the real router.
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
use gateway::{
    OrganizationId, UserAccountId,
    domain::{
        auth::AuthCrypto,
        identity::MembershipRole,
        keys::ApiKeyPepper,
        notifications::{NewNotification, NotificationKind},
    },
    persistence::{AdminStore, AuthStore, IdentityStore, notifications::NotificationStore},
    transport::notifications::router,
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
const TEST_PEPPER: &[u8] = b"notifications-integration-pepper";
const HMAC_KEY: &[u8] = b"notifications-integration-hmac-k0032";

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
    store: NotificationStore,
    identity: IdentityStore,
    crypto: AuthCrypto,
    router: Router,
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
        let router = router(
            AuthStore::new(pool.clone()),
            AdminStore::new(pool.clone()),
            Some(crypto.clone()),
            NotificationStore::new(pool.clone()),
        );
        Self {
            store: NotificationStore::new(pool.clone()),
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

    async fn get(&self, uri: &str, cookie: &str) -> Response {
        self.send(Method::GET, uri, cookie, &[], None).await
    }

    async fn read(&self, org: OrganizationId, cookie: &str, body: Value) -> Response {
        self.send(
            Method::POST,
            &format!("{}/read", base(org)),
            cookie,
            &[],
            Some(body),
        )
        .await
    }

    async fn unread(&self, org: OrganizationId, cookie: &str) -> u64 {
        let response = self
            .get(&format!("{}/unread-count", base(org)), cookie)
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.json()["unread"].as_u64().unwrap()
    }

    /// An active release, so the organization counts as using the catalog.
    async fn activate_catalog(&self, org: OrganizationId) {
        let release = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.catalog_release (id, organization_id, version, digest)
             VALUES ($1, $2, 'test', $3)",
        )
        .bind(release)
        .bind(org.0)
        .bind(Uuid::new_v4().as_bytes().repeat(2))
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.catalog_release_seal (organization_id, release_id)
             VALUES ($1, $2)",
        )
        .bind(org.0)
        .bind(release)
        .execute(&self.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.catalog_active_release (organization_id, release_id)
             VALUES ($1, $2)",
        )
        .bind(org.0)
        .bind(release)
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

fn base(org: OrganizationId) -> String {
    format!("/api/admin/v1/organizations/{}/notifications", org.0)
}

fn failing(account: &str) -> NewNotification {
    NewNotification::new(
        NotificationKind::ProviderAccountSyncFailing,
        account,
        format!("Listing {account}'s models failed"),
        "timeout",
    )
    .with_link("/admin/connections/discovery")
    .with_data(json!({ "adapter_instance_key": account }))
}

fn ids(response: &Response) -> Vec<String> {
    response.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn conditions_are_raised_once_per_subject_and_resolved() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("notify").await;
    let other = harness.org("notify-other").await;
    let reader = harness.as_role(org, "read_only_member").await;
    let now = chrono::Utc::now();

    // Raised twice: one notification, with the later text.
    let first = harness
        .store
        .raise(org, failing("aws-main"), now)
        .await
        .unwrap();
    let mut again = failing("aws-main");
    again.body = "access denied".into();
    let second = harness
        .store
        .raise(org, again, now + chrono::Duration::seconds(1))
        .await
        .unwrap();
    assert_eq!(first, second);
    harness
        .store
        .raise(org, failing("gcp-main"), now)
        .await
        .unwrap();
    // Another organization's is not this one's.
    harness
        .store
        .raise(other, failing("aws-main"), now)
        .await
        .unwrap();

    let list = harness.get(&base(org), &reader).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.headers["cache-control"], "no-store");
    let items = list.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2);
    let aws = items
        .iter()
        .find(|item| item["subject"] == "aws-main")
        .unwrap();
    assert_eq!(aws["kind"], "provider_account.sync_failing");
    assert_eq!(aws["severity"], "warning");
    assert_eq!(aws["body"], "access denied");
    assert_eq!(aws["link"], "/admin/connections/discovery");
    assert_eq!(aws["read"], false);
    assert!(aws["resolved_at"].is_null());
    assert_eq!(harness.unread(org, &reader).await, 2);

    // Resolved: still listed, no longer counted; raised again, a new one.
    assert!(
        harness
            .store
            .resolve(
                org,
                NotificationKind::ProviderAccountSyncFailing,
                "aws-main",
                now
            )
            .await
            .unwrap()
    );
    assert!(
        !harness
            .store
            .resolve(
                org,
                NotificationKind::ProviderAccountSyncFailing,
                "aws-main",
                now
            )
            .await
            .unwrap()
    );
    assert_eq!(harness.unread(org, &reader).await, 1);
    let third = harness
        .store
        .raise(org, failing("aws-main"), now)
        .await
        .unwrap();
    assert_ne!(third, first);
    assert_eq!(harness.unread(org, &reader).await, 2);

    // An event is kept once per subject and is never resolved.
    let adopted = || {
        NewNotification::new(
            NotificationKind::PriceAdopted,
            "release-1",
            "Prices adopted",
            "",
        )
    };
    let event = harness.store.raise(org, adopted(), now).await.unwrap();
    assert_eq!(
        harness.store.raise(org, adopted(), now).await.unwrap(),
        event
    );
    assert!(
        !harness
            .store
            .resolve(org, NotificationKind::PriceAdopted, "release-1", now)
            .await
            .unwrap()
    );
    assert_eq!(harness.unread(org, &reader).await, 3);

    // Filters.
    let warnings = harness
        .get(&format!("{}?severity=warning", base(org)), &reader)
        .await;
    assert_eq!(ids(&warnings).len(), 3);
    let events = harness
        .get(&format!("{}?kind=price.adopted", base(org)), &reader)
        .await;
    assert_eq!(ids(&events), [event.to_string()]);
    for bad in ["?kind=nope", "?severity=loud", "?read=maybe", "?unknown=1"] {
        let response = harness.get(&format!("{}{bad}", base(org)), &reader).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{bad}");
    }

    // Resolved everywhere, as a price source fetching again does.
    for target in [org, other] {
        harness.activate_catalog(target).await;
    }
    let source = NewNotification::new(
        NotificationKind::PriceSourceFailing,
        "aws-price-list:bedrock",
        "Price source aws-price-list:bedrock is failing",
        "timed_out",
    );
    assert!(
        harness
            .store
            .raise_for_catalog_organizations(source, now)
            .await
            .unwrap()
            >= 2
    );
    let source_items = harness
        .get(&format!("{}?kind=price_source.failing", base(org)), &reader)
        .await;
    assert_eq!(ids(&source_items).len(), 1);
    assert!(
        harness
            .store
            .resolve_everywhere(
                NotificationKind::PriceSourceFailing,
                "aws-price-list:bedrock",
                now
            )
            .await
            .unwrap()
            >= 2
    );
    let resolved = harness
        .get(&format!("{}?kind=price_source.failing", base(org)), &reader)
        .await;
    assert!(!resolved.json()["items"][0]["resolved_at"].is_null());
}

#[tokio::test]
async fn read_state_is_each_members_own_and_pages_are_stable() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("notify-read").await;
    let admin = harness.as_role(org, "organization_admin").await;
    let reader = harness.as_role(org, "read_only_member").await;
    let start = chrono::Utc::now();
    for n in 0..5 {
        harness
            .store
            .raise(
                org,
                failing(&format!("account-{n}")),
                start + chrono::Duration::seconds(n),
            )
            .await
            .unwrap();
    }

    // Two pages, newest first, then none.
    let first = harness
        .get(&format!("{}?page_size=3", base(org)), &admin)
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let token = first.json()["next_page_token"].as_str().unwrap().to_owned();
    let subjects: Vec<String> = first.json()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["subject"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(subjects, ["account-4", "account-3", "account-2"]);
    let second = harness
        .get(
            &format!("{}?page_size=3&page_token={token}", base(org)),
            &admin,
        )
        .await;
    assert_eq!(second.json()["items"].as_array().unwrap().len(), 2);
    assert!(second.json()["next_page_token"].is_null());
    // A token is bound to its filters and to the member.
    let other_filter = harness
        .get(
            &format!("{}?page_size=3&read=unread&page_token={token}", base(org)),
            &admin,
        )
        .await;
    assert_eq!(other_filter.status, StatusCode::BAD_REQUEST);
    let other_member = harness
        .get(
            &format!("{}?page_size=3&page_token={token}", base(org)),
            &reader,
        )
        .await;
    assert_eq!(other_member.status, StatusCode::BAD_REQUEST);

    // Up to the third newest: the three oldest are read, for the admin only.
    let all = ids(&harness.get(&base(org), &admin).await);
    let response = harness
        .read(org, &admin, json!({ "through": all[2] }))
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.json()["unread"], 2);
    assert_eq!(harness.unread(org, &reader).await, 5);
    // The mark never moves back.
    harness
        .read(org, &admin, json!({ "through": all[4] }))
        .await;
    assert_eq!(harness.unread(org, &admin).await, 2);
    // Chosen ones.
    let response = harness
        .read(org, &admin, json!({ "ids": [all[0], Uuid::new_v4()] }))
        .await;
    assert_eq!(response.json()["unread"], 1);
    let unread = harness
        .get(&format!("{}?read=unread", base(org)), &admin)
        .await;
    assert_eq!(ids(&unread), [all[1].clone()]);
    let read = harness
        .get(&format!("{}?read=read", base(org)), &admin)
        .await;
    assert_eq!(ids(&read).len(), 4);

    // Bad bodies.
    for (body, status) in [
        (json!({}), StatusCode::UNPROCESSABLE_ENTITY),
        (
            json!({ "through": all[0], "ids": [all[0]] }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (json!({ "ids": [] }), StatusCode::UNPROCESSABLE_ENTITY),
        (json!({ "through": Uuid::new_v4() }), StatusCode::NOT_FOUND),
        (
            json!({ "everything": true }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let response = harness.read(org, &admin, body.clone()).await;
        assert_eq!(response.status, status, "{body} {}", response.body);
    }
}

#[tokio::test]
async fn members_see_only_what_their_role_can() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let org = harness.org("notify-roles").await;
    let other = harness.org("notify-roles-other").await;
    let now = chrono::Utc::now();
    harness
        .store
        .raise(org, failing("aws-main"), now)
        .await
        .unwrap();
    let team_admin = harness.as_role(org, "team_admin").await;
    let outsider = harness.as_role(other, "organization_owner").await;

    // A team-scoped member holds no `catalog_read`: none of the catalog's.
    let list = harness.get(&base(org), &team_admin).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.json()["items"], json!([]));
    assert_eq!(harness.unread(org, &team_admin).await, 0);
    let id = ids(&harness
        .get(&base(org), &harness.as_role(org, "auditor").await)
        .await)[0]
        .clone();
    let response = harness
        .read(org, &team_admin, json!({ "through": id }))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    // Another organization's member: refused.
    let response = harness.get(&base(org), &outsider).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    // No session.
    let response = harness.get(&base(org), "").await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

/// Open notifications of `kind` in `org`, by subject.
async fn open_subjects(pool: &PgPool, org: OrganizationId, kind: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT subject FROM gateway.notification
         WHERE organization_id = $1 AND kind = $2 AND resolved_at IS NULL ORDER BY subject",
    )
    .bind(org.0)
    .bind(kind)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn created_events(pool: &PgPool, org: OrganizationId) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload FROM gateway.outbox_event
         WHERE organization_id = $1 AND event_type = 'notification.created'
         ORDER BY sequence",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn budgets_keys_tokens_and_webhooks_raise_and_resolve_their_conditions() {
    use gateway::persistence::{
        notification_conditions::NotificationConditions,
        webhooks::{ClaimedDelivery, WebhookStore},
    };
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let pool = &harness.pool;
    let conditions = NotificationConditions::new(pool.clone());
    let org = harness.org("conditions").await;
    let owner = harness.user(org, "organization_owner").await;
    let now = chrono::Utc::now();

    // A budget at 85% of this month's limit, and one at 10%.
    let account = harness
        .identity
        .create_service_account(org, owner, None, "batch-jobs")
        .await
        .unwrap();
    let hot = Uuid::new_v4();
    let cold = Uuid::new_v4();
    for (policy, scope, account_id) in [
        (hot, "organization", None),
        (cold, "service-account", Some(account.id.0)),
    ] {
        sqlx::query(
            "INSERT INTO gateway.budget_policy
                 (id, organization_id, scope_kind, service_account_id, currency, limit_minor,
                  period)
             VALUES ($1, $2, $3, $4, 'USD', 1000000, 'monthly')",
        )
        .bind(policy)
        .bind(org.0)
        .bind(scope)
        .bind(account_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, service_account_id, scope_kind, period_start, period_end,
                  currency, limit_minor, consumed_minor)
             VALUES ($1, $2, $3, $4, $5, $6, 'USD', 1000000, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(org.0)
        .bind(account_id)
        .bind(scope)
        .bind(now - chrono::Duration::days(1))
        .bind(now + chrono::Duration::days(20))
        .bind(if policy == hot { 850_000_i64 } else { 100_000 })
        .execute(pool)
        .await
        .unwrap();
    }

    // A key expiring in three days, one in thirty, and one already expired.
    let mut keys = Vec::new();
    for days in [3, 30] {
        let key = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.api_key
                 (id, organization_id, service_account_id, public_prefix, digest, scopes,
                  expires_at)
             VALUES ($1, $2, $3, $4, $5, ARRAY['inference'], $6)",
        )
        .bind(key)
        .bind(org.0)
        .bind(account.id.0)
        .bind(format!("gw_{}", &key.simple().to_string()[..12]))
        .bind(key.as_bytes().repeat(2))
        .bind(now + chrono::Duration::days(days))
        .execute(pool)
        .await
        .unwrap();
        keys.push(key);
    }

    // An agent whose token expires tomorrow.
    let agent = harness
        .identity
        .create_user(org, None, None, None, Some("agent"))
        .await
        .unwrap()
        .id;
    sqlx::query("UPDATE gateway.user_account SET kind = 'agent' WHERE id = $1")
        .bind(agent.0)
        .execute(pool)
        .await
        .unwrap();
    harness
        .identity
        .add_organization_membership(org, agent, MembershipRole::Member)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.agent (user_id, organization_id, name) VALUES ($1, $2, 'ops')",
    )
    .bind(agent.0)
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    let token = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.agent_token
             (id, organization_id, agent_user_id, name, token_digest, public_prefix, expires_at)
         VALUES ($1, $2, $3, 'deploy', $4, 'gwa_abcdefgh', $5)",
    )
    .bind(token)
    .bind(org.0)
    .bind(agent.0)
    .bind(Uuid::new_v4().as_bytes().repeat(2))
    .bind(now + chrono::Duration::hours(20))
    .execute(pool)
    .await
    .unwrap();

    conditions.check(now).await.unwrap();
    assert_eq!(
        open_subjects(pool, org, "budget.threshold").await,
        [hot.to_string()]
    );
    assert_eq!(
        open_subjects(pool, org, "api_key.expiring").await,
        [keys[0].to_string()]
    );
    assert_eq!(
        open_subjects(pool, org, "agent_token.expiring").await,
        [token.to_string()]
    );
    let events = created_events(pool, org).await;
    assert_eq!(events.len(), 3, "{events:?}");
    let budget = events
        .iter()
        .find(|event| event["kind"] == "budget.threshold")
        .unwrap();
    assert_eq!(budget["title"], "The organization's budget is at 85%");
    assert_eq!(budget["required_permission"], "budget_read");
    assert_eq!(budget["data"]["percent"], 85);

    // Checked again: nothing new is sent. The budget used up updates the
    // open notification rather than raising another.
    sqlx::query("UPDATE gateway.quota_bucket SET consumed_minor = 1000000 WHERE organization_id = $1 AND scope_kind = 'organization'")
        .bind(org.0)
        .execute(pool)
        .await
        .unwrap();
    conditions
        .check(now + chrono::Duration::minutes(5))
        .await
        .unwrap();
    assert_eq!(created_events(pool, org).await.len(), 3);
    let title: String = sqlx::query_scalar(
        "SELECT title FROM gateway.notification
         WHERE organization_id = $1 AND kind = 'budget.threshold' AND resolved_at IS NULL",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(title, "The organization's budget is used up");

    // The limit raised, the key revoked, the token past its expiry: all
    // resolved.
    sqlx::query("UPDATE gateway.budget_policy SET limit_minor = 100000000 WHERE id = $1")
        .bind(hot)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.quota_bucket SET limit_minor = 100000000 WHERE organization_id = $1 AND scope_kind = 'organization'")
        .bind(org.0)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE gateway.api_key SET status = 'revoked', revoked_at = now() WHERE id = $1")
        .bind(keys[0])
        .execute(pool)
        .await
        .unwrap();
    conditions
        .check(now + chrono::Duration::days(1))
        .await
        .unwrap();
    for kind in [
        "budget.threshold",
        "api_key.expiring",
        "agent_token.expiring",
    ] {
        assert!(open_subjects(pool, org, kind).await.is_empty(), "{kind}");
    }

    // A parked delivery raises `webhook.failing`, naming the host only; a
    // delivery that succeeds resolves it.
    let endpoint = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gateway.webhook_endpoint
             (id, organization_id, url, event_types, secret_key_id, secret_ciphertext)
         VALUES ($1, $2, 'https://hooks.example.com/services/T0/SECRET', $3, 'k', '\\x00')",
    )
    .bind(endpoint)
    .bind(org.0)
    .bind(vec!["notification.created"])
    .execute(pool)
    .await
    .unwrap();
    let webhooks = WebhookStore::new(pool.clone());
    let mut deliveries = Vec::new();
    for _ in 0..2 {
        let id = Uuid::new_v4();
        let event_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.webhook_delivery
                 (id, organization_id, endpoint_id, event_id, event_type, payload, occurred_at,
                  attempts)
             VALUES ($1, $2, $3, $4, 'notification.created', '{}', now(), 24)",
        )
        .bind(id)
        .bind(org.0)
        .bind(endpoint)
        .bind(event_id)
        .execute(pool)
        .await
        .unwrap();
        deliveries.push(ClaimedDelivery {
            id,
            organization_id: org.0,
            endpoint_id: endpoint,
            url: "https://hooks.example.com/services/T0/SECRET".into(),
            secret_key_id: "k".into(),
            secret_ciphertext: vec![0],
            event_id,
            event_type: "notification.created".into(),
            payload: json!({}),
            occurred_at: now,
            attempts: 24,
            run_attempt: 24,
        });
    }
    webhooks
        .record_failed(
            &deliveries[0],
            Some(503),
            "endpoint returned a non-success status",
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        open_subjects(pool, org, "webhook.failing").await,
        [endpoint.to_string()]
    );
    let (title, body): (String, String) = sqlx::query_as(
        "SELECT title, body FROM gateway.notification
         WHERE organization_id = $1 AND kind = 'webhook.failing'",
    )
    .bind(org.0)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(title, "Webhook deliveries to hooks.example.com are failing");
    assert!(!body.contains("SECRET"), "{body}");
    webhooks
        .record_delivered(&deliveries[1], chrono::Utc::now(), 200)
        .await
        .unwrap();
    assert!(open_subjects(pool, org, "webhook.failing").await.is_empty());

    // Failing again, then the endpoint disabled: the sweep resolves it.
    sqlx::query(
        "UPDATE gateway.webhook_delivery SET status = 'pending', delivered_at = NULL WHERE id = $1",
    )
    .bind(deliveries[1].id)
    .execute(pool)
    .await
    .unwrap();
    webhooks
        .record_failed(&deliveries[1], None, "connection failed", None)
        .await
        .unwrap();
    assert_eq!(open_subjects(pool, org, "webhook.failing").await.len(), 1);
    sqlx::query("UPDATE gateway.webhook_endpoint SET enabled = false WHERE id = $1")
        .bind(endpoint)
        .execute(pool)
        .await
        .unwrap();
    conditions
        .check(now + chrono::Duration::days(1))
        .await
        .unwrap();
    assert!(open_subjects(pool, org, "webhook.failing").await.is_empty());
    // Two openings, two events: a reopened condition is sent again.
    assert_eq!(
        created_events(pool, org)
            .await
            .iter()
            .filter(|event| event["kind"] == "webhook.failing")
            .count(),
        2
    );
}

#[tokio::test]
async fn end_users_low_on_credits_raise_and_resolve_their_condition() {
    use gateway::persistence::notification_conditions::NotificationConditions;
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let pool = &harness.pool;
    let conditions = NotificationConditions::new(pool.clone());
    let org = harness.org("credits").await;
    let owner = harness.user(org, "organization_owner").await;
    let now = chrono::Utc::now();
    let account = harness
        .identity
        .create_service_account(org, owner, None, "chat-app")
        .await
        .unwrap();
    let end_user = |external_id: &'static str| async move {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.end_user (id, organization_id, service_account_id, external_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(org.0)
        .bind(account.id.0)
        .bind(external_id)
        .execute(pool)
        .await
        .unwrap();
        id
    };
    // A grant's credits: `granted`, of which `spent`, live until `ends`.
    let grant = |user: Uuid, granted: i64, spent: i64, ends: chrono::DateTime<chrono::Utc>| async move {
        let bucket = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.quota_bucket
                 (id, organization_id, service_account_id, end_user_id, credit_grant_id,
                  scope_kind, period_start, period_end, currency, limit_minor, consumed_minor)
             VALUES ($1, $2, $3, $4, $5, 'end-user', $6, $7, 'USD', $8, $9)",
        )
        .bind(bucket)
        .bind(org.0)
        .bind(account.id.0)
        .bind(user)
        .bind(Uuid::new_v4())
        .bind(now - chrono::Duration::days(3))
        .bind(ends)
        .bind(granted)
        .bind(spent)
        .execute(pool)
        .await
        .unwrap();
        bucket
    };
    let later = now + chrono::Duration::days(30);
    // 15% left; 90% left; a grant spent and another 70% spent (15% of the
    // two); and credits that all expired.
    let low = end_user("customer-low").await;
    let low_bucket = grant(low, 1000, 850, later).await;
    let fine = end_user("customer-fine").await;
    grant(fine, 1000, 100, later).await;
    let two = end_user("customer-two").await;
    grant(two, 1000, 1000, later).await;
    grant(two, 1000, 700, later).await;
    let expired = end_user("customer-expired").await;
    grant(expired, 1000, 1000, now - chrono::Duration::hours(1)).await;

    conditions.check(now).await.unwrap();
    let mut expected = vec![low.to_string(), two.to_string()];
    expected.sort();
    assert_eq!(
        open_subjects(pool, org, "end_user.credits_low").await,
        expected
    );
    let events: Vec<Value> = created_events(pool, org)
        .await
        .into_iter()
        .filter(|event| event["kind"] == "end_user.credits_low")
        .collect();
    assert_eq!(events.len(), 2, "{events:?}");
    let event = events
        .iter()
        .find(|event| event["subject"] == low.to_string())
        .unwrap();
    assert_eq!(
        event["title"],
        "End user customer-low has 15% of their credits left"
    );
    assert_eq!(event["severity"], "warning");
    assert_eq!(event["required_permission"], "usage_read");
    assert_eq!(event["data"]["end_user"], "customer-low");
    assert_eq!(
        event["data"]["service_account_id"],
        account.id.0.to_string()
    );
    assert_eq!(event["data"]["balance_minor"], 150);
    assert_eq!(event["data"]["percent_left"], 15);

    // Spent in full: the open notification says so, and nothing is sent.
    sqlx::query("UPDATE gateway.quota_bucket SET consumed_minor = 1000 WHERE id = $1")
        .bind(low_bucket)
        .execute(pool)
        .await
        .unwrap();
    conditions
        .check(now + chrono::Duration::minutes(5))
        .await
        .unwrap();
    let title: String = sqlx::query_scalar(
        "SELECT title FROM gateway.notification
         WHERE organization_id = $1 AND kind = 'end_user.credits_low' AND subject = $2
           AND resolved_at IS NULL",
    )
    .bind(org.0)
    .bind(low.to_string())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(title, "End user customer-low has no credits left");
    let sent = created_events(pool, org)
        .await
        .into_iter()
        .filter(|event| event["kind"] == "end_user.credits_low")
        .count();
    assert_eq!(sent, 2);

    // More credits granted resolve it; a disabled service account resolves
    // its users' too.
    grant(low, 5000, 0, later).await;
    conditions
        .check(now + chrono::Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(
        open_subjects(pool, org, "end_user.credits_low").await,
        [two.to_string()]
    );
    sqlx::query("UPDATE gateway.service_account SET enabled = false WHERE id = $1")
        .bind(account.id.0)
        .execute(pool)
        .await
        .unwrap();
    conditions
        .check(now + chrono::Duration::minutes(15))
        .await
        .unwrap();
    assert!(
        open_subjects(pool, org, "end_user.credits_low")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn open_reconciliation_items_raise_one_notification_until_resolved() {
    use gateway::persistence::notification_conditions::NotificationConditions;
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let pool = &harness.pool;
    let conditions = NotificationConditions::new(pool.clone());
    let org = harness.org("reconciliation").await;
    let now = chrono::Utc::now();
    // A request whose cost is unknown, waiting for the provider's amount.
    let item = |reason: &'static str| async move {
        let reservation = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reservation
                 (id, organization_id, idempotency_key, fingerprint, state, reserved_minor,
                  expires_at, output_token_cap, input_price_minor, output_price_minor,
                  cached_input_price_minor, reasoning_price_minor, other_price_minor,
                  price_version, pricing_confidence, currency, deployment, model, trace_id)
             VALUES ($1, $2, $3, $4, 'unknown', 500, $5, 100, 1, 1, 1, 1, 1, 'v1', 'known',
                     'USD', 'deploy-a', 'model-a', $6)",
        )
        .bind(reservation)
        .bind(org.0)
        .bind(reservation.to_string())
        .bind(vec![7_u8; 32])
        .bind(now + chrono::Duration::minutes(5))
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_reconciliation (id, organization_id, reservation_id, reason)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(org.0)
        .bind(reservation)
        .bind(reason)
        .execute(pool)
        .await
        .unwrap();
        id
    };
    let title = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT title FROM gateway.notification
             WHERE organization_id = $1 AND kind = 'reconciliation.open' AND resolved_at IS NULL",
        )
        .bind(org.0)
        .fetch_optional(pool)
        .await
        .unwrap()
    };

    conditions.check(now).await.unwrap();
    assert_eq!(title().await, None);

    let first = item("provider-unknown").await;
    conditions.check(now).await.unwrap();
    assert_eq!(
        open_subjects(pool, org, "reconciliation.open").await,
        vec!["reconciliation".to_owned()]
    );
    assert_eq!(
        title().await.as_deref(),
        Some("1 request's cost needs the provider's amount")
    );
    let events: Vec<Value> = created_events(pool, org)
        .await
        .into_iter()
        .filter(|event| event["kind"] == "reconciliation.open")
        .collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["severity"], "action");
    assert_eq!(events[0]["required_permission"], "reconciliation_read");
    assert_eq!(events[0]["link"], "/admin/usage/reconciliation");

    // Another opens: the same notification counts both, and is not sent again.
    let second = item("bound-breach").await;
    conditions.check(now).await.unwrap();
    assert_eq!(
        title().await.as_deref(),
        Some("2 requests' costs need the provider's amount")
    );
    assert_eq!(
        created_events(pool, org)
            .await
            .iter()
            .filter(|event| event["kind"] == "reconciliation.open")
            .count(),
        1
    );

    // Both resolved: it resolves.
    sqlx::query(
        "UPDATE gateway.accounting_reconciliation SET status = 'resolved', resolved_at = $2
         WHERE id = ANY($1)",
    )
    .bind(vec![first, second])
    .bind(now)
    .execute(pool)
    .await
    .unwrap();
    conditions.check(now).await.unwrap();
    assert_eq!(title().await, None);
}
