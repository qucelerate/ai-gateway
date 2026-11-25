// Catalog admin application service (reads).
//
// Every release-scoped collection is read from one release: the organization's active
// release unless the caller names another. A named release that does not
// belong to the organization resolves to `NotFound`. Page tokens are bound to
// the resolved release, so a token minted before an activation fails with a
// query mismatch rather than silently mixing two releases. Catalog reads are
// configuration, not financial data, and are not audited. Provider
// credential metadata and price overrides are organization-wide rather than
// per release; secret material never leaves the credential store.

use uuid::Uuid;

use crate::domain::{
    admin::{AdminPermission, AdminScope},
    catalog::{CatalogManifest, CatalogOperation},
    catalog_admin::{
        CatalogPage, CatalogReleaseSummary, DeploymentQuery, DeploymentView, ProviderAccountView,
        RouteQuery, RouteView,
    },
    credentials::ProviderCredentialMetadata,
    identity::{OrganizationId, UserAccountId},
    pagination::{CursorClaims, Page, PageRequest, PaginationError, SignedCursorCodec},
    price_override::{PriceOverrideQuery, PriceOverrideView},
};
use crate::persistence::{
    admin::{AdminStore, AdminStoreError},
    catalog_admin::{CatalogAdminStore, CatalogAdminStoreError, RouteKeyset},
    price_overrides::PriceOverrideStore,
    provider_credentials::ProviderCredentialStore,
    reporting::{ReportingStoreError, TimeKeyset},
};

pub const ROUTE_RELEASES: &str = "/api/admin/v1/organizations/{org}/catalog/releases";
pub const ROUTE_PROVIDER_ACCOUNTS: &str =
    "/api/admin/v1/organizations/{org}/catalog/provider-accounts";
pub const ROUTE_DEPLOYMENTS: &str = "/api/admin/v1/organizations/{org}/catalog/deployments";
pub const ROUTE_ROUTES: &str = "/api/admin/v1/organizations/{org}/catalog/routes";
pub const ROUTE_PRICE_OVERRIDES: &str = "/api/admin/v1/organizations/{org}/catalog/price-overrides";
pub const ROUTE_CLOUD_TRUST: &str = "/api/admin/v1/organizations/{org}/catalog/cloud-trust";
pub const ROUTE_RELEASE_MANIFEST: &str =
    "/api/admin/v1/organizations/{org}/catalog/releases/{release_id}/manifest";

/// A page of releases and the activation pointer's version.
#[derive(Debug, serde::Serialize)]
pub struct ReleaseList {
    #[serde(flatten)]
    pub page: Page<CatalogReleaseSummary>,
    /// `If-Match` for an activation: `"v{activation_version}"`, or `*` while 0.
    pub activation_version: u64,
}

/// What a tenant's cloud must name to trust the gateway for this
/// organization, and only for it.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CloudTrust {
    /// The `sts:ExternalId` an AWS role's trust policy must require.
    pub aws_external_id: String,
    /// The session name the gateway gives assumed roles, as CloudTrail
    /// shows it.
    pub aws_role_session_name: String,
}

const RELEASE_COLLECTION: &str = "catalog_releases";
const PROVIDER_COLLECTION: &str = "catalog_provider_accounts";
const DEPLOYMENT_COLLECTION: &str = "catalog_deployments";
const ROUTE_COLLECTION: &str = "catalog_routes";
const CREDENTIAL_COLLECTION: &str = "catalog_provider_credentials";
const PRICE_OVERRIDE_COLLECTION: &str = "catalog_price_overrides";
const ORDER_CREATED_DESC: &str = "created_at desc,id desc";
const ORDER_ADAPTER_KEY: &str = "adapter_instance_key";
const ORDER_NAME: &str = "name";
const ORDER_ALIAS_OPERATION: &str = "alias,operation";

#[derive(Debug)]
pub enum CatalogAdminError {
    /// Auth crypto is unavailable, so page tokens cannot be signed.
    AuthenticationUnavailable,
    Forbidden,
    NotFound,
    Pagination(PaginationError),
    Store(CatalogAdminStoreError),
    Admin(AdminStoreError),
}

impl From<CatalogAdminStoreError> for CatalogAdminError {
    fn from(error: CatalogAdminStoreError) -> Self {
        Self::Store(error)
    }
}

/// Everything a paged collection read needs once access is checked and the
/// release is resolved.
struct Read<'a> {
    codec: &'a SignedCursorCodec,
    actor: UserAccountId,
    scope: AdminScope,
    release_id: Uuid,
}

#[derive(Clone)]
pub struct CatalogAdminService {
    admin: AdminStore,
    store: CatalogAdminStore,
    credentials: ProviderCredentialStore,
    overrides: PriceOverrideStore,
    codec: Option<SignedCursorCodec>,
}

impl CatalogAdminService {
    pub fn new(
        admin: AdminStore,
        store: CatalogAdminStore,
        credentials: ProviderCredentialStore,
        overrides: PriceOverrideStore,
        codec: Option<SignedCursorCodec>,
    ) -> Self {
        Self {
            admin,
            store,
            credentials,
            overrides,
            codec,
        }
    }

    /// Releases newest first, with the activation version to send as
    /// `If-Match` when activating one of them.
    pub async fn list_releases(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<ReleaseList, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        // Read before the releases: a version older than the list they see
        // only makes a client's activation conflict, never overwrite a switch
        // it has not seen.
        let activation_version = self.store.activation_version(organization_id).await?;
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            RELEASE_COLLECTION,
            "",
            ORDER_CREATED_DESC,
        )?
        .map(|claim| TimeKeyset::parse(&claim))
        .transpose()
        .map_err(|_: ReportingStoreError| CatalogAdminStoreError::InvalidState)?;
        let (items, has_more) = self
            .store
            .list_releases(organization_id, page.page_size, keyset)
            .await?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                RELEASE_COLLECTION,
                "",
                ORDER_CREATED_DESC,
                TimeKeyset {
                    at: last.created_at,
                    id: last.id,
                }
                .to_claim(),
            )?),
            _ => None,
        };
        Ok(ReleaseList {
            page: Page::new(items, next_page_token),
            activation_version,
        })
    }

    /// A release's catalog as a manifest: the starting point for a new
    /// release that changes it (create one from the edited manifest).
    pub async fn release_manifest(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Uuid,
    ) -> Result<CatalogManifest, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        self.store
            .release_manifest(organization_id, release_id)
            .await?
            .ok_or(CatalogAdminError::NotFound)
    }

    /// What the organization's cloud accounts must name to trust the
    /// gateway for it.
    pub async fn cloud_trust(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<CloudTrust, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        Ok(CloudTrust {
            aws_external_id: crate::domain::credentials::aws_external_id(organization_id),
            aws_role_session_name: crate::domain::credentials::aws_role_session_name(
                organization_id,
            ),
        })
    }

    /// Credential metadata for the organization, by adapter instance key.
    pub async fn list_credentials(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        page: &PageRequest,
    ) -> Result<Page<ProviderCredentialMetadata>, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let after = decode(
            codec,
            page,
            actor,
            scope,
            CREDENTIAL_COLLECTION,
            "",
            ORDER_ADAPTER_KEY,
        )?;
        let (items, has_more) = self
            .credentials
            .list(organization_id, page.page_size, after.as_deref())
            .await
            .map_err(CatalogAdminStoreError::from)?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                CREDENTIAL_COLLECTION,
                "",
                ORDER_ADAPTER_KEY,
                last.adapter_instance_key.clone(),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    /// Price overrides of the organization, newest first, in every status.
    pub async fn list_price_overrides(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        query: &PriceOverrideQuery,
        page: &PageRequest,
    ) -> Result<Page<PriceOverrideView>, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        let scope = AdminScope::Organization(organization_id);
        let codec = self.codec()?;
        let filters = serde_json::json!({
            "deployment": query.deployment,
            "operation": query.operation.map(CatalogOperation::as_str),
        })
        .to_string();
        let keyset = decode(
            codec,
            page,
            actor,
            scope,
            PRICE_OVERRIDE_COLLECTION,
            &filters,
            ORDER_CREATED_DESC,
        )?
        .map(|claim| TimeKeyset::parse(&claim))
        .transpose()
        .map_err(|_: ReportingStoreError| CatalogAdminStoreError::InvalidState)?;
        let (items, has_more) = self
            .overrides
            .list(organization_id, query, page.page_size, keyset)
            .await
            .map_err(CatalogAdminStoreError::from)?;
        let next_page_token = match items.last() {
            Some(last) if has_more => Some(encode(
                codec,
                actor,
                scope,
                PRICE_OVERRIDE_COLLECTION,
                &filters,
                ORDER_CREATED_DESC,
                TimeKeyset {
                    at: last.created_at,
                    id: last.id,
                }
                .to_claim(),
            )?),
            _ => None,
        };
        Ok(Page::new(items, next_page_token))
    }

    pub async fn list_provider_accounts(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Option<Uuid>,
        page: &PageRequest,
    ) -> Result<CatalogPage<ProviderAccountView>, CatalogAdminError> {
        let Some(read) = self.begin(actor, organization_id, release_id).await? else {
            return Ok(empty());
        };
        let filters = filters(read.release_id, serde_json::json!({}));
        let after = read.decode(page, PROVIDER_COLLECTION, &filters, ORDER_ADAPTER_KEY)?;
        let (items, has_more) = self
            .store
            .list_provider_accounts(
                organization_id,
                read.release_id,
                page.page_size,
                after.as_deref(),
            )
            .await?;
        let next = match items.last() {
            Some(last) if has_more => Some(read.encode(
                PROVIDER_COLLECTION,
                &filters,
                ORDER_ADAPTER_KEY,
                last.adapter_instance_key.clone(),
            )?),
            _ => None,
        };
        Ok(read.page(items, next))
    }

    pub async fn list_deployments(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Option<Uuid>,
        query: &DeploymentQuery,
        page: &PageRequest,
    ) -> Result<CatalogPage<DeploymentView>, CatalogAdminError> {
        let Some(read) = self.begin(actor, organization_id, release_id).await? else {
            return Ok(empty());
        };
        let filters = filters(
            read.release_id,
            serde_json::json!({
                "provider_account_id": query.provider_account_id,
                "model": query.model,
            }),
        );
        let after = read.decode(page, DEPLOYMENT_COLLECTION, &filters, ORDER_NAME)?;
        let (items, has_more) = self
            .store
            .list_deployments(
                organization_id,
                read.release_id,
                query,
                page.page_size,
                after.as_deref(),
            )
            .await?;
        let next = match items.last() {
            Some(last) if has_more => Some(read.encode(
                DEPLOYMENT_COLLECTION,
                &filters,
                ORDER_NAME,
                last.name.clone(),
            )?),
            _ => None,
        };
        Ok(read.page(items, next))
    }

    pub async fn list_routes(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Option<Uuid>,
        query: &RouteQuery,
        page: &PageRequest,
    ) -> Result<CatalogPage<RouteView>, CatalogAdminError> {
        let Some(read) = self.begin(actor, organization_id, release_id).await? else {
            return Ok(empty());
        };
        let filters = filters(
            read.release_id,
            serde_json::json!({
                "alias": query.alias,
                "operation": query.operation.map(CatalogOperation::as_str),
            }),
        );
        let keyset = read
            .decode(page, ROUTE_COLLECTION, &filters, ORDER_ALIAS_OPERATION)?
            .map(|claim| RouteKeyset::parse(&claim))
            .transpose()?;
        let (items, has_more) = self
            .store
            .list_routes(
                organization_id,
                read.release_id,
                query,
                page.page_size,
                keyset,
            )
            .await?;
        let next = match items.last() {
            Some(last) if has_more => Some(read.encode(
                ROUTE_COLLECTION,
                &filters,
                ORDER_ALIAS_OPERATION,
                RouteKeyset::of(last).to_claim(),
            )?),
            _ => None,
        };
        Ok(read.page(items, next))
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<(), CatalogAdminError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if !context.allows(
            AdminPermission::CatalogRead,
            AdminScope::Organization(organization_id),
        ) {
            return Err(CatalogAdminError::Forbidden);
        }
        Ok(())
    }

    /// Checks access and resolves the release. `None` means the caller named
    /// no release and the organization has none active yet.
    async fn begin(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        release_id: Option<Uuid>,
    ) -> Result<Option<Read<'_>>, CatalogAdminError> {
        self.authorize(actor, organization_id).await?;
        let codec = self.codec()?;
        let release_id = match release_id {
            Some(release_id) => {
                if !self
                    .store
                    .release_exists(organization_id, release_id)
                    .await?
                {
                    return Err(CatalogAdminError::NotFound);
                }
                release_id
            }
            None => match self.store.active_release(organization_id).await? {
                Some(release_id) => release_id,
                None => return Ok(None),
            },
        };
        Ok(Some(Read {
            codec,
            actor,
            scope: AdminScope::Organization(organization_id),
            release_id,
        }))
    }

    fn codec(&self) -> Result<&SignedCursorCodec, CatalogAdminError> {
        self.codec
            .as_ref()
            .ok_or(CatalogAdminError::AuthenticationUnavailable)
    }
}

impl Read<'_> {
    fn decode(
        &self,
        page: &PageRequest,
        collection: &str,
        filters: &str,
        order: &str,
    ) -> Result<Option<String>, CatalogAdminError> {
        decode(
            self.codec, page, self.actor, self.scope, collection, filters, order,
        )
    }

    fn encode(
        &self,
        collection: &str,
        filters: &str,
        order: &str,
        keyset: String,
    ) -> Result<String, CatalogAdminError> {
        encode(
            self.codec, self.actor, self.scope, collection, filters, order, keyset,
        )
    }

    fn page<T>(&self, items: Vec<T>, next_page_token: Option<String>) -> CatalogPage<T> {
        CatalogPage {
            release_id: Some(self.release_id),
            items,
            next_page_token,
        }
    }
}

fn empty<T>() -> CatalogPage<T> {
    CatalogPage {
        release_id: None,
        items: Vec::new(),
        next_page_token: None,
    }
}

/// Canonical cursor binding: the resolved release plus the JSON-encoded
/// filters, so filter values can never collide through delimiters.
fn filters(release_id: Uuid, filters: serde_json::Value) -> String {
    serde_json::json!({ "release_id": release_id, "filters": filters }).to_string()
}

fn decode(
    codec: &SignedCursorCodec,
    page: &PageRequest,
    actor: UserAccountId,
    scope: AdminScope,
    collection: &str,
    filters: &str,
    order: &str,
) -> Result<Option<String>, CatalogAdminError> {
    let Some(token) = page.page_token.as_deref() else {
        return Ok(None);
    };
    let claims = codec
        .decode_and_validate(token, actor, scope, collection, filters, order)
        .map_err(CatalogAdminError::Pagination)?;
    Ok((!claims.last_keyset.is_empty()).then_some(claims.last_keyset))
}

fn encode(
    codec: &SignedCursorCodec,
    actor: UserAccountId,
    scope: AdminScope,
    collection: &str,
    filters: &str,
    order: &str,
    keyset: String,
) -> Result<String, CatalogAdminError> {
    let claims = CursorClaims::new(actor, scope, collection, filters, order, keyset, None)
        .map_err(CatalogAdminError::Pagination)?;
    codec.encode(&claims).map_err(CatalogAdminError::Pagination)
}

fn map_admin_error(error: AdminStoreError) -> CatalogAdminError {
    match error {
        AdminStoreError::Authorization(_) => CatalogAdminError::Forbidden,
        AdminStoreError::ResourceNotFound => CatalogAdminError::NotFound,
        other => CatalogAdminError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{catalog_sync, catalog_write};
    use crate::domain::admin::{AdminAuthorizationContext, AdminRole, AdminRoleBinding};

    #[test]
    fn catalog_read_is_org_wide_and_withheld_from_team_admins() {
        let org = OrganizationId::new();
        let user = UserAccountId::new();
        let scope = AdminScope::Organization(org);
        for (binding, allowed) in [
            (
                AdminRoleBinding::organization(org, user, AdminRole::OrganizationOwner),
                true,
            ),
            (
                AdminRoleBinding::organization(org, user, AdminRole::Operator),
                true,
            ),
            (
                AdminRoleBinding::organization(org, user, AdminRole::Auditor),
                true,
            ),
            (
                AdminRoleBinding::organization(org, user, AdminRole::ReadOnlyMember),
                true,
            ),
            (
                AdminRoleBinding::team(
                    org,
                    crate::domain::identity::TeamId::new(),
                    user,
                    AdminRole::TeamAdmin,
                ),
                false,
            ),
        ] {
            let context = AdminAuthorizationContext {
                actor_id: user,
                organization_id: org,
                bindings: vec![binding.clone()],
            };
            assert_eq!(
                context.allows(AdminPermission::CatalogRead, scope),
                allowed,
                "{binding:?}"
            );
        }
    }

    #[test]
    fn cursor_filters_bind_release_and_escape_values() {
        let release = Uuid::new_v4();
        let other = Uuid::new_v4();
        let collide = |alias: &str, operation: Option<&str>| {
            filters(
                release,
                serde_json::json!({ "alias": alias, "operation": operation }),
            )
        };
        assert_ne!(
            collide("a\",\"operation\":\"chat", None),
            collide("a", Some("chat"))
        );
        assert_ne!(
            filters(release, serde_json::json!({})),
            filters(other, serde_json::json!({}))
        );
    }

    #[test]
    fn openapi_contract_documents_every_route() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let mut documented: Vec<&str> = contract["paths"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        documented.sort_unstable();
        let mut routes = vec![
            ROUTE_RELEASES,
            ROUTE_PROVIDER_ACCOUNTS,
            ROUTE_DEPLOYMENTS,
            ROUTE_ROUTES,
            catalog_write::ROUTE_RELEASE_ACTIVATE,
            ROUTE_RELEASE_MANIFEST,
            catalog_write::ROUTE_CREDENTIALS,
            catalog_write::ROUTE_CREDENTIAL,
            ROUTE_CLOUD_TRUST,
            ROUTE_PRICE_OVERRIDES,
            catalog_write::ROUTE_PRICE_OVERRIDE_REVOKE,
            catalog_sync::ROUTE_SYNC_JOBS,
            catalog_sync::ROUTE_SYNC_JOB,
            catalog_sync::ROUTE_DISCOVERED_MODELS,
            catalog_sync::ROUTE_DEPLOYABLE_MODELS,
            catalog_sync::ROUTE_DISCOVERY_DRIFT,
            catalog_sync::ROUTE_PRICE_SOURCES,
            catalog_sync::ROUTE_PRICE_SOURCE_FETCH,
            catalog_sync::ROUTE_PRICE_DRIFT,
            catalog_sync::ROUTE_PRICE_OBSERVATIONS,
            catalog_sync::ROUTE_PRICE_MAPPINGS,
            catalog_sync::ROUTE_PRICE_MAPPING,
            crate::application::auto_offer::ROUTE_AUTO_OFFER,
            crate::application::model_agreements::ROUTE_AGREEMENTS,
            crate::application::model_agreements::ROUTE_AGREEMENTS_ACCEPT,
            crate::application::price_adoption::ROUTE_PRICE_POLICY,
            crate::application::price_adoption::ROUTE_PRICE_HOLDS,
            crate::application::price_adoption::ROUTE_PRICE_HOLD_DISMISS,
            crate::application::routing_policy::ROUTE_ROUTING_POLICY,
        ];
        routes.sort_unstable();
        assert_eq!(documented, routes);
    }
}
