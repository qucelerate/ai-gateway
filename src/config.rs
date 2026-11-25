use std::{env, ffi::OsString, fmt, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use base64::Engine as _;
use reqwest::header;
use thiserror::Error;
use url::Url;

use crate::{
    azure_credentials::{AzureIdentity, AzureTokenSource},
    domain::{
        catalog::ProviderFamily,
        credentials::CredentialCipher,
        keys::{ApiKeyPepper, KeyError},
    },
    gcp_credentials::{CLOUD_PLATFORM_SCOPE, GoogleIdentity, GoogleTokenSource},
};

#[derive(Clone)]
pub(crate) struct OpenAiConfig {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct AnthropicConfig {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

/// How the Gemini adapter authenticates: an API key, or a Google OAuth
/// token billed to `quota_project` (else to the token's own project).
#[derive(Clone)]
pub(crate) enum GeminiAuth {
    ApiKey(String),
    Google {
        token: GoogleTokenSource,
        quota_project: Option<String>,
    },
}

impl fmt::Debug for GeminiAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("GeminiAuth::ApiKey(REDACTED)"),
            Self::Google { token, .. } => write!(f, "GeminiAuth::Google({token:?})"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct GeminiConfig {
    pub(crate) base_url: String,
    pub(crate) auth: GeminiAuth,
    /// The gateway's Google identity, which impersonates tenants' service
    /// accounts; `None` when its settings are invalid and a key is set.
    pub(crate) identity: Option<GoogleIdentity>,
    /// Gemini on Gemini Enterprise Agent Platform (formerly Vertex AI) in
    /// this project and location, rather than the Gemini API; `base_url` is
    /// then its publisher URL.
    pub(crate) agent_platform: Option<AgentPlatform>,
    /// `GEMINI_AGENT_PLATFORM_LOCATION`: where tenants' service accounts
    /// call Agent Platform when theirs names none, whatever the process's
    /// own Gemini uses.
    pub(crate) agent_platform_location: Option<String>,
    /// `GEMINI_STORAGE_BASE_URL`: Cloud Storage for batch staging, instead
    /// of `https://storage.googleapis.com` (for a Private Service Connect
    /// endpoint).
    pub(crate) storage_base_url: Option<String>,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

/// The Gemini API's endpoint.
pub(crate) const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Where Gemini runs on Gemini Enterprise Agent Platform (formerly Vertex
/// AI): Google identities call it there, since the Gemini API no longer
/// admits new service accounts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentPlatform {
    pub(crate) project: String,
    pub(crate) location: String,
}

impl AgentPlatform {
    /// `None` unless `project` is a Google Cloud project ID and `location` a
    /// location name (`global`, `us-central1`, `eu`...).
    pub(crate) fn new(project: &str, location: &str) -> Option<Self> {
        let project_ok = (6..=30).contains(&project.len())
            && project.starts_with(|c: char| c.is_ascii_lowercase())
            && !project.ends_with('-')
            && project
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let location_ok = (2..=32).contains(&location.len())
            && location.starts_with(|c: char| c.is_ascii_lowercase())
            && location
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        (project_ok && location_ok).then(|| Self {
            project: project.to_owned(),
            location: location.to_owned(),
        })
    }

    /// The publisher URL Gemini models are called under: the global
    /// endpoint, or the location's regional one.
    pub(crate) fn base_url(&self) -> String {
        let host = if self.location == "global" {
            "aiplatform.googleapis.com".to_owned()
        } else {
            format!("{}-aiplatform.googleapis.com", self.location)
        };
        format!(
            "https://{host}/v1/projects/{}/locations/{}/publishers/google",
            self.project, self.location
        )
    }
}

#[derive(Clone)]
pub(crate) struct BedrockConfig {
    pub(crate) base_url: String,
    /// The Bedrock control plane, which lists models; `base_url` is the
    /// runtime, which serves inference.
    pub(crate) control_base_url: String,
    pub(crate) region: String,
    /// The gateway's own AWS identity: static keys, or the runtime role.
    /// Tenant roles are assumed with it.
    pub(crate) credentials: crate::aws_credentials::AwsCredentialSource,
    /// `BEDROCK_STS_ENDPOINT`: STS for assuming tenant roles, instead of the
    /// regional endpoint (for a VPC endpoint).
    pub(crate) sts_endpoint: Option<String>,
    /// `BEDROCK_S3_BASE_URL`: S3 for batch staging, addressed path-style,
    /// instead of the bucket's regional endpoint (for a VPC endpoint).
    pub(crate) s3_base_url: Option<String>,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

/// How an Azure adapter authenticates: an API key, or an Entra token.
#[derive(Clone)]
pub(crate) enum AzureAuth {
    ApiKey(String),
    Entra(AzureTokenSource),
}

impl fmt::Debug for AzureAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("AzureAuth::ApiKey(REDACTED)"),
            Self::Entra(source) => write!(f, "AzureAuth::Entra({source:?})"),
        }
    }
}

/// Where an Azure adapter reads its resource's deployments (D-018): Azure
/// Resource Manager in the identity's cloud, the resource's ID, and Entra
/// tokens for Resource Manager in the resource's directory. Reader on the
/// resource is enough.
#[derive(Clone, Debug)]
pub(crate) struct AzureManagement {
    pub(crate) endpoint: String,
    /// `/subscriptions/<id>/resourceGroups/<group>/providers/Microsoft.CognitiveServices/accounts/<name>`.
    pub(crate) resource_id: String,
    pub(crate) token: AzureTokenSource,
}

#[derive(Clone)]
pub(crate) struct AzureOpenAiConfig {
    pub(crate) base_url: String,
    pub(crate) auth: AzureAuth,
    /// The gateway's Entra identity, which signs in to tenants' directories;
    /// `None` when its settings are invalid and a key is configured.
    pub(crate) identity: Option<AzureIdentity>,
    /// Lists the resource's deployments; `None` leaves discovery unsupported.
    pub(crate) management: Option<AzureManagement>,
    pub(crate) api_version: String,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

#[derive(Clone)]
pub(crate) struct AzureAiFoundryConfig {
    pub(crate) base_url: String,
    pub(crate) auth: AzureAuth,
    /// The gateway's Entra identity, which signs in to tenants' directories;
    /// `None` when its settings are invalid and a key is configured.
    pub(crate) identity: Option<AzureIdentity>,
    /// Lists the resource's deployments; `None` leaves discovery unsupported.
    pub(crate) management: Option<AzureManagement>,
    pub(crate) api_version: String,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
}

/// The process adapter's Resource Manager reads, from `<prefix>_RESOURCE_ID`:
/// with the gateway's own Entra identity in its own directory. `None` when
/// unset.
fn azure_management_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    identity: Option<&AzureIdentity>,
    prefix: &str,
    error: ConfigError,
) -> Result<Option<AzureManagement>, ConfigError> {
    let Some(resource_id) = provider_env(&format!("{prefix}_RESOURCE_ID"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !crate::domain::onboarding::azure_resource_id(&resource_id) {
        return Err(error);
    }
    // Resource Manager takes only Entra tokens, never a resource key.
    let identity = identity.ok_or(error)?;
    Ok(Some(AzureManagement {
        endpoint: identity.management_endpoint().to_owned(),
        resource_id,
        token: identity.token(identity.management_scope()),
    }))
}

#[derive(Clone)]
pub(crate) struct OpenAiCompatibleConfig {
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) extra_headers: Vec<(String, String)>,
    pub(crate) request_stream_usage: bool,
    pub(crate) request_timeout: Duration,
    pub(crate) connect_timeout: Duration,
    pub(crate) max_response_bytes: usize,
    /// A client that can reach only the addresses the egress policy checked
    /// for a tenant's endpoint ([`crate::jobs::egress::pinned_client`]),
    /// built from [`OpenAiCompatibleConfig::client_builder`]. `None` for
    /// the operator's own endpoint, which the adapter builds a client for.
    pub(crate) pinned_client: Option<reqwest::Client>,
}

impl OpenAiCompatibleConfig {
    /// A tenant's endpoint: its base URL and key, with settings suited to
    /// self-hosted servers. Streams ask for usage (`stream_options`), and an
    /// idle read may take two minutes, as a long prompt can before its first
    /// token. The caller sets `pinned_client` once the policy allows the
    /// endpoint.
    pub(crate) fn for_tenant_endpoint(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            extra_headers: Vec::new(),
            request_stream_usage: true,
            request_timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(10),
            max_response_bytes: 8 * 1024 * 1024,
            pinned_client: None,
        }
    }

    /// The client settings the adapter uses, for a client built elsewhere.
    pub(crate) fn client_builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.connect_timeout)
            // A total timeout would terminate an otherwise healthy long-lived
            // stream. Bound each idle read instead; connect timeout remains a
            // separate bound for establishing the request.
            .read_timeout(self.request_timeout)
    }
}

/// Families with a fixed public endpoint can serve a stored credential for an
/// adapter instance key that has no process configuration. The endpoint,
/// timeouts, and response limit are the same defaults the environment path
/// applies when only the API key is set.
impl OpenAiConfig {
    pub(crate) fn with_default_endpoint(api_key: &str) -> Result<Self, ConfigError> {
        openai_from_env(&api_key_only("OPENAI_API_KEY", api_key))?.ok_or(ConfigError::InvalidOpenAi)
    }
}

impl AnthropicConfig {
    pub(crate) fn with_default_endpoint(api_key: &str) -> Result<Self, ConfigError> {
        anthropic_from_env(&api_key_only("ANTHROPIC_API_KEY", api_key))?
            .ok_or(ConfigError::InvalidAnthropic)
    }
}

impl GeminiConfig {
    pub(crate) fn with_default_endpoint(api_key: &str) -> Result<Self, ConfigError> {
        gemini_from_env(&api_key_only("GEMINI_API_KEY", api_key), None)?
            .ok_or(ConfigError::InvalidGemini)
    }
}

/// The provider environment with `enabling` (the setting that enables the
/// family without a key: an endpoint, or Gemini's quota project) set to
/// `placeholder` and no key: a family's settings for tenants' own
/// credentials, which replace the placeholder.
fn for_tenants<'a>(
    provider_env: &'a impl Fn(&str) -> Result<String, env::VarError>,
    enabling: &'static str,
    api_key: &'static str,
    placeholder: &'static str,
) -> impl Fn(&str) -> Result<String, env::VarError> + 'a {
    move |name| {
        if name == enabling {
            Ok(placeholder.to_owned())
        } else if name == api_key || name.ends_with("_RESOURCE_ID") {
            // Nor the process's resource: each credential names its own.
            Err(env::VarError::NotPresent)
        } else {
            provider_env(name)
        }
    }
}

/// Gemini's settings for tenants' service accounts: Agent Platform in a
/// placeholder project (each credential names its own), in the configured
/// location; no key, quota project or URL of the process's.
fn gemini_for_tenants<'a>(
    provider_env: &'a impl Fn(&str) -> Result<String, env::VarError>,
) -> impl Fn(&str) -> Result<String, env::VarError> + 'a {
    move |name| match name {
        "GEMINI_AGENT_PLATFORM_PROJECT" => Ok("tenant-project".to_owned()),
        "GEMINI_API_KEY" | "GEMINI_QUOTA_PROJECT" | "GEMINI_BASE_URL" => {
            Err(env::VarError::NotPresent)
        }
        _ => provider_env(name),
    }
}

fn api_key_only<'a>(
    name: &'static str,
    api_key: &'a str,
) -> impl Fn(&str) -> Result<String, env::VarError> + 'a {
    move |requested| {
        if requested == name {
            Ok(api_key.to_owned())
        } else {
            Err(env::VarError::NotPresent)
        }
    }
}

#[derive(Default)]
pub(crate) struct ProviderConfigs {
    openai: Option<OpenAiConfig>,
    anthropic: Option<AnthropicConfig>,
    gemini: Option<GeminiConfig>,
    bedrock: Option<BedrockConfig>,
    azure_openai: Option<AzureOpenAiConfig>,
    azure_ai_foundry: Option<AzureAiFoundryConfig>,
    openai_compatible: Option<OpenAiCompatibleConfig>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OidcConfig {
    pub enabled: bool,
    /// The only browser origin accepted for state-changing admin requests.
    pub origin: Option<String>,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    /// Fetch the person's photo from Microsoft Graph at sign-in
    /// (`OIDC_PROFILE_PHOTO=microsoft-graph`; Entra ID only).
    pub microsoft_graph_photo: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionConfig {
    hmac_secret: Option<Vec<u8>>,
    encryption_key: Option<Vec<u8>>,
    pub ttl_seconds: u64,
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionConfig")
            .field(
                "hmac_secret",
                &if self.hmac_secret.is_some() {
                    "configured"
                } else {
                    "disabled"
                },
            )
            .field(
                "encryption_key",
                &if self.encryption_key.is_some() {
                    "configured"
                } else {
                    "disabled"
                },
            )
            .field("ttl_seconds", &self.ttl_seconds)
            .finish()
    }
}

const DEFAULT_BIND_ADDR: &str = "127.0.0.1:3000";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayProfile {
    AllInOne,
    Api,
    Worker,
    /// Inference only: `/api/inference/v1/*`, `/healthz` and `/readyz`, no
    /// admin API, no sign-in and no worker loops; see
    /// docs/architecture-plan.md §2.
    DataPlane,
}

impl GatewayProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AllInOne => "all-in-one",
            Self::Api => "api",
            Self::Worker => "worker",
            Self::DataPlane => "data-plane",
        }
    }

    /// Whether the profile serves inference, and so routes from loaded
    /// configuration versions.
    pub fn serves_inference(self) -> bool {
        self != Self::Worker
    }
}

impl FromStr for GatewayProfile {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all-in-one" => Ok(Self::AllInOne),
            "api" => Ok(Self::Api),
            "worker" => Ok(Self::Worker),
            "data-plane" => Ok(Self::DataPlane),
            _ => Err(ConfigError::InvalidProfile {
                value: value.to_owned(),
            }),
        }
    }
}

impl fmt::Display for GatewayProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GatewayCommand {
    Run,
    Migrate,
    CatalogApply {
        organization: String,
        file: PathBuf,
    },
    /// `config pin --organization <id> --version <id>`, `config unpin
    /// --organization <id>`, or `config list --organization <id>`: an
    /// operator's configuration pin, as the admin API sets it.
    Config {
        action: ConfigAction,
        organization: String,
        /// The version to pin; `None` for `unpin` and `list`.
        version: Option<String>,
    },
    /// `cloud-principal allow|revoke --organization <id>
    /// (--azure-tenant <tenant id> | --google-service-account <address>)`,
    /// or `cloud-principal list --organization <id>`.
    CloudPrincipal {
        action: CloudPrincipalAction,
        organization: String,
        /// Kind and value; `None` for `list`.
        principal: Option<(String, String)>,
    },
    /// `bootstrap-owner --organization <slug> --issuer <url> --subject <sub>
    /// [--organization-name <name>] [--display-name <name>]`: an
    /// organization's first owner, whom no admin can grant.
    BootstrapOwner {
        organization: String,
        organization_name: Option<String>,
        issuer: String,
        subject: String,
        display_name: Option<String>,
    },
    /// `invitation create --organization <slug> [--role owner]
    /// [--organization-name <name>] [--valid-hours 72]`, `invitation list
    /// --organization <slug>`, or `invitation revoke --organization <slug>
    /// --id <id>`.
    Invitation {
        action: InvitationAction,
        organization: String,
        organization_name: Option<String>,
        role: Option<String>,
        valid_hours: Option<u32>,
        id: Option<String>,
    },
    /// `agent create --organization <slug> --name <name> [--role owner]
    /// [--organization-name <name>] [--token-name <name>]
    /// [--expires-in-days <days>]`: an agent (created if missing) and a new
    /// token for it, so automation can reach the admin API headlessly.
    AgentCreate {
        organization: String,
        organization_name: Option<String>,
        name: String,
        role: Option<String>,
        token_name: Option<String>,
        expires_in_days: Option<u32>,
    },
    /// `setup-link`: a new first-run setup link, while no organization
    /// exists.
    SetupLink,
    /// `user reset-password --email <email>`: a single-use link that lets a
    /// local account choose a new password.
    UserResetPassword {
        email: String,
    },
    /// `sign-in-policy allow-passwords --organization <slug>`: stops
    /// requiring the organization's own identity provider, the operator's
    /// way back in when that provider fails.
    AllowPasswords {
        organization: String,
    },
    /// `admin grant (--email <email> | --issuer <url> --subject <sub>)`:
    /// makes an existing account a deployment administrator.
    AdminGrant {
        email: Option<String>,
        issuer: Option<String>,
        subject: Option<String>,
    },
    /// `doctor [--json] [--network]`: read-only checks of the configuration,
    /// database, providers and stores (see [`crate::doctor`]).
    Doctor {
        json: bool,
        network: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvitationAction {
    Create,
    List,
    Revoke,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigAction {
    Pin,
    Unpin,
    List,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudPrincipalAction {
    Allow,
    Revoke,
    List,
}

impl GatewayCommand {
    pub fn parse<I>(arguments: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut arguments = arguments.into_iter();
        match arguments.next() {
            None => Ok(Self::Run),
            Some(command) if command == OsString::from("migrate") && arguments.next().is_none() => {
                Ok(Self::Migrate)
            }
            Some(command) if command == OsString::from("catalog") => {
                if arguments.next() != Some(OsString::from("apply")) {
                    return Err(ConfigError::InvalidArgument {
                        value: "catalog apply".into(),
                    });
                }
                let mut organization = None;
                let mut file = None;
                while let Some(argument) = arguments.next() {
                    match argument.to_string_lossy().as_ref() {
                        "--organization" => organization = arguments.next(),
                        "--file" => file = arguments.next().map(PathBuf::from),
                        _ => {
                            return Err(ConfigError::InvalidArgument {
                                value: argument.to_string_lossy().into_owned(),
                            });
                        }
                    }
                }
                match (organization, file) {
                    (Some(organization), Some(file)) if !organization.is_empty() => {
                        Ok(Self::CatalogApply {
                            organization: organization.to_string_lossy().into_owned(),
                            file,
                        })
                    }
                    _ => Err(ConfigError::InvalidArgument {
                        value: "catalog apply requires --organization and --file".into(),
                    }),
                }
            }
            Some(command) if command == OsString::from("config") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "config pin --organization <id> --version <id> | \
                            config unpin|list --organization <id>"
                        .into(),
                };
                let action = match arguments.next().as_deref().and_then(|a| a.to_str()) {
                    Some("pin") => ConfigAction::Pin,
                    Some("unpin") => ConfigAction::Unpin,
                    Some("list") => ConfigAction::List,
                    _ => return Err(usage()),
                };
                let mut values: [Option<String>; 2] = Default::default();
                while let Some(argument) = arguments.next() {
                    let slot = match argument.to_string_lossy().as_ref() {
                        "--organization" => 0,
                        "--version" => 1,
                        _ => return Err(usage()),
                    };
                    let value = arguments
                        .next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(usage)?;
                    if values[slot].replace(value).is_some() {
                        return Err(usage());
                    }
                }
                let [organization, version] = values;
                let organization = organization.ok_or_else(usage)?;
                // A version for pin; none otherwise.
                if version.is_some() != (action == ConfigAction::Pin) {
                    return Err(usage());
                }
                Ok(Self::Config {
                    action,
                    organization,
                    version,
                })
            }
            Some(command) if command == OsString::from("cloud-principal") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "cloud-principal allow|revoke|list --organization <id> \
                            [--azure-tenant <id> | --google-service-account <address>]"
                        .into(),
                };
                let action = match arguments.next().as_deref().and_then(|a| a.to_str()) {
                    Some("allow") => CloudPrincipalAction::Allow,
                    Some("revoke") => CloudPrincipalAction::Revoke,
                    Some("list") => CloudPrincipalAction::List,
                    _ => return Err(usage()),
                };
                let mut organization = None;
                let mut principal = None;
                while let Some(argument) = arguments.next() {
                    let kind = match argument.to_string_lossy().as_ref() {
                        "--organization" => {
                            organization = arguments.next();
                            continue;
                        }
                        "--azure-tenant" => "azure_tenant",
                        "--google-service-account" => "google_service_account",
                        _ => return Err(usage()),
                    };
                    let value = arguments.next().ok_or_else(usage)?;
                    if principal.is_some() {
                        return Err(usage());
                    }
                    principal = Some((kind.to_owned(), value.to_string_lossy().into_owned()));
                }
                let organization = organization
                    .map(|value| value.to_string_lossy().into_owned())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(usage)?;
                // A principal for allow and revoke; none for list.
                if principal.is_some() == (action == CloudPrincipalAction::List) {
                    return Err(usage());
                }
                Ok(Self::CloudPrincipal {
                    action,
                    organization,
                    principal,
                })
            }
            Some(command) if command == OsString::from("invitation") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "invitation create --organization <slug> [--role <role>] \
                            [--organization-name <name>] [--valid-hours <hours>] | \
                            invitation list --organization <slug> | \
                            invitation revoke --organization <slug> --id <id>"
                        .into(),
                };
                let action = match arguments.next().as_deref().and_then(|a| a.to_str()) {
                    Some("create") => InvitationAction::Create,
                    Some("list") => InvitationAction::List,
                    Some("revoke") => InvitationAction::Revoke,
                    _ => return Err(usage()),
                };
                let mut values: [Option<String>; 5] = Default::default();
                while let Some(argument) = arguments.next() {
                    let slot = match argument.to_string_lossy().as_ref() {
                        "--organization" => 0,
                        "--organization-name" => 1,
                        "--role" => 2,
                        "--valid-hours" => 3,
                        "--id" => 4,
                        _ => return Err(usage()),
                    };
                    let value = arguments
                        .next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(usage)?;
                    if values[slot].replace(value).is_some() {
                        return Err(usage());
                    }
                }
                let [organization, organization_name, role, valid_hours, id] = values;
                let organization = organization.ok_or_else(usage)?;
                let valid_hours = valid_hours
                    .map(|hours| hours.parse::<u32>().ok().filter(|h| (1..=720).contains(h)))
                    .map(|hours| hours.ok_or_else(usage))
                    .transpose()?;
                // Each action takes only its own options.
                let fits = match action {
                    InvitationAction::Create => id.is_none(),
                    InvitationAction::List => {
                        id.is_none()
                            && role.is_none()
                            && valid_hours.is_none()
                            && organization_name.is_none()
                    }
                    InvitationAction::Revoke => {
                        id.is_some()
                            && role.is_none()
                            && valid_hours.is_none()
                            && organization_name.is_none()
                    }
                };
                if !fits {
                    return Err(usage());
                }
                Ok(Self::Invitation {
                    action,
                    organization,
                    organization_name,
                    role,
                    valid_hours,
                    id,
                })
            }
            Some(command) if command == OsString::from("agent") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "agent create --organization <slug> --name <name> [--role <role>] \
                            [--organization-name <name>] [--token-name <name>] \
                            [--expires-in-days <days>]"
                        .into(),
                };
                if arguments.next().as_deref().and_then(|a| a.to_str()) != Some("create") {
                    return Err(usage());
                }
                let mut values: [Option<String>; 6] = Default::default();
                while let Some(argument) = arguments.next() {
                    let slot = match argument.to_string_lossy().as_ref() {
                        "--organization" => 0,
                        "--organization-name" => 1,
                        "--name" => 2,
                        "--role" => 3,
                        "--token-name" => 4,
                        "--expires-in-days" => 5,
                        _ => return Err(usage()),
                    };
                    let value = arguments
                        .next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(usage)?;
                    if values[slot].replace(value).is_some() {
                        return Err(usage());
                    }
                }
                let [
                    organization,
                    organization_name,
                    name,
                    role,
                    token_name,
                    days,
                ] = values;
                let expires_in_days = days
                    .map(|days| days.parse::<u32>().ok().filter(|d| (1..=365).contains(d)))
                    .map(|days| days.ok_or_else(usage))
                    .transpose()?;
                Ok(Self::AgentCreate {
                    organization: organization.ok_or_else(usage)?,
                    organization_name,
                    name: name.ok_or_else(usage)?,
                    role,
                    token_name,
                    expires_in_days,
                })
            }
            Some(command) if command == OsString::from("setup-link") => {
                if let Some(extra) = arguments.next() {
                    return Err(ConfigError::InvalidArgument {
                        value: extra.to_string_lossy().into_owned(),
                    });
                }
                Ok(Self::SetupLink)
            }
            Some(command) if command == OsString::from("user") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "user reset-password --email <email>".into(),
                };
                if arguments.next().as_deref().and_then(|a| a.to_str()) != Some("reset-password")
                    || arguments.next().as_deref().and_then(|a| a.to_str()) != Some("--email")
                {
                    return Err(usage());
                }
                let email = arguments
                    .next()
                    .map(|value| value.to_string_lossy().into_owned())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(usage)?;
                if arguments.next().is_some() {
                    return Err(usage());
                }
                Ok(Self::UserResetPassword { email })
            }
            Some(command) if command == OsString::from("sign-in-policy") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "sign-in-policy allow-passwords --organization <slug>".into(),
                };
                if arguments.next().as_deref().and_then(|a| a.to_str()) != Some("allow-passwords")
                    || arguments.next().as_deref().and_then(|a| a.to_str())
                        != Some("--organization")
                {
                    return Err(usage());
                }
                let organization = arguments
                    .next()
                    .map(|value| value.to_string_lossy().into_owned())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(usage)?;
                if arguments.next().is_some() {
                    return Err(usage());
                }
                Ok(Self::AllowPasswords { organization })
            }
            Some(command) if command == OsString::from("admin") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "admin grant (--email <email> | --issuer <url> --subject <sub>)".into(),
                };
                if arguments.next().as_deref().and_then(|a| a.to_str()) != Some("grant") {
                    return Err(usage());
                }
                let mut values: [Option<String>; 3] = Default::default();
                while let Some(argument) = arguments.next() {
                    let slot = match argument.to_string_lossy().as_ref() {
                        "--email" => 0,
                        "--issuer" => 1,
                        "--subject" => 2,
                        _ => return Err(usage()),
                    };
                    let value = arguments
                        .next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(usage)?;
                    if values[slot].replace(value).is_some() {
                        return Err(usage());
                    }
                }
                let [email, issuer, subject] = values;
                match (&email, &issuer, &subject) {
                    (Some(_), None, None) | (None, Some(_), Some(_)) => {}
                    _ => return Err(usage()),
                }
                Ok(Self::AdminGrant {
                    email,
                    issuer,
                    subject,
                })
            }
            Some(command) if command == OsString::from("bootstrap-owner") => {
                let usage = || ConfigError::InvalidArgument {
                    value: "bootstrap-owner --organization <slug> --issuer <url> --subject <sub> \
                            [--organization-name <name>] [--display-name <name>]"
                        .into(),
                };
                let mut values: [Option<String>; 5] = Default::default();
                while let Some(argument) = arguments.next() {
                    let slot = match argument.to_string_lossy().as_ref() {
                        "--organization" => 0,
                        "--organization-name" => 1,
                        "--issuer" => 2,
                        "--subject" => 3,
                        "--display-name" => 4,
                        _ => return Err(usage()),
                    };
                    let value = arguments
                        .next()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(usage)?;
                    if values[slot].replace(value).is_some() {
                        return Err(usage());
                    }
                }
                let [
                    organization,
                    organization_name,
                    issuer,
                    subject,
                    display_name,
                ] = values;
                let (Some(organization), Some(issuer), Some(subject)) =
                    (organization, issuer, subject)
                else {
                    return Err(usage());
                };
                Ok(Self::BootstrapOwner {
                    organization,
                    organization_name,
                    issuer,
                    subject,
                    display_name,
                })
            }
            Some(command) if command == OsString::from("doctor") => {
                let (mut json, mut network) = (false, false);
                for argument in arguments {
                    match argument.to_str() {
                        Some("--json") if !json => json = true,
                        Some("--network") if !network => network = true,
                        _ => {
                            return Err(ConfigError::InvalidArgument {
                                value: "doctor [--json] [--network]".into(),
                            });
                        }
                    }
                }
                Ok(Self::Doctor { json, network })
            }
            Some(command) => Err(ConfigError::InvalidArgument {
                value: command.to_string_lossy().into_owned(),
            }),
        }
    }
}

/// A database URL cannot be formatted with its credentials accidentally.
#[derive(Clone)]
struct DatabaseUrl(String);

impl DatabaseUrl {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DatabaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&redact_database_url(&self.0))
    }
}

impl fmt::Display for DatabaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&redact_database_url(&self.0))
    }
}

#[derive(Clone)]
pub struct AppConfig {
    database_url: DatabaseUrl,
    api_key_pepper: Option<ApiKeyPepper>,
    pub bind_addr: SocketAddr,
    pub profile: GatewayProfile,
    pub oidc: OidcConfig,
    pub sessions: SessionConfig,
    /// `OIDC_CLIENT_SECRET`, for providers that require a confidential
    /// client; sent with HTTP Basic authentication to the token endpoint.
    pub(crate) oidc_client_secret: Option<zeroize::Zeroizing<String>>,
    pub(crate) openai: Option<OpenAiConfig>,
    pub(crate) anthropic: Option<AnthropicConfig>,
    pub(crate) gemini: Option<GeminiConfig>,
    pub(crate) bedrock: Option<BedrockConfig>,
    pub(crate) azure_openai: Option<AzureOpenAiConfig>,
    pub(crate) azure_ai_foundry: Option<AzureAiFoundryConfig>,
    pub(crate) openai_compatible: Option<OpenAiCompatibleConfig>,
    /// `GATEWAY_OPENAI_COMPATIBLE_PRIVATE_ENDPOINTS`: origins
    /// (`scheme://host[:port]`) of OpenAI-compatible endpoints that tenants
    /// may connect although they use plain HTTP or private addresses, such
    /// as a model server inside the cluster. Every other tenant endpoint must
    /// be HTTPS on public addresses.
    pub(crate) openai_compatible_private_origins: Vec<String>,
    /// What serves a tenant's Entra directory that names its own resource
    /// when its Azure family has no adapter configured: the family's
    /// settings, the gateway's Entra identity, and a placeholder endpoint
    /// that the directory's replaces. `None` without an identity.
    pub(crate) azure_openai_for_tenants: Option<AzureOpenAiConfig>,
    pub(crate) azure_ai_foundry_for_tenants: Option<AzureAiFoundryConfig>,
    /// What serves a tenant's Google service account when Gemini has no
    /// adapter configured: Gemini's settings and the gateway's Google
    /// identity. `None` without an identity.
    pub(crate) gemini_for_tenants: Option<GeminiConfig>,
    /// `{FAMILY}_ADAPTER_INSTANCE_KEY` values that were set, verbatim, read
    /// with the provider settings.
    pub(crate) adapter_instance_keys: Vec<(&'static str, String)>,
    /// Seals provider credentials stored through the admin API. Absent means
    /// credential writes are unavailable; everything else runs normally.
    pub(crate) credential_cipher: Option<CredentialCipher>,
    /// AWS KMS for envelope encryption of stored secrets; with it set, the
    /// key above only opens what it sealed until the worker re-seals it.
    pub(crate) credential_kms: Option<CredentialKmsConfig>,
    /// The S3-compatible bucket traced content is written to. Absent means
    /// tracing keeps timing only, and content is recorded as unavailable.
    pub(crate) trace_store: Option<TraceStoreConfig>,
    /// `GATEWAY_CLICKHOUSE_*`: the optional ClickHouse analytics sink
    /// (D-030). Absent means every analytics read uses PostgreSQL.
    pub(crate) clickhouse: Option<crate::clickhouse::ClickHouseConfig>,
    /// `GATEWAY_TRACE_CONTENT_MAX_BYTES`: the most a traced request, and
    /// separately its response, keeps; 5 MiB by default.
    pub(crate) trace_content_max_bytes: usize,
    /// `GATEWAY_DATABASE_MAX_CONNECTIONS`: the most connections each
    /// database pool opens; 10 by default. The API and the worker each have
    /// a pool, so `all-in-one` can open twice this.
    pub(crate) database_max_connections: u32,
    /// `GATEWAY_INFERENCE_MAX_BODY_BYTES`: the largest inference request
    /// body (chat, streams, embeddings) accepted; 4 MiB by default, enough
    /// for a prompt near a million tokens. Other routes keep 256 KiB.
    pub(crate) inference_max_body_bytes: usize,
    /// `GATEWAY_BUDGET_LEASES`: reserve against budget leases this process
    /// takes instead of locking each budget's row per request (D-030);
    /// off by default.
    pub(crate) budget_leases: bool,
    /// `GATEWAY_RATE_LIMIT_LEASES`: admit against rate-limit leases this
    /// process takes instead of locking the policies' rows per request
    /// (D-030); off by default.
    pub(crate) rate_limit_leases: bool,
    /// `GATEWAY_MODEL_CARDS_DIR`: the operator's model card and
    /// data-handling files, added to and replacing the shipped ones.
    pub(crate) model_cards_dir: Option<std::path::PathBuf>,
    /// `GATEWAY_MODEL_CARD_HUB_URL`: the Hugging Face Hub that model card
    /// imports read (`https://huggingface.co`); unset, imports are off.
    pub(crate) model_card_hub_url: Option<Url>,
    /// Operator price sheets the worker fetches, per provider family. The
    /// URLs may carry a token, so they are never logged or stored.
    pub(crate) price_sheets: Vec<(ProviderFamily, Url)>,
    /// Public price lists the worker fetches.
    pub(crate) public_prices: PublicPrices,
    /// The gateway's own cloud identities, as tenants' onboarding names them.
    pub(crate) onboarding: crate::domain::onboarding::OnboardingIdentities,
    /// Where `GET /metrics` is served, apart from the API. Unset means no
    /// metrics listener.
    pub metrics_addr: Option<SocketAddr>,
    /// The optional TLS (HTTP/1.1 and HTTP/2) and QUIC (HTTP/3) listeners
    /// beside `BIND_ADDR`'s plain one; off unless configured.
    pub listeners: crate::listeners::ListenerConfig,
    /// How this process reports itself and loads configuration versions.
    pub instance: InstanceConfig,
    /// Email-and-password accounts the gateway keeps itself.
    pub local_accounts: LocalAccountsConfig,
}

/// Local accounts (`GATEWAY_LOCAL_ACCOUNTS`): people sign in with an email
/// address and a password the gateway keeps, next to (or instead of) OIDC.
/// They need the session secrets and `GATEWAY_ORIGIN`, as OIDC does, but no
/// identity provider and no https: a first install works on
/// `http://localhost`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalAccountsConfig {
    pub enabled: bool,
    /// Take the client's address for sign-in rate limits from the last
    /// `X-Forwarded-For` entry (`GATEWAY_TRUST_X_FORWARDED_FOR=true`), as
    /// set by a reverse proxy in front of the gateway. Otherwise the peer
    /// address is used.
    pub trust_forwarded_for: bool,
    /// Any signed-in person may create an organization, and owns it
    /// (`GATEWAY_SELF_SERVE_ORGANIZATIONS=true`). Off by default: deployment
    /// admins create them.
    pub self_serve_organizations: bool,
}

/// How an API-serving process identifies itself in `gateway_instance` and
/// how often it polls the configuration feed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstanceConfig {
    /// `GATEWAY_INSTANCE_ID`; the hostname by default.
    pub id: String,
    /// `GATEWAY_REGION`; unset means not reported.
    pub region: Option<String>,
    /// `GATEWAY_BUILD`, such as the image's commit; the package version by
    /// default.
    pub build: String,
    /// `GATEWAY_CONFIG_POLL_SECONDS`, 1 to 300; 5 by default.
    pub poll: std::time::Duration,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            id: default_instance_id(),
            region: None,
            build: env!("CARGO_PKG_VERSION").to_owned(),
            poll: std::time::Duration::from_secs(5),
        }
    }
}

fn default_instance_id() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| env::var("HOSTNAME").ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty() && name.len() <= 253)
        .unwrap_or_else(|| format!("gateway-{}", uuid::Uuid::new_v4()))
}

/// `GATEWAY_INSTANCE_ID`, `GATEWAY_REGION`, `GATEWAY_BUILD` and
/// `GATEWAY_CONFIG_POLL_SECONDS`.
fn instance_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<InstanceConfig, ConfigError> {
    let set = |name| {
        config_env(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let printable =
        |value: &str, max: usize| value.len() <= max && value.chars().all(|c| c.is_ascii_graphic());
    let mut instance = InstanceConfig::default();
    if let Some(id) = set("GATEWAY_INSTANCE_ID") {
        if !printable(&id, 253) {
            return Err(ConfigError::InvalidInstance {
                name: "GATEWAY_INSTANCE_ID",
            });
        }
        instance.id = id;
    }
    if let Some(region) = set("GATEWAY_REGION") {
        if !printable(&region, 64) {
            return Err(ConfigError::InvalidInstance {
                name: "GATEWAY_REGION",
            });
        }
        instance.region = Some(region);
    }
    if let Some(build) = set("GATEWAY_BUILD") {
        if !printable(&build, 64) {
            return Err(ConfigError::InvalidInstance {
                name: "GATEWAY_BUILD",
            });
        }
        instance.build = build;
    }
    if let Some(poll) = set("GATEWAY_CONFIG_POLL_SECONDS") {
        let seconds = poll
            .parse::<u64>()
            .ok()
            .filter(|seconds| (1..=300).contains(seconds))
            .ok_or(ConfigError::InvalidInstance {
                name: "GATEWAY_CONFIG_POLL_SECONDS",
            })?;
        instance.poll = std::time::Duration::from_secs(seconds);
    }
    Ok(instance)
}

/// Where traced content is written: an S3 bucket, or a bucket of an
/// S3-compatible store at `endpoint` (addressed path-style). Content is
/// encrypted before it is sent, so the store never sees it in the clear.
#[derive(Clone)]
pub struct TraceStoreConfig {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<Url>,
    /// Ends in `/` unless empty.
    pub prefix: String,
    /// `GATEWAY_TRACE_STORE_AWS_*` or `AWS_*` keys; the process's runtime
    /// AWS identity when none are set.
    pub credentials: Option<crate::aws::AwsCredentials>,
}

impl fmt::Debug for TraceStoreConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceStoreConfig")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint.as_ref().map(Url::as_str))
            .field("prefix", &self.prefix)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "configured"),
            )
            .finish()
    }
}

/// The AWS KMS key that wraps the data keys of stored secrets.
#[derive(Clone, Debug)]
pub struct CredentialKmsConfig {
    /// A key id, key ARN, alias name or alias ARN.
    pub key_id: String,
    pub region: String,
    pub endpoint: Url,
    pub credentials: crate::aws::AwsCredentials,
}

/// Which public price lists the worker fetches. Both by default.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PublicPrices {
    /// The Azure Retail Prices API, in these currencies.
    pub(crate) azure_retail: Option<Vec<String>>,
    /// The AWS Price List for Bedrock.
    pub(crate) aws_price_list: bool,
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("database_url", &self.database_url)
            .field("api_key_pepper", &self.api_key_pepper)
            .field("bind_addr", &self.bind_addr)
            .field("profile", &self.profile)
            .field("oidc", &self.oidc)
            .field("sessions", &self.sessions.as_redacted())
            .field(
                "oidc_client_secret",
                &self.oidc_client_secret.as_ref().map(|_| "configured"),
            )
            .field(
                "credential_encryption_key",
                &if self.credential_cipher.is_some() {
                    "configured"
                } else {
                    "disabled"
                },
            )
            .field(
                "credential_kms",
                &self
                    .credential_kms
                    .as_ref()
                    .map(|kms| (kms.key_id.as_str(), kms.region.as_str())),
            )
            .field("trace_store", &self.trace_store)
            .field("clickhouse", &self.clickhouse)
            .field("trace_content_max_bytes", &self.trace_content_max_bytes)
            .field("database_max_connections", &self.database_max_connections)
            .field("inference_max_body_bytes", &self.inference_max_body_bytes)
            .field("budget_leases", &self.budget_leases)
            .field("rate_limit_leases", &self.rate_limit_leases)
            .field("model_cards_dir", &self.model_cards_dir)
            .field("model_card_hub_url", &self.model_card_hub_url)
            .field(
                "price_sheets",
                &self
                    .price_sheets
                    .iter()
                    .map(|(family, _)| family.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("public_prices", &self.public_prices)
            .field("metrics_addr", &self.metrics_addr)
            .field("listeners", &self.listeners)
            .finish()
    }
}

impl fmt::Display for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "profile={}, bind_addr={}, database_url={}",
            self.profile, self.bind_addr, self.database_url
        )
    }
}

impl SessionConfig {
    fn as_redacted(&self) -> (&'static str, &'static str, u64) {
        (
            if self.hmac_secret.is_some() {
                "configured"
            } else {
                "disabled"
            },
            if self.encryption_key.is_some() {
                "configured"
            } else {
                "disabled"
            },
            self.ttl_seconds,
        )
    }
    pub(crate) fn hmac_secret(&self) -> Option<&[u8]> {
        self.hmac_secret.as_deref()
    }
    pub(crate) fn encryption_key(&self) -> Option<&[u8]> {
        self.encryption_key.as_deref()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("missing required environment variable {name}")]
    Missing { name: &'static str },
    #[error("DATABASE_URL must be a PostgreSQL URL")]
    InvalidDatabaseUrl,
    #[error("BIND_ADDR is not a valid socket address: {value}")]
    InvalidBindAddr { value: String },
    #[error("GATEWAY_METRICS_ADDR must be a socket address other than BIND_ADDR: {value}")]
    InvalidMetricsAddr { value: String },
    #[error("{name} is not valid")]
    InvalidInstance { name: &'static str },
    #[error("{0}")]
    InvalidListener(#[from] crate::listeners::ListenerConfigError),
    #[error(
        "unsupported GATEWAY_PROFILE {value:?}; expected all-in-one, api, worker, or data-plane"
    )]
    InvalidProfile { value: String },
    #[error(
        "unsupported command or argument {value:?}; use `migrate` for migration-only execution"
    )]
    InvalidArgument { value: String },
    #[error("API_KEY_PEPPER is invalid: {0}")]
    InvalidPepper(#[from] KeyError),
    #[error("OIDC configuration is invalid")]
    InvalidOidc,
    #[error(
        "GATEWAY_LOCAL_ACCOUNTS must be true or false; true needs GATEWAY_ORIGIN and the session secrets (OIDC_SESSION_HMAC_SECRET, OIDC_SESSION_ENCRYPTION_KEY)"
    )]
    InvalidLocalAccounts,
    #[error("OIDC/session secret {name} is missing or shorter than 32 bytes")]
    InvalidSecret { name: &'static str },
    #[error("OpenAI configuration is invalid")]
    InvalidOpenAi,
    #[error("Anthropic configuration is invalid")]
    InvalidAnthropic,
    #[error("Gemini configuration is invalid")]
    InvalidGemini,
    #[error("Bedrock configuration is invalid")]
    InvalidBedrock,
    #[error("GATEWAY_ONBOARDING_* settings are invalid")]
    InvalidOnboarding,
    #[error("Azure OpenAI configuration is invalid")]
    InvalidAzureOpenAi,
    #[error("Azure AI Foundry configuration is invalid")]
    InvalidAzureAiFoundry,
    #[error("Generic OpenAI-compatible configuration is invalid")]
    InvalidOpenAiCompatible,
    #[error("GATEWAY_CREDENTIAL_ENCRYPTION_KEY must be 32 bytes (base64url without padding)")]
    InvalidCredentialKey,
    #[error(
        "GATEWAY_CREDENTIAL_KMS_KEY_ID needs a region and AWS credentials, and its endpoint must be https"
    )]
    InvalidCredentialKms,
    #[error(
        "GATEWAY_TRACE_STORE_* is invalid: the bucket needs a region, an endpoint must be an http(s) URL, the prefix may hold only letters, digits and ._/-, and keys come in pairs"
    )]
    InvalidTraceStore,
    #[error(
        "GATEWAY_CLICKHOUSE_* is invalid: the URL must be http(s) without credentials, the database may hold only letters, digits and _, the user no control characters, and GATEWAY_CLICKHOUSE_CA_FILE must be a readable PEM bundle"
    )]
    InvalidClickHouse,
    #[error(
        "GATEWAY_CLICKHOUSE_PASSWORD would be sent in clear over http:// to a host other than loopback: use https://, or set GATEWAY_CLICKHOUSE_ALLOW_HTTP_PASSWORD=true when the network is already encrypted"
    )]
    ClickHousePlaintextPassword,
    #[error("GATEWAY_TRACE_CONTENT_MAX_BYTES must be a number of bytes from 1024 to 67108864")]
    InvalidTraceContentMaxBytes,
    #[error("GATEWAY_DATABASE_MAX_CONNECTIONS must be a number from 1 to 1000")]
    InvalidDatabaseMaxConnections,
    #[error("GATEWAY_INFERENCE_MAX_BODY_BYTES must be a number of bytes from 65536 to 67108864")]
    InvalidInferenceMaxBodyBytes,
    #[error("GATEWAY_BUDGET_LEASES must be true or false")]
    InvalidBudgetLeases,
    #[error("GATEWAY_RATE_LIMIT_LEASES must be true or false")]
    InvalidRateLimitLeases,
    #[error("{name} must be an https URL")]
    InvalidPriceSheetUrl { name: String },
    #[error(
        "GATEWAY_PUBLIC_PRICE_SOURCES must be `none` or a list of azure-retail, aws-price-list"
    )]
    InvalidPublicPriceSources,
    #[error("GATEWAY_AZURE_RETAIL_PRICE_CURRENCIES must list ISO 4217 currency codes")]
    InvalidPriceCurrencies,
}

const OIDC_CALLBACK_PATH: &str = "/api/admin/v1/oidc/callback";

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_for(GatewayCommand::Run)
    }

    /// Load only the secrets required by the selected command/profile. In
    /// particular, migration and worker startup never read API_KEY_PEPPER.
    pub fn from_env_for(command: GatewayCommand) -> Result<Self, ConfigError> {
        Self::from_env_for_with(command, &|name: &str| env::var(name))
    }

    /// As [`from_env_for`](Self::from_env_for), reading each variable from
    /// `lookup` instead of the process environment, for embedding the
    /// gateway (and testing it) without touching that environment.
    pub fn from_lookup(
        command: GatewayCommand,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        Self::from_env_for_with(command, &|name: &str| {
            lookup(name).ok_or(env::VarError::NotPresent)
        })
    }

    fn from_env_for_with(
        command: GatewayCommand,
        provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    ) -> Result<Self, ConfigError> {
        let database_url = required_env(provider_env, "DATABASE_URL")?;
        let bind_addr = provider_env("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_owned());
        let profile = required_env(provider_env, "GATEWAY_PROFILE")?;
        let parsed_profile: GatewayProfile = profile.parse()?;
        let needs_api =
            matches!(command, GatewayCommand::Run) && parsed_profile != GatewayProfile::Worker;
        let pepper = if needs_api {
            Some(required_env(provider_env, "API_KEY_PEPPER")?)
        } else {
            None
        };
        let mut config = Self::from_values_with_env(
            Some(&database_url),
            Some(&bind_addr),
            Some(&profile),
            pepper.as_deref(),
            needs_api,
            provider_env,
        )?;
        // The worker lists tenants' accounts with the adapters inference
        // would use, and opens webhook signing secrets sealed with the same
        // key.
        if matches!(command, GatewayCommand::Run) && parsed_profile == GatewayProfile::Worker {
            load_provider_configs(&mut config, true, provider_env)?;
        }
        if matches!(command, GatewayCommand::Run)
            && matches!(
                parsed_profile,
                GatewayProfile::Worker | GatewayProfile::AllInOne
            )
        {
            config.price_sheets = price_sheets_from_env(provider_env)?;
            config.public_prices = public_prices_from_env(provider_env)?;
        }
        // The first owner must be provisioned under the issuer users sign in
        // with, which the command checks without the rest of the API's
        // configuration.
        if matches!(
            command,
            GatewayCommand::BootstrapOwner { .. }
                | GatewayCommand::Invitation { .. }
                | GatewayCommand::SetupLink
                | GatewayCommand::UserResetPassword { .. }
                | GatewayCommand::AllowPasswords { .. }
                | GatewayCommand::AdminGrant { .. }
        ) {
            // Local accounts are on unless turned off, as for the API.
            config.local_accounts.enabled = optional_bool(provider_env, "GATEWAY_LOCAL_ACCOUNTS")
                .map_err(|_| ConfigError::InvalidLocalAccounts)?
                .unwrap_or(true);
            config.oidc.origin = provider_env("GATEWAY_ORIGIN")
                .ok()
                .map(|origin| origin.trim().trim_end_matches('/').to_owned())
                .filter(|origin| !origin.is_empty());
            config.oidc.issuer = provider_env("OIDC_ISSUER")
                .ok()
                .map(|issuer| issuer.trim().to_owned())
                .filter(|issuer| !issuer.is_empty());
        }
        if matches!(command, GatewayCommand::Run) {
            config.metrics_addr = metrics_addr_from_env(provider_env, config.bind_addr)?;
            if parsed_profile != GatewayProfile::Worker {
                config.listeners = crate::listeners::ListenerConfig::from_env(
                    provider_env,
                    config.bind_addr,
                    config.metrics_addr,
                )?;
            }
            if parsed_profile.serves_inference() {
                config.instance = instance_from_env(provider_env)?;
            }
        }
        Ok(config)
    }

    pub fn from_values(
        database_url: Option<&str>,
        bind_addr: Option<&str>,
        profile: Option<&str>,
        api_key_pepper: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let config_env = |name: &str| env::var(name);
        Self::from_values_with_env(
            database_url,
            bind_addr,
            profile,
            api_key_pepper,
            true,
            &config_env,
        )
    }

    fn from_values_with_env(
        database_url: Option<&str>,
        bind_addr: Option<&str>,
        profile: Option<&str>,
        api_key_pepper: Option<&str>,
        require_pepper: bool,
        config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::from_values_inner(
            database_url,
            bind_addr,
            profile,
            api_key_pepper,
            require_pepper,
            config_env,
            ProviderConfigs::default(),
        )?;
        load_provider_configs(&mut config, require_pepper, config_env)?;
        Ok(config)
    }

    #[cfg(test)]
    pub(crate) fn from_values_with_providers(
        database_url: Option<&str>,
        bind_addr: Option<&str>,
        profile: Option<&str>,
        api_key_pepper: Option<&str>,
        providers: ProviderConfigs,
    ) -> Result<Self, ConfigError> {
        let no_env = |_name: &str| Err(env::VarError::NotPresent);
        Self::from_values_inner(
            database_url,
            bind_addr,
            profile,
            api_key_pepper,
            true,
            &no_env,
            providers,
        )
    }

    fn from_values_inner(
        database_url: Option<&str>,
        bind_addr: Option<&str>,
        profile: Option<&str>,
        api_key_pepper: Option<&str>,
        require_pepper: bool,
        config_env: &impl Fn(&str) -> Result<String, env::VarError>,
        providers: ProviderConfigs,
    ) -> Result<Self, ConfigError> {
        let database_url = non_empty(database_url, "DATABASE_URL")?;
        let parsed_url = Url::parse(database_url).map_err(|_| ConfigError::InvalidDatabaseUrl)?;
        if !matches!(parsed_url.scheme(), "postgres" | "postgresql") {
            return Err(ConfigError::InvalidDatabaseUrl);
        }

        let bind_addr = bind_addr.unwrap_or(DEFAULT_BIND_ADDR);
        let bind_addr = bind_addr
            .parse()
            .map_err(|_| ConfigError::InvalidBindAddr {
                value: bind_addr.to_owned(),
            })?;
        let profile = non_empty(profile, "GATEWAY_PROFILE")?.parse::<GatewayProfile>()?;
        let api_key_pepper = match api_key_pepper {
            Some(value) => Some(ApiKeyPepper::from_config(value)?),
            None if !require_pepper || profile == GatewayProfile::Worker => None,
            None => {
                return Err(ConfigError::Missing {
                    name: "API_KEY_PEPPER",
                });
            }
        };

        let mut oidc_client_secret = None;
        let mut local_accounts = LocalAccountsConfig::default();
        let (oidc, sessions) = if require_pepper
            && matches!(profile, GatewayProfile::Api | GatewayProfile::AllInOne)
        {
            let enabled = config_env("OIDC_ENABLED")
                .unwrap_or_else(|_| "false".to_owned())
                .parse::<bool>()
                .map_err(|_| ConfigError::InvalidOidc)?;
            let local_choice = optional_bool(config_env, "GATEWAY_LOCAL_ACCOUNTS")
                .map_err(|_| ConfigError::InvalidLocalAccounts)?;
            local_accounts.trust_forwarded_for =
                optional_bool(config_env, "GATEWAY_TRUST_X_FORWARDED_FOR")
                    .map_err(|_| ConfigError::InvalidLocalAccounts)?
                    .unwrap_or(false);
            local_accounts.self_serve_organizations =
                optional_bool(config_env, "GATEWAY_SELF_SERVE_ORGANIZATIONS")
                    .map_err(|_| ConfigError::InvalidLocalAccounts)?
                    .unwrap_or(false);
            if enabled {
                // With OIDC, the session secrets are there already.
                local_accounts.enabled = local_choice.unwrap_or(true);
                let origin = required_env(config_env, "GATEWAY_ORIGIN")?
                    .trim_end_matches('/')
                    .to_owned();
                let issuer = required_env(config_env, "OIDC_ISSUER")?;
                let audience = required_env(config_env, "OIDC_AUDIENCE")?;
                let client_id = required_env(config_env, "OIDC_CLIENT_ID")?;
                let redirect_uri = required_env(config_env, "OIDC_REDIRECT_URI")?;
                oidc_client_secret = config_env("OIDC_CLIENT_SECRET")
                    .ok()
                    .filter(|secret| !secret.trim().is_empty())
                    .map(zeroize::Zeroizing::new);
                validate_origin(&origin)?;
                validate_oidc_url(&issuer, true)?;
                validate_oidc_url(&redirect_uri, false)?;
                let expected_redirect =
                    format!("{}{}", origin.trim_end_matches('/'), OIDC_CALLBACK_PATH);
                if redirect_uri != expected_redirect {
                    return Err(ConfigError::InvalidOidc);
                }
                let hmac_secret = secret_bytes(config_env, "OIDC_SESSION_HMAC_SECRET", 32)?;
                let encryption_key =
                    secret_bytes_exact(config_env, "OIDC_SESSION_ENCRYPTION_KEY", 32)?;
                let microsoft_graph_photo =
                    match config_env("OIDC_PROFILE_PHOTO").unwrap_or_default().trim() {
                        "" => false,
                        "microsoft-graph" => true,
                        _ => return Err(ConfigError::InvalidOidc),
                    };
                let ttl_seconds = config_env("OIDC_SESSION_TTL_SECONDS")
                    .unwrap_or_else(|_| "3600".to_owned())
                    .parse::<u64>()
                    .ok()
                    .filter(|ttl| (60..=86_400).contains(ttl))
                    .ok_or(ConfigError::InvalidOidc)?;
                (
                    OidcConfig {
                        enabled,
                        origin: Some(origin),
                        issuer: Some(issuer),
                        audience: Some(audience),
                        client_id: Some(client_id),
                        redirect_uri: Some(redirect_uri),
                        microsoft_graph_photo,
                    },
                    SessionConfig {
                        hmac_secret: Some(hmac_secret),
                        encryption_key: Some(encryption_key),
                        ttl_seconds,
                    },
                )
            } else {
                // Without OIDC, sessions exist only for local accounts.
                match local_sessions(config_env, local_choice)? {
                    Some((origin, sessions)) => {
                        local_accounts.enabled = true;
                        // An organization's own provider still signs in
                        // through the gateway's callback.
                        let redirect_uri =
                            format!("{}{}", origin.trim_end_matches('/'), OIDC_CALLBACK_PATH);
                        (
                            OidcConfig {
                                enabled: false,
                                origin: Some(origin),
                                issuer: None,
                                audience: None,
                                client_id: None,
                                redirect_uri: Some(redirect_uri),
                                microsoft_graph_photo: false,
                            },
                            sessions,
                        )
                    }
                    None => (
                        OidcConfig {
                            enabled: false,
                            origin: None,
                            issuer: None,
                            audience: None,
                            client_id: None,
                            redirect_uri: None,
                            microsoft_graph_photo: false,
                        },
                        SessionConfig {
                            hmac_secret: None,
                            encryption_key: None,
                            ttl_seconds: 3600,
                        },
                    ),
                }
            }
        } else {
            (
                OidcConfig {
                    enabled: false,
                    origin: None,
                    issuer: None,
                    audience: None,
                    client_id: None,
                    redirect_uri: None,
                    microsoft_graph_photo: false,
                },
                SessionConfig {
                    hmac_secret: None,
                    encryption_key: None,
                    ttl_seconds: 3600,
                },
            )
        };

        Ok(Self {
            database_url: DatabaseUrl(database_url.to_owned()),
            api_key_pepper,
            bind_addr,
            profile,
            oidc,
            sessions,
            oidc_client_secret,
            openai: providers.openai,
            anthropic: providers.anthropic,
            gemini: providers.gemini,
            bedrock: providers.bedrock,
            azure_openai: providers.azure_openai,
            azure_ai_foundry: providers.azure_ai_foundry,
            openai_compatible: providers.openai_compatible,
            openai_compatible_private_origins: Vec::new(),
            azure_openai_for_tenants: None,
            azure_ai_foundry_for_tenants: None,
            gemini_for_tenants: None,
            adapter_instance_keys: Vec::new(),
            credential_cipher: None,
            credential_kms: None,
            trace_store: None,
            clickhouse: None,
            trace_content_max_bytes: crate::domain::trace_content::DEFAULT_MAX_BYTES,
            database_max_connections: DEFAULT_DATABASE_MAX_CONNECTIONS,
            inference_max_body_bytes: DEFAULT_INFERENCE_MAX_BODY_BYTES,
            budget_leases: false,
            rate_limit_leases: false,
            model_cards_dir: None,
            model_card_hub_url: None,
            price_sheets: Vec::new(),
            public_prices: PublicPrices::default(),
            onboarding: crate::domain::onboarding::OnboardingIdentities::default(),
            metrics_addr: None,
            listeners: crate::listeners::ListenerConfig::default(),
            instance: InstanceConfig::default(),
            local_accounts,
        })
    }

    pub(crate) fn database_url(&self) -> &str {
        self.database_url.as_str()
    }

    pub(crate) fn api_key_pepper(&self) -> Option<&ApiKeyPepper> {
        self.api_key_pepper.as_ref()
    }
}

/// The variables that rename a process-configured provider's adapter
/// instance key.
const ADAPTER_INSTANCE_KEY_VARIABLES: [&str; 7] = [
    "OPENAI_ADAPTER_INSTANCE_KEY",
    "ANTHROPIC_ADAPTER_INSTANCE_KEY",
    "GEMINI_ADAPTER_INSTANCE_KEY",
    "BEDROCK_ADAPTER_INSTANCE_KEY",
    "AZURE_OPENAI_ADAPTER_INSTANCE_KEY",
    "AZURE_AI_FOUNDRY_ADAPTER_INSTANCE_KEY",
    "OPENAI_COMPATIBLE_ADAPTER_INSTANCE_KEY",
];

impl AppConfig {
    /// The adapter instance key `name` set, or `default`.
    pub(crate) fn adapter_instance_key(&self, name: &str, default: &str) -> String {
        self.adapter_instance_keys
            .iter()
            .find(|(variable, _)| *variable == name)
            .map_or_else(|| default.to_owned(), |(_, value)| value.clone())
    }
}

/// The gateway's own cloud identities that tenants' onboarding names. Each
/// cloud is offered only when its identity is set:
///
/// - `GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN`: the IAM user or role tenant
///   roles trust; `GATEWAY_ONBOARDING_AWS_TEMPLATE_URL`: the published
///   CloudFormation template (https), for the one-click link.
/// - `GATEWAY_ONBOARDING_AZURE_CLIENT_ID`: the gateway's multi-tenant Entra
///   app, else `AZURE_CLIENT_ID`; consent returns to
///   `{GATEWAY_ORIGIN}/api/admin/v1/onboarding/azure/consent`.
/// - `GATEWAY_ONBOARDING_GOOGLE_SERVICE_ACCOUNT`: the gateway's Google
///   service account.
///
/// The values appear in scripts tenants run, so each is checked strictly.
fn onboarding_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    origin: Option<&str>,
) -> Result<crate::domain::onboarding::OnboardingIdentities, ConfigError> {
    let set = |name: &str| {
        provider_env(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let invalid = || ConfigError::InvalidOnboarding;
    let aws_principal_arn = set("GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN");
    if let Some(arn) = &aws_principal_arn {
        let (head, path) = arn.split_once(":iam::").ok_or_else(invalid)?;
        let (account, resource) = path.split_once(':').ok_or_else(invalid)?;
        let (kind, name) = resource.split_once('/').ok_or_else(invalid)?;
        let name_ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+=,.@_-/".contains(c));
        if !matches!(head, "arn:aws" | "arn:aws-us-gov" | "arn:aws-cn")
            || account.len() != 12
            || !account.bytes().all(|b| b.is_ascii_digit())
            || !matches!(kind, "user" | "role")
            || !name_ok
        {
            return Err(invalid());
        }
    }
    let aws_template_url = set("GATEWAY_ONBOARDING_AWS_TEMPLATE_URL")
        .map(|value| {
            Url::parse(&value)
                .ok()
                .filter(|url| url.scheme() == "https" && url.host_str().is_some())
                .ok_or_else(invalid)
        })
        .transpose()?;
    let azure_client_id =
        set("GATEWAY_ONBOARDING_AZURE_CLIENT_ID").or_else(|| set("AZURE_CLIENT_ID"));
    if let Some(client_id) = &azure_client_id {
        uuid::Uuid::parse_str(client_id).map_err(|_| invalid())?;
    }
    let azure_consent_redirect = origin
        .map(|origin| {
            Url::parse(&format!(
                "{}/api/admin/v1/onboarding/azure/consent",
                origin.trim_end_matches('/')
            ))
            .map_err(|_| invalid())
        })
        .transpose()?;
    let google_service_account = set("GATEWAY_ONBOARDING_GOOGLE_SERVICE_ACCOUNT");
    if let Some(account) = &google_service_account {
        if !crate::gcp_credentials::service_account_email(account) {
            return Err(invalid());
        }
    }
    Ok(crate::domain::onboarding::OnboardingIdentities {
        // Set with the provider settings, from the same list inference uses.
        openai_compatible_private_endpoints: Vec::new(),
        aws_principal_arn,
        aws_template_url,
        // Consent must come back to this gateway.
        azure_client_id: azure_consent_redirect.as_ref().and(azure_client_id),
        azure_consent_redirect,
        google_service_account,
    })
}

fn load_provider_configs(
    config: &mut AppConfig,
    load: bool,
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<(), ConfigError> {
    if load
        && matches!(
            config.profile,
            GatewayProfile::Api
                | GatewayProfile::AllInOne
                | GatewayProfile::DataPlane
                | GatewayProfile::Worker
        )
    {
        // Only the admin API onboards a tenant's cloud.
        if matches!(
            config.profile,
            GatewayProfile::Api | GatewayProfile::AllInOne
        ) {
            config.onboarding = onboarding_from_env(provider_env, config.oidc.origin.as_deref())?;
        }
        config.openai = openai_from_env(provider_env)?;
        config.anthropic = anthropic_from_env(provider_env)?;
        // The gateway's Google identity, for Gemini without a key and for
        // tenants' service accounts.
        let google_identity = GoogleIdentity::from_env(&|name: &str| provider_env(name).ok());
        if google_identity.is_none() {
            tracing::warn!(
                "the Google credentials file cannot be used; Google sign-in is unavailable"
            );
        }
        config.gemini = gemini_from_env(provider_env, google_identity.as_ref())?;
        if let Some(identity) = &google_identity {
            // As for Azure: settings a disabled adapter ignores only leave
            // tenants' service accounts unserved.
            config.gemini_for_tenants =
                gemini_from_env(&gemini_for_tenants(provider_env), Some(identity))
                    .inspect_err(|_| {
                        tracing::warn!(
                            family = "gemini",
                            "invalid Gemini settings; tenants' service accounts are not served"
                        );
                    })
                    .ok()
                    .flatten();
        }
        config.bedrock = bedrock_from_env(provider_env)?;
        // One identity for both Azure families and tenants' directories, so
        // that they share tokens.
        let azure_identity = AzureIdentity::from_env(&|name: &str| provider_env(name).ok());
        if azure_identity.is_none() {
            tracing::warn!(
                "the Azure Entra identity settings are incomplete or invalid; Entra sign-in is unavailable"
            );
        }
        config.azure_openai = azure_openai_from_env(provider_env, azure_identity.as_ref())?;
        config.azure_ai_foundry = azure_ai_foundry_from_env(provider_env, azure_identity.as_ref())?;
        if let Some(identity) = &azure_identity {
            // Settings an unconfigured family ignores must not stop startup:
            // invalid ones only leave tenants' resources of that family
            // unserved.
            let unserved = |family: &str| {
                tracing::warn!(
                    family,
                    "invalid Azure settings; tenants' own resources of this family are not served"
                );
            };
            config.azure_openai_for_tenants = azure_openai_from_env(
                &for_tenants(
                    provider_env,
                    "AZURE_OPENAI_BASE_URL",
                    "AZURE_OPENAI_API_KEY",
                    "https://tenant.openai.azure.com",
                ),
                Some(identity),
            )
            .inspect_err(|_| unserved("azure-openai"))
            .ok()
            .flatten();
            config.azure_ai_foundry_for_tenants = azure_ai_foundry_from_env(
                &for_tenants(
                    provider_env,
                    "AZURE_AI_FOUNDRY_BASE_URL",
                    "AZURE_AI_FOUNDRY_API_KEY",
                    "https://tenant.services.ai.azure.com",
                ),
                Some(identity),
            )
            .inspect_err(|_| unserved("azure-ai-foundry"))
            .ok()
            .flatten();
        }
        config.openai_compatible = openai_compatible_from_env(provider_env)?;
        config.openai_compatible_private_origins =
            match provider_env("GATEWAY_OPENAI_COMPATIBLE_PRIVATE_ENDPOINTS") {
                Ok(list) => private_origins(&list)?,
                Err(_) => Vec::new(),
            };
        config
            .onboarding
            .openai_compatible_private_endpoints
            .clone_from(&config.openai_compatible_private_origins);
        config.adapter_instance_keys = ADAPTER_INSTANCE_KEY_VARIABLES
            .iter()
            // Used exactly as set, since catalogs match the key exactly.
            .filter_map(|name| provider_env(name).ok().map(|value| (*name, value)))
            .collect();
        config.credential_cipher = credential_cipher_from_env(provider_env)?;
        config.credential_kms = credential_kms_from_env(provider_env)?;
        config.trace_store = trace_store_from_env(provider_env)?;
        config.clickhouse =
            crate::clickhouse::ClickHouseConfig::from_lookup(&|name| provider_env(name).ok())
                .map_err(|problem| match problem {
                    crate::clickhouse::ConfigProblem::PlaintextPassword => {
                        ConfigError::ClickHousePlaintextPassword
                    }
                    _ => ConfigError::InvalidClickHouse,
                })?;
        config.trace_content_max_bytes = trace_content_max_bytes_from_env(provider_env)?;
        config.database_max_connections = database_max_connections_from_env(provider_env)?;
        config.inference_max_body_bytes = inference_max_body_bytes_from_env(provider_env)?;
        config.budget_leases = optional_bool(provider_env, "GATEWAY_BUDGET_LEASES")
            .map_err(|_| ConfigError::InvalidBudgetLeases)?
            .unwrap_or(false);
        config.rate_limit_leases = optional_bool(provider_env, "GATEWAY_RATE_LIMIT_LEASES")
            .map_err(|_| ConfigError::InvalidRateLimitLeases)?
            .unwrap_or(false);
        config.model_cards_dir = provider_env("GATEWAY_MODEL_CARDS_DIR")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        config.model_card_hub_url = model_card_hub_url_from_env(provider_env)?;
    }
    Ok(())
}

/// `GATEWAY_MODEL_CARD_HUB_URL`: an https URL without credentials, query or
/// fragment; unset or empty turns Hugging Face imports off.
fn model_card_hub_url_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<Url>, ConfigError> {
    let Some(value) = provider_env("GATEWAY_MODEL_CARD_HUB_URL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    // An https URL without credentials, query or fragment.
    let invalid = || ConfigError::InvalidInstance {
        name: "GATEWAY_MODEL_CARD_HUB_URL",
    };
    let mut url = Url::parse(&value).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    // A base to join `api/models/...` onto.
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(Some(url))
}

/// `GATEWAY_PRICE_SHEET_URL_{FAMILY}` for each provider family, e.g.
/// `GATEWAY_PRICE_SHEET_URL_AZURE_OPENAI`. Unset or empty means no sheet.
fn price_sheets_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Vec<(ProviderFamily, Url)>, ConfigError> {
    let mut sheets = Vec::new();
    for family in ProviderFamily::ALL {
        let name = format!(
            "GATEWAY_PRICE_SHEET_URL_{}",
            family.as_str().to_ascii_uppercase().replace('-', "_")
        );
        let Some(value) = config_env(&name)
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        match Url::parse(value.trim()) {
            Ok(url) if url.scheme() == "https" && url.host().is_some() => {
                sheets.push((family, url));
            }
            _ => return Err(ConfigError::InvalidPriceSheetUrl { name }),
        }
    }
    Ok(sheets)
}

/// `GATEWAY_METRICS_ADDR`, e.g. `127.0.0.1:9464`. The metrics listener has
/// no authentication, so it must not share the API's address.
fn metrics_addr_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    bind_addr: SocketAddr,
) -> Result<Option<SocketAddr>, ConfigError> {
    let Some(value) = config_env("GATEWAY_METRICS_ADDR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    match value.parse::<SocketAddr>() {
        Ok(address) if !crate::net::listeners_overlap(address, bind_addr) => Ok(Some(address)),
        _ => Err(ConfigError::InvalidMetricsAddr { value }),
    }
}

/// `GATEWAY_PUBLIC_PRICE_SOURCES`: `none`, or a comma-separated list of
/// `azure-retail` and `aws-price-list`; unset means both.
/// `GATEWAY_AZURE_RETAIL_PRICE_CURRENCIES` lists the currencies read from
/// Azure (USD unless set).
fn public_prices_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<PublicPrices, ConfigError> {
    let set = |name| {
        config_env(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let (mut azure_retail, mut aws_price_list) = (true, true);
    match set("GATEWAY_PUBLIC_PRICE_SOURCES").as_deref() {
        None => {}
        Some("none") => (azure_retail, aws_price_list) = (false, false),
        Some(list) => {
            (azure_retail, aws_price_list) = (false, false);
            for source in list.split(',').map(str::trim) {
                match source {
                    "azure-retail" => azure_retail = true,
                    "aws-price-list" => aws_price_list = true,
                    _ => return Err(ConfigError::InvalidPublicPriceSources),
                }
            }
        }
    }
    let mut currencies = Vec::new();
    for currency in set("GATEWAY_AZURE_RETAIL_PRICE_CURRENCIES")
        .as_deref()
        .unwrap_or("USD")
        .split(',')
    {
        let currency = currency.trim().to_ascii_uppercase();
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(ConfigError::InvalidPriceCurrencies);
        }
        if !currencies.contains(&currency) {
            currencies.push(currency);
        }
    }
    if currencies.len() > 8 {
        return Err(ConfigError::InvalidPriceCurrencies);
    }
    Ok(PublicPrices {
        azure_retail: azure_retail.then_some(currencies),
        aws_price_list,
    })
}

/// The credential key is optional and separate from the session keys, so a
/// session-key rotation never strands stored provider credentials.
fn credential_cipher_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<CredentialCipher>, ConfigError> {
    let Some(value) = config_env("GATEWAY_CREDENTIAL_ENCRYPTION_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let decoded = zeroize::Zeroizing::new(
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value.trim().as_bytes())
            .unwrap_or_else(|_| value.clone().into_bytes()),
    );
    CredentialCipher::new(&decoded)
        .map(Some)
        .map_err(|_| ConfigError::InvalidCredentialKey)
}

/// `GATEWAY_CREDENTIAL_KMS_KEY_ID` enables envelope encryption under that
/// AWS KMS key. The region is `GATEWAY_CREDENTIAL_KMS_REGION` or
/// `AWS_REGION`; credentials are `GATEWAY_CREDENTIAL_KMS_AWS_*`, else the
/// standard `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
/// `AWS_SESSION_TOKEN`. `GATEWAY_CREDENTIAL_KMS_ENDPOINT` overrides the
/// regional endpoint (for a VPC endpoint); it must be https, except to a
/// loopback address.
fn credential_kms_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<CredentialKmsConfig>, ConfigError> {
    let set = |name: &str| {
        config_env(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let Some(key_id) = set("GATEWAY_CREDENTIAL_KMS_KEY_ID") else {
        return Ok(None);
    };
    let either = |prefixed: &str, standard: &str| set(prefixed).or_else(|| set(standard));
    let region = either("GATEWAY_CREDENTIAL_KMS_REGION", "AWS_REGION")
        .filter(|region| {
            region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        .ok_or(ConfigError::InvalidCredentialKms)?;
    let (Some(access_key_id), Some(secret_access_key)) = (
        either(
            "GATEWAY_CREDENTIAL_KMS_AWS_ACCESS_KEY_ID",
            "AWS_ACCESS_KEY_ID",
        ),
        either(
            "GATEWAY_CREDENTIAL_KMS_AWS_SECRET_ACCESS_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ),
    ) else {
        return Err(ConfigError::InvalidCredentialKms);
    };
    let session_token = either(
        "GATEWAY_CREDENTIAL_KMS_AWS_SESSION_TOKEN",
        "AWS_SESSION_TOKEN",
    );
    let endpoint = match set("GATEWAY_CREDENTIAL_KMS_ENDPOINT") {
        Some(value) => Url::parse(&value).map_err(|_| ConfigError::InvalidCredentialKms)?,
        None => crate::aws::KmsClient::default_endpoint(&region)
            .ok_or(ConfigError::InvalidCredentialKms)?,
    };
    let loopback = crate::net::is_loopback_url(&endpoint);
    if !(endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback))
        || endpoint.host().is_none()
    {
        return Err(ConfigError::InvalidCredentialKms);
    }
    Ok(Some(CredentialKmsConfig {
        key_id,
        region,
        endpoint,
        credentials: crate::aws::AwsCredentials {
            access_key_id,
            secret_access_key: zeroize::Zeroizing::new(secret_access_key),
            session_token: session_token.map(zeroize::Zeroizing::new),
        },
    }))
}

/// `GATEWAY_TRACE_STORE_BUCKET` enables the trace content store. The
/// region is `GATEWAY_TRACE_STORE_REGION` or `AWS_REGION`;
/// `GATEWAY_TRACE_STORE_ENDPOINT` names an S3-compatible store instead of
/// AWS; `GATEWAY_TRACE_STORE_PREFIX` is prepended to every object name.
/// Credentials are `GATEWAY_TRACE_STORE_AWS_ACCESS_KEY_ID` and
/// `GATEWAY_TRACE_STORE_AWS_SECRET_ACCESS_KEY` (with an optional session
/// token), so a store's keys never replace the identity the gateway calls
/// providers with; else the standard `AWS_*` keys, else the process's
/// runtime AWS identity (web identity, container or instance).
fn trace_store_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<TraceStoreConfig>, ConfigError> {
    let set = |name: &str| {
        config_env(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let Some(bucket) = set("GATEWAY_TRACE_STORE_BUCKET") else {
        return Ok(None);
    };
    // S3's bucket naming rules, which S3-compatible stores share.
    let bucket_ok = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
    let region = set("GATEWAY_TRACE_STORE_REGION")
        .or_else(|| set("AWS_REGION"))
        .filter(|region| {
            region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    let (true, Some(region)) = (bucket_ok, region) else {
        return Err(ConfigError::InvalidTraceStore);
    };
    let endpoint = match set("GATEWAY_TRACE_STORE_ENDPOINT") {
        Some(value) => {
            let url = Url::parse(&value).map_err(|_| ConfigError::InvalidTraceStore)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host().is_none()
                || url.query().is_some()
            {
                return Err(ConfigError::InvalidTraceStore);
            }
            Some(url)
        }
        None => None,
    };
    let mut prefix = set("GATEWAY_TRACE_STORE_PREFIX").unwrap_or_default();
    let prefix_ok = prefix.len() <= 256
        && !prefix.starts_with('/')
        && !prefix.contains("//")
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'));
    if !prefix_ok {
        return Err(ConfigError::InvalidTraceStore);
    }
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let credentials = match (
        set("GATEWAY_TRACE_STORE_AWS_ACCESS_KEY_ID"),
        set("GATEWAY_TRACE_STORE_AWS_SECRET_ACCESS_KEY"),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => Some(crate::aws::AwsCredentials {
            access_key_id,
            secret_access_key: zeroize::Zeroizing::new(secret_access_key),
            session_token: set("GATEWAY_TRACE_STORE_AWS_SESSION_TOKEN")
                .map(zeroize::Zeroizing::new),
        }),
        (None, None) => match (set("AWS_ACCESS_KEY_ID"), set("AWS_SECRET_ACCESS_KEY")) {
            (Some(access_key_id), Some(secret_access_key)) => Some(crate::aws::AwsCredentials {
                access_key_id,
                secret_access_key: zeroize::Zeroizing::new(secret_access_key),
                session_token: set("AWS_SESSION_TOKEN").map(zeroize::Zeroizing::new),
            }),
            _ => None,
        },
        _ => return Err(ConfigError::InvalidTraceStore),
    };
    Ok(Some(TraceStoreConfig {
        bucket,
        region,
        endpoint,
        prefix,
        credentials,
    }))
}

/// Database connections a process opens unless configured.
pub(crate) const DEFAULT_DATABASE_MAX_CONNECTIONS: u32 = 10;

fn database_max_connections_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<u32, ConfigError> {
    match config_env("GATEWAY_DATABASE_MAX_CONNECTIONS")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        None => Ok(DEFAULT_DATABASE_MAX_CONNECTIONS),
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|connections| (1..=1000).contains(connections))
            .ok_or(ConfigError::InvalidDatabaseMaxConnections),
    }
}

/// The largest inference request body unless configured: 4 MiB, room for a
/// prompt near a million tokens (about 4 bytes a token) in its JSON.
pub(crate) const DEFAULT_INFERENCE_MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

fn inference_max_body_bytes_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<usize, ConfigError> {
    match config_env("GATEWAY_INFERENCE_MAX_BODY_BYTES")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        None => Ok(DEFAULT_INFERENCE_MAX_BODY_BYTES),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|bytes| (64 * 1024..=64 * 1024 * 1024).contains(bytes))
            .ok_or(ConfigError::InvalidInferenceMaxBodyBytes),
    }
}

fn trace_content_max_bytes_from_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<usize, ConfigError> {
    use crate::domain::trace_content::{DEFAULT_MAX_BYTES, MAX_MAX_BYTES, MIN_MAX_BYTES};
    match config_env("GATEWAY_TRACE_CONTENT_MAX_BYTES")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        None => Ok(DEFAULT_MAX_BYTES),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|bytes| (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(bytes))
            .ok_or(ConfigError::InvalidTraceContentMaxBytes),
    }
}

fn required_env(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    name: &'static str,
) -> Result<String, ConfigError> {
    non_empty(config_env(name).ok().as_deref(), name).map(ToOwned::to_owned)
}

fn openai_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<OpenAiConfig>, ConfigError> {
    let api_key = provider_env("OPENAI_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let base_url = provider_env("OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            api_key
                .as_ref()
                .map(|_| "https://api.openai.com/v1".to_owned())
        });
    match (base_url, api_key) {
        (None, None) => Ok(None),
        (Some(base_url), Some(api_key)) => {
            validate_openai_base_url(&base_url)?;
            let request_timeout = provider_env("OPENAI_REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=300).contains(value))
                .ok_or(ConfigError::InvalidOpenAi)?;
            let connect_timeout = provider_env("OPENAI_CONNECT_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "10".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=60).contains(value))
                .ok_or(ConfigError::InvalidOpenAi)?;
            let max_response_bytes = provider_env("OPENAI_MAX_RESPONSE_BYTES")
                .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
                .parse::<usize>()
                .ok()
                .filter(|value| (1..=64 * 1024 * 1024).contains(value))
                .ok_or(ConfigError::InvalidOpenAi)?;
            Ok(Some(OpenAiConfig {
                base_url: base_url.trim_end_matches('/').to_owned(),
                api_key,
                request_timeout: Duration::from_secs(request_timeout),
                connect_timeout: Duration::from_secs(connect_timeout),
                max_response_bytes,
            }))
        }
        _ => Err(ConfigError::InvalidOpenAi),
    }
}

fn anthropic_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<AnthropicConfig>, ConfigError> {
    let api_key = provider_env("ANTHROPIC_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let base_url = provider_env("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            api_key
                .as_ref()
                .map(|_| "https://api.anthropic.com".to_owned())
        });
    match (base_url, api_key) {
        (None, None) => Ok(None),
        (Some(base_url), Some(api_key)) => {
            let url = Url::parse(&base_url).map_err(|_| ConfigError::InvalidAnthropic)?;
            url.host_str().ok_or(ConfigError::InvalidAnthropic)?;
            let loopback = crate::net::is_loopback_url(&url);
            if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
                || url.username() != ""
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ConfigError::InvalidAnthropic);
            }
            let request_timeout = provider_env("ANTHROPIC_REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".into())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=300).contains(value))
                .ok_or(ConfigError::InvalidAnthropic)?;
            let connect_timeout = provider_env("ANTHROPIC_CONNECT_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "10".into())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=60).contains(value))
                .ok_or(ConfigError::InvalidAnthropic)?;
            let max_response_bytes = provider_env("ANTHROPIC_MAX_RESPONSE_BYTES")
                .unwrap_or_else(|_| "1048576".into())
                .parse::<usize>()
                .ok()
                .filter(|value| (1024..=16 * 1024 * 1024).contains(value))
                .ok_or(ConfigError::InvalidAnthropic)?;
            Ok(Some(AnthropicConfig {
                base_url,
                api_key,
                request_timeout: Duration::from_secs(request_timeout),
                connect_timeout: Duration::from_secs(connect_timeout),
                max_response_bytes,
            }))
        }
        _ => Err(ConfigError::InvalidAnthropic),
    }
}

/// Gemini is enabled by `GEMINI_API_KEY`, or with no key by the gateway's
/// Google identity: on Gemini Enterprise Agent Platform with
/// `GEMINI_AGENT_PLATFORM_PROJECT` (and `GEMINI_AGENT_PLATFORM_LOCATION`,
/// default `global`), or on the Gemini API billed to `GEMINI_QUOTA_PROJECT`,
/// which Google admits only for identities it already did. A key or the
/// quota project with an Agent Platform project is an error.
fn gemini_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    identity: Option<&GoogleIdentity>,
) -> Result<Option<GeminiConfig>, ConfigError> {
    let set = |name: &str| {
        provider_env(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let api_key = set("GEMINI_API_KEY");
    let quota_project = set("GEMINI_QUOTA_PROJECT");
    let agent_platform_location = set("GEMINI_AGENT_PLATFORM_LOCATION");
    // Checked only when Gemini is enabled: a leftover setting of a disabled
    // adapter must not stop startup. Tenants' settings are loaded the same
    // way, and an error there only leaves them unserved.
    let enabled_at_all = api_key.is_some()
        || quota_project.is_some()
        || set("GEMINI_AGENT_PLATFORM_PROJECT").is_some();
    if let Some(location) = agent_platform_location
        .as_deref()
        .filter(|_| enabled_at_all)
    {
        // Validated with any valid project.
        AgentPlatform::new("validate", location).ok_or(ConfigError::InvalidGemini)?;
    }
    let agent_platform = match set("GEMINI_AGENT_PLATFORM_PROJECT") {
        Some(project) => {
            if api_key.is_some() || quota_project.is_some() {
                return Err(ConfigError::InvalidGemini);
            }
            let location = agent_platform_location.as_deref().unwrap_or("global");
            Some(AgentPlatform::new(&project, location).ok_or(ConfigError::InvalidGemini)?)
        }
        None => None,
    };
    let enabled = api_key.is_some() || quota_project.is_some() || agent_platform.is_some();
    // GEMINI_BASE_URL replaces the whole URL, for Agent Platform its
    // publisher URL (a Private Service Connect endpoint, say).
    let base_url = set("GEMINI_BASE_URL").or_else(|| {
        enabled.then(|| match &agent_platform {
            Some(platform) => platform.base_url(),
            None => GEMINI_API_URL.to_owned(),
        })
    });
    match (base_url, enabled) {
        (None, false) => Ok(None),
        (Some(base_url), true) => {
            let auth = match api_key {
                Some(api_key) => GeminiAuth::ApiKey(api_key),
                None => {
                    let identity = identity.ok_or(ConfigError::InvalidGemini)?;
                    GeminiAuth::Google {
                        token: identity.token(CLOUD_PLATFORM_SCOPE),
                        // Agent Platform bills the project in its URL, but
                        // its model listing names none: a person's
                        // credentials then need the quota project their
                        // file names, as Google's own libraries send it.
                        quota_project: if agent_platform.is_some() {
                            identity.quota_project().map(str::to_owned)
                        } else {
                            quota_project
                        },
                    }
                }
            };
            let url = Url::parse(&base_url).map_err(|_| ConfigError::InvalidGemini)?;
            url.host_str().ok_or(ConfigError::InvalidGemini)?;
            let loopback = crate::net::is_loopback_url(&url);
            if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
                || url.username() != ""
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(ConfigError::InvalidGemini);
            }
            let storage_base_url = match set("GEMINI_STORAGE_BASE_URL") {
                Some(storage) => {
                    let parsed = Url::parse(&storage).map_err(|_| ConfigError::InvalidGemini)?;
                    let loopback = crate::net::is_loopback_url(&parsed);
                    if !((parsed.scheme() == "https" && !loopback)
                        || (parsed.scheme() == "http" && loopback))
                        || parsed.host_str().is_none()
                        || parsed.query().is_some()
                    {
                        return Err(ConfigError::InvalidGemini);
                    }
                    Some(storage.trim_end_matches('/').to_owned())
                }
                None => None,
            };
            let request_timeout = provider_env("GEMINI_REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".into())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=300).contains(value))
                .ok_or(ConfigError::InvalidGemini)?;
            let connect_timeout = provider_env("GEMINI_CONNECT_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "10".into())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=60).contains(value))
                .ok_or(ConfigError::InvalidGemini)?;
            let max_response_bytes = provider_env("GEMINI_MAX_RESPONSE_BYTES")
                .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
                .parse::<usize>()
                .ok()
                .filter(|value| (1024..=64 * 1024 * 1024).contains(value))
                .ok_or(ConfigError::InvalidGemini)?;
            Ok(Some(GeminiConfig {
                base_url: base_url.trim_end_matches('/').to_owned(),
                auth,
                identity: identity.cloned(),
                agent_platform,
                agent_platform_location,
                storage_base_url,
                request_timeout: Duration::from_secs(request_timeout),
                connect_timeout: Duration::from_secs(connect_timeout),
                max_response_bytes,
            }))
        }
        _ => Err(ConfigError::InvalidGemini),
    }
}

/// Bedrock is enabled by `BEDROCK_AWS_REGION`. Its AWS identity is
/// `BEDROCK_AWS_ACCESS_KEY_ID`/`..._SECRET_ACCESS_KEY` (and optional
/// `..._SESSION_TOKEN`) when set; otherwise the process's runtime identity,
/// found the way the AWS SDKs find it (standard `AWS_*` keys, web identity,
/// container credentials, EC2 instance profile).
fn bedrock_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<BedrockConfig>, ConfigError> {
    let set = |name: &str| {
        provider_env(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let (access_key_id, secret_access_key) = (
        set("BEDROCK_AWS_ACCESS_KEY_ID"),
        set("BEDROCK_AWS_SECRET_ACCESS_KEY"),
    );
    let Some(region) = set("BEDROCK_AWS_REGION") else {
        return if access_key_id.is_some() || secret_access_key.is_some() {
            Err(ConfigError::InvalidBedrock)
        } else {
            Ok(None)
        };
    };
    validate_bedrock_region(&region)?;
    let fixed = |access_key_id: String, secret: String, token: Option<String>| {
        crate::aws_credentials::AwsCredentialSource::fixed(crate::aws::AwsCredentials {
            access_key_id,
            secret_access_key: zeroize::Zeroizing::new(secret),
            session_token: token.map(zeroize::Zeroizing::new),
        })
    };
    let credentials = match (access_key_id, secret_access_key) {
        (Some(id), Some(secret)) => fixed(id, secret, set("BEDROCK_AWS_SESSION_TOKEN")),
        (None, None) => match (set("AWS_ACCESS_KEY_ID"), set("AWS_SECRET_ACCESS_KEY")) {
            (Some(id), Some(secret)) => fixed(id, secret, set("AWS_SESSION_TOKEN")),
            _ => crate::aws_credentials::AwsCredentialSource::runtime(
                &|name: &str| set(name),
                &region,
            )
            .ok_or(ConfigError::InvalidBedrock)?,
        },
        _ => return Err(ConfigError::InvalidBedrock),
    };
    let sts_endpoint = set("BEDROCK_STS_ENDPOINT");
    if let Some(endpoint) = &sts_endpoint {
        validate_bedrock_base_url(endpoint)?;
    }
    let s3_base_url = set("BEDROCK_S3_BASE_URL");
    if let Some(endpoint) = &s3_base_url {
        validate_bedrock_base_url(endpoint)?;
    }
    let base_url = provider_env("BEDROCK_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
    validate_bedrock_base_url(&base_url)?;
    let control_base_url = provider_env("BEDROCK_CONTROL_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("https://bedrock.{region}.amazonaws.com"));
    validate_bedrock_base_url(&control_base_url)?;
    let request_timeout = provider_env("BEDROCK_REQUEST_TIMEOUT_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=300).contains(value))
        .ok_or(ConfigError::InvalidBedrock)?;
    let connect_timeout = provider_env("BEDROCK_CONNECT_TIMEOUT_SECONDS")
        .unwrap_or_else(|_| "10".into())
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=60).contains(value))
        .ok_or(ConfigError::InvalidBedrock)?;
    let max_response_bytes = provider_env("BEDROCK_MAX_RESPONSE_BYTES")
        .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
        .parse::<usize>()
        .ok()
        .filter(|value| (1024..=64 * 1024 * 1024).contains(value))
        .ok_or(ConfigError::InvalidBedrock)?;
    Ok(Some(BedrockConfig {
        base_url: base_url.trim_end_matches('/').to_owned(),
        control_base_url: control_base_url.trim_end_matches('/').to_owned(),
        region,
        credentials,
        sts_endpoint,
        s3_base_url: s3_base_url.map(|url| url.trim_end_matches('/').to_owned()),
        request_timeout: Duration::from_secs(request_timeout),
        connect_timeout: Duration::from_secs(connect_timeout),
        max_response_bytes,
    }))
}

const DEFAULT_AZURE_OPENAI_API_VERSION: &str = "2024-06-01";

fn azure_openai_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    identity: Option<&AzureIdentity>,
) -> Result<Option<AzureOpenAiConfig>, ConfigError> {
    let api_key = provider_env("AZURE_OPENAI_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let base_url = provider_env("AZURE_OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    match (base_url, api_key) {
        (None, None) => Ok(None),
        // Without a key, the gateway's Entra identity.
        (Some(base_url), api_key) => {
            validate_azure_openai_base_url(&base_url)?;
            let auth = match api_key {
                Some(api_key) => AzureAuth::ApiKey(api_key),
                None => {
                    let identity = identity.ok_or(ConfigError::InvalidAzureOpenAi)?;
                    AzureAuth::Entra(identity.token(identity.cognitive_services_scope()))
                }
            };
            let management = azure_management_from_env(
                provider_env,
                identity,
                "AZURE_OPENAI",
                ConfigError::InvalidAzureOpenAi,
            )?;
            let identity = identity.cloned();
            let api_version = provider_env("AZURE_OPENAI_API_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_owned());
            let request_timeout = provider_env("AZURE_OPENAI_REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=300).contains(value))
                .ok_or(ConfigError::InvalidAzureOpenAi)?;
            let connect_timeout = provider_env("AZURE_OPENAI_CONNECT_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "10".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=60).contains(value))
                .ok_or(ConfigError::InvalidAzureOpenAi)?;
            let max_response_bytes = provider_env("AZURE_OPENAI_MAX_RESPONSE_BYTES")
                .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
                .parse::<usize>()
                .ok()
                .filter(|value| (1..=64 * 1024 * 1024).contains(value))
                .ok_or(ConfigError::InvalidAzureOpenAi)?;
            Ok(Some(AzureOpenAiConfig {
                base_url: base_url.trim_end_matches('/').to_owned(),
                auth,
                identity,
                management,
                api_version,
                request_timeout: Duration::from_secs(request_timeout),
                connect_timeout: Duration::from_secs(connect_timeout),
                max_response_bytes,
            }))
        }
        _ => Err(ConfigError::InvalidAzureOpenAi),
    }
}

fn validate_azure_openai_base_url(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidAzureOpenAi)?;
    url.host_str().ok_or(ConfigError::InvalidAzureOpenAi)?;
    let loopback = crate::net::is_loopback_url(&url);
    if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidAzureOpenAi);
    }
    Ok(())
}

// The Azure AI Model Inference API (services.ai.azure.com) that fronts
// Foundry-hosted models predates and is independent of the OpenAI
// `max_completion_tokens` split; `2024-05-01-preview` is the version tied to
// the REST reference and SDK examples this adapter was built against.
const DEFAULT_AZURE_AI_FOUNDRY_API_VERSION: &str = "2024-05-01-preview";

fn azure_ai_foundry_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
    identity: Option<&AzureIdentity>,
) -> Result<Option<AzureAiFoundryConfig>, ConfigError> {
    let api_key = provider_env("AZURE_AI_FOUNDRY_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let base_url = provider_env("AZURE_AI_FOUNDRY_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    match (base_url, api_key) {
        (None, None) => Ok(None),
        // Without a key, the gateway's Entra identity.
        (Some(base_url), api_key) => {
            validate_azure_ai_foundry_base_url(&base_url)?;
            let auth = match api_key {
                Some(api_key) => AzureAuth::ApiKey(api_key),
                None => {
                    let identity = identity.ok_or(ConfigError::InvalidAzureAiFoundry)?;
                    AzureAuth::Entra(identity.token(identity.cognitive_services_scope()))
                }
            };
            let management = azure_management_from_env(
                provider_env,
                identity,
                "AZURE_AI_FOUNDRY",
                ConfigError::InvalidAzureAiFoundry,
            )?;
            let identity = identity.cloned();
            let api_version = provider_env("AZURE_AI_FOUNDRY_API_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_AZURE_AI_FOUNDRY_API_VERSION.to_owned());
            let request_timeout = provider_env("AZURE_AI_FOUNDRY_REQUEST_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "60".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=300).contains(value))
                .ok_or(ConfigError::InvalidAzureAiFoundry)?;
            let connect_timeout = provider_env("AZURE_AI_FOUNDRY_CONNECT_TIMEOUT_SECONDS")
                .unwrap_or_else(|_| "10".to_owned())
                .parse::<u64>()
                .ok()
                .filter(|value| (1..=60).contains(value))
                .ok_or(ConfigError::InvalidAzureAiFoundry)?;
            let max_response_bytes = provider_env("AZURE_AI_FOUNDRY_MAX_RESPONSE_BYTES")
                .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
                .parse::<usize>()
                .ok()
                .filter(|value| (1..=64 * 1024 * 1024).contains(value))
                .ok_or(ConfigError::InvalidAzureAiFoundry)?;
            Ok(Some(AzureAiFoundryConfig {
                base_url: base_url.trim_end_matches('/').to_owned(),
                auth,
                identity,
                management,
                api_version,
                request_timeout: Duration::from_secs(request_timeout),
                connect_timeout: Duration::from_secs(connect_timeout),
                max_response_bytes,
            }))
        }
        _ => Err(ConfigError::InvalidAzureAiFoundry),
    }
}

fn validate_azure_ai_foundry_base_url(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidAzureAiFoundry)?;
    url.host_str().ok_or(ConfigError::InvalidAzureAiFoundry)?;
    let loopback = crate::net::is_loopback_url(&url);
    if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidAzureAiFoundry);
    }
    Ok(())
}

const OPENAI_COMPATIBLE_MAX_EXTRA_HEADERS: usize = 16;
const OPENAI_COMPATIBLE_MAX_HEADER_LEN: usize = 4096;
// Headers the adapter itself attaches to every request; an operator-supplied
// extra header colliding with one of these (case-insensitively) would create
// an ambiguous override, so configuration is rejected outright rather than
// defining a precedence rule.
const OPENAI_COMPATIBLE_RESERVED_HEADERS: [&str; 6] = [
    "authorization",
    "content-type",
    "idempotency-key",
    "x-correlation-id",
    "x-client-request-id",
    "host",
];

fn openai_compatible_from_env(
    provider_env: &impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<OpenAiCompatibleConfig>, ConfigError> {
    let base_url = provider_env("OPENAI_COMPATIBLE_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let Some(base_url) = base_url else {
        return Ok(None);
    };
    validate_openai_compatible_base_url(&base_url)?;
    let api_key = provider_env("OPENAI_COMPATIBLE_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let extra_headers = match provider_env("OPENAI_COMPATIBLE_EXTRA_HEADERS")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(value) => parse_openai_compatible_extra_headers(&value)?,
        None => Vec::new(),
    };
    let request_stream_usage = provider_env("OPENAI_COMPATIBLE_REQUEST_STREAM_USAGE")
        .unwrap_or_else(|_| "false".to_owned())
        .parse::<bool>()
        .map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
    let request_timeout = provider_env("OPENAI_COMPATIBLE_REQUEST_TIMEOUT_SECONDS")
        .unwrap_or_else(|_| "60".to_owned())
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=300).contains(value))
        .ok_or(ConfigError::InvalidOpenAiCompatible)?;
    let connect_timeout = provider_env("OPENAI_COMPATIBLE_CONNECT_TIMEOUT_SECONDS")
        .unwrap_or_else(|_| "10".to_owned())
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=60).contains(value))
        .ok_or(ConfigError::InvalidOpenAiCompatible)?;
    let max_response_bytes = provider_env("OPENAI_COMPATIBLE_MAX_RESPONSE_BYTES")
        .unwrap_or_else(|_| (8 * 1024 * 1024).to_string())
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=64 * 1024 * 1024).contains(value))
        .ok_or(ConfigError::InvalidOpenAiCompatible)?;
    Ok(Some(OpenAiCompatibleConfig {
        base_url: base_url.trim_end_matches('/').to_owned(),
        api_key,
        extra_headers,
        request_stream_usage,
        request_timeout: Duration::from_secs(request_timeout),
        connect_timeout: Duration::from_secs(connect_timeout),
        max_response_bytes,
        pinned_client: None,
    }))
}

/// The origins in a comma-separated list of URLs, as
/// `url::Origin::ascii_serialization` writes them (`http://host:8080`). Only
/// scheme, host and port count; a path is ignored.
fn private_origins(list: &str) -> Result<Vec<String>, ConfigError> {
    let mut origins = Vec::new();
    for entry in list
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let url = Url::parse(entry).map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ConfigError::InvalidOpenAiCompatible);
        }
        let origin = url.origin().ascii_serialization();
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    Ok(origins)
}

fn validate_openai_compatible_base_url(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
    url.host_str().ok_or(ConfigError::InvalidOpenAiCompatible)?;
    let loopback = crate::net::is_loopback_url(&url);
    if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidOpenAiCompatible);
    }
    Ok(())
}

/// `OPENAI_COMPATIBLE_EXTRA_HEADERS` is a JSON object of string header
/// name/value pairs, validated eagerly so a misconfigured operator value
/// fails at startup rather than as a per-request adapter error.
fn parse_openai_compatible_extra_headers(
    value: &str,
) -> Result<Vec<(String, String)>, ConfigError> {
    if value.len() > OPENAI_COMPATIBLE_MAX_EXTRA_HEADERS * OPENAI_COMPATIBLE_MAX_HEADER_LEN {
        return Err(ConfigError::InvalidOpenAiCompatible);
    }
    let parsed: std::collections::BTreeMap<String, String> =
        serde_json::from_str(value).map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
    if parsed.len() > OPENAI_COMPATIBLE_MAX_EXTRA_HEADERS {
        return Err(ConfigError::InvalidOpenAiCompatible);
    }
    parsed
        .into_iter()
        .map(|(name, header_value)| {
            if name.len() > OPENAI_COMPATIBLE_MAX_HEADER_LEN
                || header_value.len() > OPENAI_COMPATIBLE_MAX_HEADER_LEN
            {
                return Err(ConfigError::InvalidOpenAiCompatible);
            }
            if OPENAI_COMPATIBLE_RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(ConfigError::InvalidOpenAiCompatible);
            }
            header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
            header::HeaderValue::from_str(&header_value)
                .map_err(|_| ConfigError::InvalidOpenAiCompatible)?;
            Ok((name, header_value))
        })
        .collect()
}

fn validate_bedrock_region(value: &str) -> Result<(), ConfigError> {
    let valid = (3..=25).contains(&value.len())
        && value.contains('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-');
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidBedrock)
    }
}

fn validate_bedrock_base_url(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidBedrock)?;
    url.host_str().ok_or(ConfigError::InvalidBedrock)?;
    let loopback = crate::net::is_loopback_url(&url);
    if !((url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback))
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidBedrock);
    }
    Ok(())
}

fn validate_openai_base_url(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidOpenAi)?;
    url.host_str().ok_or(ConfigError::InvalidOpenAi)?;
    let loopback = crate::net::is_loopback_url(&url);
    let valid_transport =
        (url.scheme() == "https" && !loopback) || (url.scheme() == "http" && loopback);
    if !valid_transport
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidOpenAi);
    }
    Ok(())
}

fn secret_bytes(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    name: &'static str,
    minimum: usize,
) -> Result<Vec<u8>, ConfigError> {
    let value = required_env(config_env, name)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .unwrap_or_else(|_| value.into_bytes());
    if decoded.len() < minimum {
        return Err(ConfigError::InvalidSecret { name });
    }
    Ok(decoded)
}

fn secret_bytes_exact(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    name: &'static str,
    length: usize,
) -> Result<Vec<u8>, ConfigError> {
    let value = required_env(config_env, name)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .unwrap_or_else(|_| value.into_bytes());
    if decoded.len() != length {
        return Err(ConfigError::InvalidSecret { name });
    }
    Ok(decoded)
}

/// A boolean variable: `None` when unset or empty, an error for anything
/// but `true` or `false`.
fn optional_bool(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    name: &str,
) -> Result<Option<bool>, ()> {
    match config_env(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => value.trim().parse::<bool>().map(Some).map_err(|_| ()),
        Err(_) => Ok(None),
    }
}

/// The origin and session secrets local accounts sign in with when OIDC is
/// off. `choice` is `GATEWAY_LOCAL_ACCOUNTS`: `false` turns them off;
/// `true` requires the settings; unset turns them on when every setting is
/// there and valid, and leaves them off otherwise (so an API configured
/// before local accounts existed starts as it did).
fn local_sessions(
    config_env: &impl Fn(&str) -> Result<String, env::VarError>,
    choice: Option<bool>,
) -> Result<Option<(String, SessionConfig)>, ConfigError> {
    if choice == Some(false) {
        return Ok(None);
    }
    let read = || -> Result<(String, SessionConfig), ConfigError> {
        let origin = required_env(config_env, "GATEWAY_ORIGIN")?
            .trim()
            .trim_end_matches('/')
            .to_owned();
        validate_origin(&origin)?;
        let hmac_secret = secret_bytes(config_env, "OIDC_SESSION_HMAC_SECRET", 32)?;
        let encryption_key = secret_bytes_exact(config_env, "OIDC_SESSION_ENCRYPTION_KEY", 32)?;
        let ttl_seconds = config_env("OIDC_SESSION_TTL_SECONDS")
            .unwrap_or_else(|_| "3600".to_owned())
            .parse::<u64>()
            .ok()
            .filter(|ttl| (60..=86_400).contains(ttl))
            .ok_or(ConfigError::InvalidOidc)?;
        Ok((
            origin,
            SessionConfig {
                hmac_secret: Some(hmac_secret),
                encryption_key: Some(encryption_key),
                ttl_seconds,
            },
        ))
    };
    match (choice, read()) {
        (_, Ok(settings)) => Ok(Some(settings)),
        (Some(true), Err(_)) => Err(ConfigError::InvalidLocalAccounts),
        (_, Err(_)) => Ok(None),
    }
}

fn validate_oidc_url(value: &str, issuer: bool) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidOidc)?;
    let valid_scheme = if issuer {
        url.scheme() == "https"
    } else {
        (url.scheme() == "https") || (url.scheme() == "http" && crate::net::is_loopback_url(&url))
    };
    if !valid_scheme
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidOidc);
    }
    Ok(())
}

pub(crate) fn validate_origin(value: &str) -> Result<(), ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidOidc)?;
    if url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(ConfigError::InvalidOidc);
    }
    url.host_str().ok_or(ConfigError::InvalidOidc)?;
    if url.scheme() == "http" {
        if !crate::net::is_loopback_url(&url) {
            return Err(ConfigError::InvalidOidc);
        }
    } else if url.scheme() != "https" {
        return Err(ConfigError::InvalidOidc);
    }
    Ok(())
}

fn non_empty<'a>(value: Option<&'a str>, name: &'static str) -> Result<&'a str, ConfigError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::Missing { name })
}

pub fn redact_database_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "<redacted database URL>".to_owned();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("REDACTED");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("REDACTED"));
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_max_connections_defaults_and_bounds() {
        let with = |value: Option<&'static str>| {
            move |name: &str| match (name, value) {
                ("GATEWAY_DATABASE_MAX_CONNECTIONS", Some(value)) => Ok(value.to_owned()),
                _ => Err(env::VarError::NotPresent),
            }
        };
        assert_eq!(database_max_connections_from_env(&with(None)).unwrap(), 10);
        assert_eq!(
            database_max_connections_from_env(&with(Some(" "))).unwrap(),
            10
        );
        assert_eq!(
            database_max_connections_from_env(&with(Some("40"))).unwrap(),
            40
        );
        for bad in ["0", "1001", "-1", "ten"] {
            assert!(matches!(
                database_max_connections_from_env(&with(Some(bad))),
                Err(ConfigError::InvalidDatabaseMaxConnections)
            ));
        }
    }

    #[test]
    fn inference_max_body_bytes_defaults_and_bounds() {
        let with = |value: Option<&'static str>| {
            move |name: &str| match (name, value) {
                ("GATEWAY_INFERENCE_MAX_BODY_BYTES", Some(value)) => Ok(value.to_owned()),
                _ => Err(env::VarError::NotPresent),
            }
        };
        assert_eq!(
            inference_max_body_bytes_from_env(&with(None)).unwrap(),
            4 * 1024 * 1024
        );
        assert_eq!(
            inference_max_body_bytes_from_env(&with(Some("16777216"))).unwrap(),
            16 * 1024 * 1024
        );
        for bad in ["65535", "67108865", "-1", "4MiB"] {
            assert!(matches!(
                inference_max_body_bytes_from_env(&with(Some(bad))),
                Err(ConfigError::InvalidInferenceMaxBodyBytes)
            ));
        }
    }

    #[test]
    fn budget_leases_are_off_unless_turned_on() {
        let load = |value: Option<&'static str>| {
            AppConfig::from_lookup(GatewayCommand::Run, &move |name: &str| {
                match name {
                    "DATABASE_URL" => Some("postgres://gateway_app:secret@localhost/gateway"),
                    "GATEWAY_PROFILE" => Some("all-in-one"),
                    "API_KEY_PEPPER" => Some("pepper-0000000000000000"),
                    "GATEWAY_BUDGET_LEASES" => value,
                    _ => None,
                }
                .map(str::to_owned)
            })
        };
        assert!(!load(None).unwrap().budget_leases);
        assert!(!load(Some("false")).unwrap().budget_leases);
        assert!(load(Some("true")).unwrap().budget_leases);
        assert_eq!(
            load(Some("yes")).unwrap_err(),
            ConfigError::InvalidBudgetLeases
        );
    }

    #[test]
    fn rate_limit_leases_are_off_unless_turned_on() {
        let load = |value: Option<&'static str>| {
            AppConfig::from_lookup(GatewayCommand::Run, &move |name: &str| {
                match name {
                    "DATABASE_URL" => Some("postgres://gateway_app:secret@localhost/gateway"),
                    "GATEWAY_PROFILE" => Some("all-in-one"),
                    "API_KEY_PEPPER" => Some("pepper-0000000000000000"),
                    "GATEWAY_RATE_LIMIT_LEASES" => value,
                    _ => None,
                }
                .map(str::to_owned)
            })
        };
        assert!(!load(None).unwrap().rate_limit_leases);
        assert!(!load(Some("false")).unwrap().rate_limit_leases);
        assert!(load(Some("true")).unwrap().rate_limit_leases);
        // Independent of budget leases.
        assert!(!load(None).unwrap().budget_leases);
        assert_eq!(
            load(Some("yes")).unwrap_err(),
            ConfigError::InvalidRateLimitLeases
        );
    }

    #[test]
    fn instance_settings_are_validated() {
        let lookup = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
                    .ok_or(env::VarError::NotPresent)
            }
        };
        let instance = instance_from_env(&lookup(&[
            ("GATEWAY_INSTANCE_ID", "dp-eu-1"),
            ("GATEWAY_REGION", "eu-west-1"),
            ("GATEWAY_BUILD", "2017ab33b970"),
            ("GATEWAY_CONFIG_POLL_SECONDS", "2"),
        ]))
        .unwrap();
        assert_eq!(
            (
                instance.id.as_str(),
                instance.region.as_deref(),
                instance.build.as_str(),
                instance.poll
            ),
            (
                "dp-eu-1",
                Some("eu-west-1"),
                "2017ab33b970",
                std::time::Duration::from_secs(2)
            )
        );
        let defaults = instance_from_env(&lookup(&[])).unwrap();
        assert_eq!(
            (defaults.region, defaults.poll),
            (None, std::time::Duration::from_secs(5))
        );
        assert!(!defaults.id.is_empty());
        for (name, value) in [
            ("GATEWAY_CONFIG_POLL_SECONDS", "0"),
            ("GATEWAY_CONFIG_POLL_SECONDS", "301"),
            ("GATEWAY_CONFIG_POLL_SECONDS", "soon"),
            ("GATEWAY_REGION", "eu west"),
            ("GATEWAY_INSTANCE_ID", "a\u{7f}"),
        ] {
            let vars: &'static [(&'static str, &'static str)] =
                Box::leak(vec![(name, value)].into_boxed_slice());
            assert_eq!(
                instance_from_env(&lookup(vars)),
                Err(ConfigError::InvalidInstance { name }),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn parses_profiles_and_migration_command() {
        assert_eq!("all-in-one".parse(), Ok(GatewayProfile::AllInOne));
        assert_eq!("api".parse(), Ok(GatewayProfile::Api));
        assert_eq!("worker".parse(), Ok(GatewayProfile::Worker));
        assert_eq!("data-plane".parse(), Ok(GatewayProfile::DataPlane));
        assert_eq!(GatewayProfile::DataPlane.to_string(), "data-plane");
        assert_eq!(
            GatewayCommand::parse(Vec::<OsString>::new()),
            Ok(GatewayCommand::Run)
        );
        assert_eq!(
            GatewayCommand::parse([OsString::from("migrate")]),
            Ok(GatewayCommand::Migrate)
        );
        assert!(GatewayCommand::parse([OsString::from("--migrate")]).is_err());
    }

    #[test]
    fn migrate_config_does_not_require_pepper() {
        let no_env = |_name: &str| Err(env::VarError::NotPresent);
        let config = AppConfig::from_values_inner(
            Some("postgres://gateway_app:secret@localhost/gateway"),
            None,
            Some("all-in-one"),
            None,
            false,
            &no_env,
            ProviderConfigs {
                openai: None,
                anthropic: None,
                gemini: None,
                bedrock: None,
                azure_openai: None,
                azure_ai_foundry: None,
                openai_compatible: None,
            },
        )
        .unwrap();
        assert!(config.api_key_pepper.is_none());
    }

    fn bedrock_env(
        control_base_url: Option<&str>,
    ) -> impl Fn(&str) -> Result<String, env::VarError> + '_ {
        move |name| {
            match name {
                "BEDROCK_AWS_ACCESS_KEY_ID" => Some("AKIDEXAMPLE"),
                "BEDROCK_AWS_SECRET_ACCESS_KEY" => Some("secret"),
                "BEDROCK_AWS_REGION" => Some("us-east-1"),
                "BEDROCK_CONTROL_BASE_URL" => control_base_url,
                _ => None,
            }
            .map(ToOwned::to_owned)
            .ok_or(env::VarError::NotPresent)
        }
    }

    #[test]
    fn private_endpoint_origins_are_normalized() {
        assert_eq!(
            private_origins(
                " http://qwen.models.svc.cluster.local:8080/v1, https://Models.Internal,http://qwen.models.svc.cluster.local:8080 ,"
            )
            .unwrap(),
            [
                "http://qwen.models.svc.cluster.local:8080",
                "https://models.internal"
            ]
        );
        assert!(private_origins("").unwrap().is_empty());
        for invalid in [
            "qwen:8080",
            "ftp://models.internal",
            "http://user:pass@models.internal",
        ] {
            assert!(private_origins(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn onboarding_identities_are_checked_and_azure_needs_an_origin() {
        let load = |vars: &[(&'static str, &'static str)], origin: Option<&str>| {
            let vars = vars.to_vec();
            onboarding_from_env(
                &move |name: &str| {
                    vars.iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| (*value).to_owned())
                        .ok_or(env::VarError::NotPresent)
                },
                origin,
            )
        };
        let all = [
            (
                "GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN",
                "arn:aws:iam::123456789012:user/example-gateway",
            ),
            (
                "GATEWAY_ONBOARDING_AWS_TEMPLATE_URL",
                "https://bucket.s3.us-east-2.amazonaws.com/templates/aws-bedrock-role-v1.yaml",
            ),
            ("AZURE_CLIENT_ID", "00000000-0000-4000-8000-000000000020"),
            (
                "GATEWAY_ONBOARDING_GOOGLE_SERVICE_ACCOUNT",
                "gateway-dev@example-project-123456.iam.gserviceaccount.com",
            ),
        ];
        let identities = load(&all, Some("https://gateway.example/")).unwrap();
        assert!(
            identities.aws_principal_arn.is_some() && identities.google_service_account.is_some()
        );
        assert_eq!(
            identities.azure_client_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000020")
        );
        assert_eq!(
            identities.azure_consent_redirect.unwrap().as_str(),
            "https://gateway.example/api/admin/v1/onboarding/azure/consent"
        );
        // Without an origin, consent could not return: Azure is not offered.
        assert_eq!(load(&all, None).unwrap().azure_client_id, None);
        assert_eq!(
            load(&[], None).unwrap(),
            crate::domain::onboarding::OnboardingIdentities::default()
        );
        for bad in [
            (
                "GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN",
                "arn:aws:iam::123:user/x",
            ),
            (
                "GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN",
                "arn:aws:iam::123456789012:user/x\"; rm -rf /",
            ),
            (
                "GATEWAY_ONBOARDING_AWS_TEMPLATE_URL",
                "http://bucket.example/t.yaml",
            ),
            ("AZURE_CLIENT_ID", "not-a-uuid"),
            (
                "GATEWAY_ONBOARDING_GOOGLE_SERVICE_ACCOUNT",
                "someone@example.com",
            ),
        ] {
            assert!(
                matches!(
                    load(&[bad], Some("https://gateway.example")),
                    Err(ConfigError::InvalidOnboarding)
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_microsoft_graph_photo_is_opt_in() {
        let load = |photo: Option<&'static str>| {
            let env = move |name: &str| {
                let value = match name {
                    "OIDC_ENABLED" => "true",
                    "GATEWAY_ORIGIN" => "https://gateway.example",
                    "OIDC_ISSUER" => "https://login.example/tenant/v2.0",
                    "OIDC_AUDIENCE" | "OIDC_CLIENT_ID" => "client",
                    "OIDC_REDIRECT_URI" => "https://gateway.example/api/admin/v1/oidc/callback",
                    "OIDC_SESSION_HMAC_SECRET" | "OIDC_SESSION_ENCRYPTION_KEY" => {
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    }
                    "OIDC_PROFILE_PHOTO" => photo.ok_or(env::VarError::NotPresent)?,
                    _ => return Err(env::VarError::NotPresent),
                };
                Ok(value.to_owned())
            };
            AppConfig::from_values_with_env(
                Some("postgres://gateway@localhost/gateway"),
                None,
                Some("api"),
                Some("pepper"),
                true,
                &env,
            )
            .map(|config| config.oidc.microsoft_graph_photo)
        };
        assert!(!load(None).unwrap());
        assert!(!load(Some("")).unwrap());
        assert!(load(Some("microsoft-graph")).unwrap());
        assert!(matches!(
            load(Some("gravatar")),
            Err(ConfigError::InvalidOidc)
        ));
    }

    #[test]
    fn bedrock_control_plane_defaults_to_the_regional_host() {
        let config = bedrock_from_env(&bedrock_env(None)).unwrap().unwrap();
        assert_eq!(
            config.control_base_url,
            "https://bedrock.us-east-1.amazonaws.com"
        );
        assert_eq!(
            config.base_url,
            "https://bedrock-runtime.us-east-1.amazonaws.com"
        );
        let config = bedrock_from_env(&bedrock_env(Some("http://127.0.0.1:9000/")))
            .unwrap()
            .unwrap();
        assert_eq!(config.control_base_url, "http://127.0.0.1:9000");
        for invalid in [
            "http://bedrock.example.com",
            "https://bedrock.example.com/?a=b",
        ] {
            assert!(matches!(
                bedrock_from_env(&bedrock_env(Some(invalid))),
                Err(ConfigError::InvalidBedrock)
            ));
        }
    }

    #[test]
    fn bedrock_identity_comes_from_keys_or_the_runtime_role() {
        let from = |vars: &[(&'static str, &'static str)]| {
            let vars = vars.to_vec();
            bedrock_from_env(&move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
                    .ok_or(env::VarError::NotPresent)
            })
        };
        let region = ("BEDROCK_AWS_REGION", "us-east-1");
        let describe = |vars: &[(&'static str, &'static str)]| {
            format!("{:?}", from(vars).unwrap().unwrap().credentials)
        };
        assert!(matches!(from(&[]), Ok(None)));
        assert!(matches!(
            from(&[("BEDROCK_AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")]),
            Err(ConfigError::InvalidBedrock)
        ));
        assert!(matches!(
            from(&[region, ("BEDROCK_AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")]),
            Err(ConfigError::InvalidBedrock)
        ));
        assert!(
            describe(&[
                region,
                ("AWS_ACCESS_KEY_ID", "AKID"),
                ("AWS_SECRET_ACCESS_KEY", "s")
            ])
            .contains("static keys")
        );
        // Region alone: the role the gateway runs as.
        assert!(
            describe(&[
                region,
                (
                    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                    "/v2/credentials/id"
                )
            ])
            .contains("container credentials")
        );
        assert!(!describe(&[region]).contains("static keys"));
        assert!(matches!(
            from(&[region, ("AWS_EC2_METADATA_DISABLED", "true")]),
            Err(ConfigError::InvalidBedrock)
        ));

        let sts = |endpoint| {
            from(&[
                region,
                ("AWS_ACCESS_KEY_ID", "AKID"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
                ("BEDROCK_STS_ENDPOINT", endpoint),
            ])
        };
        assert_eq!(
            sts("https://vpce-1.sts.us-east-1.vpce.amazonaws.com")
                .unwrap()
                .unwrap()
                .sts_endpoint
                .as_deref(),
            Some("https://vpce-1.sts.us-east-1.vpce.amazonaws.com")
        );
        assert!(matches!(
            sts("http://sts.example.com"),
            Err(ConfigError::InvalidBedrock)
        ));
    }

    #[test]
    fn azure_authenticates_with_a_key_or_the_gateway_entra_identity() {
        let from = |vars: &[(&'static str, &'static str)]| {
            let vars = vars.to_vec();
            let env = move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
                    .ok_or(env::VarError::NotPresent)
            };
            let identity = AzureIdentity::from_env(&|name: &str| env(name).ok());
            (
                azure_openai_from_env(&env, identity.as_ref()),
                azure_ai_foundry_from_env(&env, identity.as_ref()),
            )
        };
        let openai = ("AZURE_OPENAI_BASE_URL", "https://res.openai.azure.com");
        let foundry = (
            "AZURE_AI_FOUNDRY_BASE_URL",
            "https://res.services.ai.azure.com",
        );
        let (with_key, _) = from(&[openai, ("AZURE_OPENAI_API_KEY", "key")]);
        assert!(matches!(
            with_key.unwrap().unwrap().auth,
            AzureAuth::ApiKey(_)
        ));
        // No key: the gateway's identity, here a client secret.
        let secret = [
            ("AZURE_TENANT_ID", "gateway-tenant"),
            ("AZURE_CLIENT_ID", "gateway-app"),
            ("AZURE_CLIENT_SECRET", "s"),
        ];
        let (openai_config, foundry_config) =
            from(&[openai, foundry, secret[0], secret[1], secret[2]]);
        let (openai_config, foundry_config) = (
            openai_config.unwrap().unwrap(),
            foundry_config.unwrap().unwrap(),
        );
        assert_eq!(
            format!("{:?} {:?}", openai_config.auth, openai_config.identity),
            "AzureAuth::Entra(AzureTokenSource(Entra token)) Some(AzureIdentity(client secret))"
        );
        assert!(matches!(foundry_config.auth, AzureAuth::Entra(_)));
        // An incomplete identity does not stop a key; it is an error only
        // where it is needed.
        let (with_key, _) = from(&[openai, ("AZURE_OPENAI_API_KEY", "key"), secret[2]]);
        let with_key = with_key.unwrap().unwrap();
        assert!(matches!(with_key.auth, AzureAuth::ApiKey(_)));
        assert!(with_key.identity.is_none());
        // A sovereign cloud's sign-in host selects its scope.
        let (government, _) = from(&[
            openai,
            secret[0],
            secret[1],
            secret[2],
            ("AZURE_AUTHORITY_HOST", "https://login.microsoftonline.us"),
        ]);
        assert_eq!(
            government
                .unwrap()
                .unwrap()
                .identity
                .unwrap()
                .cognitive_services_scope(),
            "https://cognitiveservices.azure.us/.default"
        );
        let (_, incomplete) = from(&[foundry, secret[2]]);
        assert!(matches!(
            incomplete,
            Err(ConfigError::InvalidAzureAiFoundry)
        ));
        // A key alone still names no endpoint.
        let (keyless, _) = from(&[("AZURE_OPENAI_API_KEY", "key")]);
        assert!(matches!(keyless, Err(ConfigError::InvalidAzureOpenAi)));

        // The resource's ID lets discovery read Resource Manager with the
        // gateway's identity (D-018).
        let resource = (
            "AZURE_AI_FOUNDRY_RESOURCE_ID",
            "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/res",
        );
        let (_, listed) = from(&[foundry, resource, secret[0], secret[1], secret[2]]);
        let management = listed.unwrap().unwrap().management.unwrap();
        assert_eq!(management.endpoint, "https://management.azure.com");
        assert_eq!(management.resource_id, resource.1);
        assert!(foundry_config.management.is_none());
        // Tenants' settings never carry the process's resource.
        let env = |name: &str| {
            [foundry, resource]
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
                .ok_or(env::VarError::NotPresent)
        };
        let tenants = for_tenants(
            &env,
            "AZURE_AI_FOUNDRY_BASE_URL",
            "AZURE_AI_FOUNDRY_API_KEY",
            "https://tenant.services.ai.azure.com",
        );
        assert!(tenants("AZURE_AI_FOUNDRY_RESOURCE_ID").is_err());
        // Never without the gateway's identity (a key does not sign in to
        // Resource Manager), nor for anything but an account.
        let (_, key_only) = from(&[
            foundry,
            resource,
            ("AZURE_AI_FOUNDRY_API_KEY", "key"),
            secret[2],
        ]);
        assert!(matches!(key_only, Err(ConfigError::InvalidAzureAiFoundry)));
        let (_, not_an_account) = from(&[
            foundry,
            ("AZURE_AI_FOUNDRY_RESOURCE_ID", "/subscriptions/x"),
            secret[0],
            secret[1],
            secret[2],
        ]);
        assert!(matches!(
            not_an_account,
            Err(ConfigError::InvalidAzureAiFoundry)
        ));
        assert!(matches!(from(&[]), (Ok(None), Ok(None))));

        // Tenants' own resources take the family's settings, never its key.
        let env = |name: &str| match name {
            "AZURE_OPENAI_API_VERSION" => Ok("2025-configured".to_owned()),
            "AZURE_OPENAI_API_KEY" => Ok("operator-key".to_owned()),
            _ => Err(env::VarError::NotPresent),
        };
        let identity = AzureIdentity::from_env(&|_: &str| None).unwrap();
        let for_tenant = azure_openai_from_env(
            &for_tenants(
                &env,
                "AZURE_OPENAI_BASE_URL",
                "AZURE_OPENAI_API_KEY",
                "https://tenant.openai.azure.com",
            ),
            Some(&identity),
        )
        .unwrap()
        .unwrap();
        assert_eq!(for_tenant.api_version, "2025-configured");
        assert!(matches!(for_tenant.auth, AzureAuth::Entra(_)));
    }

    #[test]
    fn gemini_authenticates_with_a_key_or_the_gateway_google_identity() {
        let dir = crate::gcp_credentials::tests::TempDir::new();
        let adc = dir.write(
            "adc.json",
            r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r"}"#,
        );
        let from = |vars: Vec<(&'static str, String)>| {
            let env = move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
                    .ok_or(env::VarError::NotPresent)
            };
            let identity = GoogleIdentity::from_env(&|name: &str| env(name).ok());
            (gemini_from_env(&env, identity.as_ref()), identity)
        };
        let credentials = ("GOOGLE_APPLICATION_CREDENTIALS", adc.clone());
        // A key, as before.
        let (with_key, _) = from(vec![("GEMINI_API_KEY", "key".into())]);
        assert!(matches!(
            with_key.unwrap().unwrap().auth,
            GeminiAuth::ApiKey(_)
        ));
        // A quota project and no key: the gateway's identity.
        let (google, identity) = from(vec![
            ("GEMINI_QUOTA_PROJECT", "billing-project".into()),
            ("GEMINI_REQUEST_TIMEOUT_SECONDS", "30".into()),
            credentials.clone(),
        ]);
        let google = google.unwrap().unwrap();
        assert_eq!(
            google.base_url,
            "https://generativelanguage.googleapis.com/v1beta"
        );
        let GeminiAuth::Google { quota_project, .. } = &google.auth else {
            panic!("Google auth without a key");
        };
        assert_eq!(quota_project.as_deref(), Some("billing-project"));
        assert_eq!(
            format!("{:?}", identity.unwrap()),
            "GoogleIdentity(user credentials)"
        );
        // A quota project with a credentials file that cannot be used.
        let (unusable, _) = from(vec![
            ("GEMINI_QUOTA_PROJECT", "billing-project".into()),
            (
                "GOOGLE_APPLICATION_CREDENTIALS",
                "/nonexistent/adc.json".into(),
            ),
        ]);
        assert!(matches!(unusable, Err(ConfigError::InvalidGemini)));
        // ...does not stop a key.
        let (with_key, identity) = from(vec![
            ("GEMINI_API_KEY", "key".into()),
            (
                "GOOGLE_APPLICATION_CREDENTIALS",
                "/nonexistent/adc.json".into(),
            ),
        ]);
        assert!(with_key.unwrap().is_some() && identity.is_none());
        assert!(matches!(from(vec![]).0, Ok(None)));

        // Tenants' service accounts take Gemini's settings, never its key.
        let env = |name: &str| match name {
            "GEMINI_API_KEY" => Ok("operator-key".to_owned()),
            "GEMINI_REQUEST_TIMEOUT_SECONDS" => Ok("30".to_owned()),
            "GOOGLE_APPLICATION_CREDENTIALS" => Ok(adc.clone()),
            _ => Err(env::VarError::NotPresent),
        };
        let identity = GoogleIdentity::from_env(&|name: &str| env(name).ok()).unwrap();
        let for_tenant = gemini_from_env(
            &for_tenants(
                &env,
                "GEMINI_QUOTA_PROJECT",
                "GEMINI_API_KEY",
                "tenant-project",
            ),
            Some(&identity),
        )
        .unwrap()
        .unwrap();
        assert_eq!(for_tenant.request_timeout, Duration::from_secs(30));
        assert!(matches!(for_tenant.auth, GeminiAuth::Google { .. }));
    }

    #[test]
    fn gemini_runs_on_agent_platform_with_the_gateway_identity() {
        let dir = crate::gcp_credentials::tests::TempDir::new();
        let adc = dir.write(
            "adc.json",
            r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r"}"#,
        );
        let from = |vars: Vec<(&'static str, &'static str)>| {
            let adc = adc.clone();
            let env = move |name: &str| {
                if name == "GOOGLE_APPLICATION_CREDENTIALS" {
                    return Ok(adc.clone());
                }
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
                    .ok_or(env::VarError::NotPresent)
            };
            let identity = GoogleIdentity::from_env(&|name: &str| env(name).ok());
            gemini_from_env(&env, identity.as_ref())
        };
        let global = from(vec![("GEMINI_AGENT_PLATFORM_PROJECT", "gateway-project")])
            .unwrap()
            .unwrap();
        assert_eq!(
            global.base_url,
            "https://aiplatform.googleapis.com/v1/projects/gateway-project/locations/global/publishers/google"
        );
        assert!(matches!(
            global.auth,
            GeminiAuth::Google {
                quota_project: None,
                ..
            }
        ));
        // A person's credentials file names a quota project; the model
        // listing needs it.
        let with_quota = dir.write(
            "adc-quota.json",
            r#"{"type":"authorized_user","client_id":"c","client_secret":"s","refresh_token":"r","quota_project_id":"quota-project"}"#,
        );
        let identity = GoogleIdentity::from_env(&|name: &str| {
            (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| with_quota.clone())
        });
        let quota = gemini_from_env(
            &|name: &str| match name {
                "GEMINI_AGENT_PLATFORM_PROJECT" => Ok("gateway-project".to_owned()),
                _ => Err(env::VarError::NotPresent),
            },
            identity.as_ref(),
        )
        .unwrap()
        .unwrap();
        let GeminiAuth::Google { quota_project, .. } = &quota.auth else {
            panic!("the gateway's identity");
        };
        assert_eq!(quota_project.as_deref(), Some("quota-project"));
        let regional = from(vec![
            ("GEMINI_AGENT_PLATFORM_PROJECT", "gateway-project"),
            ("GEMINI_AGENT_PLATFORM_LOCATION", "us-central1"),
        ])
        .unwrap()
        .unwrap();
        assert!(regional.base_url.starts_with("https://us-central1-aiplatform.googleapis.com/v1/projects/gateway-project/locations/us-central1/"));
        for invalid in [
            vec![
                ("GEMINI_AGENT_PLATFORM_PROJECT", "gateway-project"),
                ("GEMINI_API_KEY", "k"),
            ],
            vec![
                ("GEMINI_AGENT_PLATFORM_PROJECT", "gateway-project"),
                ("GEMINI_QUOTA_PROJECT", "billing-project"),
            ],
            vec![("GEMINI_AGENT_PLATFORM_PROJECT", "Bad_Project")],
            vec![
                ("GEMINI_AGENT_PLATFORM_PROJECT", "gateway-project"),
                ("GEMINI_AGENT_PLATFORM_LOCATION", "us central"),
            ],
        ] {
            assert!(
                matches!(from(invalid.clone()), Err(ConfigError::InvalidGemini)),
                "{invalid:?}"
            );
        }
        // Tenants' service accounts run on Agent Platform in the configured
        // location, whatever the process's own Gemini is.
        let env = |name: &str| match name {
            "GEMINI_API_KEY" => Ok("operator-key".to_owned()),
            "GEMINI_AGENT_PLATFORM_LOCATION" => Ok("europe-west4".to_owned()),
            _ => Err(env::VarError::NotPresent),
        };
        let identity = GoogleIdentity::from_env(&|name: &str| {
            (name == "GOOGLE_APPLICATION_CREDENTIALS").then(|| adc.clone())
        })
        .unwrap();
        let for_tenants = gemini_from_env(&gemini_for_tenants(&env), Some(&identity))
            .unwrap()
            .unwrap();
        assert_eq!(
            for_tenants.agent_platform.map(|platform| platform.location),
            Some("europe-west4".to_owned())
        );
    }

    #[test]
    fn an_invalid_location_of_a_disabled_gemini_never_stops_startup() {
        let env = |name: &str| match name {
            "GEMINI_AGENT_PLATFORM_LOCATION" => Ok("US-CENTRAL1".to_owned()),
            _ => Err(env::VarError::NotPresent),
        };
        assert!(matches!(gemini_from_env(&env, None), Ok(None)));
        // With Gemini enabled it is an error, which for tenants' settings
        // only leaves their service accounts unserved.
        let enabled = |name: &str| match name {
            "GEMINI_API_KEY" => Ok("key".to_owned()),
            other => env(other),
        };
        assert!(matches!(
            gemini_from_env(&enabled, None),
            Err(ConfigError::InvalidGemini)
        ));
    }

    #[test]
    fn unused_azure_settings_never_stop_startup() {
        let config = AppConfig::from_values_with_env(
            Some("postgres://gateway_app:secret@localhost/gateway"),
            None,
            Some("api"),
            Some("test-pepper"),
            true,
            &|name: &str| match name {
                "AZURE_OPENAI_REQUEST_TIMEOUT_SECONDS" => Ok("0".to_owned()),
                _ => Err(env::VarError::NotPresent),
            },
        )
        .unwrap();
        assert!(config.azure_openai.is_none());
        // The invalid setting leaves only that family's tenants unserved.
        assert!(config.azure_openai_for_tenants.is_none());
        assert!(config.azure_ai_foundry_for_tenants.is_some());
    }

    #[test]
    fn parses_invitation_commands() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(
            parse(&[
                "invitation",
                "create",
                "--organization",
                "acme",
                "--valid-hours",
                "24"
            ])
            .unwrap(),
            GatewayCommand::Invitation {
                action: InvitationAction::Create,
                organization: "acme".into(),
                organization_name: None,
                role: None,
                valid_hours: Some(24),
                id: None,
            }
        );
        assert!(matches!(
            parse(&[
                "invitation",
                "revoke",
                "--organization",
                "acme",
                "--id",
                "x"
            ])
            .unwrap(),
            GatewayCommand::Invitation {
                action: InvitationAction::Revoke,
                ..
            }
        ));
        for invalid in [
            &["invitation"][..],
            &["invitation", "create"],
            &[
                "invitation",
                "create",
                "--organization",
                "a",
                "--valid-hours",
                "0",
            ],
            &[
                "invitation",
                "create",
                "--organization",
                "a",
                "--valid-hours",
                "721",
            ],
            &["invitation", "create", "--organization", "a", "--id", "x"],
            &[
                "invitation",
                "list",
                "--organization",
                "a",
                "--role",
                "owner",
            ],
            &["invitation", "revoke", "--organization", "a"],
            &["invitation", "accept", "--organization", "a"],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn parses_the_agent_create_command() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(
            parse(&[
                "agent",
                "create",
                "--organization",
                "acme",
                "--name",
                "ci",
                "--role",
                "admin",
                "--expires-in-days",
                "30",
            ])
            .unwrap(),
            GatewayCommand::AgentCreate {
                organization: "acme".into(),
                organization_name: None,
                name: "ci".into(),
                role: Some("admin".into()),
                token_name: None,
                expires_in_days: Some(30),
            }
        );
        for invalid in [
            &["agent"][..],
            &["agent", "list", "--organization", "a", "--name", "ci"],
            &["agent", "create", "--organization", "a"],
            &["agent", "create", "--name", "ci"],
            &["agent", "create", "--organization", "a", "--name", ""],
            &[
                "agent",
                "create",
                "--organization",
                "a",
                "--name",
                "ci",
                "--expires-in-days",
                "366",
            ],
            &[
                "agent",
                "create",
                "--organization",
                "a",
                "--name",
                "ci",
                "--name",
                "cd",
            ],
            &[
                "agent",
                "create",
                "--organization",
                "a",
                "--name",
                "ci",
                "--token",
                "x",
            ],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn parses_the_bootstrap_owner_command() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(
            parse(&[
                "bootstrap-owner",
                "--organization",
                "qucelerate",
                "--issuer",
                "https://login.microsoftonline.com/t/v2.0",
                "--subject",
                "abc",
                "--display-name",
                "Riley",
            ])
            .unwrap(),
            GatewayCommand::BootstrapOwner {
                organization: "qucelerate".into(),
                organization_name: None,
                issuer: "https://login.microsoftonline.com/t/v2.0".into(),
                subject: "abc".into(),
                display_name: Some("Riley".into()),
            }
        );
        for invalid in [
            &["bootstrap-owner"][..],
            &["bootstrap-owner", "--organization", "o", "--issuer", "i"],
            &["bootstrap-owner", "--organization", "o", "--subject", "s"],
            &[
                "bootstrap-owner",
                "--organization",
                "o",
                "--issuer",
                "i",
                "--subject",
                "s",
                "--subject",
                "t",
            ],
            &[
                "bootstrap-owner",
                "--organization",
                "o",
                "--issuer",
                "i",
                "--subject",
                " ",
            ],
            &[
                "bootstrap-owner",
                "--organization",
                "o",
                "--issuer",
                "i",
                "--subject",
                "s",
                "--x",
                "y",
            ],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn bootstrap_owner_loads_the_configured_issuer_alone() {
        let env = |name: &str| match name {
            "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
            "GATEWAY_PROFILE" => Ok("api".to_owned()),
            "OIDC_ISSUER" => Ok("https://login.microsoftonline.com/t/v2.0".to_owned()),
            // The rest of the API's OIDC settings are not needed.
            _ => Err(env::VarError::NotPresent),
        };
        let command = GatewayCommand::BootstrapOwner {
            organization: "o".into(),
            organization_name: None,
            issuer: "https://login.microsoftonline.com/t/v2.0".into(),
            subject: "s".into(),
            display_name: None,
        };
        let config = AppConfig::from_env_for_with(command, &env).unwrap();
        assert_eq!(
            config.oidc.issuer.as_deref(),
            Some("https://login.microsoftonline.com/t/v2.0")
        );
    }

    #[test]
    fn parses_config_commands() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(
            parse(&[
                "config",
                "pin",
                "--version",
                "v-id",
                "--organization",
                "org-id"
            ])
            .unwrap(),
            GatewayCommand::Config {
                action: ConfigAction::Pin,
                organization: "org-id".into(),
                version: Some("v-id".into()),
            }
        );
        for (action, expected) in [("unpin", ConfigAction::Unpin), ("list", ConfigAction::List)] {
            assert_eq!(
                parse(&["config", action, "--organization", "org-id"]).unwrap(),
                GatewayCommand::Config {
                    action: expected,
                    organization: "org-id".into(),
                    version: None,
                }
            );
        }
        for invalid in [
            &["config"][..],
            &["config", "rollback", "--organization", "o"],
            &["config", "pin", "--organization", "o"],
            &["config", "unpin", "--organization", "o", "--version", "v"],
            &["config", "list"],
            &[
                "config",
                "list",
                "--organization",
                "o",
                "--organization",
                "p",
            ],
            &["config", "list", "--organization", ""],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn parses_cloud_principal_commands() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(
            parse(&[
                "cloud-principal",
                "allow",
                "--organization",
                "org-id",
                "--azure-tenant",
                "tenant-id",
            ])
            .unwrap(),
            GatewayCommand::CloudPrincipal {
                action: CloudPrincipalAction::Allow,
                organization: "org-id".into(),
                principal: Some(("azure_tenant".into(), "tenant-id".into())),
            }
        );
        assert_eq!(
            parse(&[
                "cloud-principal",
                "revoke",
                "--google-service-account",
                "sa@p-roject.iam.gserviceaccount.com",
                "--organization",
                "org-id",
            ])
            .unwrap(),
            GatewayCommand::CloudPrincipal {
                action: CloudPrincipalAction::Revoke,
                organization: "org-id".into(),
                principal: Some((
                    "google_service_account".into(),
                    "sa@p-roject.iam.gserviceaccount.com".into()
                )),
            }
        );
        assert!(matches!(
            parse(&["cloud-principal", "list", "--organization", "org-id"]).unwrap(),
            GatewayCommand::CloudPrincipal {
                action: CloudPrincipalAction::List,
                principal: None,
                ..
            }
        ));
        for invalid in [
            &["cloud-principal"][..],
            &["cloud-principal", "grant", "--organization", "o"],
            &["cloud-principal", "allow", "--organization", "o"],
            &["cloud-principal", "allow", "--azure-tenant", "t"],
            &[
                "cloud-principal",
                "list",
                "--organization",
                "o",
                "--azure-tenant",
                "t",
            ],
            &[
                "cloud-principal",
                "allow",
                "--organization",
                "o",
                "--azure-tenant",
                "t",
                "--google-service-account",
                "s",
            ],
            &[
                "cloud-principal",
                "allow",
                "--organization",
                "o",
                "--azure-tenant",
            ],
            &[
                "cloud-principal",
                "allow",
                "--organization",
                "o",
                "--other",
                "x",
            ],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn parses_catalog_apply_command_without_accepting_extra_arguments() {
        let command = GatewayCommand::parse([
            "catalog".into(),
            "apply".into(),
            "--organization".into(),
            "00000000-0000-0000-0000-000000000001".into(),
            "--file".into(),
            "manifest.json".into(),
        ])
        .unwrap();
        assert!(matches!(command, GatewayCommand::CatalogApply { .. }));
        assert!(GatewayCommand::parse(["catalog".into(), "apply".into()]).is_err());
    }

    #[test]
    fn worker_config_does_not_require_pepper() {
        let config = AppConfig::from_values_with_providers(
            Some("postgres://gateway_app:secret@localhost/gateway"),
            None,
            Some("worker"),
            None,
            ProviderConfigs {
                openai: None,
                anthropic: None,
                gemini: None,
                bedrock: None,
                azure_openai: None,
                azure_ai_foundry: None,
                openai_compatible: None,
            },
        )
        .unwrap();
        assert!(config.api_key_pepper.is_none());
    }

    #[test]
    fn api_config_requires_pepper() {
        assert!(matches!(
            AppConfig::from_values_with_providers(
                Some("postgres://gateway_app:secret@localhost/gateway"),
                None,
                Some("api"),
                None,
                ProviderConfigs {
                    openai: None,
                    anthropic: None,
                    gemini: None,
                    bedrock: None,
                    azure_openai: None,
                    azure_ai_foundry: None,
                    openai_compatible: None,
                },
            ),
            Err(ConfigError::Missing {
                name: "API_KEY_PEPPER"
            })
        ));
    }

    #[test]
    fn applies_safe_bind_default() {
        let config = AppConfig::from_values_with_providers(
            Some("postgres://gateway_app:secret@localhost/gateway"),
            None,
            Some("api"),
            Some("test-pepper"),
            ProviderConfigs {
                openai: None,
                anthropic: None,
                gemini: None,
                bedrock: None,
                azure_openai: None,
                azure_ai_foundry: None,
                openai_compatible: None,
            },
        )
        .unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:3000".parse().unwrap());
    }

    #[test]
    fn bind_addresses_and_database_hosts_may_be_ipv6() {
        let bind = |value: &'static str| {
            AppConfig::from_env_for_with(GatewayCommand::Run, &move |name: &str| match name {
                "DATABASE_URL" => {
                    Ok("postgres://gateway_app:secret@[fd00:db::5]:5432/gateway".to_owned())
                }
                "GATEWAY_PROFILE" => Ok("worker".to_owned()),
                "BIND_ADDR" => Ok(value.to_owned()),
                _ => Err(env::VarError::NotPresent),
            })
        };
        for value in [
            "[::]:3000",
            "[::1]:3000",
            "[2001:db8::5]:3000",
            "0.0.0.0:3000",
        ] {
            assert_eq!(
                bind(value).unwrap().bind_addr,
                value.parse().unwrap(),
                "{value}"
            );
        }
        for value in ["::3000", "[::]", "::1:3000"] {
            assert!(
                matches!(bind(value), Err(ConfigError::InvalidBindAddr { .. })),
                "{value}"
            );
        }
        assert_eq!(
            redact_database_url("postgres://gateway_app:secret@[fd00:db::5]:5432/gateway"),
            "postgres://REDACTED:REDACTED@[fd00:db::5]:5432/gateway"
        );
    }

    #[test]
    fn database_url_and_pepper_debug_display_are_redacted() {
        let config = AppConfig::from_values_with_providers(
            Some("postgres://alice:super-secret@localhost/gateway"),
            None,
            Some("api"),
            Some("pepper-secret"),
            ProviderConfigs {
                openai: None,
                anthropic: None,
                gemini: None,
                bedrock: None,
                azure_openai: None,
                azure_ai_foundry: None,
                openai_compatible: None,
            },
        )
        .unwrap();
        let debug = format!("{config:?}");
        let display = format!("{config}");
        assert!(!debug.contains("super-secret"));
        assert!(!debug.contains("pepper-secret"));
        assert!(!display.contains("super-secret"));
        assert!(!display.contains("pepper-secret"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn credential_encryption_key_is_optional_validated_and_redacted() {
        use std::collections::HashMap;

        let load = |value: Option<&str>| {
            let values: HashMap<&str, String> = value
                .map(|value| ("GATEWAY_CREDENTIAL_ENCRYPTION_KEY", value.to_owned()))
                .into_iter()
                .collect();
            AppConfig::from_values_with_env(
                Some("postgres://gateway_app:secret@localhost/gateway"),
                None,
                Some("api"),
                Some("test-pepper"),
                true,
                &|name: &str| values.get(name).cloned().ok_or(env::VarError::NotPresent),
            )
        };
        assert!(load(None).unwrap().credential_cipher.is_none());
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([5_u8; 32]);
        let config = load(Some(&key)).unwrap();
        assert!(config.credential_cipher.is_some());
        let debug = format!("{config:?}");
        assert!(debug.contains("credential_encryption_key: \"configured\""));
        assert!(!debug.contains(&key));

        // Worker startup loads the key, to open webhook signing secrets, but
        // still never reads the pepper; migrations read neither.
        let worker_env = |name: &str| match name {
            "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
            "GATEWAY_PROFILE" => Ok("worker".to_owned()),
            "GATEWAY_CREDENTIAL_ENCRYPTION_KEY" => Ok(key.clone()),
            "API_KEY_PEPPER" => panic!("worker startup must not read the pepper"),
            _ => Err(env::VarError::NotPresent),
        };
        let worker = AppConfig::from_env_for_with(GatewayCommand::Run, &worker_env).unwrap();
        assert!(worker.credential_cipher.is_some());
        let migrate = AppConfig::from_env_for_with(GatewayCommand::Migrate, &worker_env).unwrap();
        assert!(migrate.credential_cipher.is_none());
        assert_eq!(
            load(Some("too-short")).unwrap_err(),
            ConfigError::InvalidCredentialKey
        );
    }

    #[test]
    fn worker_loads_provider_settings_for_listing_accounts() {
        let env = |name: &str| match name {
            "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
            "GATEWAY_PROFILE" => Ok("worker".to_owned()),
            "BEDROCK_AWS_REGION" => Ok("us-east-2".to_owned()),
            "BEDROCK_AWS_ACCESS_KEY_ID" => Ok("AKIAEXAMPLE".to_owned()),
            "BEDROCK_AWS_SECRET_ACCESS_KEY" => Ok("secret".to_owned()),
            "BEDROCK_ADAPTER_INSTANCE_KEY" => Ok("bedrock-shared".to_owned()),
            "GATEWAY_ONBOARDING_AWS_PRINCIPAL_ARN" => Ok("not an arn".to_owned()),
            _ => Err(env::VarError::NotPresent),
        };
        let config = AppConfig::from_env_for_with(GatewayCommand::Run, &env).unwrap();
        assert_eq!(
            config
                .bedrock
                .as_ref()
                .map(|bedrock| bedrock.region.as_str()),
            Some("us-east-2")
        );
        assert_eq!(
            config.adapter_instance_key("BEDROCK_ADAPTER_INSTANCE_KEY", "bedrock-main"),
            "bedrock-shared"
        );
        // Onboarding settings are the admin API's and are not checked.
        assert_eq!(
            config.onboarding,
            crate::domain::onboarding::OnboardingIdentities::default()
        );
        let migrate = AppConfig::from_env_for_with(GatewayCommand::Migrate, &env).unwrap();
        assert!(migrate.bedrock.is_none());
    }

    #[test]
    fn worker_loads_price_sheets_without_logging_urls() {
        let env_with = |url: &'static str| {
            move |name: &str| match name {
                "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
                "GATEWAY_PROFILE" => Ok("worker".to_owned()),
                "GATEWAY_PRICE_SHEET_URL_AZURE_OPENAI" => Ok(url.to_owned()),
                "GATEWAY_PRICE_SHEET_URL_OPENAI" => Ok(" ".to_owned()),
                _ => Err(env::VarError::NotPresent),
            }
        };
        let config = AppConfig::from_env_for_with(
            GatewayCommand::Run,
            &env_with("https://prices.example.test/azure.json?token=sheet-secret"),
        )
        .unwrap();
        assert_eq!(config.price_sheets.len(), 1);
        assert_eq!(config.price_sheets[0].0, ProviderFamily::AzureOpenAi);
        let debug = format!("{config:?}");
        assert!(debug.contains("price_sheets: [\"azure-openai\"]"));
        assert!(!debug.contains("sheet-secret"));
        let migrate = AppConfig::from_env_for_with(
            GatewayCommand::Migrate,
            &env_with("https://prices.example.test/azure.json"),
        )
        .unwrap();
        assert!(migrate.price_sheets.is_empty());
        assert_eq!(migrate.public_prices, PublicPrices::default());
        for invalid in ["http://prices.example.test/a.json", "not a url"] {
            assert_eq!(
                AppConfig::from_env_for_with(GatewayCommand::Run, &env_with(invalid)).unwrap_err(),
                ConfigError::InvalidPriceSheetUrl {
                    name: "GATEWAY_PRICE_SHEET_URL_AZURE_OPENAI".into()
                }
            );
        }
    }

    #[test]
    fn credential_kms_is_opt_in_and_needs_region_and_credentials() {
        let load = |vars: &'static [(&'static str, &'static str)]| {
            AppConfig::from_env_for_with(GatewayCommand::Run, &move |name: &str| {
                match name {
                    "DATABASE_URL" => {
                        return Ok("postgres://gateway_app:secret@localhost/gateway".to_owned());
                    }
                    "GATEWAY_PROFILE" => return Ok("worker".to_owned()),
                    _ => {}
                }
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
                    .ok_or(env::VarError::NotPresent)
            })
        };
        assert!(load(&[]).unwrap().credential_kms.is_none());
        let config = load(&[
            ("GATEWAY_CREDENTIAL_KMS_KEY_ID", "alias/gateway"),
            ("AWS_REGION", "eu-west-1"),
            ("AWS_ACCESS_KEY_ID", "AKIDSTANDARD"),
            ("AWS_SECRET_ACCESS_KEY", "standard-secret"),
            ("GATEWAY_CREDENTIAL_KMS_AWS_ACCESS_KEY_ID", "AKIDPREFIXED"),
            (
                "GATEWAY_CREDENTIAL_KMS_AWS_SECRET_ACCESS_KEY",
                "prefixed-secret",
            ),
        ])
        .unwrap();
        let kms = config.credential_kms.as_ref().unwrap();
        assert_eq!(kms.key_id, "alias/gateway");
        assert_eq!(kms.region, "eu-west-1");
        assert_eq!(
            kms.endpoint.as_str(),
            "https://kms.eu-west-1.amazonaws.com/"
        );
        assert_eq!(kms.credentials.access_key_id, "AKIDPREFIXED");
        let debug = format!("{config:?}");
        assert!(debug.contains("credential_kms: Some((\"alias/gateway\", \"eu-west-1\"))"));
        assert!(!debug.contains("prefixed-secret") && !debug.contains("AKIDPREFIXED"));
        let local = load(&[
            ("GATEWAY_CREDENTIAL_KMS_KEY_ID", "k"),
            ("GATEWAY_CREDENTIAL_KMS_REGION", "us-east-1"),
            ("GATEWAY_CREDENTIAL_KMS_ENDPOINT", "http://127.0.0.1:4566/"),
            ("AWS_ACCESS_KEY_ID", "a"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
        ])
        .unwrap();
        assert_eq!(
            local.credential_kms.unwrap().endpoint.as_str(),
            "http://127.0.0.1:4566/"
        );
        for invalid in [
            &[
                ("GATEWAY_CREDENTIAL_KMS_KEY_ID", "k"),
                ("AWS_REGION", "us-east-1"),
            ][..],
            &[
                ("GATEWAY_CREDENTIAL_KMS_KEY_ID", "k"),
                ("AWS_ACCESS_KEY_ID", "a"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
            ][..],
            &[
                ("GATEWAY_CREDENTIAL_KMS_KEY_ID", "k"),
                ("AWS_REGION", "us-east-1"),
                ("AWS_ACCESS_KEY_ID", "a"),
                ("AWS_SECRET_ACCESS_KEY", "s"),
                (
                    "GATEWAY_CREDENTIAL_KMS_ENDPOINT",
                    "http://kms.example.test/",
                ),
            ][..],
        ] {
            let invalid: &'static [(&'static str, &'static str)] = invalid;
            assert_eq!(
                load(invalid).unwrap_err(),
                ConfigError::InvalidCredentialKms
            );
        }
    }

    #[test]
    fn local_accounts_need_the_origin_and_session_secrets_but_no_oidc() {
        let load = |extra: &[(&'static str, &'static str)]| {
            let extra = extra.to_vec();
            AppConfig::from_lookup(GatewayCommand::Run, &move |name: &str| {
                let base = match name {
                    "DATABASE_URL" => Some("postgres://gateway_app:secret@localhost/gateway"),
                    "GATEWAY_PROFILE" => Some("all-in-one"),
                    "API_KEY_PEPPER" => Some("pepper-0000000000000000"),
                    _ => None,
                };
                extra
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| *value)
                    .or(base)
                    .map(str::to_owned)
            })
        };
        let sessions = [
            ("GATEWAY_ORIGIN", "http://localhost:8080"),
            (
                "OIDC_SESSION_HMAC_SECRET",
                "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
            ),
            (
                "OIDC_SESSION_ENCRYPTION_KEY",
                "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY",
            ),
        ];
        // Unset: on when the settings are there, on plain http.
        let config = load(&sessions).unwrap();
        assert!(config.local_accounts.enabled);
        assert!(!config.oidc.enabled);
        assert_eq!(config.oidc.origin.as_deref(), Some("http://localhost:8080"));
        // Organizations' own providers sign in through the shared callback.
        assert_eq!(
            config.oidc.redirect_uri.as_deref(),
            Some("http://localhost:8080/api/admin/v1/oidc/callback")
        );
        assert!(config.sessions.hmac_secret().is_some());
        assert!(!config.local_accounts.trust_forwarded_for);
        // Unset without them: off, as before local accounts existed.
        let config = load(&[]).unwrap();
        assert!(!config.local_accounts.enabled);
        assert!(config.sessions.hmac_secret().is_none());
        // Asked for without them: an error, not a silent off.
        assert_eq!(
            load(&[("GATEWAY_LOCAL_ACCOUNTS", "true")]).unwrap_err(),
            ConfigError::InvalidLocalAccounts
        );
        // Turned off: no sessions at all without OIDC.
        let mut off = sessions.to_vec();
        off.push(("GATEWAY_LOCAL_ACCOUNTS", "false"));
        let config = load(&off).unwrap();
        assert!(!config.local_accounts.enabled);
        assert!(config.sessions.hmac_secret().is_none());
        assert_eq!(
            load(&[("GATEWAY_LOCAL_ACCOUNTS", "yes")]).unwrap_err(),
            ConfigError::InvalidLocalAccounts
        );
        let mut proxied = sessions.to_vec();
        proxied.push(("GATEWAY_TRUST_X_FORWARDED_FOR", "true"));
        assert!(load(&proxied).unwrap().local_accounts.trust_forwarded_for);
    }

    #[test]
    fn local_account_commands_parse() {
        let parse =
            |arguments: &[&str]| GatewayCommand::parse(arguments.iter().map(OsString::from));
        assert_eq!(parse(&["setup-link"]).unwrap(), GatewayCommand::SetupLink);
        assert_eq!(
            parse(&[
                "sign-in-policy",
                "allow-passwords",
                "--organization",
                "acme"
            ])
            .unwrap(),
            GatewayCommand::AllowPasswords {
                organization: "acme".into()
            }
        );
        assert_eq!(
            parse(&["user", "reset-password", "--email", "ada@example.com"]).unwrap(),
            GatewayCommand::UserResetPassword {
                email: "ada@example.com".into()
            }
        );
        assert_eq!(
            parse(&["admin", "grant", "--email", "ada@example.com"]).unwrap(),
            GatewayCommand::AdminGrant {
                email: Some("ada@example.com".into()),
                issuer: None,
                subject: None,
            }
        );
        assert_eq!(
            parse(&[
                "admin",
                "grant",
                "--issuer",
                "https://login.example",
                "--subject",
                "s"
            ])
            .unwrap(),
            GatewayCommand::AdminGrant {
                email: None,
                issuer: Some("https://login.example".into()),
                subject: Some("s".into()),
            }
        );
        for invalid in [
            &["setup-link", "now"][..],
            &["user", "reset-password"],
            &["user", "delete", "--email", "a@b.c"],
            &["user", "reset-password", "--email", "a@b.c", "extra"],
            &["admin", "grant"],
            &["admin", "grant", "--issuer", "https://login.example"],
            &["admin", "grant", "--email", "a@b.c", "--subject", "s"],
            &["admin", "revoke", "--email", "a@b.c"],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn the_model_card_hub_is_an_https_base() {
        let read = |value: &'static str| {
            model_card_hub_url_from_env(&move |name: &str| {
                if name == "GATEWAY_MODEL_CARD_HUB_URL" {
                    Ok(value.to_owned())
                } else {
                    Err(env::VarError::NotPresent)
                }
            })
        };
        assert_eq!(read("").unwrap(), None);
        assert_eq!(
            read("https://huggingface.co").unwrap().unwrap().as_str(),
            "https://huggingface.co/"
        );
        assert_eq!(
            read("https://hub.example/mirror")
                .unwrap()
                .unwrap()
                .as_str(),
            "https://hub.example/mirror/"
        );
        for bad in [
            "http://huggingface.co",
            "https://u:p@hub.example",
            "https://hub.example/?t=1",
            "nonsense",
        ] {
            assert!(read(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn adapter_instance_keys_come_from_the_lookup() {
        let config = AppConfig::from_lookup(GatewayCommand::Run, &|name: &str| {
            match name {
                "DATABASE_URL" => Some("postgres://gateway_app:secret@localhost/gateway"),
                "GATEWAY_PROFILE" => Some("api"),
                "API_KEY_PEPPER" => Some("pepper-0000000000000000"),
                "OPENAI_ADAPTER_INSTANCE_KEY" => Some("openai-eu"),
                "GEMINI_ADAPTER_INSTANCE_KEY" => Some(" gemini spaced "),
                _ => None,
            }
            .map(str::to_owned)
        })
        .unwrap();
        assert_eq!(
            config.adapter_instance_key("OPENAI_ADAPTER_INSTANCE_KEY", "openai-main"),
            "openai-eu"
        );
        // Verbatim: a catalog matches the key exactly.
        assert_eq!(
            config.adapter_instance_key("GEMINI_ADAPTER_INSTANCE_KEY", "gemini-main"),
            " gemini spaced "
        );
        assert_eq!(
            config.adapter_instance_key("BEDROCK_ADAPTER_INSTANCE_KEY", "bedrock-main"),
            "bedrock-main"
        );
    }

    #[test]
    fn metrics_listener_is_opt_in_and_apart_from_the_api() {
        let load = |command: GatewayCommand, metrics: Option<&'static str>| {
            AppConfig::from_env_for_with(command, &move |name: &str| match name {
                "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
                "GATEWAY_PROFILE" => Ok("worker".to_owned()),
                "BIND_ADDR" => Ok("127.0.0.1:3000".to_owned()),
                "GATEWAY_METRICS_ADDR" => {
                    metrics.map(str::to_owned).ok_or(env::VarError::NotPresent)
                }
                _ => Err(env::VarError::NotPresent),
            })
        };
        assert_eq!(load(GatewayCommand::Run, None).unwrap().metrics_addr, None);
        assert_eq!(
            load(GatewayCommand::Run, Some(" ")).unwrap().metrics_addr,
            None
        );
        let config = load(GatewayCommand::Run, Some("127.0.0.1:9464")).unwrap();
        assert_eq!(config.metrics_addr, Some("127.0.0.1:9464".parse().unwrap()));
        assert!(format!("{config:?}").contains("metrics_addr: Some(127.0.0.1:9464)"));
        assert_eq!(
            load(GatewayCommand::Migrate, Some("127.0.0.1:9464"))
                .unwrap()
                .metrics_addr,
            None
        );
        let config = load(GatewayCommand::Run, Some("[::1]:9464")).unwrap();
        assert_eq!(config.metrics_addr, Some("[::1]:9464".parse().unwrap()));
        // `[::]` is dual-stack: it would claim the API's IPv4 port too.
        for invalid in [
            "127.0.0.1:3000",
            "[::]:3000",
            "0.0.0.0:3000",
            "localhost",
            "9464",
            "::1:9464",
        ] {
            assert_eq!(
                load(GatewayCommand::Run, Some(invalid)).unwrap_err(),
                ConfigError::InvalidMetricsAddr {
                    value: invalid.to_owned()
                }
            );
        }
    }

    #[test]
    fn serving_profiles_read_the_tls_and_quic_listeners() {
        let load = |profile: &'static str| {
            AppConfig::from_env_for_with(GatewayCommand::Run, &move |name: &str| match name {
                "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
                "GATEWAY_PROFILE" => Ok(profile.to_owned()),
                "BIND_ADDR" => Ok("127.0.0.1:3000".to_owned()),
                "API_KEY_PEPPER" => Ok("listener-test-pepper-0000000000000000".to_owned()),
                "GATEWAY_HTTP3_ADDR" => Ok("[::]:8443".to_owned()),
                "GATEWAY_TLS_CERT_FILE" => Ok("/tls/tls.crt".to_owned()),
                "GATEWAY_TLS_KEY_FILE" => Ok("/tls/tls.key".to_owned()),
                _ => Err(env::VarError::NotPresent),
            })
        };
        let config = load("api").unwrap();
        assert_eq!(
            config.listeners.http3_addr,
            Some("[::]:8443".parse().unwrap())
        );
        assert_eq!(
            config.listeners.alt_svc.as_deref(),
            Some("h3=\":8443\"; ma=86400")
        );
        // The worker serves no API.
        assert_eq!(
            load("worker").unwrap().listeners,
            crate::listeners::ListenerConfig::default()
        );
    }

    #[test]
    fn worker_fetches_public_prices_unless_disabled() {
        let load = |sources: Option<&'static str>, currencies: Option<&'static str>| {
            AppConfig::from_env_for_with(GatewayCommand::Run, &move |name: &str| match name {
                "DATABASE_URL" => Ok("postgres://gateway_app:secret@localhost/gateway".to_owned()),
                "GATEWAY_PROFILE" => Ok("worker".to_owned()),
                "GATEWAY_PUBLIC_PRICE_SOURCES" => {
                    sources.map(str::to_owned).ok_or(env::VarError::NotPresent)
                }
                "GATEWAY_AZURE_RETAIL_PRICE_CURRENCIES" => currencies
                    .map(str::to_owned)
                    .ok_or(env::VarError::NotPresent),
                _ => Err(env::VarError::NotPresent),
            })
        };
        let both = PublicPrices {
            azure_retail: Some(vec!["USD".into()]),
            aws_price_list: true,
        };
        assert_eq!(load(None, None).unwrap().public_prices, both);
        assert_eq!(load(Some(" "), None).unwrap().public_prices, both);
        assert_eq!(
            load(Some("none"), Some("EUR")).unwrap().public_prices,
            PublicPrices::default()
        );
        assert_eq!(
            load(Some("azure-retail"), Some("usd, EUR,USD"))
                .unwrap()
                .public_prices,
            PublicPrices {
                azure_retail: Some(vec!["USD".into(), "EUR".into()]),
                aws_price_list: false,
            }
        );
        assert_eq!(
            load(Some(" aws-price-list "), None).unwrap().public_prices,
            PublicPrices {
                azure_retail: None,
                aws_price_list: true,
            }
        );
        assert_eq!(
            load(Some("azure-retail,openai"), None).unwrap_err(),
            ConfigError::InvalidPublicPriceSources
        );
        for currencies in ["US", "USD,", "U5D"] {
            assert_eq!(
                load(None, Some(currencies)).unwrap_err(),
                ConfigError::InvalidPriceCurrencies
            );
        }
    }

    #[test]
    fn from_values_loads_and_validates_provider_config_with_controlled_env() {
        use std::collections::HashMap;

        let values = HashMap::from([
            ("OPENAI_API_KEY", "map-openai-key".to_owned()),
            ("OPENAI_BASE_URL", "https://api.example.test/v1".to_owned()),
            ("OPENAI_REQUEST_TIMEOUT_SECONDS", "42".to_owned()),
            ("OPENAI_CONNECT_TIMEOUT_SECONDS", "7".to_owned()),
            ("OPENAI_MAX_RESPONSE_BYTES", "123456".to_owned()),
        ]);
        let read_env = |name: &str| values.get(name).cloned().ok_or(env::VarError::NotPresent);

        let config = AppConfig::from_values_with_env(
            Some("postgres://gateway_app:secret@localhost/gateway"),
            None,
            Some("api"),
            Some("test-pepper"),
            true,
            &read_env,
        )
        .unwrap();
        let openai = config.openai.as_ref().expect("OpenAI config should load");
        assert_eq!(openai.api_key, "map-openai-key");
        assert_eq!(openai.base_url, "https://api.example.test/v1");
        assert_eq!(openai.request_timeout, Duration::from_secs(42));
        assert_eq!(openai.connect_timeout, Duration::from_secs(7));
        assert_eq!(openai.max_response_bytes, 123456);

        let mut invalid_values = values;
        invalid_values.insert("OPENAI_REQUEST_TIMEOUT_SECONDS", "0".to_owned());
        let read_invalid_env = |name: &str| {
            invalid_values
                .get(name)
                .cloned()
                .ok_or(env::VarError::NotPresent)
        };
        assert!(matches!(
            AppConfig::from_values_with_env(
                Some("postgres://gateway_app:secret@localhost/gateway"),
                None,
                Some("api"),
                Some("test-pepper"),
                true,
                &read_invalid_env,
            ),
            Err(ConfigError::InvalidOpenAi)
        ));
    }

    #[test]
    fn openai_compatible_base_url_requires_https_unless_loopback() {
        assert!(validate_openai_compatible_base_url("https://compat.example.com/v1").is_ok());
        assert!(validate_openai_compatible_base_url("http://localhost:8000/v1").is_ok());
        assert!(validate_openai_compatible_base_url("http://127.0.0.1:8000/v1").is_ok());
        // IPv6 loopback is loopback; a public IPv6 address needs https.
        assert!(validate_openai_compatible_base_url("http://[::1]:8000/v1").is_ok());
        assert!(validate_openai_compatible_base_url("http://[2001:db8::1]:8000/v1").is_err());
        assert!(validate_openai_compatible_base_url("https://[2001:db8::1]:8000/v1").is_ok());
        assert!(validate_openai_compatible_base_url("http://compat.example.com/v1").is_err());
    }

    #[test]
    fn openai_compatible_base_url_rejects_userinfo_query_and_fragment() {
        assert!(
            validate_openai_compatible_base_url("https://user:pass@compat.example.com/v1").is_err()
        );
        assert!(validate_openai_compatible_base_url("https://compat.example.com/v1?a=b").is_err());
        assert!(validate_openai_compatible_base_url("https://compat.example.com/v1#frag").is_err());
    }

    #[test]
    fn openai_compatible_extra_headers_parses_valid_object() {
        let headers =
            parse_openai_compatible_extra_headers(r#"{"x-tenant-id":"tenant-1"}"#).unwrap();
        assert_eq!(
            headers,
            vec![("x-tenant-id".to_owned(), "tenant-1".to_owned())]
        );
    }

    #[test]
    fn openai_compatible_extra_headers_rejects_reserved_name_case_insensitively() {
        assert!(matches!(
            parse_openai_compatible_extra_headers(r#"{"Authorization":"Bearer abc"}"#),
            Err(ConfigError::InvalidOpenAiCompatible)
        ));
        assert!(matches!(
            parse_openai_compatible_extra_headers(r#"{"Content-Type":"text/plain"}"#),
            Err(ConfigError::InvalidOpenAiCompatible)
        ));
    }

    #[test]
    fn openai_compatible_extra_headers_rejects_invalid_name_and_value() {
        assert!(matches!(
            parse_openai_compatible_extra_headers(r#"{"bad header":"value"}"#),
            Err(ConfigError::InvalidOpenAiCompatible)
        ));
        assert!(matches!(
            parse_openai_compatible_extra_headers("{\"x-ok\":\"bad\\nvalue\"}"),
            Err(ConfigError::InvalidOpenAiCompatible)
        ));
    }

    #[test]
    fn openai_compatible_extra_headers_rejects_too_many_entries() {
        let entries: Vec<String> = (0..=OPENAI_COMPATIBLE_MAX_EXTRA_HEADERS)
            .map(|index| format!("\"x-header-{index}\":\"value\""))
            .collect();
        let value = format!("{{{}}}", entries.join(","));
        assert!(matches!(
            parse_openai_compatible_extra_headers(&value),
            Err(ConfigError::InvalidOpenAiCompatible)
        ));
    }

    /// Which secret values decode as unpadded base64url and which are taken
    /// as raw bytes. Recorded before the base64 0.23 upgrade: a change here
    /// would silently change a configured key.
    #[test]
    fn secret_values_decode_as_unpadded_base64url_or_stay_raw() {
        let read = |value: &'static str| {
            secret_bytes(&|_: &str| Ok(value.to_owned()), "SECRET", 1).unwrap()
        };
        let key: Vec<u8> = (0..32).collect();
        assert_eq!(read("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"), key);
        assert_eq!(read("-_-_"), [0xfb, 0xff, 0xbf]);
        assert_eq!(read("AA"), [0]);
        for raw in [
            // Padding, the standard alphabet, non-zero trailing bits, an
            // impossible length, and characters outside any alphabet.
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
            "AA==",
            "+/+/",
            "AB",
            "AAAAA",
            "not base64!",
        ] {
            assert_eq!(read(raw), raw.as_bytes(), "{raw}");
        }
        assert_eq!(
            secret_bytes_exact(
                &|_: &str| Ok("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned()),
                "SECRET",
                32
            )
            .unwrap(),
            key
        );
    }
}
