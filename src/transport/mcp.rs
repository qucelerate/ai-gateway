//! `/api/mcp`: the agent tools (`crate::agent_tools::mcp`, the same ones
//! `gwctl mcp` serves over stdio) over MCP's streamable HTTP transport, for
//! an agent with only a `gwa_` token and no local `gwctl`.
//!
//! Each POST carries one JSON-RPC message or a batch and gets the response
//! as `application/json` (202 with no body for notifications). The server
//! is stateless: no session id, no server-sent events, so GET and DELETE
//! are refused with 405. A tool call runs as the admin API request its
//! `gwctl` command makes, sent in process to the admin routes with the
//! caller's token, so permissions, the role ceiling, idempotency, `If-Match`
//! and audit apply exactly as they do to any other client.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};

use crate::{
    agent_tools::{
        client::Client,
        mcp::{PROTOCOL_VERSIONS, handle},
    },
    persistence::auth::AuthStore,
    transport::{
        admin_auth::agent_token,
        http::{error, origins_match},
    },
};

pub const MCP_PATH: &str = "/api/mcp";

#[derive(Clone)]
struct McpState {
    /// The admin and inference routes tool calls are sent to.
    routes: Router,
    auth: AuthStore,
    /// The gateway's public origin; a browser `Origin` must match it.
    origin: Option<String>,
}

pub fn router(routes: Router, auth: AuthStore, origin: Option<String>) -> Router {
    Router::new()
        .route(MCP_PATH, post(message))
        .with_state(McpState {
            routes,
            auth,
            origin,
        })
}

async fn message(State(state): State<McpState>, headers: HeaderMap, body: String) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    // Against DNS rebinding (MCP transport security): a page on another
    // origin must not reach the endpoint through a browser.
    if let Some(sent) = header("origin") {
        let allowed = state
            .origin
            .as_deref()
            .is_some_and(|expected| origins_match(sent, expected));
        if !allowed {
            return error(StatusCode::FORBIDDEN, "cross_origin_request_refused");
        }
    }
    if let Some(version) = header("mcp-protocol-version") {
        if !PROTOCOL_VERSIONS.contains(&version) {
            return error(StatusCode::BAD_REQUEST, "unsupported_mcp_protocol_version");
        }
    }
    let Some(token) = agent_token(&headers) else {
        return unauthorized("agent_token_required");
    };
    match state.auth.find_agent_token(token).await {
        Ok(Some(_)) => {}
        Ok(None) => return unauthorized("invalid_agent_token"),
        Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_store_unavailable"),
    }
    let client = Client::local(state.routes, token.to_owned());
    match handle(&client, &body).await {
        Some(response) => axum::Json(response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn unauthorized(code: &'static str) -> Response {
    let mut response = error(StatusCode::UNAUTHORIZED, code);
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}
