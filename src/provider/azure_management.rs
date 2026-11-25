//! An Azure OpenAI or Azure AI Foundry resource's deployments, from Azure
//! Resource Manager (D-018, Phase 6 milestone 2). Unlike the data-plane
//! listing (`/openai/deployments`, a retired API version that works with a
//! key), Resource Manager says each deployment's model format (`OpenAI`,
//! `Anthropic`, `xAI`, ...), model, version, SKU and state, and reads need
//! only Reader on the resource.

use futures::StreamExt;
use reqwest::{Client, Response, StatusCode, header};
use serde_json::Value;
use std::time::Duration;
use url::Url;

use crate::{
    config::{AzureAuth, AzureManagement},
    domain::{
        catalog::CatalogOperation,
        discovery::{
            ContentFilterPolicy, DeployableListBuilder, DeployableModel, DeploymentDetails,
            DiscoveredModel, ModelListBuilder,
        },
        inference::{ProviderError, ProviderErrorCategory},
        model_evidence::Entitlement,
    },
};

use super::MAX_MODEL_LIST_PAGES;

/// The Resource Manager version of `Microsoft.CognitiveServices` deployments.
const API_VERSION: &str = "2024-10-01";
/// The version of `Microsoft.Authorization/permissions`.
const PERMISSIONS_API_VERSION: &str = "2022-04-01";
const MAX_ERROR_BODY: usize = 16 * 1024;

/// Model formats the Azure OpenAI adapter serves: it calls every
/// deployment through `openai/deployments/{name}/chat/completions`, which
/// serves only OpenAI's models (Foundry serves the others, and Claude through
/// its Messages API).
pub(super) const OPENAI_FORMATS: &[&str] = &["OpenAI"];

pub(super) struct DeploymentLister {
    client: Client,
    /// `<endpoint><resource_id>?api-version=...`: the account itself.
    account_url: Url,
    /// `<endpoint><resource_id>/deployments?api-version=...`.
    url: Url,
    /// `<endpoint><resource_id>/models?api-version=...`: the account's
    /// catalog of deployable models.
    models_url: Url,
    /// `<endpoint><resource_id>/raiPolicies?api-version=...`: the account's
    /// content filter policies, which deployments name.
    policies_url: Url,
    /// `<endpoint><resource_id>/providers/Microsoft.Authorization/permissions?...`:
    /// what the calling identity may do on the resource.
    permissions_url: Url,
    /// The custom subdomain of the resource inference calls: the account
    /// listed must be the same one.
    subdomain: String,
    /// The model formats the adapter can call; `None` for every format.
    formats: Option<&'static [&'static str]>,
    /// Whether the identity listing is the one inference calls with, so its
    /// permissions say whether a call is allowed. Not with an API key: the
    /// key's access has nothing to do with the identity's roles.
    checks_entitlement: bool,
    auth: AzureAuth,
    request_timeout: Duration,
    max_response_bytes: usize,
    provider: &'static str,
}

impl DeploymentLister {
    /// `inference`: the resource requests go to, whose subdomain the listed
    /// account must have.
    pub(super) fn new(
        management: &AzureManagement,
        inference: &Url,
        formats: Option<&'static [&'static str]>,
        provider: &'static str,
        connect_timeout: Duration,
        request_timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let resource = |suffix: &str, version: &str| {
            let mut url = Url::parse(&format!(
                "{}{}{suffix}",
                management.endpoint.trim_end_matches('/'),
                management.resource_id
            ))
            .map_err(|_| protocol_error())?;
            url.query_pairs_mut().append_pair("api-version", version);
            Ok::<_, ProviderError>(url)
        };
        let subdomain = inference
            .host_str()
            .and_then(|host| host.split('.').next())
            .ok_or_else(protocol_error)?
            .to_ascii_lowercase();
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .read_timeout(request_timeout)
            .build()
            .map_err(|_| ProviderError::new(ProviderErrorCategory::Internal))?;
        Ok(Self {
            client,
            account_url: resource("", API_VERSION)?,
            url: resource("/deployments", API_VERSION)?,
            models_url: resource("/models", API_VERSION)?,
            policies_url: resource("/raiPolicies", API_VERSION)?,
            permissions_url: resource(
                "/providers/Microsoft.Authorization/permissions",
                PERMISSIONS_API_VERSION,
            )?,
            subdomain,
            formats,
            checks_entitlement: false,
            auth: AzureAuth::Entra(management.token.clone()),
            request_timeout,
            max_response_bytes,
            provider,
        })
    }

    /// Records each listed deployment's entitlement from the identity's
    /// permissions when inference authenticates as `inference`: only an
    /// Entra identity's roles decide what it may call.
    pub(super) fn checking_entitlement_for(mut self, inference: &AzureAuth) -> Self {
        self.checks_entitlement = matches!(inference, AzureAuth::Entra(_));
        self
    }

    /// Every deployment that can serve requests, following every page,
    /// once the account is shown to be the resource inference calls: a
    /// resource ID naming another account would list models requests cannot
    /// reach.
    pub(super) async fn list(&self) -> Result<Vec<DiscoveredModel>, ProviderError> {
        self.check_account().await?;
        let permissions = if self.checks_entitlement {
            self.permissions().await
        } else {
            None
        };
        let mut models = ModelListBuilder::default();
        let mut url = self.url.clone();
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.page(url).await?;
            push_deployments(&mut models, &page, self.formats, permissions.as_ref())?;
            match super::model_list::next_token(&page, "nextLink") {
                Some(next) => url = self.next_page(next)?,
                None => {
                    let mut models = models.finish();
                    self.attach_content_filters(&mut models).await;
                    return Ok(models);
                }
            }
        }
        Err(super::model_list::too_many_pages())
    }

    /// What the account may deploy (D-018): its model catalog, which Reader
    /// on the resource may read, following every page, once the account is
    /// shown to be the resource inference calls. With `formats`, only those
    /// the adapter could call once deployed.
    pub(super) async fn list_deployable(&self) -> Result<Vec<DeployableModel>, ProviderError> {
        self.check_account().await?;
        let mut models = DeployableListBuilder::default();
        let mut url = self.models_url.clone();
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.page(url).await?;
            push_deployable(&mut models, &page, self.formats)?;
            match super::model_list::next_token(&page, "nextLink") {
                Some(next) => url = self.next_page(next)?,
                None => return Ok(models.finish()),
            }
        }
        Err(super::model_list::too_many_pages())
    }

    /// Fails with `ResourceMismatch` unless the account's custom subdomain
    /// is the one inference calls: a resource ID naming another account
    /// would list models requests cannot reach.
    async fn check_account(&self) -> Result<(), ProviderError> {
        let account = self.page(self.account_url.clone()).await?;
        let subdomain = account
            .pointer("/properties/customSubDomainName")
            .and_then(Value::as_str);
        if !subdomain.is_some_and(|subdomain| subdomain.eq_ignore_ascii_case(&self.subdomain)) {
            return Err(ProviderError::from_parts(
                ProviderErrorCategory::InvalidRequest,
                Some(self.provider.to_owned()),
                None,
                None,
                Some("ResourceMismatch".to_owned()),
                None,
                None,
            )
            .unwrap_or_else(|_| protocol_error()));
        }
        Ok(())
    }

    /// What the identity may do on the resource (Reader may read its own
    /// permissions), following every page, or `None` when the whole list
    /// cannot be read: a grant on a page not read would be missed, so the
    /// listing still stands and its entitlement is unknown.
    async fn permissions(&self) -> Option<Permissions> {
        match self.all_permissions().await {
            Ok(permissions) => permissions,
            Err(error) => {
                tracing::debug!(
                    provider = self.provider,
                    category = ?error.category(),
                    "reading the identity's permissions on the resource failed"
                );
                None
            }
        }
    }

    /// Adds each named content filter policy's settings to the deployments
    /// that apply it. Best effort, like permissions: the listing stands
    /// without them (the policy's name is still shown), since reading
    /// policies may need more than reading deployments.
    async fn attach_content_filters(&self, models: &mut [DiscoveredModel]) {
        let named = models.iter().any(|model| {
            model
                .deployment
                .as_ref()
                .is_some_and(|details| details.content_filter.is_some())
        });
        if !named {
            return;
        }
        let policies = match self.policies().await {
            Ok(policies) => policies,
            Err(error) => {
                tracing::debug!(
                    provider = self.provider,
                    category = ?error.category(),
                    "reading the account's content filter policies failed"
                );
                return;
            }
        };
        attach_policies(models, &policies);
    }

    async fn policies(&self) -> Result<Vec<ContentFilterPolicy>, ProviderError> {
        let mut policies = Vec::new();
        let mut url = self.policies_url.clone();
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.page(url).await?;
            let entries = page
                .get("value")
                .and_then(Value::as_array)
                .ok_or_else(protocol_error)?;
            policies.extend(entries.iter().filter_map(ContentFilterPolicy::from_azure));
            match super::model_list::next_token(&page, "nextLink") {
                Some(next) => url = self.next_page(next)?,
                None => return Ok(policies),
            }
        }
        Err(super::model_list::too_many_pages())
    }

    async fn all_permissions(&self) -> Result<Option<Permissions>, ProviderError> {
        let mut permissions = Permissions::default();
        let mut url = self.permissions_url.clone();
        for _ in 0..MAX_MODEL_LIST_PAGES {
            let page = self.page(url).await?;
            let Some(entries) = Permissions::from_page(&page) else {
                return Ok(None);
            };
            permissions.entries.extend(entries.entries);
            match super::model_list::next_token(&page, "nextLink") {
                Some(next) => url = self.next_page(next)?,
                None => return Ok(Some(permissions)),
            }
        }
        Err(super::model_list::too_many_pages())
    }

    /// The next page, which must be on the same Resource Manager endpoint:
    /// the token is never sent anywhere else.
    fn next_page(&self, next: &str) -> Result<Url, ProviderError> {
        let next = Url::parse(next).map_err(|_| protocol_error())?;
        if next.origin() != self.url.origin() {
            return Err(protocol_error());
        }
        Ok(next)
    }

    async fn page(&self, url: Url) -> Result<Value, ProviderError> {
        let authorization = super::azure_bearer(&self.auth, self.request_timeout).await?;
        let response = tokio::time::timeout(
            self.request_timeout,
            self.client
                .get(url)
                .header(header::AUTHORIZATION, authorization)
                .header(header::ACCEPT, "application/json")
                .send(),
        )
        .await
        .map_err(|_| ProviderError::new(ProviderErrorCategory::Timeout))?
        .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(self.error(response).await);
        }
        let body = read_bounded(response, self.max_response_bytes).await?;
        serde_json::from_slice(&body).map_err(|_| protocol_error())
    }

    /// Resource Manager's errors are `{"error": {"code", "message"}}`; the
    /// code (`AuthorizationFailed`, `ResourceNotFound`, ...) is kept, never
    /// the message.
    async fn error(&self, response: Response) -> ProviderError {
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-ms-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let retry_after = super::retry_after(response.headers());
        let body = read_bounded(response, MAX_ERROR_BODY)
            .await
            .unwrap_or_default();
        let code = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|body| {
                body.pointer("/error/code")
                    .and_then(Value::as_str)
                    .filter(|code| code.len() <= 128)
                    .map(ToOwned::to_owned)
            });
        ProviderError::from_parts(
            category_for_status(status),
            Some(self.provider.to_owned()),
            request_id,
            Some(status.as_u16()),
            code,
            retry_after,
            None,
        )
        .unwrap_or_else(|_| protocol_error())
    }
}

/// Gives each deployment naming a policy in `policies` its settings
/// (names compare case-insensitively, as Azure's do).
pub(super) fn attach_policies(models: &mut [DiscoveredModel], policies: &[ContentFilterPolicy]) {
    for details in models
        .iter_mut()
        .filter_map(|model| model.deployment.as_mut())
    {
        if let Some(name) = &details.content_filter {
            details.content_filter_policy = policies
                .iter()
                .find(|policy| policy.name.eq_ignore_ascii_case(name))
                .cloned();
        }
    }
}

/// Adds one page of deployments: `{"value": [{"name", "properties":
/// {"model": {"format", "name", "version"}, "provisioningState",
/// "capabilities": {"chatCompletion": "true", "embeddings": "true", ...}}}]}`.
///
/// Requests name the deployment, so it is the model identifier; the display
/// name says which model and version it serves. Only a `Succeeded`
/// deployment can serve requests: one being created, updated, deleted, or
/// that failed is left out. Operations come from the deployment's
/// capabilities: chat completion chats, streamed or not, and embeddings
/// embed; a deployment stating neither has unknown operations. With
/// `formats`, a deployment of another model format (or none) is left out:
/// the adapter cannot call it.
pub(super) fn push_deployments(
    builder: &mut ModelListBuilder,
    page: &Value,
    formats: Option<&[&str]>,
    permissions: Option<&Permissions>,
) -> Result<(), ProviderError> {
    let items = page
        .get("value")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    for item in items {
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let properties = item.get("properties");
        let state = properties
            .and_then(|properties| properties.get("provisioningState"))
            .and_then(Value::as_str);
        if !state.is_some_and(|state| state.eq_ignore_ascii_case("succeeded")) {
            continue;
        }
        let model = properties.and_then(|properties| properties.get("model"));
        let text = |field: &str| {
            model
                .and_then(|model| model.get(field))
                .and_then(Value::as_str)
        };
        if let Some(formats) = formats
            && !text("format").is_some_and(|format| {
                formats
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(format))
            })
        {
            continue;
        }
        let display_name = text("name").map(|model| match text("version") {
            Some(version) if !version.is_empty() => format!("{model} {version}"),
            _ => model.to_owned(),
        });
        let Some(deployment) = DiscoveredModel::new(name, display_name.as_deref()) else {
            continue;
        };
        let capable = |capability: &str| {
            properties
                .and_then(|properties| properties.pointer(&format!("/capabilities/{capability}")))
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        };
        // A batch-type deployment (`GlobalBatch`, `DataZoneBatch`) runs
        // batch jobs only: it serves no single request.
        let batch = item
            .pointer("/sku/name")
            .and_then(Value::as_str)
            .is_some_and(is_batch_sku);
        let mut operations = Vec::new();
        if capable("chatCompletion") {
            operations.push(if batch {
                CatalogOperation::ChatBatch
            } else {
                CatalogOperation::Chat
            });
        }
        if capable("embeddings") {
            operations.push(if batch {
                CatalogOperation::EmbeddingsBatch
            } else {
                CatalogOperation::Embeddings
            });
        }
        let entitlement = permissions.map(|permissions| {
            if permissions.allows(inference_action(text("format"), &operations)) {
                Entitlement::Entitled
            } else {
                Entitlement::PermissionMissing
            }
        });
        let sku = item.get("sku");
        let details = DeploymentDetails::new(
            text("format"),
            text("name"),
            text("version"),
            sku.and_then(|sku| sku.get("name")).and_then(Value::as_str),
            sku.and_then(|sku| sku.get("capacity"))
                .and_then(Value::as_i64),
        )
        .map(|details| {
            details.with_content_filter(
                properties
                    .and_then(|properties| properties.get("raiPolicyName"))
                    .and_then(Value::as_str),
            )
        });
        builder
            .push_model(
                deployment
                    .with_operations(&operations)
                    .with_entitlement(entitlement)
                    .with_deployment(details),
            )
            .map_err(|_| protocol_error())?;
    }
    Ok(())
}

/// Adds one page of an account's model catalog (`accounts/{name}/models`)
/// to `builder`: each entry's format, name and version, lifecycle status,
/// whether it is the default version, the SKUs it can be deployed with, the
/// most capacity a deployment may have, when inference on it retires, and
/// its operations from its capabilities as for deployments. An entry
/// without a usable format, name or version is skipped; with `formats`, so
/// is one of another format.
pub(super) fn push_deployable(
    builder: &mut DeployableListBuilder,
    page: &Value,
    formats: Option<&[&str]>,
) -> Result<(), ProviderError> {
    let items = page
        .get("value")
        .and_then(Value::as_array)
        .ok_or_else(protocol_error)?;
    for item in items {
        // Older API versions nest the model under `model`.
        let model = item.get("model").unwrap_or(item);
        let text = |field: &str| model.get(field).and_then(Value::as_str);
        let (Some(format), Some(name), Some(version)) =
            (text("format"), text("name"), text("version"))
        else {
            continue;
        };
        if formats.is_some_and(|formats| {
            !formats
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(format))
        }) {
            continue;
        }
        let Some(deployable) = DeployableModel::new(format, name, version) else {
            continue;
        };
        let skus: Vec<&str> = model
            .get("skus")
            .and_then(Value::as_array)
            .map(|skus| {
                skus.iter()
                    .filter_map(|sku| sku.get("name").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        let retires_at = model
            .pointer("/deprecation/inference")
            .and_then(Value::as_str)
            .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
            .map(|date| date.with_timezone(&chrono::Utc));
        let capable = |capability: &str| {
            model
                .pointer(&format!("/capabilities/{capability}"))
                .and_then(Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        };
        let mut operations = Vec::new();
        if capable("chatCompletion") {
            operations.push(CatalogOperation::Chat);
        }
        if capable("embeddings") {
            operations.push(CatalogOperation::Embeddings);
        }
        builder
            .push(
                deployable
                    .with_terms(
                        text("lifecycleStatus"),
                        model
                            .get("isDefaultVersion")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        &skus,
                        model.get("maxCapacity").and_then(Value::as_i64),
                        retires_at,
                    )
                    .with_operations(&operations),
            )
            .map_err(|_| protocol_error())?;
    }
    Ok(())
}

/// The data action that calling a deployment needs. OpenAI's models are
/// served through the OpenAI data plane; every other format (Claude, Grok,
/// Llama, ...) through Foundry's models-as-a-service one. Azure publishes no
/// data action of its own for Anthropic's Messages API, so Claude is checked
/// against models-as-a-service chat, which the onboarding role
/// (*Cognitive Services User*, `Microsoft.CognitiveServices/*`) grants. An
/// embeddings-only deployment needs the embeddings action instead.
/// Whether a deployment SKU is a batch type, which takes batch jobs only.
fn is_batch_sku(sku: &str) -> bool {
    sku.to_ascii_lowercase().ends_with("batch")
}

fn inference_action(format: Option<&str>, operations: &[CatalogOperation]) -> &'static str {
    let embeddings_only = !operations.is_empty()
        && operations.iter().all(|operation| {
            matches!(
                operation,
                CatalogOperation::Embeddings | CatalogOperation::EmbeddingsBatch
            )
        });
    let openai = format.is_some_and(|format| format.eq_ignore_ascii_case("OpenAI"));
    match (openai, embeddings_only) {
        (true, false) => {
            "Microsoft.CognitiveServices/accounts/OpenAI/deployments/chat/completions/action"
        }
        (true, true) => "Microsoft.CognitiveServices/accounts/OpenAI/deployments/embeddings/action",
        (false, false) => "Microsoft.CognitiveServices/accounts/MaaS/chat/completions/action",
        (false, true) => "Microsoft.CognitiveServices/accounts/MaaS/embeddings/action",
    }
}

/// The identity's effective data permissions on a resource, as
/// `Microsoft.Authorization/permissions` reports them: one entry per role it
/// holds there, each its data actions less its excluded ones.
#[derive(Debug, Default)]
pub(super) struct Permissions {
    entries: Vec<(Vec<String>, Vec<String>)>,
}

impl Permissions {
    /// `None` for a page that is not a permission list.
    pub(super) fn from_page(page: &Value) -> Option<Self> {
        let strings = |entry: &Value, field: &str| -> Vec<String> {
            entry
                .get(field)
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let entries = page
            .get("value")?
            .as_array()?
            .iter()
            .map(|entry| {
                (
                    strings(entry, "dataActions"),
                    strings(entry, "notDataActions"),
                )
            })
            .collect();
        Some(Self { entries })
    }

    /// Whether one of the identity's roles grants `action` without
    /// excluding it.
    pub(super) fn allows(&self, action: &str) -> bool {
        self.entries.iter().any(|(granted, excluded)| {
            granted
                .iter()
                .any(|pattern| matches_action(pattern, action))
                && !excluded
                    .iter()
                    .any(|pattern| matches_action(pattern, action))
        })
    }
}

/// Azure's action patterns: case-insensitive, `*` matching any run of
/// characters.
fn matches_action(pattern: &str, action: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let action = action.to_ascii_lowercase();
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = action.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        // No `*`: the whole action must match.
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

async fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(request_error)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(protocol_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn category_for_status(status: StatusCode) -> ProviderErrorCategory {
    match status {
        StatusCode::UNAUTHORIZED => ProviderErrorCategory::Authentication,
        StatusCode::FORBIDDEN => ProviderErrorCategory::Authorization,
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ProviderErrorCategory::InvalidRequest
        }
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorCategory::RateLimited,
        StatusCode::REQUEST_TIMEOUT => ProviderErrorCategory::Timeout,
        status if status.is_server_error() => ProviderErrorCategory::Unavailable,
        _ => ProviderErrorCategory::Provider,
    }
}

fn request_error(error: reqwest::Error) -> ProviderError {
    // A connection that was never opened carried nothing to the provider.
    if error.is_connect() {
        return ProviderError::not_sent(if error.is_timeout() {
            ProviderErrorCategory::Timeout
        } else {
            ProviderErrorCategory::Unavailable
        });
    }
    ProviderError::new(if error.is_timeout() {
        ProviderErrorCategory::Timeout
    } else {
        ProviderErrorCategory::Unavailable
    })
}

fn protocol_error() -> ProviderError {
    ProviderError::new(ProviderErrorCategory::Protocol)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::azure_credentials::tests as entra;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    const RESOURCE: &str = "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/acme";

    /// A page shaped like the sandbox resource's (2026-11-03).
    pub(crate) fn sandbox_page() -> Value {
        json!({"value": [
            {"name": "gpt-6.1-sol", "sku": {"name": "GlobalStandard", "capacity": 125},
             "properties": {"model": {"format": "OpenAI", "name": "gpt-6.1-sol", "version": "2026-09-29"},
                            "provisioningState": "Succeeded",
                            "capabilities": {"chatCompletion": "true", "responses": "true", "area": "US"}}},
            {"name": "claude-sonnet-5-5", "sku": {"name": "GlobalStandard", "capacity": 50},
             "properties": {"model": {"format": "Anthropic", "name": "claude-sonnet-5-5", "version": "2"},
                            "provisioningState": "Succeeded",
                            "capabilities": {"chatCompletion": "true"}}},
            {"name": "embed", "properties": {"model": {"format": "OpenAI", "name": "text-embedding-3-large", "version": "1"},
                            "provisioningState": "Succeeded",
                            "capabilities": {"embeddings": "true"}}},
            {"name": "grok-4.7", "properties": {"model": {"format": "xAI", "name": "grok-4.7", "version": "1"},
                            "provisioningState": "Succeeded", "capabilities": {}}},
            {"name": "creating", "properties": {"model": {"name": "gpt-6-luna"}, "provisioningState": "Creating",
                            "capabilities": {"chatCompletion": "true"}}},
            {"name": "failed", "properties": {"provisioningState": "Failed"}},
            {"properties": {"provisioningState": "Succeeded"}}
        ]})
    }

    #[test]
    fn succeeded_deployments_are_listed_with_their_model_and_operations() {
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &sandbox_page(), None, None).unwrap();
        let models = builder.finish();
        let summary: Vec<_> = models
            .iter()
            .map(|model| {
                (
                    model.model.as_str(),
                    model.display_name.as_deref(),
                    model.operations.clone(),
                )
            })
            .collect();
        let chat = Some(vec![CatalogOperation::Chat]);
        assert_eq!(
            summary,
            [
                (
                    "claude-sonnet-5-5",
                    Some("claude-sonnet-5-5 2"),
                    chat.clone()
                ),
                (
                    "embed",
                    Some("text-embedding-3-large 1"),
                    Some(vec![CatalogOperation::Embeddings])
                ),
                ("gpt-6.1-sol", Some("gpt-6.1-sol 2026-09-29"), chat),
                ("grok-4.7", Some("grok-4.7 1"), None),
            ]
        );
    }

    /// Each deployment keeps what it serves and its SKU; a deployment
    /// without a SKU has none.
    /// An account's model catalog as Resource Manager reports it.
    fn catalog_page() -> Value {
        json!({"value": [
            {"format": "Anthropic", "name": "claude-opus-5-5", "version": "2",
             "lifecycleStatus": "GenerallyAvailable", "isDefaultVersion": true,
             "maxCapacity": 5000,
             "skus": [{"name": "GlobalStandard", "capacity": {"default": 50, "maximum": 5000}},
                      {"name": "DataZoneStandard"}],
             "deprecation": {"inference": "2027-11-01T00:00:00Z"},
             "capabilities": {"chatCompletion": "true", "area": "US"}},
            {"format": "OpenAI", "name": "text-embedding-3-large", "version": "1",
             "skus": [{"name": "Standard"}], "capabilities": {"embeddings": "true"}},
            {"format": "OpenAI", "name": "whisper", "version": "001",
             "lifecycleStatus": "Deprecating", "skus": [], "capabilities": {"audio": "true"},
             "deprecation": {"inference": "not a date"}},
            {"format": "OpenAI", "name": "no-version"},
            {"name": "no-format", "version": "1"}
        ]})
    }

    #[test]
    fn the_catalog_lists_what_the_account_may_deploy() {
        let mut builder = DeployableListBuilder::default();
        push_deployable(&mut builder, &catalog_page(), None).unwrap();
        let models = builder.finish();
        let keys: Vec<_> = models
            .iter()
            .map(|model| {
                (
                    model.format.as_str(),
                    model.model.as_str(),
                    model.version.as_str(),
                )
            })
            .collect();
        assert_eq!(
            keys,
            [
                ("Anthropic", "claude-opus-5-5", "2"),
                ("OpenAI", "text-embedding-3-large", "1"),
                ("OpenAI", "whisper", "001"),
            ]
        );
        let claude = &models[0];
        assert_eq!(
            claude.lifecycle_status.as_deref(),
            Some("GenerallyAvailable")
        );
        assert!(claude.default_version);
        assert_eq!(claude.skus, ["GlobalStandard", "DataZoneStandard"]);
        assert_eq!(claude.max_capacity, Some(5000));
        assert_eq!(
            claude.retires_at.map(|at| at.to_rfc3339()),
            Some("2027-11-01T00:00:00+00:00".to_owned())
        );
        assert_eq!(claude.operations, Some(vec![CatalogOperation::Chat]));
        assert_eq!(
            models[1].operations,
            Some(vec![CatalogOperation::Embeddings])
        );
        let whisper = &models[2];
        assert_eq!(
            (whisper.operations.clone(), whisper.retires_at),
            (None, None)
        );
        assert!(!whisper.default_version && whisper.skus.is_empty());

        // Azure OpenAI could call only OpenAI's models once deployed.
        let mut builder = DeployableListBuilder::default();
        push_deployable(&mut builder, &catalog_page(), Some(OPENAI_FORMATS)).unwrap();
        assert!(
            builder
                .finish()
                .iter()
                .all(|model| model.format == "OpenAI")
        );
        assert!(push_deployable(&mut DeployableListBuilder::default(), &json!({}), None).is_err());
    }

    #[tokio::test]
    async fn the_catalog_is_read_from_the_checked_account() {
        let (endpoint, requests) = serve(|endpoint| {
            vec![
                account("acme"),
                (
                    200,
                    json!({"value": [catalog_page()["value"][0]],
                           "nextLink": format!("{endpoint}{RESOURCE}/models?api-version={API_VERSION}&$skipToken=2")}),
                ),
                (200, json!({"value": [catalog_page()["value"][1]]})),
            ]
        });
        let (paged, _keep) = lister(&endpoint).await;
        let models = paged.list_deployable().await.unwrap();
        assert_eq!(models.len(), 2);
        requests.recv().unwrap();
        assert_eq!(
            requests.recv().unwrap(),
            format!("GET {RESOURCE}/models?api-version={API_VERSION} HTTP/1.1 | Bearer token-1")
        );
        assert!(requests.recv().unwrap().contains("$skipToken=2"));

        let (endpoint, _requests) = serve(|_| vec![account("other")]);
        let (lister, _keep) = lister(&endpoint).await;
        let error = lister.list_deployable().await.unwrap_err();
        assert_eq!(error.provider_code(), Some("ResourceMismatch"));
    }

    #[test]
    fn batch_type_deployments_take_batch_operations_only() {
        let page = json!({"value": [
            {"name": "gpt-batch", "sku": {"name": "GlobalBatch", "capacity": 100},
             "properties": {"model": {"format": "OpenAI", "name": "gpt-4.1", "version": "1"},
                            "provisioningState": "Succeeded",
                            "capabilities": {"chatCompletion": "true"}}},
            {"name": "embed-batch", "sku": {"name": "DataZoneBatch", "capacity": 10},
             "properties": {"model": {"format": "OpenAI", "name": "text-embedding-3-large", "version": "1"},
                            "provisioningState": "Succeeded",
                            "capabilities": {"embeddings": "true"}}}
        ]});
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &page, None, None).unwrap();
        let operations: Vec<_> = builder
            .finish()
            .into_iter()
            .map(|model| (model.model, model.operations))
            .collect();
        assert_eq!(
            operations,
            [
                (
                    "embed-batch".to_owned(),
                    Some(vec![CatalogOperation::EmbeddingsBatch])
                ),
                (
                    "gpt-batch".to_owned(),
                    Some(vec![CatalogOperation::ChatBatch])
                ),
            ]
        );
        assert_eq!(
            inference_action(Some("OpenAI"), &[CatalogOperation::EmbeddingsBatch]),
            "Microsoft.CognitiveServices/accounts/OpenAI/deployments/embeddings/action"
        );
    }

    #[test]
    fn deployments_carry_their_content_filter_policy() {
        let page = serde_json::json!({"value": [
            {"name": "sol", "sku": {"name": "GlobalStandard", "capacity": 10},
             "properties": {"model": {"format": "OpenAI", "name": "gpt-6.1-sol", "version": "1"},
                            "raiPolicyName": "strict-policy", "provisioningState": "Succeeded",
                            "capabilities": {"chatCompletion": "true"}}},
            {"name": "luna", "properties": {"model": {"format": "OpenAI", "name": "gpt-6-luna", "version": "1"},
                            "raiPolicyName": "Unlisted", "provisioningState": "Succeeded",
                            "capabilities": {"chatCompletion": "true"}}},
            {"name": "plain", "properties": {"model": {"format": "OpenAI", "name": "gpt-6-astra", "version": "1"},
                            "provisioningState": "Succeeded", "capabilities": {"chatCompletion": "true"}}}
        ]});
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &page, None, None).unwrap();
        let mut models = builder.finish();
        let policies: Vec<ContentFilterPolicy> = [
            serde_json::json!({"name": "Strict-Policy", "properties": {
                "basePolicyName": "Microsoft.DefaultV2", "mode": "Asynchronous_filter", "type": "UserManaged",
                "contentFilters": [
                    {"name": "Hate", "source": "Prompt", "enabled": true, "blocking": true, "severityThreshold": "Low"},
                    {"name": "Jailbreak", "source": "Prompt", "enabled": true, "blocking": true},
                    {"source": "Completion", "enabled": true}
                ]}}),
            serde_json::json!({"properties": {"mode": "Default"}}),
        ]
        .iter()
        .filter_map(ContentFilterPolicy::from_azure)
        .collect();
        assert_eq!(policies.len(), 1, "a policy without a name is left out");
        attach_policies(&mut models, &policies);
        let by_name = |name: &str| {
            models
                .iter()
                .find(|model| model.model == name)
                .and_then(|model| model.deployment.clone())
                .unwrap()
        };
        let sol = by_name("sol");
        assert_eq!(sol.content_filter.as_deref(), Some("strict-policy"));
        let policy = sol.content_filter_policy.unwrap();
        assert_eq!(policy.base_policy.as_deref(), Some("Microsoft.DefaultV2"));
        assert_eq!(policy.mode.as_deref(), Some("Asynchronous_filter"));
        assert_eq!(policy.system_policy, Some(false));
        assert_eq!(
            policy.filters.len(),
            2,
            "a filter without a name is left out"
        );
        assert_eq!(policy.filters[0].severity_threshold.as_deref(), Some("Low"));
        assert_eq!(policy.filters[1].severity_threshold, None);
        // A policy the list does not name keeps its name only.
        let luna = by_name("luna");
        assert_eq!(luna.content_filter.as_deref(), Some("Unlisted"));
        assert!(luna.content_filter_policy.is_none());
        assert!(by_name("plain").content_filter.is_none());
    }

    #[test]
    fn deployments_keep_their_format_model_and_sku() {
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &sandbox_page(), None, None).unwrap();
        let listed: Vec<_> = builder
            .finish()
            .into_iter()
            .map(|model| (model.model, model.deployment))
            .collect();
        let details = |format: &str, model: &str, version: &str, sku: Option<(&str, i32)>| {
            Some(DeploymentDetails {
                format: Some(format.into()),
                model: Some(model.into()),
                version: Some(version.into()),
                sku: sku.map(|(name, _)| name.into()),
                capacity: sku.map(|(_, capacity)| capacity),
                ..DeploymentDetails::default()
            })
        };
        assert_eq!(
            listed,
            [
                (
                    "claude-sonnet-5-5".to_owned(),
                    details(
                        "Anthropic",
                        "claude-sonnet-5-5",
                        "2",
                        Some(("GlobalStandard", 50))
                    )
                ),
                (
                    "embed".to_owned(),
                    details("OpenAI", "text-embedding-3-large", "1", None)
                ),
                (
                    "gpt-6.1-sol".to_owned(),
                    details(
                        "OpenAI",
                        "gpt-6.1-sol",
                        "2026-09-29",
                        Some(("GlobalStandard", 125))
                    )
                ),
                ("grok-4.7".to_owned(), details("xAI", "grok-4.7", "1", None)),
            ]
        );
    }

    /// Azure OpenAI calls every deployment through its OpenAI API, so it
    /// lists only OpenAI-format deployments: not Claude, nor xAI.
    #[test]
    fn an_adapter_lists_only_the_formats_it_can_call() {
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &sandbox_page(), Some(OPENAI_FORMATS), None).unwrap();
        let models = builder.finish();
        assert_eq!(
            models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["embed", "gpt-6.1-sol"]
        );
    }

    #[test]
    fn a_page_without_deployments_is_a_protocol_error() {
        let mut builder = ModelListBuilder::default();
        let error = push_deployments(&mut builder, &json!({"data": []}), None, None).unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
    }

    /// Serves the pages `pages` builds from the server's address, one per
    /// connection, recording each request line and authorization header.
    fn serve(pages: impl FnOnce(&str) -> Vec<(u16, Value)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let responses = pages(&address);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                let line = request.lines().next().unwrap_or_default().to_owned();
                let authorization = request
                    .lines()
                    .find_map(|line| line.strip_prefix("authorization: "))
                    .unwrap_or_default()
                    .to_owned();
                let _ = sender.send(format!("{line} | {authorization}"));
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nx-ms-request-id: req-{status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (address, receiver)
    }

    /// The account Resource Manager reports for the resource inference
    /// calls (`https://acme.services.ai.azure.com`).
    fn account(subdomain: &str) -> (u16, Value) {
        (
            200,
            json!({"kind": "AIServices", "properties": {"customSubDomainName": subdomain}}),
        )
    }

    /// *Cognitive Services User*, as Azure reports it, with the excluded
    /// actions it carries, and Reader.
    const USER_ROLE: &str = r#"{"value": [
        {"actions": ["*/read"], "notActions": [], "dataActions": [], "notDataActions": []},
        {"actions": ["Microsoft.CognitiveServices/*/read"], "notActions": [],
         "dataActions": ["Microsoft.CognitiveServices/*"],
         "notDataActions": ["Microsoft.CognitiveServices/accounts/OpenAI/fine-tunes-deployments/write"]}
    ]}"#;
    /// *Cognitive Services OpenAI User*: OpenAI's data plane only.
    const OPENAI_USER_ROLE: &str = r#"{"value": [
        {"actions": ["Microsoft.CognitiveServices/*/read"], "notActions": [],
         "dataActions": ["Microsoft.CognitiveServices/accounts/OpenAI/*/read",
                         "Microsoft.CognitiveServices/accounts/OpenAI/deployments/chat/completions/action",
                         "Microsoft.CognitiveServices/accounts/OpenAI/deployments/embeddings/action"],
         "notDataActions": []}
    ]}"#;
    /// Reader alone: no data actions.
    const READER_ONLY: &str = r#"{"value": [{"actions": ["*/read"], "notActions": [], "dataActions": [], "notDataActions": []}]}"#;

    fn permissions(role: &str) -> (u16, Value) {
        (200, serde_json::from_str(role).unwrap())
    }

    fn entitlements(role: &str) -> Vec<(String, Option<Entitlement>)> {
        let permissions = Permissions::from_page(&serde_json::from_str(role).unwrap()).unwrap();
        let mut builder = ModelListBuilder::default();
        push_deployments(&mut builder, &sandbox_page(), None, Some(&permissions)).unwrap();
        builder
            .finish()
            .into_iter()
            .map(|model| (model.model, model.entitlement))
            .collect()
    }

    #[test]
    fn action_patterns_match_like_azures() {
        let chat =
            "Microsoft.CognitiveServices/accounts/OpenAI/deployments/chat/completions/action";
        for (pattern, expected) in [
            ("Microsoft.CognitiveServices/*", true),
            ("microsoft.cognitiveservices/accounts/openai/*", true),
            ("*", true),
            (
                "Microsoft.CognitiveServices/accounts/*/deployments/*/action",
                true,
            ),
            (chat, true),
            ("Microsoft.CognitiveServices/accounts/OpenAI/*/read", false),
            ("Microsoft.CognitiveServices/accounts/MaaS/*", false),
            (
                "Microsoft.CognitiveServices/accounts/OpenAI/deployments/chat",
                false,
            ),
            ("", false),
        ] {
            assert_eq!(matches_action(pattern, chat), expected, "{pattern}");
        }
        assert!(!matches_action("a*a", "a"));
        assert!(matches_action("a*ba", "aba"));
    }

    /// The onboarding role entitles every deployment; OpenAI's role only
    /// OpenAI's models; Reader alone none (D-018).
    #[test]
    fn deployments_are_entitled_by_the_identitys_inference_role() {
        use Entitlement::*;
        let all = |entitlement| {
            ["claude-sonnet-5-5", "embed", "gpt-6.1-sol", "grok-4.7"]
                .into_iter()
                .map(|model| (model.to_owned(), Some(entitlement)))
                .collect::<Vec<_>>()
        };
        assert_eq!(entitlements(USER_ROLE), all(Entitled));
        assert_eq!(entitlements(READER_ONLY), all(PermissionMissing));
        assert_eq!(
            entitlements(OPENAI_USER_ROLE),
            [
                ("claude-sonnet-5-5".to_owned(), Some(PermissionMissing)),
                ("embed".to_owned(), Some(Entitled)),
                ("gpt-6.1-sol".to_owned(), Some(Entitled)),
                ("grok-4.7".to_owned(), Some(PermissionMissing)),
            ]
        );
        // An excluded action is not granted, even under a wildcard.
        let excluded = r#"{"value": [{"dataActions": ["Microsoft.CognitiveServices/*"],
            "notDataActions": ["Microsoft.CognitiveServices/accounts/MaaS/*"]}]}"#;
        assert_eq!(
            entitlements(excluded)[0],
            ("claude-sonnet-5-5".to_owned(), Some(PermissionMissing))
        );
    }

    /// Permissions that cannot be read leave the entitlement unknown; the
    /// listing still stands.
    #[tokio::test]
    async fn unreadable_permissions_leave_entitlement_unknown() {
        let (endpoint, _requests) = serve(|_| {
            vec![
                account("acme"),
                (403, json!({"error": {"code": "AuthorizationFailed"}})),
                (200, json!({"value": [sandbox_page()["value"][0]]})),
            ]
        });
        let (lister, _keep) = lister(&endpoint).await;
        let models = lister.list().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].entitlement, None);
    }

    /// The lister for inference with Entra, and what keeps its token source
    /// working: the fake directory, and the identity it holds only weakly.
    async fn lister(
        endpoint: &str,
    ) -> (
        DeploymentLister,
        (entra::FakeEntra, crate::azure_credentials::AzureIdentity),
    ) {
        lister_for(endpoint, None).await
    }

    /// The lister for inference with `api_key`, or with Entra if `None`.
    async fn lister_for(
        endpoint: &str,
        api_key: Option<&str>,
    ) -> (
        DeploymentLister,
        (entra::FakeEntra, crate::azure_credentials::AzureIdentity),
    ) {
        let fake = entra::start().await;
        let identity = entra::secret_identity(&fake);
        let management = AzureManagement {
            endpoint: endpoint.to_owned(),
            resource_id: RESOURCE.to_owned(),
            token: identity.token(identity.management_scope()),
        };
        let inference = match api_key {
            Some(key) => AzureAuth::ApiKey(key.to_owned()),
            None => AzureAuth::Entra(management.token.clone()),
        };
        (
            DeploymentLister::new(
                &management,
                &Url::parse("https://acme.services.ai.azure.com").unwrap(),
                None,
                "azure-ai-foundry",
                Duration::from_secs(1),
                Duration::from_secs(5),
                1024 * 1024,
            )
            .unwrap()
            .checking_entitlement_for(&inference),
            (fake, identity),
        )
    }

    /// With a key, inference does not use the identity's roles, so they
    /// are not read and entitlement stays unknown.
    #[tokio::test]
    async fn key_inference_leaves_entitlement_unknown() {
        let (endpoint, requests) = serve(|_| {
            vec![
                account("acme"),
                (200, json!({"value": [sandbox_page()["value"][0]]})),
            ]
        });
        let (lister, _keep) = lister_for(&endpoint, Some("key")).await;
        let models = lister.list().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].entitlement, None);
        requests.recv().unwrap();
        assert!(requests.recv().unwrap().contains("/deployments?"));
    }

    /// A grant on a later page of permissions counts; a list that cannot
    /// be read in full leaves entitlement unknown.
    #[tokio::test]
    async fn every_page_of_permissions_is_read() {
        let reader_then_user = |endpoint: &str| {
            let mut first: Value = serde_json::from_str(READER_ONLY).unwrap();
            first["nextLink"] = json!(format!(
                "{endpoint}{RESOURCE}/providers/Microsoft.Authorization/permissions?api-version={PERMISSIONS_API_VERSION}&$skipToken=2"
            ));
            first
        };
        let (endpoint, requests) = serve(|endpoint| {
            vec![
                account("acme"),
                (200, reader_then_user(endpoint)),
                permissions(USER_ROLE),
                (200, json!({"value": [sandbox_page()["value"][0]]})),
            ]
        });
        let (paged, _keep) = lister(&endpoint).await;
        let models = paged.list().await.unwrap();
        assert_eq!(models[0].entitlement, Some(Entitlement::Entitled));
        requests.recv().unwrap();
        requests.recv().unwrap();
        assert!(
            requests
                .recv()
                .unwrap()
                .contains("permissions?api-version=2022-04-01&$skipToken=2")
        );

        let (endpoint, _requests) = serve(|_| {
            vec![
                account("acme"),
                (200, reader_then_user("https://attacker.example")),
                (200, json!({"value": [sandbox_page()["value"][0]]})),
            ]
        });
        let (offsite, _keep) = lister(&endpoint).await;
        let models = offsite.list().await.unwrap();
        assert_eq!(models[0].entitlement, None);
    }

    #[tokio::test]
    async fn lists_every_page_with_a_resource_manager_token() {
        let (endpoint, requests) = serve(|endpoint| {
            vec![
                account("Acme"),
                permissions(USER_ROLE),
                (
                    200,
                    json!({"value": [sandbox_page()["value"][0]],
                           "nextLink": format!("{endpoint}{RESOURCE}/deployments?api-version={API_VERSION}&$skipToken=2")}),
                ),
                (200, json!({"value": [sandbox_page()["value"][1]]})),
            ]
        });
        let (lister, (fake, _identity)) = lister(&endpoint).await;
        let models = lister.list().await.unwrap();
        assert_eq!(
            models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["claude-sonnet-5-5", "gpt-6.1-sol"]
        );
        assert!(
            models
                .iter()
                .all(|model| model.entitlement == Some(Entitlement::Entitled))
        );
        assert_eq!(
            requests.recv().unwrap(),
            format!("GET {RESOURCE}?api-version={API_VERSION} HTTP/1.1 | Bearer token-1")
        );
        assert_eq!(
            requests.recv().unwrap(),
            format!(
                "GET {RESOURCE}/providers/Microsoft.Authorization/permissions?api-version={PERMISSIONS_API_VERSION} HTTP/1.1 | Bearer token-1"
            )
        );
        assert_eq!(
            requests.recv().unwrap(),
            format!(
                "GET {RESOURCE}/deployments?api-version={API_VERSION} HTTP/1.1 | Bearer token-1"
            )
        );
        assert!(requests.recv().unwrap().contains("$skipToken=2"));
        let (_, form) = fake.requests.lock().unwrap()[0].clone();
        assert_eq!(form["scope"], "https://management.azure.com/.default");
    }

    #[tokio::test]
    async fn a_next_link_elsewhere_is_refused() {
        let (endpoint, _requests) = serve(|_| {
            vec![
                account("acme"),
                permissions(USER_ROLE),
                (
                    200,
                    json!({"value": [], "nextLink": format!("https://attacker.example{RESOURCE}/deployments")}),
                ),
            ]
        });
        let (lister, _keep) = lister(&endpoint).await;
        let error = lister.list().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Protocol);
    }

    #[tokio::test]
    async fn a_missing_role_reports_resource_managers_code() {
        let (endpoint, _requests) = serve(|_| {
            vec![(
                403,
                json!({"error": {"code": "AuthorizationFailed", "message": "The client 'x' does not have authorization"}}),
            )]
        });
        let (lister, _keep) = lister(&endpoint).await;
        let error = lister.list().await.unwrap_err();
        assert_eq!(error.category(), ProviderErrorCategory::Authorization);
        assert_eq!(error.provider_code(), Some("AuthorizationFailed"));
        assert_eq!(error.http_status(), Some(403));
        assert_eq!(error.provider_request_id(), Some("req-403"));
    }

    /// A resource ID naming another account than the one inference calls
    /// lists nothing: its deployments are not where requests go.
    #[tokio::test]
    async fn another_account_than_inference_calls_is_not_listed() {
        for other in [
            account("contoso"),
            (200, json!({"kind": "AIServices", "properties": {}})),
        ] {
            let (endpoint, requests) = serve(|_| vec![other]);
            let (lister, _keep) = lister(&endpoint).await;
            let error = lister.list().await.unwrap_err();
            assert_eq!(error.category(), ProviderErrorCategory::InvalidRequest);
            assert_eq!(error.provider_code(), Some("ResourceMismatch"));
            requests.recv().unwrap();
            assert!(requests.try_recv().is_err(), "no deployments are read");
        }
    }
}
