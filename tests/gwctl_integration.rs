#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for `gwctl`: the binary itself, run against the admin
//! router served on a local port and authenticated with an agent token made
//! the way `gateway agent create` makes one.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::{
    process::Command,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use gateway::{
    AppState, OrganizationId, UnavailableOidcBackend,
    config::OidcConfig,
    domain::{
        agents::AgentTokenInput, auth::AuthCrypto, credentials::CredentialCipher,
        identity::MembershipRole, keys::ApiKeyPepper,
    },
    jobs::egress::DeliveryPolicy,
    persistence::agents::AgentStore,
};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const TEST_PEPPER: &[u8] = b"gwctl-integration-pepper";
const HMAC_KEY: &[u8] = b"gwctl-integration-hmac-key-0000032";
const ORIGIN: &str = "https://gateway.gwctl.test";

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

/// The admin router on a local port, and an owner agent's token for a new
/// organization.
struct Served {
    pool: PgPool,
    url: String,
    org: OrganizationId,
    token: String,
}

async fn serve(database_url: &str) -> Served {
    let pool = connect_test_pool(database_url).await;
    gateway::persistence::migrations::run(&pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
    let state = AppState::for_sign_in_tests(
        pool.clone(),
        ApiKeyPepper::from_bytes(TEST_PEPPER).unwrap(),
        AuthCrypto::new(HMAC_KEY, &[41_u8; 32]).unwrap(),
        OidcConfig {
            enabled: true,
            origin: Some(ORIGIN.into()),
            issuer: Some("https://login.gwctl.test/v2.0".into()),
            audience: Some("gwctl-client".into()),
            client_id: Some("gwctl-client".into()),
            redirect_uri: Some(format!("{ORIGIN}/api/admin/v1/oidc/callback")),
            microsoft_graph_photo: false,
        },
        Arc::new(UnavailableOidcBackend),
        Some(CredentialCipher::new(&[47_u8; 32]).unwrap()),
        DeliveryPolicy::allow_private_targets_for_tests(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, gateway::transport::http::router(state))
            .await
            .unwrap();
    });
    let slug = format!("gwctl-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let agent = AgentStore::new(pool.clone())
        .bootstrap(
            &slug,
            Some("gwctl"),
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
    Served {
        pool,
        url,
        org: agent.organization_id,
        token: agent.token.as_str().to_owned(),
    }
}

/// Runs `gwctl` with the agent token: its exit code, standard output as
/// JSON (or null), and standard error.
async fn gwctl(served: &Served, args: &[&str]) -> (i32, Value, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gwctl"));
    command
        .args(args)
        .env_clear()
        .env("GATEWAY_URL", &served.url)
        .env("GATEWAY_AGENT_TOKEN", &served.token);
    let output = tokio::task::spawn_blocking(move || command.output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    (
        output.status.code().unwrap_or(-1),
        serde_json::from_str(&stdout).unwrap_or(Value::Null),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[tokio::test]
async fn an_agent_issues_and_revokes_an_api_key_with_gwctl_alone() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let served = serve(&database_url).await;

    // The token's only organization is used without --org.
    let (code, orgs, _) = gwctl(&served, &["orgs"]).await;
    assert_eq!(code, 0);
    assert_eq!(orgs[0]["id"], served.org.0.to_string());

    let (code, account, stderr) = gwctl(
        &served,
        &["service-accounts", "create", "--name", "consumer"],
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let account_id = account["id"].as_str().unwrap().to_owned();

    // Scopes as written anywhere else in the API: lowercase, kebab-case.
    let (code, key, stderr) = gwctl(
        &served,
        &[
            "keys",
            "create",
            &account_id,
            "--scopes",
            "inference,embeddings",
        ],
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        key["secret"].as_str().unwrap().starts_with("gw_"),
        "the new key's secret is printed once"
    );
    assert_eq!(
        key["metadata"]["scopes"],
        serde_json::json!(["Inference", "Embeddings"])
    );
    let key_id = key["metadata"]["id"].as_str().unwrap().to_owned();

    // Revoking needs If-Match: gwctl reads the key's version first.
    let (code, _, stderr) = gwctl(&served, &["keys", "revoke", &account_id, &key_id]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, listed, _) = gwctl(&served, &["keys", "list", &account_id, "--all"]).await;
    assert_eq!(code, 0);
    assert_eq!(listed["items"][0]["status"], "revoked");

    // Every change is audited under the agent.
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM gateway.audit_event
          WHERE organization_id = $1 AND action LIKE 'api_key.%' ORDER BY occurred_at",
    )
    .bind(served.org.0)
    .fetch_all(&served.pool)
    .await
    .unwrap();
    assert_eq!(actions.len(), 2, "{actions:?}");
}

#[tokio::test]
async fn gwctl_reports_problems_and_usage_errors_by_exit_code() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let served = serve(&database_url).await;

    let (code, _, stderr) = gwctl(&served, &["offers", "get", "no-such-account"]).await;
    assert_eq!(code, 1);
    let problem: Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(problem["status"], 404);

    let (code, _, stderr) = gwctl(&served, &["keys", "create", "sa"]).await;
    assert_eq!(code, 2);
    assert!(stderr.contains("--scopes is required"), "{stderr}");

    // A raw call, relative to the organization.
    let (code, listed, stderr) = gwctl(&served, &["api", "get", "agents"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(listed["items"][0]["name"], "ops");
}

#[tokio::test]
async fn an_mcp_client_issues_an_api_key_through_gwctl_mcp() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let served = serve(&database_url).await;

    let messages = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_owned(),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"create_service_account","arguments":{"name":"mcp-consumer"}}}"#.to_owned(),
    ];
    let responses = mcp_session(&served, &messages).await;
    assert_eq!(responses.len(), 3, "a notification gets no response");
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "qucelerate-gateway"
    );
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "create_api_key"));
    let created = &responses[2]["result"];
    assert_eq!(created["isError"], false);
    let account: Value =
        serde_json::from_str(created["content"][0]["text"].as_str().unwrap()).unwrap();
    let account_id = account["id"].as_str().unwrap();

    let call = serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "create_api_key", "arguments": { "service_account_id": account_id, "scopes": ["inference"] } },
    });
    let refused = serde_json::json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "get_automatic_offers", "arguments": { "adapter_instance_key": "missing" } },
    });
    let responses = mcp_session(&served, &[call.to_string(), refused.to_string()]).await;
    let key: Value = serde_json::from_str(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(key["secret"].as_str().unwrap().starts_with("gw_"));
    assert_eq!(responses[1]["result"]["isError"], true);

    let actor_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT u.kind FROM gateway.audit_event e JOIN gateway.user_account u ON u.id = e.actor_user_id
          WHERE e.organization_id = $1 AND e.action IN ('service_account.created', 'api_key.created')",
    )
    .bind(served.org.0)
    .fetch_all(&served.pool)
    .await
    .unwrap();
    assert_eq!(actor_kinds, ["agent", "agent"], "audited under the agent");
}

#[tokio::test]
async fn an_mcp_client_issues_and_revokes_an_api_key_over_http() {
    let Some(database_url) = test_database_url() else {
        return;
    };
    let _guard = database_guard();
    let served = serve(&database_url).await;
    let http = reqwest::Client::new();
    let endpoint = format!("{}/api/mcp", served.url);
    let post = |body: Value| {
        http.post(&endpoint)
            .bearer_auth(&served.token)
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&body)
    };
    let call = |id: i64, name: &str, arguments: Value| {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
    };
    let result = |response: &Value| -> Value {
        assert_eq!(response["result"]["isError"], false, "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let initialize = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } },
    });

    // Only an agent token is accepted, and the refusal says how to authenticate.
    let response = http.post(&endpoint).json(&initialize).send().await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.headers()["www-authenticate"], "Bearer");
    let response = http
        .post(&endpoint)
        .bearer_auth("gwa_not-a-token")
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"],
        "invalid_agent_token"
    );
    // A browser page on another origin cannot use it (DNS rebinding).
    let response = post(initialize.clone())
        .header("Origin", "https://elsewhere.test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = post(initialize.clone())
        .header("Origin", ORIGIN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = http
        .post(&endpoint)
        .bearer_auth(&served.token)
        .header("MCP-Protocol-Version", "1999-01-01")
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    // Stateless: no server-sent event stream to open.
    let response = http
        .get(&endpoint)
        .bearer_auth(&served.token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    let response =
        post(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), 202);

    let response: Value = post(initialize).send().await.unwrap().json().await.unwrap();
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");

    let account = result(
        &post(call(
            2,
            "create_service_account",
            serde_json::json!({ "name": "http-consumer" }),
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap(),
    );
    let account_id = account["id"].as_str().unwrap();
    let key = result(
        &post(call(
            3,
            "create_api_key",
            serde_json::json!({ "service_account_id": account_id, "scopes": ["inference"] }),
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap(),
    );
    assert!(key["secret"].as_str().unwrap().starts_with("gw_"));
    let key_id = key["metadata"]["id"].as_str().unwrap();

    // A chat through MCP is an inference request like any other, and is
    // counted as one (here refused: the model does not exist).
    let chats = || -> u64 {
        gateway::metrics::global()
            .render()
            .lines()
            .filter(|line| line.starts_with("gateway_inference_requests_total{operation=\"chat\""))
            .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
            .sum()
    };
    let before = chats();
    let chat: Value = post(call(
        6,
        "chat",
        serde_json::json!({ "model": "no-such-model", "message": "hi", "api_key": key["secret"] }),
    ))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(chat["result"]["isError"], true, "{chat}");
    assert_eq!(chats(), before + 1);

    // Revoking needs If-Match, which the tool looks up in process. A batch
    // answers in one array.
    let batch: Value = post(serde_json::json!([
        call(
            4,
            "revoke_api_key",
            serde_json::json!({ "service_account_id": account_id, "key_id": key_id })
        ),
        call(
            5,
            "chat",
            serde_json::json!({ "model": "any", "message": "hi" })
        ),
    ]))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let responses = batch.as_array().unwrap();
    let revoked = responses
        .iter()
        .find(|response| response["id"] == 4)
        .unwrap();
    assert_eq!(result(revoked)["status"], "revoked");
    // Inference needs an API key, which an HTTP caller passes as an argument.
    let chat = responses
        .iter()
        .find(|response| response["id"] == 5)
        .unwrap();
    assert_eq!(chat["result"]["isError"], true);
    assert!(
        chat["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("api_key")
    );

    // A raw DELETE needs If-Match too; without `if_match` the tool sends
    // the resource's current version.
    let removed: Value = post(call(
        7,
        "gateway_api",
        serde_json::json!({ "method": "DELETE", "path": format!("service-accounts/{account_id}") }),
    ))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    // It made no requests, so it is deleted (204) rather than disabled.
    assert_eq!(result(&removed), Value::Null);

    // A write-only path (no GET) is still sent, with no If-Match, so the
    // gateway says what it needs instead of the lookup failing with 405.
    let unmapped: Value = post(call(
        8,
        "gateway_api",
        serde_json::json!({ "method": "DELETE", "path": "catalog/price-mappings/no-such-deployment/no-such-source" }),
    ))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(unmapped["result"]["isError"], true);
    let problem: Value =
        serde_json::from_str(unmapped["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(problem["code"], "if_match_required", "{problem}");

    // The agent reads its own trail back, summarized.
    let summary: Value = post(call(
        9,
        "audit_summary",
        serde_json::json!({ "group_by": "action", "filters": { "q": "api_key" } }),
    ))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let summary = result(&summary);
    let counted: Vec<(String, i64)> = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["values"]["action"].as_str().unwrap().to_owned(),
                group["count"].as_i64().unwrap(),
            )
        })
        .collect();
    assert!(
        counted.contains(&("api_key.created".into(), 1)),
        "{summary}"
    );
    assert!(
        counted.contains(&("api_key.revoked".into(), 1)),
        "{summary}"
    );

    let actor_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT u.kind FROM gateway.audit_event e JOIN gateway.user_account u ON u.id = e.actor_user_id
          WHERE e.organization_id = $1
            AND e.action IN ('service_account.created', 'api_key.created', 'api_key.revoked',
                             'service_account.deleted')
          ORDER BY e.occurred_at",
    )
    .bind(served.org.0)
    .fetch_all(&served.pool)
    .await
    .unwrap();
    assert_eq!(
        actor_kinds,
        ["agent", "agent", "agent", "agent"],
        "audited under the agent"
    );
}

/// Sends `messages` to `gwctl mcp` and returns its responses in request
/// order (calls run concurrently, so they may arrive in any order).
async fn mcp_session(served: &Served, messages: &[String]) -> Vec<Value> {
    let input = messages.join("\n") + "\n";
    let mut command = Command::new(env!("CARGO_BIN_EXE_gwctl"));
    command
        .arg("mcp")
        .env_clear()
        .env("GATEWAY_URL", &served.url)
        .env("GATEWAY_AGENT_TOKEN", &served.token)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let output = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let mut responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    responses.sort_by_key(|response| response["id"].as_i64());
    responses
}
