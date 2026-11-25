//! Live tests against real AWS, Azure and Google accounts: each tenant
//! credential kind, signed in and used for one small chat completion, plus
//! the refusals the per-organization trust relies on.
//!
//! They call real, billable services, so they never run unless their
//! variables are set (`scripts/cloud-live-test.sh` sets them from the
//! `pulumi` repository's dev stacks, which create the test identities):
//!
//! - AWS: `GATEWAY_LIVE_AWS_ROLE_ARN`, `GATEWAY_LIVE_AWS_REGION`,
//!   `GATEWAY_LIVE_AWS_MODEL`, `GATEWAY_LIVE_ORGANIZATION_ID`, and the
//!   gateway's own credentials in `AWS_ACCESS_KEY_ID`,
//!   `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`;
//! - Azure: `GATEWAY_LIVE_AZURE_ENDPOINT`, `GATEWAY_LIVE_AZURE_DEPLOYMENT`,
//!   and the gateway's app in `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and
//!   `AZURE_CLIENT_SECRET`;
//! - Google: `GATEWAY_LIVE_GCP_SERVICE_ACCOUNT`, `GATEWAY_LIVE_GCP_PROJECT`,
//!   `GATEWAY_LIVE_GEMINI_MODEL`, and Application Default Credentials. On
//!   Gemini Enterprise Agent Platform: the Gemini API no longer admits new
//!   service accounts. Model listing needs only the project and Application
//!   Default Credentials; one embeddings call also needs
//!   `GATEWAY_LIVE_GEMINI_EMBEDDING_MODEL` (for example
//!   `text-embedding-005`). Both run in `GATEWAY_LIVE_GCP_LOCATION`, default
//!   `global`.
//!
//! - Bedrock embeddings: `GATEWAY_LIVE_AWS_REGION`,
//!   `GATEWAY_LIVE_AWS_EMBEDDING_MODELS` (comma-separated, for example
//!   `amazon.titan-embed-text-v2:0,cohere.embed-v4:0`) and the gateway's own
//!   AWS credentials as above.
//!
//! Prompts are a fixed test string; nothing tenant-related is logged.

use std::time::Duration;

use crate::{
    application::provider_resolver::ProviderTemplate,
    aws_credentials::AwsCredentialSource,
    azure_credentials::AzureIdentity,
    config::{
        AgentPlatform, AzureAiFoundryConfig, AzureAuth, AzureOpenAiConfig, BedrockConfig,
        GeminiAuth, GeminiConfig,
    },
    domain::{
        catalog::{CatalogOperation, ProviderFamily},
        credentials::{ProviderSecret, ProviderSecretInput},
        discovery::ModelAvailability,
        identity::OrganizationId,
        inference::{
            ChatMessage, ChatRequest, ChatRole, CorrelationId, DeploymentModelReference,
            EmbeddingRequest, InferenceContext, ProviderAttemptIdempotencyKey,
            ProviderErrorCategory, UsageQuality,
        },
    },
    gcp_credentials::{CLOUD_PLATFORM_SCOPE, GoogleIdentity},
};

/// The variable, or `None` (with a note) when the test should not run.
fn var(name: &str) -> Option<String> {
    let value = std::env::var(name).ok().filter(|value| !value.is_empty());
    if value.is_none() {
        eprintln!("skipping: {name} is not set");
    }
    value
}

fn context(model: &str) -> InferenceContext {
    InferenceContext::new(
        CorrelationId::new(),
        ProviderAttemptIdempotencyKey::new(uuid::Uuid::new_v4().to_string()).unwrap(),
        DeploymentModelReference::new("live-test", model).unwrap(),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage {
            role: ChatRole::User,
            content: "Reply with the single word: ready".into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }],
        temperature: Some(0.0),
        max_tokens: Some(16),
        ..Default::default()
    }
}

/// One chat completion through `template`'s adapter, which must answer.
async fn chat(template: &ProviderTemplate, model: &str) -> String {
    let response = template
        .adapter()
        .unwrap()
        .chat(context(model), request())
        .await
        .unwrap_or_else(|error| panic!("{:?}: {error:?}", template.family()));
    let text = response.choices()[0].message.content.clone();
    assert!(
        !text.trim().is_empty(),
        "{:?} answered nothing",
        template.family()
    );
    text
}

fn secret(family: ProviderFamily, input: ProviderSecretInput) -> ProviderSecret {
    ProviderSecret::for_family(family, input).unwrap()
}

#[tokio::test]
async fn live_aws_assumes_the_organization_role_and_no_other() {
    let (Some(role_arn), Some(region), Some(model), Some(organization), Some(id), Some(key)) = (
        var("GATEWAY_LIVE_AWS_ROLE_ARN"),
        var("GATEWAY_LIVE_AWS_REGION"),
        var("GATEWAY_LIVE_AWS_MODEL"),
        var("GATEWAY_LIVE_ORGANIZATION_ID"),
        var("AWS_ACCESS_KEY_ID"),
        var("AWS_SECRET_ACCESS_KEY"),
    ) else {
        return;
    };
    let gateway = AwsCredentialSource::fixed(crate::aws::AwsCredentials {
        access_key_id: id,
        secret_access_key: zeroize::Zeroizing::new(key),
        session_token: std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .map(zeroize::Zeroizing::new),
    });
    let template = ProviderTemplate::Bedrock(BedrockConfig {
        base_url: format!("https://bedrock-runtime.{region}.amazonaws.com"),
        control_base_url: format!("https://bedrock.{region}.amazonaws.com"),
        region,
        credentials: gateway,
        sts_endpoint: None,
        s3_base_url: None,
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
    });
    // The process's own identity.
    chat(&template, &model).await;

    // The tenant's role, for the organization its trust policy names.
    let role = secret(
        ProviderFamily::Bedrock,
        ProviderSecretInput {
            role_arn: Some(role_arn),
            ..ProviderSecretInput::default()
        },
    );
    let organization = OrganizationId(organization.parse().unwrap());
    let assumed = template.with_secret(&role, organization).unwrap();
    chat(&assumed, &model).await;

    // Any other organization presents its own external ID: refused.
    let other = template.with_secret(&role, OrganizationId::new()).unwrap();
    let error = other
        .adapter()
        .unwrap()
        .chat(context(&model), request())
        .await
        .unwrap_err();
    assert_eq!(error.category(), ProviderErrorCategory::Authentication);
    assert!(error.was_not_sent());
}

#[tokio::test]
async fn live_aws_embeds_with_each_embedding_schema() {
    let (Some(region), Some(models), Some(id), Some(key)) = (
        var("GATEWAY_LIVE_AWS_REGION"),
        var("GATEWAY_LIVE_AWS_EMBEDDING_MODELS"),
        var("AWS_ACCESS_KEY_ID"),
        var("AWS_SECRET_ACCESS_KEY"),
    ) else {
        return;
    };
    let credentials = AwsCredentialSource::fixed(crate::aws::AwsCredentials {
        access_key_id: id,
        secret_access_key: zeroize::Zeroizing::new(key),
        session_token: std::env::var("AWS_SESSION_TOKEN")
            .ok()
            .map(zeroize::Zeroizing::new),
    });
    let template = ProviderTemplate::Bedrock(BedrockConfig {
        base_url: format!("https://bedrock-runtime.{region}.amazonaws.com"),
        control_base_url: format!("https://bedrock.{region}.amazonaws.com"),
        region,
        credentials,
        sts_endpoint: None,
        s3_base_url: None,
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 4 * 1024 * 1024,
    });
    let adapter = template.adapter().unwrap();
    for model in models
        .split(',')
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        let response = adapter
            .embeddings(
                context(model),
                EmbeddingRequest {
                    input: vec!["ready".into(), "steady".into()],
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{model} embeddings: {error:?}"));
        assert_eq!(response.data().len(), 2);
        let dimensions = response.data()[0].vector().len();
        assert!(dimensions > 0);
        assert!(
            response
                .data()
                .iter()
                .all(|data| data.vector().len() == dimensions)
        );
        let input = response.usage().unwrap().usage().input_tokens().unwrap();
        assert!(input.value().unwrap() > 0);
        eprintln!(
            "{model}: {dimensions} dimensions, input tokens {:?}",
            input.value()
        );
        adapter
            .probe_model(model)
            .await
            .unwrap_or_else(|error| panic!("{model} probe: {error:?}"));
    }
}

#[tokio::test]
async fn live_azure_signs_in_as_the_gateway_and_in_the_tenant_directory() {
    // The client ID and secret are checked too: without the secret,
    // `from_env` would pick a managed identity instead of the test's app.
    let (Some(endpoint), Some(deployment), Some(tenant_id), Some(_), Some(_)) = (
        var("GATEWAY_LIVE_AZURE_ENDPOINT"),
        var("GATEWAY_LIVE_AZURE_DEPLOYMENT"),
        var("AZURE_TENANT_ID"),
        var("AZURE_CLIENT_ID"),
        var("AZURE_CLIENT_SECRET"),
    ) else {
        return;
    };
    let identity = AzureIdentity::from_env(&|name: &str| std::env::var(name).ok())
        .expect("AZURE_TENANT_ID, AZURE_CLIENT_ID and AZURE_CLIENT_SECRET name an identity");
    let endpoint = endpoint.trim_end_matches('/').to_owned();
    let openai = ProviderTemplate::AzureOpenAi(AzureOpenAiConfig {
        base_url: endpoint.clone(),
        auth: AzureAuth::Entra(identity.token(identity.cognitive_services_scope())),
        identity: Some(identity.clone()),
        management: None,
        api_version: "2024-10-21".into(),
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
    });
    // The process's own identity, no key.
    chat(&openai, &deployment).await;

    // A tenant's directory (here the gateway app's own), with the resource
    // endpoint the credential names.
    let directory = secret(
        ProviderFamily::AzureOpenAi,
        ProviderSecretInput {
            tenant_id: Some(tenant_id.clone()),
            endpoint: Some(endpoint.clone()),
            ..ProviderSecretInput::default()
        },
    );
    let tenant = openai
        .with_secret(&directory, OrganizationId::new())
        .unwrap();
    chat(&tenant, &deployment).await;

    // Azure AI Foundry's model inference API on the same resource.
    let foundry = ProviderTemplate::AzureAiFoundry(AzureAiFoundryConfig {
        base_url: endpoint.clone(),
        auth: AzureAuth::Entra(identity.token(identity.cognitive_services_scope())),
        identity: Some(identity.clone()),
        management: None,
        api_version: "2024-05-01-preview".into(),
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
    });
    chat(&foundry, &deployment).await;

    // A directory the gateway's app was never consented in: refused.
    let stranger = secret(
        ProviderFamily::AzureOpenAi,
        ProviderSecretInput {
            tenant_id: Some("00000000-0000-4000-8000-000000000001".into()),
            endpoint: Some(endpoint),
            ..ProviderSecretInput::default()
        },
    );
    let error = openai
        .with_secret(&stranger, OrganizationId::new())
        .unwrap()
        .adapter()
        .unwrap()
        .chat(context(&deployment), request())
        .await
        .unwrap_err();
    assert_eq!(error.category(), ProviderErrorCategory::Authentication);
    assert!(error.was_not_sent());
}

#[tokio::test]
async fn live_google_runs_gemini_on_agent_platform() {
    let (Some(service_account), Some(project), Some(model)) = (
        var("GATEWAY_LIVE_GCP_SERVICE_ACCOUNT"),
        var("GATEWAY_LIVE_GCP_PROJECT"),
        var("GATEWAY_LIVE_GEMINI_MODEL"),
    ) else {
        return;
    };
    let identity = GoogleIdentity::from_env(&|name: &str| std::env::var(name).ok())
        .expect("Application Default Credentials name an identity");
    let platform = AgentPlatform::new(&project, "global").unwrap();
    let gemini = ProviderTemplate::Gemini(GeminiConfig {
        base_url: platform.base_url(),
        auth: GeminiAuth::Google {
            token: identity.token(CLOUD_PLATFORM_SCOPE),
            // As the gateway's configuration does.
            quota_project: identity.quota_project().map(str::to_owned),
        },
        identity: Some(identity),
        agent_platform: Some(platform),
        agent_platform_location: None,
        storage_base_url: None,
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 1024 * 1024,
    });
    // The process's own identity, keyless.
    chat(&gemini, &model).await;
    // Token counting, which reservations rely on.
    let counted = gemini
        .adapter()
        .unwrap()
        .count_chat_input_tokens(&context(&model), &request())
        .await
        .unwrap();
    assert!(counted > 0);

    // The tenant's service account, impersonated, in its own project.
    let account = secret(
        ProviderFamily::Gemini,
        ProviderSecretInput {
            service_account: Some(service_account),
            project_id: Some(project.clone()),
            ..ProviderSecretInput::default()
        },
    );
    let tenant = gemini.with_secret(&account, OrganizationId::new()).unwrap();
    chat(&tenant, &model).await;

    // A service account the gateway may not impersonate: refused.
    let stranger = secret(
        ProviderFamily::Gemini,
        ProviderSecretInput {
            service_account: Some(format!("no-such-account@{project}.iam.gserviceaccount.com")),
            ..ProviderSecretInput::default()
        },
    );
    let error = gemini
        .with_secret(&stranger, OrganizationId::new())
        .unwrap()
        .adapter()
        .unwrap()
        .chat(context(&model), request())
        .await
        .unwrap_err();
    assert!(error.was_not_sent());
}

/// Gemini on Agent Platform with the process's own identity, in
/// `GATEWAY_LIVE_GCP_LOCATION` (default `global`).
fn agent_platform(project: &str) -> ProviderTemplate {
    let location = std::env::var("GATEWAY_LIVE_GCP_LOCATION")
        .ok()
        .filter(|location| !location.is_empty())
        .unwrap_or_else(|| "global".to_owned());
    let identity = GoogleIdentity::from_env(&|name: &str| std::env::var(name).ok())
        .expect("Application Default Credentials name an identity");
    let platform = AgentPlatform::new(project, &location).unwrap();
    ProviderTemplate::Gemini(GeminiConfig {
        base_url: platform.base_url(),
        auth: GeminiAuth::Google {
            token: identity.token(CLOUD_PLATFORM_SCOPE),
            // As the gateway's configuration does.
            quota_project: identity.quota_project().map(str::to_owned),
        },
        identity: Some(identity),
        agent_platform: Some(platform),
        agent_platform_location: None,
        storage_base_url: None,
        request_timeout: Duration::from_secs(60),
        connect_timeout: Duration::from_secs(10),
        max_response_bytes: 8 * 1024 * 1024,
    })
}

#[tokio::test]
async fn live_google_lists_gemini_and_embedding_models_on_agent_platform() {
    let Some(project) = var("GATEWAY_LIVE_GCP_PROJECT") else {
        return;
    };
    let models = agent_platform(&project)
        .adapter()
        .unwrap()
        .list_models()
        .await
        .unwrap_or_else(|error| panic!("listing: {error:?}"));
    let with = |operation: CatalogOperation| {
        models
            .iter()
            .filter(|model| {
                model
                    .operations
                    .as_ref()
                    .is_some_and(|operations| operations.contains(&operation))
            })
            .count()
    };
    assert!(
        with(CatalogOperation::Chat) > 0,
        "no Gemini chat model listed"
    );
    assert!(
        with(CatalogOperation::Embeddings) > 0,
        "no embedding model listed"
    );
    // Google's catalog covers every location.
    assert!(
        models
            .iter()
            .all(|model| model.availability == ModelAvailability::Unknown)
    );
    eprintln!("listed {} models", models.len());
}

#[tokio::test]
async fn live_google_embeds_on_agent_platform() {
    let (Some(project), Some(model)) = (
        var("GATEWAY_LIVE_GCP_PROJECT"),
        var("GATEWAY_LIVE_GEMINI_EMBEDDING_MODEL"),
    ) else {
        return;
    };
    let response = agent_platform(&project)
        .adapter()
        .unwrap()
        .embeddings(
            context(&model),
            EmbeddingRequest {
                input: vec!["ready".into(), "steady".into()],
            },
        )
        .await
        .unwrap_or_else(|error| panic!("embeddings: {error:?}"));
    assert_eq!(response.data().len(), 2);
    assert!(response.data().iter().all(|data| !data.vector().is_empty()));
    let usage = response.usage().unwrap().usage();
    let input = usage.input_tokens().unwrap();
    assert!(input.value().unwrap() > 0);
    // Exact when Google reports token counts, which is what settles it.
    eprintln!("input tokens: {:?} ({:?})", input.value(), usage.quality());
    assert_ne!(usage.quality(), UsageQuality::Unknown);
}
