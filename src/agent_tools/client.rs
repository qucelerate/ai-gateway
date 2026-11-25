//! Runs a planned call against the gateway: authentication, the
//! organization, `If-Match`, idempotency keys, retries and paging. `gwctl`
//! sends it over HTTP; the gateway's own MCP endpoint hands it to the admin
//! routes in process, with the caller's token.

use std::time::Duration;

use axum::{Router, body::Body};
use reqwest::{Method, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use super::commands::{Auth, Call, Precondition, merge_pages, page_items, version_of};

/// A failed call: the gateway's problem document, or why there was none.
pub enum Failure {
    Problem {
        status: StatusCode,
        body: Value,
    },
    Usage(String),
    Transport(String),
    /// A write whose response was lost: it may have been applied. Sending
    /// it again with this key and `If-Match` makes the gateway replay it.
    Unknown {
        message: String,
        idempotency_key: String,
        if_match: Option<String>,
    },
}

#[derive(Clone)]
pub struct Settings {
    pub url: String,
    pub agent_token: Option<String>,
    pub api_key: Option<String>,
    pub organization: Option<String>,
}

#[derive(Clone)]
pub struct Client {
    transport: Transport,
    settings: Settings,
}

#[derive(Clone)]
enum Transport {
    Http(reqwest::Client),
    /// The gateway's admin and inference routes, called in process.
    Local(Router),
}

/// The largest response body read from the in-process routes.
const LOCAL_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Attempts per request: a lost connection or a 503 is retried with the
/// same idempotency key, so a write is applied at most once.
const ATTEMPTS: u32 = 3;
/// Pages followed by `--all` before giving up.
const MAX_PAGES: usize = 1000;

impl Client {
    pub fn new(settings: Settings) -> Result<Self, Failure> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("gwctl/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|error| Failure::Transport(error.to_string()))?;
        Ok(Self {
            transport: Transport::Http(http),
            settings,
        })
    }

    /// A client of `routes`, the gateway's own, acting with `agent_token`.
    pub fn local(routes: Router, agent_token: String) -> Self {
        Self {
            transport: Transport::Local(routes),
            settings: Settings {
                url: String::new(),
                agent_token: Some(agent_token),
                api_key: None,
                organization: None,
            },
        }
    }

    /// The same client, calling inference with `api_key`.
    pub fn with_api_key(&self, api_key: String) -> Self {
        let mut client = self.clone();
        client.settings.api_key = Some(api_key);
        client
    }

    pub async fn run(&self, call: Call) -> Result<Value, Failure> {
        let organization = if call.path.contains("{org}") {
            match call
                .organization
                .clone()
                .or_else(|| self.settings.organization.clone())
            {
                Some(organization) => organization,
                None => self.only_organization().await?,
            }
        } else {
            String::new()
        };
        let organization_path = |template: &str| template.replace("{org}", &organization);
        let path = organization_path(&call.path);
        let if_match = match &call.precondition {
            Precondition::None => None,
            Precondition::Value(value) => Some(value.clone()),
            Precondition::Fetch(target) => Some(
                self.current_version(&organization_path(target), call.auth)
                    .await?,
            ),
            Precondition::IfReadable(target) => {
                match self
                    .current_version(&organization_path(target), call.auth)
                    .await
                {
                    Ok(version) => Some(version),
                    // A write-only path: send none, and let the gateway ask.
                    Err(Failure::Problem {
                        status: StatusCode::METHOD_NOT_ALLOWED,
                        ..
                    }) => None,
                    Err(failure) => return Err(failure),
                }
            }
            Precondition::Find { list, matches } => {
                let items = self.every_item(&organization_path(list), call.auth).await?;
                Some(version_of(&items, matches).unwrap_or_else(|| "*".into()))
            }
            Precondition::ListField { list, field } => {
                let (page, _) = self
                    .send(
                        Method::GET,
                        &organization_path(list),
                        &[],
                        None,
                        None,
                        None,
                        call.auth,
                    )
                    .await?;
                let version = page.get(*field).and_then(Value::as_u64).unwrap_or(0);
                Some(format!("\"v{version}\""))
            }
        };
        let method = Method::from_bytes(call.method.as_bytes())
            .map_err(|error| Failure::Usage(error.to_string()))?;
        let key = call.is_write().then(|| {
            call.idempotency_key
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
        });
        if call.all_pages && !call.is_write() {
            let mut pages = Vec::new();
            let mut query = call.query.clone();
            loop {
                let (page, _) = self
                    .send(method.clone(), &path, &query, None, None, None, call.auth)
                    .await?;
                let next = page
                    .get("next_page_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                pages.push(page);
                match next {
                    Some(_) if pages.len() >= MAX_PAGES => return Err(too_many_pages()),
                    Some(token) => {
                        query.retain(|(name, _)| name != "page_token");
                        query.push(("page_token".into(), token));
                    }
                    None => break,
                }
            }
            return Ok(merge_pages(pages));
        }
        let sent = self
            .send_as(
                method,
                &path,
                &call.query,
                call.body.as_ref(),
                if_match.as_deref(),
                key.as_deref(),
                call.auth,
                call.raw,
            )
            .await;
        match (sent, key) {
            (Ok((body, _)), _) => Ok(body),
            // The write may have been applied: say how to retry it as the
            // same request, which the gateway then replays.
            (Err(Failure::Transport(message)), Some(idempotency_key)) => Err(Failure::Unknown {
                message,
                idempotency_key,
                if_match,
            }),
            (Err(failure), _) => Err(failure),
        }
    }

    /// The one organization the token can see.
    async fn only_organization(&self) -> Result<String, Failure> {
        let (body, _) = self
            .send(
                Method::GET,
                "/api/admin/v1/organizations",
                &[],
                None,
                None,
                None,
                Auth::Agent,
            )
            .await?;
        let ids: Vec<&str> = body
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("id").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        match ids.as_slice() {
            [only] => Ok((*only).to_owned()),
            [] => Err(Failure::Usage("the token sees no organization".into())),
            _ => Err(Failure::Usage(
                "the token sees several organizations; pass --org or set GATEWAY_ORGANIZATION"
                    .into(),
            )),
        }
    }

    /// A resource's `If-Match`: its `ETag`, else its `version`, else `*`
    /// when it does not exist.
    async fn current_version(&self, path: &str, auth: Auth) -> Result<String, Failure> {
        match self
            .send(Method::GET, path, &[], None, None, None, auth)
            .await
        {
            Ok((_, Some(etag))) => Ok(etag),
            Ok((body, None)) => Ok(body
                .get("version")
                .and_then(Value::as_u64)
                .map_or_else(|| "*".to_owned(), |version| format!("\"v{version}\""))),
            Err(Failure::Problem {
                status: StatusCode::NOT_FOUND,
                ..
            }) => Ok("*".into()),
            Err(failure) => Err(failure),
        }
    }

    async fn every_item(&self, path: &str, auth: Auth) -> Result<Vec<Value>, Failure> {
        let mut items = Vec::new();
        let mut query: Vec<(String, String)> = vec![("page_size".into(), "100".into())];
        for _ in 0..MAX_PAGES {
            let (page, _) = self
                .send(Method::GET, path, &query, None, None, None, auth)
                .await?;
            if let Some((_, more)) = page_items(&page) {
                items.extend(more.iter().cloned());
            }
            match page.get("next_page_token").and_then(Value::as_str) {
                Some(token) => {
                    query.retain(|(name, _)| name != "page_token");
                    query.push(("page_token".into(), token.to_owned()));
                }
                None => return Ok(items),
            }
        }
        Err(too_many_pages())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        if_match: Option<&str>,
        idempotency_key: Option<&str>,
        auth: Auth,
    ) -> Result<(Value, Option<String>), Failure> {
        self.send_as(
            method,
            path,
            query,
            body,
            if_match,
            idempotency_key,
            auth,
            false,
        )
        .await
    }

    /// [`Self::send`]; with `raw`, a successful body is returned as the
    /// text it came as ([`Value::String`]), even when it parses as JSON.
    #[allow(clippy::too_many_arguments)]
    async fn send_as(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        if_match: Option<&str>,
        idempotency_key: Option<&str>,
        auth: Auth,
        raw: bool,
    ) -> Result<(Value, Option<String>), Failure> {
        let secret = match auth {
            Auth::Agent => self.settings.agent_token.as_deref().ok_or_else(|| {
                Failure::Usage("set GATEWAY_AGENT_TOKEN to an agent token (gwa_...)".into())
            })?,
            Auth::ApiKey => self.settings.api_key.as_deref().ok_or_else(|| {
                Failure::Usage(
                    match self.transport {
                        Transport::Http(_) => "set GATEWAY_API_KEY to an API key (gw_...)",
                        Transport::Local(_) => "pass api_key, an API key (gw_...)",
                    }
                    .into(),
                )
            })?,
        };
        let mut last = Failure::Transport("no attempt was made".into());
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
            let sent = match &self.transport {
                Transport::Http(http) => {
                    let url = format!("{}{}", self.settings.url.trim_end_matches('/'), path);
                    send_http(
                        http,
                        method.clone(),
                        &url,
                        query,
                        body,
                        if_match,
                        idempotency_key,
                        secret,
                    )
                    .await
                }
                Transport::Local(routes) => {
                    send_local(
                        routes,
                        method.clone(),
                        path,
                        query,
                        body,
                        if_match,
                        idempotency_key,
                        secret,
                    )
                    .await
                }
            };
            let (status, etag, text) = match sent {
                Ok(sent) => sent,
                Err(Sent::Retry(message)) => {
                    last = Failure::Transport(message);
                    continue;
                }
                Err(Sent::Fail(failure)) => return Err(failure),
            };
            let value = if raw && status.is_success() {
                Value::String(text)
            } else if text.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text).unwrap_or(Value::String(text))
            };
            if status.is_success() {
                return Ok((value, etag));
            }
            let in_progress =
                value.get("code").and_then(Value::as_str) == Some("idempotency_in_progress");
            last = Failure::Problem {
                status,
                body: value,
            };
            if status != StatusCode::SERVICE_UNAVAILABLE && !in_progress {
                break;
            }
        }
        Err(last)
    }
}

/// Why a request got no response.
enum Sent {
    /// The connection failed or dropped: worth another attempt.
    Retry(String),
    Fail(Failure),
}

async fn send_http(
    http: &reqwest::Client,
    method: Method,
    url: &str,
    query: &[(String, String)],
    body: Option<&Value>,
    if_match: Option<&str>,
    idempotency_key: Option<&str>,
    secret: &str,
) -> Result<(StatusCode, Option<String>, String), Sent> {
    let mut request = http
        .request(method, url)
        .bearer_auth(secret)
        .header(header::ACCEPT, "application/json");
    if !query.is_empty() {
        request = request.query(query);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(value) = if_match {
        request = request.header(header::IF_MATCH, value);
    }
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_connect() || error.is_timeout() || error.is_request() => {
            return Err(Sent::Retry(error.to_string()));
        }
        Err(error) => return Err(Sent::Fail(Failure::Transport(error.to_string()))),
    };
    let status = response.status();
    let etag = etag(response.headers());
    // The connection can also drop while the body is read.
    let text = response
        .text()
        .await
        .map_err(|error| Sent::Retry(error.to_string()))?;
    Ok((status, etag, text))
}

async fn send_local(
    routes: &Router,
    method: Method,
    path: &str,
    query: &[(String, String)],
    body: Option<&Value>,
    if_match: Option<&str>,
    idempotency_key: Option<&str>,
    secret: &str,
) -> Result<(StatusCode, Option<String>, String), Sent> {
    let fail = |message: String| Sent::Fail(Failure::Transport(message));
    let mut uri = path.to_owned();
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(
            &url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query)
                .finish(),
        );
    }
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .header(header::ACCEPT, "application/json");
    if let Some(value) = if_match {
        request = request.header(header::IF_MATCH, value);
    }
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let request = request
        .body(body)
        .map_err(|error| fail(error.to_string()))?;
    let response = match routes.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let etag = etag(response.headers());
    let bytes = axum::body::to_bytes(response.into_body(), LOCAL_BODY_LIMIT)
        .await
        .map_err(|error| fail(error.to_string()))?;
    Ok((status, etag, String::from_utf8_lossy(&bytes).into_owned()))
}

fn etag(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// `--all` and version lookups stop at `MAX_PAGES` rather than return a
/// list that looks complete but is not.
fn too_many_pages() -> Failure {
    Failure::Usage(format!(
        "the list has more than {MAX_PAGES} pages; narrow it with filters"
    ))
}
