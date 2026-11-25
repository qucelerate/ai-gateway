use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(OrganizationId);
id_type!(TeamId);
id_type!(UserAccountId);
id_type!(ServiceAccountId);
id_type!(ApiKeyId);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Organization {
    pub id: OrganizationId,
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Team {
    pub id: TeamId,
    pub organization_id: OrganizationId,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UserAccount {
    pub id: UserAccountId,
    pub oidc_issuer: Option<String>,
    pub oidc_subject: Option<String>,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MembershipRole {
    Owner,
    Admin,
    Member,
    Auditor,
    ReadOnly,
}

impl MembershipRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Auditor => "auditor",
            Self::ReadOnly => "read-only",
        }
    }
}

impl FromStr for MembershipRole {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            "auditor" => Ok(Self::Auditor),
            "read-only" => Ok(Self::ReadOnly),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceAccount {
    pub id: ServiceAccountId,
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub owner_user_id: UserAccountId,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiKeyMetadata {
    pub id: ApiKeyId,
    pub organization_id: OrganizationId,
    pub service_account_id: ServiceAccountId,
    pub successor_of: Option<ApiKeyId>,
    pub public_prefix: String,
    pub scopes: Vec<ApiKeyScope>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub disabled_at: Option<DateTime<Utc>>,
    pub status: String,
}

/// Serialized in PascalCase (`CatalogRead`); requests may also spell a scope
/// the way the admin API spells its other values (`catalog-read`).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ApiKeyScope {
    #[serde(alias = "inference")]
    Inference,
    #[serde(alias = "embeddings")]
    Embeddings,
    #[serde(alias = "catalog-read")]
    CatalogRead,
    #[serde(alias = "admin-read")]
    AdminRead,
}

impl ApiKeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inference => "inference",
            Self::Embeddings => "embeddings",
            Self::CatalogRead => "catalog-read",
            Self::AdminRead => "admin-read",
        }
    }
}

impl FromStr for ApiKeyScope {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "inference" => Ok(Self::Inference),
            "embeddings" => Ok(Self::Embeddings),
            "catalog-read" => Ok(Self::CatalogRead),
            "admin-read" => Ok(Self::AdminRead),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CreatedApiKey {
    pub id: ApiKeyId,
    pub organization_id: OrganizationId,
    pub service_account_id: ServiceAccountId,
    pub public_prefix: String,
    /// The bearer is returned only by key creation and is never persisted.
    pub bearer: String,
    pub scopes: Vec<ApiKeyScope>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for CreatedApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreatedApiKey")
            .field("id", &self.id)
            .field("organization_id", &self.organization_id)
            .field("service_account_id", &self.service_account_id)
            .field("public_prefix", &self.public_prefix)
            .field("bearer", &"REDACTED")
            .field("scopes", &self.scopes)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedIdentity {
    pub organization_id: OrganizationId,
    pub service_account_id: ServiceAccountId,
    pub team_id: Option<TeamId>,
    /// The API key the request presented; recorded on its trace so usage can
    /// be sliced by key. `None` only for identities built without a key.
    pub api_key_id: Option<ApiKeyId>,
    pub scopes: Vec<ApiKeyScope>,
    /// The app's end user the request is for (D-028), from the
    /// `Gateway-End-User` header or the body's `user`; set by the transport
    /// after the key is verified.
    pub end_user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_read_in_either_spelling_and_write_in_pascal_case() {
        let scopes: Vec<ApiKeyScope> = serde_json::from_str(
            r#"["Inference", "embeddings", "catalog-read", "CatalogRead", "admin-read"]"#,
        )
        .unwrap();
        assert_eq!(
            scopes,
            [
                ApiKeyScope::Inference,
                ApiKeyScope::Embeddings,
                ApiKeyScope::CatalogRead,
                ApiKeyScope::CatalogRead,
                ApiKeyScope::AdminRead
            ]
        );
        assert_eq!(
            serde_json::to_string(&scopes).unwrap(),
            r#"["Inference","Embeddings","CatalogRead","CatalogRead","AdminRead"]"#
        );
        assert!(serde_json::from_str::<ApiKeyScope>(r#""catalog_read""#).is_err());
    }

    #[test]
    fn created_key_debug_redacts_bearer() {
        let key = CreatedApiKey {
            id: ApiKeyId::new(),
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            public_prefix: "gw_public".to_owned(),
            bearer: "gw_plaintext-bearer".to_owned(),
            scopes: vec![ApiKeyScope::Inference],
            expires_at: None,
        };
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("gw_plaintext-bearer"));
        assert!(rendered.contains("REDACTED"));
    }
}
