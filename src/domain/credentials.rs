//! Provider credentials stored encrypted in the database.
//!
//! A credential is sealed with AES-256-GCM under a key derived from the
//! active key of a keyring: `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`, or a data
//! key that AWS KMS wraps (`application::credential_keys`). Older keys stay
//! in the keyring to open what they sealed until it is re-sealed under the
//! active key. The associated data binds each
//! ciphertext to its organization, adapter instance, and provider family, so a
//! row copied to another tenant or provider account fails to decrypt. Secrets
//! are held in zeroizing buffers, never implement a revealing `Debug`, and are
//! never serialized into responses, audit, or logs.

use std::{
    fmt,
    sync::{Arc, RwLock},
};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, Payload},
};
use chrono::{DateTime, Utc};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use super::{catalog::ProviderFamily, identity::OrganizationId};

pub const MAX_ADAPTER_INSTANCE_KEY_LENGTH: usize = 128;
const MAX_SECRET_BYTES: usize = 8192;
const MAX_ACCESS_KEY_ID_BYTES: usize = 128;
const NONCE_BYTES: usize = 12;
const AAD_CONTEXT: &str = "gateway.provider_credential.v1";
const FINGERPRINT_CONTEXT: &str = "gateway.provider_credential_request.v1";
const WEBHOOK_AAD_CONTEXT: &str = "gateway.webhook_secret.v1";
const IDENTITY_PROVIDER_AAD_CONTEXT: &str = "gateway.identity_provider_secret.v1";
const TRACE_CONTENT_KEY_AAD_CONTEXT: &str = "gateway.trace_content_key.v1";
const IDENTITY_PROVIDER_FINGERPRINT_CONTEXT: &str = "gateway.identity_provider_request.v1";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CredentialError {
    #[error("credential encryption key must be 32 bytes")]
    InvalidKeyLength,
    #[error("credential encryption failed")]
    Encryption,
    #[error("credential decryption failed")]
    Decryption,
    #[error("credential was sealed with a different key")]
    UnknownKey,
    #[error("{0}")]
    Invalid(&'static str),
}

/// Identity a ciphertext is bound to. Changing any field makes decryption fail.
#[derive(Clone, Copy, Debug)]
pub struct CredentialBinding<'a> {
    pub organization_id: OrganizationId,
    pub adapter_instance_key: &'a str,
    pub family: ProviderFamily,
}

impl CredentialBinding<'_> {
    fn aad(&self) -> Vec<u8> {
        // Components are NUL-separated; none of them can contain NUL.
        [
            AAD_CONTEXT,
            &self.organization_id.0.to_string(),
            self.adapter_instance_key,
            self.family.as_str(),
        ]
        .join("\0")
        .into_bytes()
    }
}

/// A provider secret. Which variant a family accepts is fixed by
/// [`ProviderSecret::for_family`].
pub enum ProviderSecret {
    ApiKey {
        api_key: Zeroizing<String>,
    },
    AwsSigV4 {
        access_key_id: Zeroizing<String>,
        secret_access_key: Zeroizing<String>,
        session_token: Option<Zeroizing<String>>,
    },
    /// An IAM role in the tenant's account that the gateway's own AWS
    /// identity assumes. The external ID its trust policy requires is the
    /// organization's, assigned by the gateway ([`aws_external_id`]), so no
    /// organization can assume a role that trusts another's. Not a secret,
    /// but stored like one.
    AwsAssumeRole {
        role_arn: String,
    },
    /// A Microsoft Entra directory that the gateway's own Entra identity
    /// signs in to: as the gateway's multi-tenant app, or as the tenant's
    /// app `client_id` that trusts the gateway through a federated identity
    /// credential. `endpoint` is the tenant's Azure resource, else the
    /// process's; `resource_id` is the same resource in Azure Resource
    /// Manager, which lists its deployments (Reader on it is enough).
    AzureEntra {
        tenant_id: String,
        client_id: Option<String>,
        endpoint: Option<String>,
        resource_id: Option<String>,
    },
    /// A Google service account in the tenant's project that the gateway's
    /// own Google identity impersonates, and the project the Gemini API
    /// bills (else the service account's).
    GoogleServiceAccount {
        service_account: String,
        project_id: Option<String>,
        /// The Agent Platform location; else the process's.
        location: Option<String>,
    },
    /// A tenant's own OpenAI-compatible endpoint (OpenAI-compatible family
    /// only): its base URL, which request paths such as `chat/completions`
    /// are joined to, and the key it takes, if any. Whether the gateway may
    /// reach it is decided on every connection, not here: public HTTPS, or
    /// an origin the operator allows.
    OpenAiCompatibleEndpoint {
        base_url: String,
        api_key: Option<Zeroizing<String>>,
    },
}

impl fmt::Debug for ProviderSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { .. } => f.write_str("ProviderSecret::ApiKey(REDACTED)"),
            Self::AwsSigV4 { .. } => f.write_str("ProviderSecret::AwsSigV4(REDACTED)"),
            Self::AwsAssumeRole { .. } => f.write_str("ProviderSecret::AwsAssumeRole(REDACTED)"),
            Self::AzureEntra { .. } => f.write_str("ProviderSecret::AzureEntra(REDACTED)"),
            Self::GoogleServiceAccount { .. } => {
                f.write_str("ProviderSecret::GoogleServiceAccount(REDACTED)")
            }
            Self::OpenAiCompatibleEndpoint { .. } => {
                f.write_str("ProviderSecret::OpenAiCompatibleEndpoint(REDACTED)")
            }
        }
    }
}

/// Client-supplied secret fields. Unknown fields are rejected so a request can
/// never smuggle endpoints or other configuration through the credential API.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSecretInput {
    pub api_key: Option<Zeroizing<String>>,
    pub access_key_id: Option<Zeroizing<String>>,
    pub secret_access_key: Option<Zeroizing<String>>,
    pub session_token: Option<Zeroizing<String>>,
    /// Instead of keys: the IAM role to assume (Bedrock only).
    pub role_arn: Option<String>,
    pub external_id: Option<String>,
    /// Instead of a key: the Entra directory to sign in to (Azure OpenAI
    /// and Azure AI Foundry only), the tenant's app that trusts the
    /// gateway, and the tenant's resource endpoint.
    pub tenant_id: Option<String>,
    pub client_id: Option<String>,
    /// Also, for the OpenAI-compatible family, the endpoint's base URL (with
    /// an optional api_key).
    pub endpoint: Option<String>,
    /// The resource's Azure Resource Manager ID, so discovery can list its
    /// deployments (with tenant_id only).
    pub resource_id: Option<String>,
    /// Instead of a key: the Google service account to impersonate (Gemini
    /// only), and the project and location where Gemini Enterprise Agent
    /// Platform serves it.
    pub service_account: Option<String>,
    pub project_id: Option<String>,
    pub location: Option<String>,
}

impl fmt::Debug for ProviderSecretInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderSecretInput(REDACTED)")
    }
}

impl ProviderSecret {
    /// The cloud principal that trusts the gateway's identity and that this
    /// credential would have it act as, which must be bound to the
    /// organization before it is used. AWS roles need none: the gateway's
    /// external ID ties them to one organization.
    pub fn cloud_principal(&self) -> Option<CloudPrincipal> {
        match self {
            Self::AzureEntra { tenant_id, .. } => {
                Some(CloudPrincipal::AzureTenant(tenant_id.clone()))
            }
            Self::GoogleServiceAccount {
                service_account, ..
            } => Some(CloudPrincipal::GoogleServiceAccount(
                service_account.clone(),
            )),
            Self::ApiKey { .. }
            | Self::AwsSigV4 { .. }
            | Self::AwsAssumeRole { .. }
            | Self::OpenAiCompatibleEndpoint { .. } => None,
        }
    }

    /// Bedrock signs requests with AWS credentials; the Azure families take
    /// an API key or an Entra directory; Gemini an API key or a Google
    /// service account; every other family authenticates with a single API
    /// key.
    pub fn for_family(
        family: ProviderFamily,
        input: ProviderSecretInput,
    ) -> Result<Self, CredentialError> {
        let gemini = family == ProviderFamily::Gemini;
        if !gemini
            && (input.service_account.is_some()
                || input.project_id.is_some()
                || input.location.is_some())
        {
            return Err(CredentialError::Invalid(
                "service_account, project_id and location are only for gemini",
            ));
        }
        if gemini && let Some(service_account) = input.service_account {
            if input.api_key.is_some()
                || input.access_key_id.is_some()
                || input.secret_access_key.is_some()
                || input.session_token.is_some()
                || input.role_arn.is_some()
                || input.external_id.is_some()
                || input.tenant_id.is_some()
                || input.client_id.is_some()
                || input.endpoint.is_some()
            {
                return Err(CredentialError::Invalid(
                    "gemini credentials use either api_key or service_account, not both",
                ));
            }
            let service_account = service_account.to_ascii_lowercase();
            if !crate::gcp_credentials::service_account_email(&service_account) {
                return Err(CredentialError::Invalid(
                    "service_account must be a service account address: <name>@<project>.iam.gserviceaccount.com",
                ));
            }
            match &input.project_id {
                Some(project_id) => validate_project_id(project_id)?,
                // Agent Platform's URL names a project: a default service
                // account's address names none.
                None if crate::gcp_credentials::service_account_project(&service_account)
                    .is_none() =>
                {
                    return Err(CredentialError::Invalid(
                        "project_id is required for a default service account",
                    ));
                }
                None => {}
            }
            if let Some(location) = &input.location {
                validate_location(location)?;
            }
            return Ok(Self::GoogleServiceAccount {
                service_account,
                project_id: input.project_id,
                location: input.location,
            });
        }
        if input.project_id.is_some() || input.location.is_some() {
            return Err(CredentialError::Invalid(
                "project_id and location require service_account",
            ));
        }
        if family == ProviderFamily::OpenAiCompatible
            && let Some(endpoint) = input.endpoint
        {
            if input.access_key_id.is_some()
                || input.secret_access_key.is_some()
                || input.session_token.is_some()
                || input.role_arn.is_some()
                || input.external_id.is_some()
                || input.tenant_id.is_some()
                || input.client_id.is_some()
                || input.resource_id.is_some()
            {
                return Err(CredentialError::Invalid(
                    "an openai-compatible endpoint takes only endpoint and api_key",
                ));
            }
            if let Some(api_key) = &input.api_key {
                validate_secret(api_key, "api_key")?;
            }
            return Ok(Self::OpenAiCompatibleEndpoint {
                base_url: openai_compatible_base_url(&endpoint)?,
                api_key: input.api_key,
            });
        }
        let azure = matches!(
            family,
            ProviderFamily::AzureOpenAi | ProviderFamily::AzureAiFoundry
        );
        if !azure
            && (input.tenant_id.is_some()
                || input.client_id.is_some()
                || input.endpoint.is_some()
                || input.resource_id.is_some())
        {
            return Err(CredentialError::Invalid(
                "tenant_id, client_id, endpoint and resource_id are only for the Azure families",
            ));
        }
        if azure && let Some(tenant_id) = input.tenant_id {
            if input.api_key.is_some()
                || input.access_key_id.is_some()
                || input.secret_access_key.is_some()
                || input.session_token.is_some()
                || input.role_arn.is_some()
                || input.external_id.is_some()
            {
                return Err(CredentialError::Invalid(
                    "azure credentials use either api_key or tenant_id, not both",
                ));
            }
            validate_directory_id(&tenant_id, "tenant_id")?;
            if let Some(client_id) = &input.client_id {
                validate_directory_id(client_id, "client_id")?;
            }
            let endpoint = input
                .endpoint
                .map(|endpoint| azure_endpoint(family, &endpoint))
                .transpose()?;
            if let Some(resource_id) = &input.resource_id
                && !crate::domain::onboarding::azure_resource_id(resource_id)
            {
                return Err(CredentialError::Invalid(
                    "resource_id must be /subscriptions/<id>/resourceGroups/<group>/providers/Microsoft.CognitiveServices/accounts/<name>",
                ));
            }
            return Ok(Self::AzureEntra {
                tenant_id: tenant_id.to_ascii_lowercase(),
                client_id: input.client_id.map(|id| id.to_ascii_lowercase()),
                endpoint,
                resource_id: input.resource_id,
            });
        }
        if input.client_id.is_some() || input.endpoint.is_some() || input.resource_id.is_some() {
            return Err(CredentialError::Invalid(
                "client_id, endpoint and resource_id require tenant_id",
            ));
        }
        match family {
            ProviderFamily::Bedrock => {
                if input.api_key.is_some() {
                    return Err(CredentialError::Invalid(
                        "bedrock credentials use access_key_id and secret_access_key, not api_key",
                    ));
                }
                if input.external_id.is_some() {
                    return Err(CredentialError::Invalid(
                        "external_id is assigned by the gateway: see cloud-trust",
                    ));
                }
                if let Some(role_arn) = input.role_arn {
                    if input.access_key_id.is_some()
                        || input.secret_access_key.is_some()
                        || input.session_token.is_some()
                    {
                        return Err(CredentialError::Invalid(
                            "bedrock credentials use either role_arn or access keys, not both",
                        ));
                    }
                    validate_role_arn(&role_arn)?;
                    return Ok(Self::AwsAssumeRole { role_arn });
                }
                let access_key_id = input.access_key_id.ok_or(CredentialError::Invalid(
                    "access_key_id is required for bedrock",
                ))?;
                let secret_access_key = input.secret_access_key.ok_or(CredentialError::Invalid(
                    "secret_access_key is required for bedrock",
                ))?;
                if access_key_id.is_empty()
                    || access_key_id.len() > MAX_ACCESS_KEY_ID_BYTES
                    || !access_key_id.bytes().all(|b| b.is_ascii_alphanumeric())
                {
                    return Err(CredentialError::Invalid(
                        "access_key_id must be 1-128 ASCII letters or digits",
                    ));
                }
                validate_secret(&secret_access_key, "secret_access_key")?;
                if let Some(token) = &input.session_token {
                    validate_secret(token, "session_token")?;
                }
                Ok(Self::AwsSigV4 {
                    access_key_id,
                    secret_access_key,
                    session_token: input.session_token,
                })
            }
            _ => {
                if input.access_key_id.is_some()
                    || input.secret_access_key.is_some()
                    || input.session_token.is_some()
                    || input.role_arn.is_some()
                    || input.external_id.is_some()
                {
                    return Err(CredentialError::Invalid(
                        "this provider family accepts only api_key",
                    ));
                }
                let api_key = input
                    .api_key
                    .ok_or(CredentialError::Invalid("api_key is required"))?;
                validate_secret(&api_key, "api_key")?;
                Ok(Self::ApiKey { api_key })
            }
        }
    }

    fn to_plaintext(&self) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
        let payload = match self {
            Self::ApiKey { api_key } => SecretPayload::ApiKey {
                api_key: api_key.as_str(),
            },
            Self::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => SecretPayload::AwsSigV4 {
                access_key_id: access_key_id.as_str(),
                secret_access_key: secret_access_key.as_str(),
                session_token: session_token.as_deref().map(String::as_str),
            },
            Self::AwsAssumeRole { role_arn } => SecretPayload::AwsAssumeRole { role_arn },
            Self::AzureEntra {
                tenant_id,
                client_id,
                endpoint,
                resource_id,
            } => SecretPayload::AzureEntra {
                tenant_id,
                client_id: client_id.as_deref(),
                endpoint: endpoint.as_deref(),
                resource_id: resource_id.as_deref(),
            },
            Self::GoogleServiceAccount {
                service_account,
                project_id,
                location,
            } => SecretPayload::GoogleServiceAccount {
                service_account,
                project_id: project_id.as_deref(),
                location: location.as_deref(),
            },
            Self::OpenAiCompatibleEndpoint { base_url, api_key } => {
                SecretPayload::OpenAiCompatibleEndpoint {
                    base_url,
                    api_key: api_key.as_deref().map(String::as_str),
                }
            }
        };
        serde_json::to_vec(&payload)
            .map(Zeroizing::new)
            .map_err(|_| CredentialError::Encryption)
    }

    fn from_plaintext(plaintext: &[u8]) -> Result<Self, CredentialError> {
        let payload: OwnedSecretPayload =
            serde_json::from_slice(plaintext).map_err(|_| CredentialError::Decryption)?;
        Ok(match payload {
            OwnedSecretPayload::ApiKey { api_key } => Self::ApiKey { api_key },
            OwnedSecretPayload::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => Self::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            },
            // An external ID stored before the gateway assigned them is
            // never used.
            OwnedSecretPayload::AwsAssumeRole { role_arn, .. } => Self::AwsAssumeRole { role_arn },
            OwnedSecretPayload::AzureEntra {
                tenant_id,
                client_id,
                endpoint,
                resource_id,
            } => Self::AzureEntra {
                tenant_id,
                client_id,
                endpoint,
                resource_id,
            },
            OwnedSecretPayload::GoogleServiceAccount {
                service_account,
                project_id,
                location,
            } => Self::GoogleServiceAccount {
                service_account,
                project_id,
                location,
            },
            OwnedSecretPayload::OpenAiCompatibleEndpoint { base_url, api_key } => {
                Self::OpenAiCompatibleEndpoint { base_url, api_key }
            }
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SecretPayload<'a> {
    ApiKey {
        api_key: &'a str,
    },
    AwsSigV4 {
        access_key_id: &'a str,
        secret_access_key: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_token: Option<&'a str>,
    },
    AwsAssumeRole {
        role_arn: &'a str,
    },
    AzureEntra {
        tenant_id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        endpoint: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resource_id: Option<&'a str>,
    },
    GoogleServiceAccount {
        service_account: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<&'a str>,
    },
    OpenAiCompatibleEndpoint {
        base_url: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key: Option<&'a str>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OwnedSecretPayload {
    ApiKey {
        api_key: Zeroizing<String>,
    },
    AwsSigV4 {
        access_key_id: Zeroizing<String>,
        secret_access_key: Zeroizing<String>,
        session_token: Option<Zeroizing<String>>,
    },
    AwsAssumeRole {
        role_arn: String,
        #[allow(dead_code)]
        external_id: Option<String>,
    },
    AzureEntra {
        tenant_id: String,
        client_id: Option<String>,
        endpoint: Option<String>,
        // Credentials stored before discovery read Resource Manager have
        // none.
        #[serde(default)]
        resource_id: Option<String>,
    },
    GoogleServiceAccount {
        service_account: String,
        project_id: Option<String>,
        #[serde(default)]
        location: Option<String>,
    },
    OpenAiCompatibleEndpoint {
        base_url: String,
        #[serde(default)]
        api_key: Option<Zeroizing<String>>,
    },
}

/// Longest base URL an OpenAI-compatible endpoint may have.
const MAX_BASE_URL_BYTES: usize = 2048;

/// An OpenAI-compatible endpoint's base URL as stored: `http` or `https`,
/// with a host, no user name, password, query or fragment, and no trailing
/// slash. Request paths such as `chat/completions` are joined to it, so it
/// usually ends in `/v1`. Which hosts and schemes the gateway may actually
/// reach is the egress policy's to decide.
pub fn openai_compatible_base_url(value: &str) -> Result<String, CredentialError> {
    const INVALID: CredentialError = CredentialError::Invalid(
        "endpoint must be an http or https URL with a host and no credentials, query or \
         fragment, such as https://models.example.com/v1",
    );
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_BASE_URL_BYTES {
        return Err(INVALID);
    }
    let url = url::Url::parse(value).map_err(|_| INVALID)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(INVALID);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The external ID the gateway presents when it assumes an organization's
/// AWS role. The gateway sets it from the organization it acts for, never
/// from the credential, so a role whose trust policy requires it can be
/// assumed only for that organization.
pub fn aws_external_id(organization_id: OrganizationId) -> String {
    format!("qucelerate-{}", organization_id.0)
}

/// The session name of roles the gateway assumes for an organization, so
/// CloudTrail in the tenant's account shows which it acted for.
pub fn aws_role_session_name(organization_id: OrganizationId) -> String {
    format!("qucelerate-{}", organization_id.0.simple())
}

/// A cloud identity that trusts the gateway's own: anyone who names it
/// could have the gateway act as it, so an operator binds it to the
/// organizations that may use it ([`ProviderSecret::cloud_principal`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CloudPrincipal {
    /// A Microsoft Entra directory, by tenant ID.
    AzureTenant(String),
    /// A Google service account, by address.
    GoogleServiceAccount(String),
}

impl CloudPrincipal {
    /// A principal as an operator names it, validated and normalized.
    pub fn parse(kind: &str, value: &str) -> Result<Self, CredentialError> {
        let value = value.trim().to_ascii_lowercase();
        match kind {
            "azure_tenant" => {
                validate_directory_id(&value, "tenant_id")?;
                Ok(Self::AzureTenant(value))
            }
            "google_service_account" => {
                if !crate::gcp_credentials::service_account_email(&value) {
                    return Err(CredentialError::Invalid(
                        "service_account must be a service account address: <name>@<project>.iam.gserviceaccount.com",
                    ));
                }
                Ok(Self::GoogleServiceAccount(value))
            }
            _ => Err(CredentialError::Invalid(
                "kind must be azure_tenant or google_service_account",
            )),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::AzureTenant(_) => "azure_tenant",
            Self::GoogleServiceAccount(_) => "google_service_account",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            Self::AzureTenant(value) | Self::GoogleServiceAccount(value) => value,
        }
    }

    /// What a credential row stores to find its binding: SHA-256 of the
    /// kind and value, so the row holds no identifier in the clear.
    pub fn digest(&self) -> Vec<u8> {
        use sha2::Digest as _;
        let mut hash = Sha256::new();
        hash.update(b"qucelerate cloud principal v1\0");
        hash.update(self.kind().as_bytes());
        hash.update(b"\0");
        hash.update(self.value().as_bytes());
        hash.finalize().to_vec()
    }
}

/// Whether an operator has bound a credential's cloud principal to its
/// organization.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudPrincipalStatus {
    Approved,
    Pending,
}

/// A Google Cloud project ID: 6-30 lowercase letters, digits and hyphens,
/// starting with a letter and not ending with a hyphen.
fn validate_project_id(value: &str) -> Result<(), CredentialError> {
    let valid = (6..=30).contains(&value.len())
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(CredentialError::Invalid(
            "project_id must be a Google Cloud project ID: 6-30 lowercase letters, digits and hyphens",
        ));
    }
    Ok(())
}

/// A Google Cloud location name: `global`, `us-central1`, `eu`...
fn validate_location(value: &str) -> Result<(), CredentialError> {
    let valid = (2..=32).contains(&value.len())
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(CredentialError::Invalid(
            "location must be a Google Cloud location: global, us-central1, eu...",
        ));
    }
    Ok(())
}

/// A directory (tenant) or application (client) ID: a GUID.
fn validate_directory_id(value: &str, field: &'static str) -> Result<(), CredentialError> {
    let groups: Vec<&str> = value.split('-').collect();
    let guid = groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|group| group.bytes().all(|b| b.is_ascii_hexdigit()));
    if !guid {
        return Err(CredentialError::Invalid(match field {
            "tenant_id" => "tenant_id must be a directory ID (a GUID)",
            _ => "client_id must be an application (client) ID (a GUID)",
        }));
    }
    Ok(())
}

/// The tenant's Azure resource: `https://<resource>.<service host>` and
/// nothing else, on a host that only Azure serves for the family.
fn azure_endpoint(family: ProviderFamily, value: &str) -> Result<String, CredentialError> {
    let invalid = || {
        CredentialError::Invalid(match family {
            ProviderFamily::AzureOpenAi => {
                "endpoint must be https://<resource>.openai.azure.com or https://<resource>.cognitiveservices.azure.com (or their Azure Government or China host)"
            }
            _ => {
                "endpoint must be https://<resource>.services.ai.azure.com or https://<resource>.cognitiveservices.azure.com (or their Azure Government or China host)"
            }
        })
    };
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    // The public cloud, Azure Government, and Azure operated by 21Vianet.
    let suffixes: &[&str] = match family {
        ProviderFamily::AzureOpenAi => &[
            ".openai.azure.com",
            ".cognitiveservices.azure.com",
            ".openai.azure.us",
            ".cognitiveservices.azure.us",
            ".openai.azure.cn",
            ".cognitiveservices.azure.cn",
        ],
        _ => &[
            ".services.ai.azure.com",
            ".cognitiveservices.azure.com",
            ".services.ai.azure.us",
            ".cognitiveservices.azure.us",
            ".services.ai.azure.cn",
            ".cognitiveservices.azure.cn",
        ],
    };
    let host = url.host_str().ok_or_else(invalid)?;
    let resource = suffixes
        .iter()
        .find_map(|suffix| host.strip_suffix(suffix))
        .ok_or_else(invalid)?;
    let resource_ok = (2..=64).contains(&resource.len())
        && resource
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !resource.starts_with('-');
    if url.scheme() != "https"
        || !resource_ok
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(format!("https://{host}"))
}

/// `arn:{partition}:iam::{12-digit account}:role/{path/}{name}`, as IAM allows.
fn validate_role_arn(value: &str) -> Result<(), CredentialError> {
    let invalid = || {
        CredentialError::Invalid(
            "role_arn must be an IAM role ARN: arn:aws:iam::<12-digit account>:role/<name>",
        )
    };
    let mut parts = value.splitn(6, ':');
    let (Some("arn"), Some(partition), Some("iam"), Some(""), Some(account), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(invalid());
    };
    let name = resource.strip_prefix("role/").ok_or_else(invalid)?;
    let name_ok = !name.is_empty()
        && name.len() <= 512
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+=,.@_-/".contains(&b));
    if !matches!(partition, "aws" | "aws-cn" | "aws-us-gov")
        || account.len() != 12
        || !account.bytes().all(|b| b.is_ascii_digit())
        || !name_ok
        || value.len() > 2048
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_secret(value: &str, field: &'static str) -> Result<(), CredentialError> {
    let invalid = match field {
        "api_key" => "api_key must be 1-8192 bytes without whitespace or control characters",
        "secret_access_key" => {
            "secret_access_key must be 1-8192 bytes without whitespace or control characters"
        }
        _ => "session_token must be 1-8192 bytes without whitespace or control characters",
    };
    if value.is_empty()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(CredentialError::Invalid(invalid));
    }
    Ok(())
}

/// Adapter instance keys follow the catalog manifest rule (non-blank, at most
/// 128 bytes) and additionally exclude control characters, which keeps the
/// NUL-separated AAD unambiguous.
pub fn validate_adapter_instance_key(value: &str) -> Result<(), CredentialError> {
    if value.trim().is_empty()
        || value.len() > MAX_ADAPTER_INSTANCE_KEY_LENGTH
        || value.chars().any(char::is_control)
    {
        return Err(CredentialError::Invalid(
            "adapter_instance_key must be 1-128 bytes without control characters",
        ));
    }
    Ok(())
}

fn webhook_aad(organization_id: OrganizationId, endpoint_id: uuid::Uuid) -> Vec<u8> {
    [
        WEBHOOK_AAD_CONTEXT,
        &organization_id.0.to_string(),
        &endpoint_id.to_string(),
    ]
    .join("\0")
    .into_bytes()
}

fn trace_content_key_aad(organization_id: OrganizationId, trace_id: uuid::Uuid) -> Vec<u8> {
    [
        TRACE_CONTENT_KEY_AAD_CONTEXT,
        &organization_id.0.to_string(),
        &trace_id.to_string(),
    ]
    .join("\0")
    .into_bytes()
}

fn identity_provider_aad(organization_id: OrganizationId, configuration_id: uuid::Uuid) -> Vec<u8> {
    [
        IDENTITY_PROVIDER_AAD_CONTEXT,
        &organization_id.0.to_string(),
        &configuration_id.to_string(),
    ]
    .join("\0")
    .into_bytes()
}

/// A sealed credential ready for storage.
pub struct SealedCredential {
    pub key_id: String,
    pub ciphertext: Vec<u8>,
}

/// One key of the keyring. The key material is never used directly: HKDF
/// derives an encryption key, a fingerprint key for idempotency, and a
/// public key id.
struct KeyMaterial {
    encryption_key: Zeroizing<[u8; 32]>,
    fingerprint_key: Zeroizing<[u8; 32]>,
    key_id: String,
}

impl KeyMaterial {
    fn derive(key: &[u8]) -> Result<Self, CredentialError> {
        if key.len() != 32 {
            return Err(CredentialError::InvalidKeyLength);
        }
        let hkdf = Hkdf::<Sha256>::new(Some(b"gateway provider credentials"), key);
        let mut encryption_key = Zeroizing::new([0_u8; 32]);
        let mut fingerprint_key = Zeroizing::new([0_u8; 32]);
        let mut key_id = [0_u8; 8];
        hkdf.expand(b"encryption v1", encryption_key.as_mut())
            .and_then(|()| hkdf.expand(b"fingerprint v1", fingerprint_key.as_mut()))
            .and_then(|()| hkdf.expand(b"key id v1", &mut key_id))
            .map_err(|_| CredentialError::InvalidKeyLength)?;
        Ok(Self {
            encryption_key,
            fingerprint_key,
            key_id: key_id.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }
}

struct Keyring {
    active: Arc<KeyMaterial>,
    /// Keys that may still open what they sealed.
    previous: Vec<Arc<KeyMaterial>>,
}

/// AES-256-GCM sealing for provider credentials and webhook secrets. New
/// secrets are sealed under the active key; a secret sealed under any key
/// of the keyring opens. Clones share the keyring, so [`install`] reaches
/// every holder.
///
/// [`install`]: CredentialCipher::install
#[derive(Clone)]
pub struct CredentialCipher {
    keyring: Arc<RwLock<Keyring>>,
}

impl fmt::Debug for CredentialCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialCipher")
            .field("key_id", &self.key_id())
            .finish_non_exhaustive()
    }
}

impl CredentialCipher {
    /// A keyring of one key.
    pub fn new(key: &[u8]) -> Result<Self, CredentialError> {
        Self::with_keys(key, &[])
    }

    /// A keyring that seals under `active` and also opens what `previous`
    /// sealed.
    pub fn with_keys(active: &[u8], previous: &[&[u8]]) -> Result<Self, CredentialError> {
        Ok(Self {
            keyring: Arc::new(RwLock::new(Self::keyring(active, previous, None)?)),
        })
    }

    fn keyring(
        active: &[u8],
        previous: &[&[u8]],
        inherit: Option<&CredentialCipher>,
    ) -> Result<Keyring, CredentialError> {
        let active = Arc::new(KeyMaterial::derive(active)?);
        let mut ids = vec![active.key_id.clone()];
        let mut kept = Vec::new();
        let inherited = inherit.map(|cipher| {
            let keyring = cipher.read();
            std::iter::once(keyring.active.clone())
                .chain(keyring.previous.iter().cloned())
                .collect::<Vec<_>>()
        });
        let derived = previous
            .iter()
            .map(|key| KeyMaterial::derive(key).map(Arc::new))
            .collect::<Result<Vec<_>, _>>()?;
        for key in derived.into_iter().chain(inherited.into_iter().flatten()) {
            if !ids.contains(&key.key_id) {
                ids.push(key.key_id.clone());
                kept.push(key);
            }
        }
        Ok(Keyring {
            active,
            previous: kept,
        })
    }

    /// Replaces the keys of this cipher and every clone of it. The keys of
    /// `inherit` (another keyring, such as the one of
    /// `GATEWAY_CREDENTIAL_ENCRYPTION_KEY`) stay able to open what they
    /// sealed.
    pub fn install(
        &self,
        active: &[u8],
        previous: &[&[u8]],
        inherit: Option<&CredentialCipher>,
    ) -> Result<(), CredentialError> {
        let keyring = Self::keyring(active, previous, inherit)?;
        *self
            .keyring
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = keyring;
        Ok(())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Keyring> {
        self.keyring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn active(&self) -> Arc<KeyMaterial> {
        self.read().active.clone()
    }

    fn find(&self, key_id: &str) -> Option<Arc<KeyMaterial>> {
        let keyring = self.read();
        std::iter::once(&keyring.active)
            .chain(&keyring.previous)
            .find(|key| key.key_id == key_id)
            .cloned()
    }

    /// The id of the key new secrets are sealed under.
    pub fn key_id(&self) -> String {
        self.read().active.key_id.clone()
    }

    /// The ids of every key that can open a secret.
    pub fn key_ids(&self) -> Vec<String> {
        let keyring = self.read();
        std::iter::once(&keyring.active)
            .chain(&keyring.previous)
            .map(|key| key.key_id.clone())
            .collect()
    }

    pub fn seal(
        &self,
        binding: CredentialBinding<'_>,
        secret: &ProviderSecret,
    ) -> Result<SealedCredential, CredentialError> {
        self.encrypt(&binding.aad(), &secret.to_plaintext()?)
    }

    pub fn open(
        &self,
        binding: CredentialBinding<'_>,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<ProviderSecret, CredentialError> {
        let plaintext = self.decrypt(&binding.aad(), key_id, ciphertext)?;
        ProviderSecret::from_plaintext(&plaintext)
    }

    /// Seals a webhook signing secret. The associated data binds it to its
    /// organization and endpoint, and differs from any provider credential's.
    pub fn seal_webhook_secret(
        &self,
        organization_id: OrganizationId,
        endpoint_id: uuid::Uuid,
        secret: &[u8],
    ) -> Result<SealedCredential, CredentialError> {
        self.encrypt(&webhook_aad(organization_id, endpoint_id), secret)
    }

    pub fn open_webhook_secret(
        &self,
        organization_id: OrganizationId,
        endpoint_id: uuid::Uuid,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
        self.decrypt(
            &webhook_aad(organization_id, endpoint_id),
            key_id,
            ciphertext,
        )
    }

    /// Seals an organization's identity-provider client secret. The
    /// associated data binds it to the organization and the configuration,
    /// so it cannot be moved to another row, and differs from every other
    /// secret's.
    pub fn seal_identity_provider_secret(
        &self,
        organization_id: OrganizationId,
        configuration_id: uuid::Uuid,
        secret: &str,
    ) -> Result<SealedCredential, CredentialError> {
        self.encrypt(
            &identity_provider_aad(organization_id, configuration_id),
            secret.as_bytes(),
        )
    }

    pub fn open_identity_provider_secret(
        &self,
        organization_id: OrganizationId,
        configuration_id: uuid::Uuid,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<String>, CredentialError> {
        let plaintext = self.decrypt(
            &identity_provider_aad(organization_id, configuration_id),
            key_id,
            ciphertext,
        )?;
        std::str::from_utf8(&plaintext)
            .map(|secret| Zeroizing::new(secret.to_owned()))
            .map_err(|_| CredentialError::Decryption)
    }

    /// Keyed digest of an identity-provider client secret, used only in the
    /// idempotency fingerprint of the request that set it, so a retry with
    /// another secret conflicts while the record cannot confirm a guessed
    /// secret offline. `None` (the secret is kept) has its own encoding.
    pub fn identity_provider_fingerprint(
        &self,
        organization_id: OrganizationId,
        secret: Option<&str>,
    ) -> Result<String, CredentialError> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(self.active().fingerprint_key.as_ref())
            .map_err(|_| CredentialError::Encryption)?;
        mac.update(
            [
                IDENTITY_PROVIDER_FINGERPRINT_CONTEXT,
                &organization_id.0.to_string(),
            ]
            .join("\0")
            .as_bytes(),
        );
        match secret {
            Some(secret) => {
                mac.update(&[1]);
                mac.update(&(secret.len() as u64).to_be_bytes());
                mac.update(secret.as_bytes());
            }
            None => mac.update(&[0]),
        }
        Ok(mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    fn encrypt(&self, aad: &[u8], plaintext: &[u8]) -> Result<SealedCredential, CredentialError> {
        let key = self.active();
        let cipher = Aes256Gcm::new_from_slice(key.encryption_key.as_ref())
            .map_err(|_| CredentialError::Encryption)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CredentialError::Encryption)?;
        Ok(SealedCredential {
            key_id: key.key_id.clone(),
            ciphertext: [nonce.as_slice(), ciphertext.as_slice()].concat(),
        })
    }

    fn decrypt(
        &self,
        aad: &[u8],
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
        let key = self.find(key_id).ok_or(CredentialError::UnknownKey)?;
        if ciphertext.len() <= NONCE_BYTES {
            return Err(CredentialError::Decryption);
        }
        let cipher = Aes256Gcm::new_from_slice(key.encryption_key.as_ref())
            .map_err(|_| CredentialError::Decryption)?;
        cipher
            .decrypt(
                Nonce::from_slice(&ciphertext[..NONCE_BYTES]),
                Payload {
                    msg: &ciphertext[NONCE_BYTES..],
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| CredentialError::Decryption)
    }

    /// Seals the key one trace's content is encrypted under. The associated
    /// data binds it to the organization and trace, and differs from every
    /// other secret's.
    pub fn seal_trace_content_key(
        &self,
        organization_id: OrganizationId,
        trace_id: uuid::Uuid,
        key: &[u8],
    ) -> Result<SealedCredential, CredentialError> {
        self.encrypt(&trace_content_key_aad(organization_id, trace_id), key)
    }

    pub fn open_trace_content_key(
        &self,
        organization_id: OrganizationId,
        trace_id: uuid::Uuid,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CredentialError> {
        self.decrypt(
            &trace_content_key_aad(organization_id, trace_id),
            key_id,
            ciphertext,
        )
    }

    /// Keyed digest of a credential write request, used only as the
    /// idempotency fingerprint input. It is keyed by the active key, so a
    /// retry that spans a key change no longer matches its first attempt. Being keyed, it cannot confirm a guessed
    /// secret offline the way a plain hash in the idempotency table could.
    ///
    /// It covers what the client sent, not the family the key currently
    /// resolves to, so retrying a completed write still matches its record
    /// after an activation redeclares the key under another family.
    pub fn fingerprint(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        input: &ProviderSecretInput,
    ) -> Result<String, CredentialError> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(self.active().fingerprint_key.as_ref())
            .map_err(|_| CredentialError::Encryption)?;
        mac.update(
            [
                FINGERPRINT_CONTEXT,
                &organization_id.0.to_string(),
                adapter_instance_key,
            ]
            .join("\0")
            .as_bytes(),
        );
        // Each field is tagged present or absent and length-prefixed, so no
        // two distinct inputs share an encoding.
        let field = |mac: &mut HmacSha256, value: Option<&str>| match value {
            Some(value) => {
                mac.update(&[1]);
                mac.update(&(value.len() as u64).to_be_bytes());
                mac.update(value.as_bytes());
            }
            None => mac.update(&[0]),
        };
        for value in [
            &input.api_key,
            &input.access_key_id,
            &input.secret_access_key,
            &input.session_token,
        ] {
            field(&mut mac, value.as_deref().map(String::as_str));
        }
        // The role fields are encoded only when present, so the fingerprint
        // of every request that has none is unchanged.
        if input.role_arn.is_some() || input.external_id.is_some() {
            mac.update(b"\0role");
            field(&mut mac, input.role_arn.as_deref());
            field(&mut mac, input.external_id.as_deref());
        }
        if input.tenant_id.is_some() || input.client_id.is_some() || input.endpoint.is_some() {
            mac.update(b"\0entra");
            field(&mut mac, input.tenant_id.as_deref());
            field(&mut mac, input.client_id.as_deref());
            field(&mut mac, input.endpoint.as_deref());
        }
        if input.service_account.is_some() || input.project_id.is_some() {
            mac.update(b"\0google");
            field(&mut mac, input.service_account.as_deref());
            field(&mut mac, input.project_id.as_deref());
        }
        // Added later, and only when present, so the fingerprint of every
        // request without it is unchanged.
        if input.location.is_some() {
            mac.update(b"\0google-location");
            field(&mut mac, input.location.as_deref());
        }
        Ok(mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
}

/// What the admin API may say about a stored credential. There is no field
/// that can carry the secret, its ciphertext, or a digest of it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderCredentialMetadata {
    pub adapter_instance_key: String,
    pub family: String,
    pub key_id: String,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub rotated_at: DateTime<Utc>,
    /// For a credential that acts as a tenant's cloud principal: whether an
    /// operator has bound it to the organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_principal: Option<CloudPrincipalStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_provider_secrets_open_only_for_their_configuration() {
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let (org, config) = (OrganizationId::new(), uuid::Uuid::new_v4());
        let sealed = cipher
            .seal_identity_provider_secret(org, config, "client-s3cret")
            .unwrap();
        assert!(
            !sealed
                .ciphertext
                .windows(13)
                .any(|window| window == b"client-s3cret")
        );
        assert_eq!(
            cipher
                .open_identity_provider_secret(org, config, &sealed.key_id, &sealed.ciphertext)
                .unwrap()
                .as_str(),
            "client-s3cret"
        );
        for (other_org, other_config) in
            [(OrganizationId::new(), config), (org, uuid::Uuid::new_v4())]
        {
            assert!(
                cipher
                    .open_identity_provider_secret(
                        other_org,
                        other_config,
                        &sealed.key_id,
                        &sealed.ciphertext
                    )
                    .is_err()
            );
        }
        // Nor as a webhook secret of the same ids.
        assert!(
            cipher
                .open_webhook_secret(org, config, &sealed.key_id, &sealed.ciphertext)
                .is_err()
        );
        // The fingerprint is keyed and tells secrets apart.
        let a = cipher
            .identity_provider_fingerprint(org, Some("a"))
            .unwrap();
        assert_ne!(
            a,
            cipher
                .identity_provider_fingerprint(org, Some("b"))
                .unwrap()
        );
        assert_ne!(a, cipher.identity_provider_fingerprint(org, None).unwrap());
        let other_key = CredentialCipher::new(&[8_u8; 32]).unwrap();
        assert_ne!(
            a,
            other_key
                .identity_provider_fingerprint(org, Some("a"))
                .unwrap()
        );
    }

    fn binding(organization_id: OrganizationId) -> CredentialBinding<'static> {
        CredentialBinding {
            organization_id,
            adapter_instance_key: "openai-main",
            family: ProviderFamily::OpenAi,
        }
    }

    fn api_key(value: &str) -> ProviderSecret {
        ProviderSecret::for_family(
            ProviderFamily::OpenAi,
            ProviderSecretInput {
                api_key: Some(Zeroizing::new(value.to_owned())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap()
    }

    fn input(json: &str) -> ProviderSecretInput {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_keyring_seals_with_its_active_key_and_opens_with_any() {
        let org = OrganizationId::new();
        let old = CredentialCipher::new(&[1_u8; 32]).unwrap();
        let sealed_old = old.seal(binding(org), &api_key("sk-old")).unwrap();
        let ring = CredentialCipher::with_keys(&[2_u8; 32], &[&[1_u8; 32]]).unwrap();
        assert_ne!(ring.key_id(), old.key_id());
        assert_eq!(ring.key_ids(), [ring.key_id(), old.key_id()]);
        // Old secrets open; new ones are sealed under the active key.
        assert!(
            ring.open(binding(org), &sealed_old.key_id, &sealed_old.ciphertext)
                .is_ok()
        );
        let sealed_new = ring.seal(binding(org), &api_key("sk-new")).unwrap();
        assert_eq!(sealed_new.key_id, ring.key_id());
        assert_eq!(
            old.open(binding(org), &sealed_new.key_id, &sealed_new.ciphertext)
                .unwrap_err(),
            CredentialError::UnknownKey
        );
        // Installing reaches every clone; a dropped key no longer opens.
        let clone = ring.clone();
        ring.install(&[3_u8; 32], &[&[2_u8; 32]], None).unwrap();
        assert_ne!(clone.key_id(), sealed_new.key_id);
        assert!(
            clone
                .open(binding(org), &sealed_new.key_id, &sealed_new.ciphertext)
                .is_ok()
        );
        assert_eq!(
            clone
                .open(binding(org), &sealed_old.key_id, &sealed_old.ciphertext)
                .unwrap_err(),
            CredentialError::UnknownKey
        );
        // An inherited keyring keeps opening what it sealed.
        clone.install(&[3_u8; 32], &[], Some(&old)).unwrap();
        assert!(
            clone
                .open(binding(org), &sealed_old.key_id, &sealed_old.ciphertext)
                .is_ok()
        );
        // Duplicates of the active key are ignored.
        let ring = CredentialCipher::with_keys(&[4_u8; 32], &[&[4_u8; 32]]).unwrap();
        assert_eq!(ring.key_ids().len(), 1);
    }

    #[test]
    fn sealed_credentials_round_trip_and_hide_plaintext() {
        let cipher = CredentialCipher::new(&[7_u8; 32]).unwrap();
        let org = OrganizationId::new();
        let sealed = cipher
            .seal(binding(org), &api_key("sk-test-secret"))
            .unwrap();
        assert_eq!(sealed.key_id, cipher.key_id());
        assert_eq!(sealed.key_id.len(), 16);
        assert!(
            !sealed
                .ciphertext
                .windows(b"sk-test-secret".len())
                .any(|w| w == b"sk-test-secret")
        );
        match cipher
            .open(binding(org), &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::ApiKey { api_key } => assert_eq!(api_key.as_str(), "sk-test-secret"),
            other => panic!("unexpected {other:?}"),
        }
        // A fresh nonce makes every seal distinct.
        let again = cipher
            .seal(binding(org), &api_key("sk-test-secret"))
            .unwrap();
        assert_ne!(again.ciphertext, sealed.ciphertext);
    }

    #[test]
    fn ciphertext_is_bound_to_tenant_account_family_and_key() {
        let cipher = CredentialCipher::new(&[7_u8; 32]).unwrap();
        let org = OrganizationId::new();
        let sealed = cipher.seal(binding(org), &api_key("sk-test")).unwrap();
        let open = |b: CredentialBinding<'_>| cipher.open(b, &sealed.key_id, &sealed.ciphertext);
        assert_eq!(
            open(binding(OrganizationId::new())).unwrap_err(),
            CredentialError::Decryption
        );
        assert_eq!(
            open(CredentialBinding {
                adapter_instance_key: "openai-other",
                ..binding(org)
            })
            .unwrap_err(),
            CredentialError::Decryption
        );
        assert_eq!(
            open(CredentialBinding {
                family: ProviderFamily::AzureOpenAi,
                ..binding(org)
            })
            .unwrap_err(),
            CredentialError::Decryption
        );
        let other_key = CredentialCipher::new(&[8_u8; 32]).unwrap();
        assert_ne!(other_key.key_id(), cipher.key_id());
        assert_eq!(
            other_key
                .open(binding(org), &sealed.key_id, &sealed.ciphertext)
                .unwrap_err(),
            CredentialError::UnknownKey
        );
        let mut tampered = sealed.ciphertext.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(
            cipher
                .open(binding(org), &sealed.key_id, &tampered)
                .unwrap_err(),
            CredentialError::Decryption
        );
    }

    #[test]
    fn secrets_are_validated_per_family_and_never_debug_printed() {
        let input = |json: &str| serde_json::from_str::<ProviderSecretInput>(json).unwrap();
        assert!(
            ProviderSecret::for_family(ProviderFamily::OpenAi, input(r#"{"api_key":"sk-1"}"#))
                .is_ok()
        );
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::OpenAi,
                input(r#"{"api_key":"sk-1","secret_access_key":"x"}"#)
            )
            .is_err()
        );
        assert!(
            ProviderSecret::for_family(ProviderFamily::Anthropic, input(r#"{"api_key":"a b"}"#))
                .is_err()
        );
        assert!(
            ProviderSecret::for_family(ProviderFamily::Gemini, input(r#"{"api_key":""}"#)).is_err()
        );
        assert!(
            ProviderSecret::for_family(ProviderFamily::Bedrock, input(r#"{"api_key":"k"}"#))
                .is_err()
        );
        let bedrock = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            input(r#"{"access_key_id":"AKIAEXAMPLE","secret_access_key":"s3cr3t/value"}"#),
        )
        .unwrap();
        assert_eq!(format!("{bedrock:?}"), "ProviderSecret::AwsSigV4(REDACTED)");
        assert!(
            serde_json::from_str::<ProviderSecretInput>(r#"{"api_key":"k","base_url":"x"}"#)
                .is_err()
        );
        let parsed = input(r#"{"api_key":"sk-visible"}"#);
        assert!(!format!("{parsed:?}").contains("sk-visible"));
        let cipher = CredentialCipher::new(&[1_u8; 32]).unwrap();
        assert!(!format!("{cipher:?}").contains("encryption_key"));
    }

    #[test]
    fn bedrock_credentials_round_trip_with_optional_session_token() {
        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let binding = CredentialBinding {
            organization_id: OrganizationId::new(),
            adapter_instance_key: "bedrock-main",
            family: ProviderFamily::Bedrock,
        };
        let secret = ProviderSecret::for_family(
            ProviderFamily::Bedrock,
            serde_json::from_str(
                r#"{"access_key_id":"AKIA1","secret_access_key":"s","session_token":"t"}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let sealed = cipher.seal(binding, &secret).unwrap();
        match cipher
            .open(binding, &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                assert_eq!(access_key_id.as_str(), "AKIA1");
                assert_eq!(secret_access_key.as_str(), "s");
                assert_eq!(session_token.as_deref().map(String::as_str), Some("t"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn fingerprints_are_keyed_and_request_specific() {
        let cipher = CredentialCipher::new(&[7_u8; 32]).unwrap();
        let org = OrganizationId::new();
        let print = |cipher: &CredentialCipher, org, key, body| {
            cipher.fingerprint(org, key, &input(body)).unwrap()
        };
        let first = print(&cipher, org, "openai-main", r#"{"api_key":"sk-a"}"#);
        assert_eq!(
            first,
            print(&cipher, org, "openai-main", r#"{"api_key":"sk-a"}"#)
        );
        for (org, key, body) in [
            (org, "openai-main", r#"{"api_key":"sk-b"}"#),
            (org, "other-main", r#"{"api_key":"sk-a"}"#),
            (
                OrganizationId::new(),
                "openai-main",
                r#"{"api_key":"sk-a"}"#,
            ),
            // Same bytes in a different field.
            (org, "openai-main", r#"{"access_key_id":"sk-a"}"#),
        ] {
            assert_ne!(first, print(&cipher, org, key, body), "{key} {body}");
        }
        let other = CredentialCipher::new(&[8_u8; 32]).unwrap();
        assert_ne!(
            first,
            print(&other, org, "openai-main", r#"{"api_key":"sk-a"}"#)
        );
    }

    #[test]
    fn adapter_instance_keys_are_bounded_and_printable() {
        assert!(validate_adapter_instance_key("openai-main").is_ok());
        assert!(validate_adapter_instance_key("bedrock_us.east-1").is_ok());
        assert!(validate_adapter_instance_key("").is_err());
        assert!(validate_adapter_instance_key("   ").is_err());
        assert!(validate_adapter_instance_key("a\0b").is_err());
        assert!(validate_adapter_instance_key(&"k".repeat(129)).is_err());
    }

    #[test]
    fn bedrock_roles_are_validated_sealed_and_fingerprinted() {
        let role = r#"{"role_arn":"arn:aws:iam::123456789012:role/gateway-tenant"}"#;
        let secret = ProviderSecret::for_family(ProviderFamily::Bedrock, input(role)).unwrap();
        assert_eq!(
            format!("{secret:?}"),
            "ProviderSecret::AwsAssumeRole(REDACTED)"
        );
        // An AWS role acts as no principal an operator binds: the gateway's
        // external ID ties it to one organization.
        assert!(secret.cloud_principal().is_none());
        for invalid in [
            // Keys and a role are alternatives.
            r#"{"role_arn":"arn:aws:iam::123456789012:role/r","access_key_id":"AKIA1","secret_access_key":"s"}"#,
            // The external ID is the gateway's to choose, never the tenant's.
            r#"{"role_arn":"arn:aws:iam::123456789012:role/r","external_id":"ext-123"}"#,
            r#"{"external_id":"ext-123"}"#,
            r#"{"role_arn":"arn:aws:iam::12345:role/r"}"#,
            r#"{"role_arn":"arn:aws:iam::123456789012:user/r"}"#,
            r#"{"role_arn":"arn:aws:s3:::bucket"}"#,
        ] {
            assert!(
                ProviderSecret::for_family(ProviderFamily::Bedrock, input(invalid)).is_err(),
                "{invalid}"
            );
        }
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::OpenAi,
                input(r#"{"role_arn":"arn:aws:iam::123456789012:role/r"}"#)
            )
            .is_err()
        );
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::Bedrock,
                input(r#"{"role_arn":"arn:aws-us-gov:iam::123456789012:role/path/r"}"#)
            )
            .is_ok()
        );

        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let binding = CredentialBinding {
            organization_id: OrganizationId::new(),
            adapter_instance_key: "bedrock-main",
            family: ProviderFamily::Bedrock,
        };
        let sealed = cipher.seal(binding, &secret).unwrap();
        match cipher
            .open(binding, &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::AwsAssumeRole { role_arn } => {
                assert_eq!(role_arn, "arn:aws:iam::123456789012:role/gateway-tenant");
            }
            other => panic!("unexpected {other:?}"),
        }
        // A role stored with a tenant's external ID still opens; the ID is
        // never used.
        let legacy = cipher
            .encrypt(
                &binding.aad(),
                br#"{"kind":"aws_assume_role","role_arn":"arn:aws:iam::123456789012:role/r","external_id":"ext-123"}"#,
            )
            .unwrap();
        assert!(matches!(
            cipher.open(binding, &legacy.key_id, &legacy.ciphertext),
            Ok(ProviderSecret::AwsAssumeRole { .. })
        ));

        let org = OrganizationId::new();
        let print = |body| {
            cipher
                .fingerprint(org, "bedrock-main", &input(body))
                .unwrap()
        };
        assert_ne!(
            print(role),
            print(r#"{"role_arn":"arn:aws:iam::123456789012:role/other"}"#)
        );
        assert_eq!(
            aws_external_id(org),
            format!("qucelerate-{}", org.0),
            "derived from the organization alone"
        );
    }

    #[test]
    fn cloud_principals_are_parsed_and_digested() {
        let tenant =
            CloudPrincipal::parse("azure_tenant", " 8F2C1A4E-0000-4000-8000-00000000000A ")
                .unwrap();
        assert_eq!(tenant.value(), "8f2c1a4e-0000-4000-8000-00000000000a");
        let account = CloudPrincipal::parse(
            "google_service_account",
            "Gemini@Tenant-Project.iam.gserviceaccount.com",
        )
        .unwrap();
        assert_eq!(account.kind(), "google_service_account");
        for (kind, value) in [
            ("azure_tenant", "contoso.onmicrosoft.com"),
            ("google_service_account", "person@example.com"),
            ("aws_role", "arn:aws:iam::123456789012:role/r"),
        ] {
            assert!(
                CloudPrincipal::parse(kind, value).is_err(),
                "{kind} {value}"
            );
        }
        // A credential's principal digests as the operator's binding does.
        let directory = ProviderSecret::for_family(
            ProviderFamily::AzureOpenAi,
            input(r#"{"tenant_id":"8F2C1A4E-0000-4000-8000-00000000000A"}"#),
        )
        .unwrap();
        assert_eq!(directory.cloud_principal(), Some(tenant.clone()));
        assert_eq!(
            directory.cloud_principal().unwrap().digest(),
            tenant.digest()
        );
        assert_eq!(tenant.digest().len(), 32);
        // Kinds are part of the digest.
        assert_ne!(
            CloudPrincipal::AzureTenant("x".into()).digest(),
            CloudPrincipal::GoogleServiceAccount("x".into()).digest()
        );
        assert!(
            ProviderSecret::for_family(ProviderFamily::OpenAi, input(r#"{"api_key":"k"}"#))
                .unwrap()
                .cloud_principal()
                .is_none()
        );
    }

    /// Fingerprints of requests made before `location` existed must not
    /// change: a retried write would otherwise conflict with itself.
    #[test]
    fn a_service_account_fingerprint_without_location_is_unchanged() {
        let cipher = CredentialCipher::new(&[7_u8; 32]).unwrap();
        let org = OrganizationId(uuid::Uuid::nil());
        let body = r#"{"service_account":"g@tenant-project.iam.gserviceaccount.com","project_id":"tenant-project"}"#;
        // The HMAC of the encoding before `location`, computed by hand.
        let mut mac =
            <HmacSha256 as Mac>::new_from_slice(cipher.active().fingerprint_key.as_ref()).unwrap();
        mac.update(
            [FINGERPRINT_CONTEXT, &org.0.to_string(), "gemini-main"]
                .join("\0")
                .as_bytes(),
        );
        for _ in 0..4 {
            mac.update(&[0]);
        }
        mac.update(b"\0google");
        for value in ["g@tenant-project.iam.gserviceaccount.com", "tenant-project"] {
            mac.update(&[1]);
            mac.update(&(value.len() as u64).to_be_bytes());
            mac.update(value.as_bytes());
        }
        let before: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            cipher
                .fingerprint(org, "gemini-main", &input(body))
                .unwrap(),
            before
        );
    }

    #[test]
    fn google_service_accounts_are_validated_sealed_and_fingerprinted() {
        let account = r#"{"service_account":"Gemini@Tenant-Project.iam.gserviceaccount.com","project_id":"tenant-project","location":"us-central1"}"#;
        let secret = ProviderSecret::for_family(ProviderFamily::Gemini, input(account)).unwrap();
        assert_eq!(
            format!("{secret:?}"),
            "ProviderSecret::GoogleServiceAccount(REDACTED)"
        );
        for (family, invalid) in [
            // A key and a service account are alternatives.
            (
                ProviderFamily::Gemini,
                r#"{"service_account":"g@p-roject.iam.gserviceaccount.com","api_key":"k"}"#,
            ),
            (
                ProviderFamily::Gemini,
                r#"{"service_account":"person@example.com"}"#,
            ),
            (
                ProviderFamily::Gemini,
                r#"{"service_account":"g@p-roject.iam.gserviceaccount.com","project_id":"Bad_Project"}"#,
            ),
            (ProviderFamily::Gemini, r#"{"project_id":"tenant-project"}"#),
            (
                ProviderFamily::Gemini,
                r#"{"service_account":"g@p-roject.iam.gserviceaccount.com","tenant_id":"8f2c1a4e-0000-4000-8000-00000000000a"}"#,
            ),
            (
                ProviderFamily::OpenAi,
                r#"{"service_account":"g@p-roject.iam.gserviceaccount.com"}"#,
            ),
            (
                ProviderFamily::AzureOpenAi,
                r#"{"tenant_id":"8f2c1a4e-0000-4000-8000-00000000000a","project_id":"tenant-project"}"#,
            ),
        ] {
            assert!(
                ProviderSecret::for_family(family, input(invalid)).is_err(),
                "{family:?} {invalid}"
            );
        }
        // The project is optional when the address names one...
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::Gemini,
                input(r#"{"service_account":"g@tenant-project.iam.gserviceaccount.com"}"#)
            )
            .is_ok()
        );
        // ...and required for a default service account, whose does not.
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::Gemini,
                input(r#"{"service_account":"123456789-compute@developer.gserviceaccount.com"}"#)
            )
            .is_err()
        );
        for location in ["Bad_Location", "x"] {
            assert!(
                ProviderSecret::for_family(
                    ProviderFamily::Gemini,
                    input(&format!(
                        r#"{{"service_account":"g@tenant-project.iam.gserviceaccount.com","location":"{location}"}}"#
                    ))
                )
                .is_err(),
                "{location}"
            );
        }
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::Gemini,
                input(
                    r#"{"service_account":"123456789-compute@developer.gserviceaccount.com","project_id":"tenant-project"}"#
                )
            )
            .is_ok()
        );

        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let binding = CredentialBinding {
            organization_id: OrganizationId::new(),
            adapter_instance_key: "gemini-main",
            family: ProviderFamily::Gemini,
        };
        let sealed = cipher.seal(binding, &secret).unwrap();
        match cipher
            .open(binding, &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::GoogleServiceAccount {
                service_account,
                project_id,
                location,
            } => {
                assert_eq!(location.as_deref(), Some("us-central1"));
                assert_eq!(
                    service_account,
                    "gemini@tenant-project.iam.gserviceaccount.com"
                );
                assert_eq!(project_id.as_deref(), Some("tenant-project"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let org = OrganizationId::new();
        let print = |body: &str| {
            cipher
                .fingerprint(org, "gemini-main", &input(body))
                .unwrap()
        };
        assert_ne!(
            print(account),
            print(r#"{"service_account":"Gemini@Tenant-Project.iam.gserviceaccount.com"}"#)
        );
        assert_ne!(print(account), print(r#"{"api_key":"tenant-project"}"#));
    }

    const RESOURCE_ID: &str = "/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/contoso-ai";

    #[test]
    fn an_entra_directory_stored_before_resource_ids_still_opens() {
        let secret = ProviderSecret::from_plaintext(
            br#"{"kind":"azure_entra","tenant_id":"8f2c1a4e-0000-4000-8000-00000000000a"}"#,
        )
        .unwrap();
        assert!(matches!(
            secret,
            ProviderSecret::AzureEntra {
                resource_id: None,
                ..
            }
        ));
    }

    #[test]
    fn openai_compatible_endpoints_are_validated_and_sealed() {
        let secret = ProviderSecret::for_family(
            ProviderFamily::OpenAiCompatible,
            input(
                r#"{"endpoint":" http://qwen.models.svc.cluster.local:8080/v1/ ","api_key":"k-1"}"#,
            ),
        )
        .unwrap();
        assert_eq!(
            format!("{secret:?}"),
            "ProviderSecret::OpenAiCompatibleEndpoint(REDACTED)"
        );
        assert_eq!(secret.cloud_principal(), None);
        let ProviderSecret::OpenAiCompatibleEndpoint { base_url, api_key } = &secret else {
            panic!("an endpoint");
        };
        assert_eq!(base_url, "http://qwen.models.svc.cluster.local:8080/v1");
        assert_eq!(api_key.as_deref().map(String::as_str), Some("k-1"));
        // The key is optional; without an endpoint, a key is the operator's
        // endpoint's as before.
        assert!(matches!(
            ProviderSecret::for_family(
                ProviderFamily::OpenAiCompatible,
                input(r#"{"endpoint":"https://models.example.com/v1"}"#)
            )
            .unwrap(),
            ProviderSecret::OpenAiCompatibleEndpoint { api_key: None, .. }
        ));
        assert!(matches!(
            ProviderSecret::for_family(
                ProviderFamily::OpenAiCompatible,
                input(r#"{"api_key":"k"}"#)
            )
            .unwrap(),
            ProviderSecret::ApiKey { .. }
        ));
        for invalid in [
            r#"{"endpoint":"ftp://models.example.com/v1"}"#,
            r#"{"endpoint":"https://user:pass@models.example.com/v1"}"#,
            r#"{"endpoint":"https://models.example.com/v1?key=k"}"#,
            r#"{"endpoint":"https://models.example.com/v1#x"}"#,
            r#"{"endpoint":"models.example.com/v1"}"#,
            r#"{"endpoint":""}"#,
            r#"{"endpoint":"https://models.example.com/v1","tenant_id":"8f2c1a4e-0000-4000-8000-00000000000a"}"#,
            r#"{"endpoint":"https://models.example.com/v1","role_arn":"arn:aws:iam::123456789012:role/x"}"#,
            r#"{"endpoint":"https://models.example.com/v1","api_key":""}"#,
        ] {
            assert!(
                ProviderSecret::for_family(ProviderFamily::OpenAiCompatible, input(invalid))
                    .is_err(),
                "{invalid}"
            );
        }
        // Endpoints are only for this family.
        assert!(
            ProviderSecret::for_family(
                ProviderFamily::OpenAi,
                input(r#"{"endpoint":"https://models.example.com/v1","api_key":"k"}"#)
            )
            .is_err()
        );

        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let binding = CredentialBinding {
            organization_id: OrganizationId::new(),
            adapter_instance_key: "openai-compatible-qwen",
            family: ProviderFamily::OpenAiCompatible,
        };
        let sealed = cipher.seal(binding, &secret).unwrap();
        match cipher
            .open(binding, &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::OpenAiCompatibleEndpoint { base_url, api_key } => {
                assert_eq!(base_url, "http://qwen.models.svc.cluster.local:8080/v1");
                assert_eq!(api_key.as_deref().map(String::as_str), Some("k-1"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn azure_directories_are_validated_sealed_and_fingerprinted() {
        let tenant = r#"{"tenant_id":"8F2C1A4E-0000-4000-8000-00000000000A","client_id":"11111111-2222-4333-8444-555555555555","endpoint":"https://Contoso-AI.openai.azure.com/","resource_id":"/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/contoso-ai"}"#;
        let secret =
            ProviderSecret::for_family(ProviderFamily::AzureOpenAi, input(tenant)).unwrap();
        assert_eq!(
            format!("{secret:?}"),
            "ProviderSecret::AzureEntra(REDACTED)"
        );
        let ProviderSecret::AzureEntra {
            tenant_id,
            client_id,
            endpoint,
            resource_id,
        } = &secret
        else {
            panic!("an Entra directory");
        };
        assert_eq!(resource_id.as_deref(), Some(RESOURCE_ID));
        assert_eq!(tenant_id, "8f2c1a4e-0000-4000-8000-00000000000a");
        assert_eq!(
            client_id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(
            endpoint.as_deref(),
            Some("https://contoso-ai.openai.azure.com")
        );
        let directory = r#""tenant_id":"8f2c1a4e-0000-4000-8000-00000000000a""#;
        for (family, invalid) in [
            // A key and a directory are alternatives.
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"api_key":"k"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                r#"{"tenant_id":"contoso.onmicrosoft.com"}"#.into(),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"client_id":"not-a-guid"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                r#"{"client_id":"11111111-2222-4333-8444-555555555555"}"#.into(),
            ),
            (
                ProviderFamily::AzureOpenAi,
                r#"{"api_key":"k","endpoint":"https://a.openai.azure.com"}"#.into(),
            ),
            // Endpoints are Azure's own hosts for the family, and nothing more.
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"https://evil.example.com"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"http://a.openai.azure.com"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"https://a.openai.azure.com/x"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"https://a.openai.azure.com:8443"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"https://a.b.openai.azure.com"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"endpoint":"https://a.services.ai.azure.com"}}"#),
            ),
            (
                ProviderFamily::AzureAiFoundry,
                format!(r#"{{{directory},"endpoint":"https://a.openai.azure.com"}}"#),
            ),
            (ProviderFamily::OpenAi, format!("{{{directory}}}")),
            (ProviderFamily::Bedrock, format!("{{{directory}}}")),
            // A resource ID names a Cognitive Services account, and comes
            // with a directory, on an Azure family only.
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{{directory},"resource_id":"/subscriptions/x/resourceGroups/g"}}"#),
            ),
            (
                ProviderFamily::AzureOpenAi,
                format!(r#"{{"api_key":"k","resource_id":"{RESOURCE_ID}"}}"#),
            ),
            (
                ProviderFamily::OpenAi,
                format!(r#"{{"api_key":"k","resource_id":"{RESOURCE_ID}"}}"#),
            ),
        ] {
            assert!(
                ProviderSecret::for_family(family, input(&invalid)).is_err(),
                "{family:?} {invalid}"
            );
        }
        for (family, endpoint) in [
            (
                ProviderFamily::AzureAiFoundry,
                "https://ai-res.cognitiveservices.azure.com",
            ),
            (
                ProviderFamily::AzureOpenAi,
                "https://gov-res.openai.azure.us",
            ),
            (
                ProviderFamily::AzureAiFoundry,
                "https://cn-res.cognitiveservices.azure.cn",
            ),
        ] {
            assert!(
                ProviderSecret::for_family(
                    family,
                    input(&format!(r#"{{{directory},"endpoint":"{endpoint}"}}"#))
                )
                .is_ok(),
                "{endpoint}"
            );
        }

        let cipher = CredentialCipher::new(&[9_u8; 32]).unwrap();
        let binding = CredentialBinding {
            organization_id: OrganizationId::new(),
            adapter_instance_key: "azure-openai-main",
            family: ProviderFamily::AzureOpenAi,
        };
        let sealed = cipher.seal(binding, &secret).unwrap();
        match cipher
            .open(binding, &sealed.key_id, &sealed.ciphertext)
            .unwrap()
        {
            ProviderSecret::AzureEntra {
                tenant_id,
                client_id,
                endpoint,
                resource_id,
            } => {
                assert_eq!(tenant_id, "8f2c1a4e-0000-4000-8000-00000000000a");
                assert_eq!(resource_id.as_deref(), Some(RESOURCE_ID));
                assert!(client_id.is_some());
                assert_eq!(
                    endpoint.as_deref(),
                    Some("https://contoso-ai.openai.azure.com")
                );
            }
            other => panic!("unexpected {other:?}"),
        }

        let org = OrganizationId::new();
        let print = |body: &str| {
            cipher
                .fingerprint(org, "azure-openai-main", &input(body))
                .unwrap()
        };
        assert_ne!(print(tenant), print(&format!("{{{directory}}}")));
        assert_ne!(
            print(&format!("{{{directory}}}")),
            print(r#"{"tenant_id":"8f2c1a4e-0000-4000-8000-00000000000b"}"#)
        );
    }
}
