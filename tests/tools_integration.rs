//! Tool calling through the inference API (D-032), against fake OpenAI and
//! Anthropic providers: definitions, calls and results reach each provider
//! in its own shape, calls come back in the contract's, unary and streamed;
//! a route that cannot take tools refuses them before anything is held or
//! sent; and the reservation bounds the tools.
//!
//! Needs `GATEWAY_TEST_DATABASE_URL` and `GATEWAY_TEST_DATA_PLANE_DATABASE_URL`
//! (as `data_plane_integration` does).

use std::sync::{Arc, Mutex};

use axum::{Json, Router, body::Body, extract::State, response::Response, routing::post};
use gateway::{
    ApiKeyPepper, AppConfig, CatalogManifest, CatalogStore, GatewayCommand, IdentityStore,
    OrganizationId,
    domain::identity::{ApiKeyScope, MembershipRole},
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

const PEPPER: &str = "tools-test-pepper-00000000000000000000";

fn database_urls() -> Option<(String, String)> {
    let app = std::env::var("GATEWAY_TEST_DATABASE_URL").ok();
    let data_plane = std::env::var("GATEWAY_TEST_DATA_PLANE_DATABASE_URL").ok();
    match (app, data_plane) {
        (Some(app), Some(data_plane)) => Some((app, data_plane)),
        _ if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!(
                "GATEWAY_TEST_DATABASE_URL and GATEWAY_TEST_DATA_PLANE_DATABASE_URL must be set in CI"
            )
        }
        _ => None,
    }
}

/// The bodies each fake provider received.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Value>>>);

impl Seen {
    fn bodies(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

fn json_response(value: Value) -> Response {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

/// OpenAI's Chat Completions: a request with tools and no tool results is
/// answered with two parallel calls (streamed as OpenAI streams them when
/// asked); any other with text.
async fn openai(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
    seen.0.lock().unwrap().push(body.clone());
    let answered = body["messages"]
        .as_array()
        .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
    let calls = body.get("tools").is_some() && !answered;
    if body["stream"] == true {
        let chunk = |delta: Value, finish: Value| {
            format!(
                "data: {}\n\n",
                json!({"id": "c", "object": "chat.completion.chunk", "model": body["model"],
                       "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
            )
        };
        let mut events = String::new();
        events += &chunk(json!({"role": "assistant", "content": null}), Value::Null);
        events += &chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                                   "function": {"name": "get_weather", "arguments": ""}}]}),
            Value::Null,
        );
        events += &chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":"}}]}),
            Value::Null,
        );
        events += &chunk(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "\"Paris\"}"}}]}),
            Value::Null,
        );
        events += &chunk(json!({}), json!("tool_calls"));
        events += &format!(
            "data: {}\n\n",
            json!({"id": "c", "object": "chat.completion.chunk", "model": body["model"],
                   "choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 9}})
        );
        events += "data: [DONE]\n\n";
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(events))
            .unwrap();
    }
    let message = if calls {
        json!({"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_a", "type": "function",
             "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
            {"id": "call_b", "type": "function",
             "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}]})
    } else {
        json!({"role": "assistant", "content": "Paris 18C, Rome 24C"})
    };
    json_response(json!({
        "id": "c", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "message": message,
                     "finish_reason": if calls { "tool_calls" } else { "stop" }}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 9},
    }))
}

/// Anthropic's Messages API: a request with tools is answered with a
/// `tool_use` block.
async fn anthropic(State(seen): State<Seen>, Json(body): Json<Value>) -> Response {
    seen.0.lock().unwrap().push(body.clone());
    json_response(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": body["model"],
        "content": [
            {"type": "text", "text": "Let me check."},
            {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
        ],
        "stop_reason": "tool_use", "stop_sequence": null,
        "usage": {"input_tokens": 400, "output_tokens": 30}
    }))
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

/// `tools` routes to a deployment set to take tools and `plain` to one
/// not; `mixed` to both, so it does not take them; `claude` to Claude,
/// whose input Anthropic counts.
fn manifest() -> CatalogManifest {
    let deployment = |name: &str, model: &str, key: &str, tools: bool, count: &str| {
        json!({
            "name": name, "model": model, "location": null, "adapter_instance_key": key,
            "capabilities": [
                {"operation": "chat", "max_input_bytes": 65536, "max_output_tokens": 64,
                 "max_tokens_per_byte": 1, "tokenizer": "configured",
                 "reservation_ttl_seconds": 60, "chat_input_count_mode": count, "tools": tools},
                {"operation": "chat_stream", "max_input_bytes": 65536, "max_output_tokens": 64,
                 "max_tokens_per_byte": 1, "tokenizer": "configured",
                 "reservation_ttl_seconds": 60, "chat_input_count_mode": count, "tools": tools}
            ]
        })
    };
    let price = |deployment: &str, operation: &str| {
        json!({
            "deployment": deployment, "operation": operation, "input_minor_per_token": 1,
            "output_minor_per_token": 2, "cached_input_minor_per_token": 0,
            "reasoning_minor_per_token": 0, "other_minor_per_token": 0, "currency": "USD",
            "source": "tools-test", "source_version": "v1", "confidence": "known"
        })
    };
    let mut prices = Vec::new();
    for deployment in ["gpt-tools", "gpt-plain", "claude-tools"] {
        for operation in ["chat", "chat_stream"] {
            prices.push(price(deployment, operation));
        }
    }
    let mut routes = Vec::new();
    for operation in ["chat", "chat_stream"] {
        routes.extend([
            json!({"alias": "tools", "operation": operation, "deployment": "gpt-tools"}),
            json!({"alias": "plain", "operation": operation, "deployment": "gpt-plain"}),
            json!({"alias": "mixed", "operation": operation, "deployment": "gpt-tools"}),
            json!({"alias": "mixed", "operation": operation, "deployment": "gpt-plain",
                   "priority": 2}),
            json!({"alias": "claude", "operation": operation, "deployment": "claude-tools"}),
        ]);
    }
    serde_json::from_value(json!({
        "release": {"version": "tools-1"},
        "providers": [
            {"adapter_instance_key": "openai-main", "family": "openai"},
            {"adapter_instance_key": "anthropic-main", "family": "anthropic"}
        ],
        "deployments": [
            deployment("gpt-tools", "gpt-tools", "openai-main", true, "unsupported"),
            deployment("gpt-plain", "gpt-plain", "openai-main", false, "unsupported"),
            deployment("claude-tools", "claude-test", "anthropic-main", true, "provider_api_exact")
        ],
        "prices": prices,
        "routes": routes
    }))
    .unwrap()
}

async fn seed(pool: &PgPool) -> (OrganizationId, String) {
    let identity = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(PEPPER.as_bytes()).unwrap(),
    );
    let suffix = Uuid::new_v4().simple().to_string();
    let org = identity
        .create_organization(&format!("tools-{suffix}"), "Tools test")
        .await
        .unwrap()
        .id;
    let user = identity
        .create_user(org, None, None, None, Some("organization_owner"))
        .await
        .unwrap()
        .id;
    identity
        .add_organization_membership(org, user, MembershipRole::Owner)
        .await
        .unwrap();
    let account = identity
        .create_service_account(org, user, None, "tools")
        .await
        .unwrap()
        .id;
    let bearer = identity
        .create_api_key(org, account, &[ApiKeyScope::Inference], None, user)
        .await
        .unwrap()
        .bearer;
    sqlx::query(
        "INSERT INTO gateway.quota_bucket
             (id, organization_id, scope_kind, period_start, period_end, currency, limit_minor)
         VALUES ($1, $2, 'organization', now() - interval '1 day', now() + interval '1 day',
                 'USD', 1000000000000)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.rate_limit_policy
             (id, organization_id, scope_kind, requests_per_minute, max_concurrent)
         VALUES ($1, $2, 'organization', 100000, 50)",
    )
    .bind(Uuid::new_v4())
    .bind(org.0)
    .execute(pool)
    .await
    .unwrap();
    let manifest = manifest();
    CatalogStore::new(pool.clone())
        .apply(org, &manifest, manifest.digest().unwrap())
        .await
        .unwrap();
    (org, bearer)
}

fn weather() -> Value {
    json!([{"type": "function", "function": {
        "name": "get_weather", "description": "Current weather in a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                       "required": ["city"]}}}])
}

struct Stack {
    base: String,
    bearer: String,
    client: reqwest::Client,
}

impl Stack {
    async fn post(&self, path: &str, body: Value) -> (u16, String) {
        let response = self
            .client
            .post(format!("{}/api/inference/v1/{path}", self.base))
            .bearer_auth(&self.bearer)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .json(&body)
            .send()
            .await
            .unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn chat(&self, body: Value) -> (u16, Value) {
        let (status, text) = self.post("chat/completions", body).await;
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

async fn reservations(pool: &PgPool, org: OrganizationId) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT r.deployment, t.input_bound
         FROM gateway.accounting_reservation r
         JOIN gateway.inference_trace t ON t.id = r.trace_id
         WHERE r.organization_id = $1 ORDER BY r.created_at",
    )
    .bind(org.0)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn tools_reach_each_provider_in_its_shape_and_calls_come_back() {
    let Some((app_url, data_plane_url)) = database_urls() else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_url)
        .await
        .unwrap();
    gateway::persistence::migrations::run(&pool).await.unwrap();
    let (org, bearer) = seed(&pool).await;

    let openai_seen = Seen::default();
    let anthropic_seen = Seen::default();
    let openai_base = serve(
        Router::new()
            .route("/v1/chat/completions", post(openai))
            .with_state(openai_seen.clone()),
    )
    .await;
    let anthropic_base = serve(
        Router::new()
            .route("/v1/messages", post(anthropic))
            .with_state(anthropic_seen.clone()),
    )
    .await;
    let vars = [
        ("DATABASE_URL", data_plane_url.clone()),
        ("GATEWAY_PROFILE", "data-plane".to_owned()),
        ("BIND_ADDR", "127.0.0.1:0".to_owned()),
        ("API_KEY_PEPPER", PEPPER.to_owned()),
        ("OPENAI_API_KEY", "sk-tools".to_owned()),
        ("OPENAI_BASE_URL", format!("{openai_base}/v1")),
        ("ANTHROPIC_API_KEY", "anthropic-tools".to_owned()),
        ("ANTHROPIC_BASE_URL", anthropic_base.clone()),
        (
            "GATEWAY_INSTANCE_ID",
            format!("tools-test-{}", Uuid::new_v4().simple()),
        ),
        ("GATEWAY_CONFIG_POLL_SECONDS", "1".to_owned()),
    ];
    let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let state = gateway::application::connect(&config)
        .await
        .unwrap()
        .with_trace_objects(Some(Arc::new(
            gateway::application::trace_objects::MemoryTraceObjects::default(),
        )));
    let stack = Stack {
        base: serve(gateway::transport::data_plane_router(state)).await,
        bearer,
        client: reqwest::Client::new(),
    };
    let ask = json!([{"role": "user", "content": "Weather in Paris and Rome?"}]);

    // Calls come back on the message, OpenAI-shaped, and flat on the choice.
    let (status, body) = stack
        .chat(
            json!({"model": "tools", "messages": ask, "tools": weather(),
                     "tool_choice": "required", "max_tokens": 32}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(
        choice["message"]["tool_calls"],
        json!([
            {"id": "call_a", "type": "function",
             "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}},
            {"id": "call_b", "type": "function",
             "function": {"name": "get_weather", "arguments": "{\"city\":\"Rome\"}"}}
        ])
    );
    assert_eq!(choice["tool_calls"][1]["name"], "get_weather");
    let sent = openai_seen.bodies().pop().unwrap();
    assert_eq!(sent["tools"], weather());
    assert_eq!(sent["tool_choice"], "required");
    // The reservation bounds the tool definitions and their allowance.
    let held = reservations(&pool, org).await;
    assert!(held[0].1 > 2048, "{held:?}");

    // The results continue the conversation, in OpenAI's shape.
    let mut conversation = ask.as_array().unwrap().clone();
    conversation.push(json!({"role": "assistant", "content": null,
                             "tool_calls": choice["message"]["tool_calls"]}));
    conversation.push(json!({"role": "tool", "tool_call_id": "call_a", "content": "18C"}));
    conversation.push(json!({"role": "tool", "tool_call_id": "call_b", "content": "24C"}));
    let (status, body) = stack
        .chat(json!({"model": "tools", "messages": conversation, "tools": weather()}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Paris 18C, Rome 24C"
    );
    assert!(body["choices"][0]["message"].get("tool_calls").is_none());
    let sent = openai_seen.bodies().pop().unwrap();
    assert_eq!(sent["messages"][1]["content"], Value::Null);
    assert_eq!(sent["messages"][1]["tool_calls"][0]["id"], "call_a");
    assert_eq!(
        sent["messages"][2],
        json!({"role": "tool", "content": "18C", "tool_call_id": "call_a"})
    );

    // Streamed calls arrive as tool_call.delta events.
    let (status, events) = stack
        .post(
            "chat/completions/stream",
            json!({"model": "tools", "messages": ask, "tools": weather()}),
        )
        .await;
    assert_eq!(status, 200, "{events}");
    let deltas = events
        .split("\n\n")
        .filter_map(|event| event.strip_prefix("event: tool_call.delta\ndata: "))
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(deltas[0]["id"], "call_a");
    assert_eq!(deltas[0]["name"], "get_weather");
    let arguments = deltas
        .iter()
        .map(|delta| delta["arguments_delta"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(arguments, "{\"city\":\"Paris\"}");
    assert!(
        events.contains("\"finish_reason\":\"tool_calls\""),
        "{events}"
    );

    // Claude takes them as Anthropic's tools; its input is reserved against
    // the byte bound, so no count is asked for (the fake serves none).
    let (status, body) = stack
        .chat(
            json!({"model": "claude", "messages": ask, "tools": weather(),
                     "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
                     "parallel_tool_calls": false}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(body["choices"][0]["message"]["content"], "Let me check.");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0],
        json!({"id": "toolu_1", "type": "function",
               "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}})
    );
    let sent = anthropic_seen.bodies().pop().unwrap();
    assert_eq!(sent["tools"][0]["name"], "get_weather");
    assert_eq!(
        sent["tools"][0]["input_schema"]["required"],
        json!(["city"])
    );
    assert_eq!(
        sent["tool_choice"],
        json!({"type": "tool", "name": "get_weather", "disable_parallel_tool_use": true})
    );
    assert_eq!(anthropic_seen.bodies().len(), 1);

    // A route that cannot take tools refuses them, before anything is held
    // or sent; so does one only some of whose deployments can.
    let before = (
        openai_seen.bodies().len(),
        reservations(&pool, org).await.len(),
    );
    for model in ["plain", "mixed"] {
        let (status, body) = stack
            .chat(json!({"model": model, "messages": ask, "tools": weather()}))
            .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["code"], "tools_unsupported", "{body}");
        let (status, _) = stack
            .post(
                "chat/completions/stream",
                json!({"model": model, "messages": ask, "tools": weather()}),
            )
            .await;
        assert_eq!(status, 422);
    }
    // A malformed one is refused with what is wrong.
    let (status, body) = stack
        .chat(json!({"model": "tools", "messages": conversation}))
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "chat_tools_invalid", "{body}");
    assert!(
        body["detail"].as_str().unwrap().contains("define tools"),
        "{body}"
    );
    assert_eq!(
        before,
        (
            openai_seen.bodies().len(),
            reservations(&pool, org).await.len()
        )
    );
    // A request without tools is untouched on the plain route.
    let (status, body) = stack.chat(json!({"model": "plain", "messages": ask})).await;
    assert_eq!(status, 200, "{body}");
    assert!(openai_seen.bodies().pop().unwrap().get("tools").is_none());
}
