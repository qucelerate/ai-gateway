// One record per model across providers (Phase 13, D-033): master records,
// the organization's links to them, and route proposals built from them.
//
// Reads need `catalog_read`; writes need `catalog_write` and run through the
// admin mutation coordinator, so each is idempotent (`Idempotency-Key`),
// versioned (`If-Match`: `"v0"` when new, `"v<n>"` after) and audited in the
// same transaction. Deletes are soft. Links and records are catalog
// metadata: nothing here changes routing, pricing or configuration versions,
// and a route proposal reaches routing only when an admin publishes it as a
// release.

use std::{collections::HashMap, sync::Arc};

use serde::Serialize;
use uuid::Uuid;

use crate::application::admin_identities::deterministic_target_id;
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditSnapshot, AuditTargetResource},
    catalog::{CatalogManifest, CatalogOperation, ProviderFamily, RouteManifest},
    identity::{OrganizationId, UserAccountId},
    model_card::{CardAvailability, CardDeployment, CardOperation, CardRoute, valid_model},
    model_card_files::{CardLibrary, FileOrigin},
    model_master::{
        Exclusion, MasterLink, Modality, OrgMaster, PolicyLists, ProposalDeployment,
        ProposalOperation, ProposedRoute, ResolvedBy, RouteChange, RouteSuggestion, default_alias,
        diff, org_slug, propose, resolve, valid_master_id, valid_slug,
    },
    version::{IfMatch, VersionConflict},
};
use crate::persistence::{
    admin::{AdminMutationRequest, AdminStore, AdminStoreError, MutationStart},
    catalog_admin::CatalogAdminStore,
    model_cards::ModelCardStore,
    model_masters::ModelMasterStore,
    routing_policy::RoutingPolicyStore,
};

pub const ROUTE_MODEL_MASTERS: &str = "/api/admin/v1/organizations/{org}/catalog/model-masters";
pub const ROUTE_MODEL_MASTER: &str =
    "/api/admin/v1/organizations/{org}/catalog/model-masters/{master}";
pub const ROUTE_ROUTE_PROPOSAL: &str =
    "/api/admin/v1/organizations/{org}/catalog/model-masters/{master}/route-proposal";
pub const ROUTE_MODEL_MASTER_LINK: &str =
    "/api/admin/v1/organizations/{org}/catalog/model-master-links/{family}/{model}";

/// The longest alias, as manifests allow.
const MAX_ALIAS_LEN: usize = 128;

#[derive(Debug)]
pub enum ModelMasterError {
    Forbidden,
    /// Not a master id (`<vendor>/<model>`), or, for a write, not an
    /// organization record (`org/<slug>`).
    InvalidMaster,
    InvalidName,
    InvalidFamily,
    InvalidModel,
    InvalidOperation,
    InvalidAlias,
    /// No card or organization record has that id.
    UnknownMaster,
    /// The deployment serves none of the master's modality.
    ModalityMismatch(Modality),
    /// The active release has no deployment of that family and model.
    DeploymentNotFound,
    /// Links still name the record.
    MasterInUse(i64),
    NoActiveRelease,
    NotFound,
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(sqlx::Error),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for ModelMasterError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error)
    }
}

/// A deployment of the active release and its master.
#[derive(Clone, Debug, Serialize)]
pub struct MasterDeployment {
    pub deployment: String,
    pub model: String,
    pub provider_family: String,
    /// The provider account.
    pub adapter_instance_key: String,
    pub location: Option<String>,
    /// Its master; `null` for none.
    pub master: Option<String>,
    pub resolved_by: ResolvedBy,
    /// The card that lists its family and model, whether or not a link
    /// overrides it.
    pub card: Option<String>,
    /// The organization's link, when one was ever set: in use, or cleared
    /// (`cleared`, so the card decides) with the version to set it again at.
    pub link: Option<LinkRef>,
    /// Operations with limits, tool calling and the price billed now.
    pub operations: Vec<CardOperation>,
    pub availability: Option<CardAvailability>,
    /// The aliases that route to it.
    pub routes: Vec<CardRoute>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LinkRef {
    pub master: Option<String>,
    pub version: u64,
    pub cleared: bool,
}

/// A master record.
#[derive(Clone, Debug, Serialize)]
pub struct MasterView {
    pub id: String,
    pub name: String,
    /// `card` or `organization`.
    pub kind: &'static str,
    /// The card's vendor; `null` for an organization record.
    pub vendor: Option<String>,
    /// Where a card's file comes from; `null` for an organization record.
    pub origin: Option<FileOrigin>,
    pub modality: Modality,
    /// An organization record's version (its `ETag`); `null` for a card.
    pub version: Option<u64>,
    /// Its linked deployments in the active release.
    pub deployments: Vec<MasterDeployment>,
    /// Linked deployments the master's own aliases (see
    /// [`route_suggestions`]) leave out: computed, read-only; nothing
    /// changes a route until a release is published (D-033).
    pub suggestions: Vec<RouteSuggestion>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MasterList {
    /// The active release; `null` when the organization has none.
    pub release_id: Option<Uuid>,
    /// Every card and organization record, by id.
    pub items: Vec<MasterView>,
    /// Deployments of the active release with no master.
    pub unlinked: Vec<MasterDeployment>,
    /// Every link the organization has set, served now or not, cleared
    /// ones included (`cleared`, each with the version to set it again at).
    pub links: Vec<MasterLink>,
    /// The organization's deleted records, each with the version to create
    /// it again at.
    pub deleted_records: Vec<OrgMaster>,
}

/// A route proposal for one master.
#[derive(Clone, Debug, Serialize)]
pub struct RouteProposal {
    pub master: String,
    pub release_id: Uuid,
    pub alias: String,
    /// Whether the active release routes the alias already (for any
    /// operation); `changes` then compares with its routes.
    pub alias_exists: bool,
    pub operations: Vec<String>,
    pub tools: bool,
    pub routes: Vec<ProposedRoute>,
    pub excluded: Vec<Exclusion>,
    /// The alias's routes of these operations in the active release.
    pub existing: Vec<ProposedRoute>,
    /// The proposal compared with `existing`.
    pub changes: Vec<RouteChange>,
    /// Why the active release with these routes would not publish; `null`
    /// when it would.
    pub problem: Option<String>,
}

#[derive(Clone)]
pub struct ModelMasterService {
    admin: AdminStore,
    cards: ModelCardStore,
    masters: ModelMasterStore,
    catalog: CatalogAdminStore,
    routing_policy: RoutingPolicyStore,
    library: Arc<CardLibrary>,
}

impl ModelMasterService {
    pub fn new(
        admin: AdminStore,
        cards: ModelCardStore,
        masters: ModelMasterStore,
        catalog: CatalogAdminStore,
        routing_policy: RoutingPolicyStore,
        library: Arc<CardLibrary>,
    ) -> Self {
        Self {
            admin,
            cards,
            masters,
            catalog,
            routing_policy,
            library,
        }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), ModelMasterError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(ModelMasterError::Forbidden)
        }
    }

    /// The active release's deployments, each with its master.
    async fn resolved(
        &self,
        organization_id: OrganizationId,
        release_id: Uuid,
        links: &[MasterLink],
    ) -> Result<Vec<MasterDeployment>, ModelMasterError> {
        let by_key: HashMap<(&str, &str), &MasterLink> = links
            .iter()
            .map(|link| ((link.provider_family.as_str(), link.model.as_str()), link))
            .collect();
        let deployments = self
            .cards
            .deployments_of(organization_id, release_id, None)
            .await?;
        Ok(deployments
            .into_iter()
            .map(|(model, deployment)| self.resolve_one(&by_key, model, deployment))
            .collect())
    }

    fn resolve_one(
        &self,
        links: &HashMap<(&str, &str), &MasterLink>,
        model: String,
        deployment: CardDeployment,
    ) -> MasterDeployment {
        let family = deployment.provider_family.as_str();
        // The card lists the catalog's identifier or, where a deployment is
        // named apart from its model (Azure), the one its listing names.
        let listed = deployment
            .provider_listing
            .as_ref()
            .and_then(|listing| listing.model.as_deref());
        let card = std::iter::once(model.as_str())
            .chain(listed)
            .find_map(|candidate| self.library.card(family, candidate))
            .map(|(card, _)| card.id.clone());
        let link = links.get(&(family, model.as_str())).copied();
        // A cleared link is kept for its version, and leaves the card to
        // decide.
        let in_use = link.filter(|link| !link.cleared);
        let (master, resolved_by) =
            resolve(in_use.map(|link| link.master.as_deref()), card.as_deref());
        let master = master.map(str::to_owned);
        MasterDeployment {
            master,
            resolved_by,
            card,
            link: link.map(|link| LinkRef {
                master: link.master.clone(),
                version: link.version,
                cleared: link.cleared,
            }),
            deployment: deployment.deployment,
            model,
            provider_family: deployment.provider_family,
            adapter_instance_key: deployment.adapter_instance_key,
            location: deployment.location,
            operations: deployment.operations,
            availability: deployment.availability,
            routes: deployment.routes,
        }
    }

    /// Every master record with its linked deployments in the active
    /// release, the deployments with none, and every link. Needs
    /// `catalog_read`.
    pub async fn list(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<MasterList, ModelMasterError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        let links = self.masters.every_link(organization_id).await?;
        let records = self.masters.records(organization_id).await?;
        let deleted_records = self.masters.deleted_records(organization_id).await?;
        let release_id = self.cards.active_release(organization_id).await?;
        let deployments = match release_id {
            Some(release_id) => self.resolved(organization_id, release_id, &links).await?,
            None => Vec::new(),
        };
        let mut items: Vec<MasterView> = self
            .library
            .cards()
            .map(|(card, origin)| MasterView {
                id: card.id.clone(),
                name: card.name.clone(),
                kind: "card",
                vendor: Some(card.vendor.clone()),
                origin: Some(origin),
                modality: card.modality,
                version: None,
                deployments: Vec::new(),
                suggestions: Vec::new(),
            })
            .chain(records.into_iter().map(org_view))
            .collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        let mut unlinked = Vec::new();
        for deployment in deployments {
            match deployment
                .master
                .as_deref()
                .and_then(|master| items.iter_mut().find(|item| item.id == master))
            {
                Some(item) => item.deployments.push(deployment),
                // No master, or a link to one that is gone (a card file an
                // operator removed).
                None => unlinked.push(deployment),
            }
        }
        for item in &mut items {
            item.suggestions = route_suggestions(&item.id, &item.deployments);
        }
        Ok(MasterList {
            release_id,
            items,
            unlinked,
            links,
            deleted_records,
        })
    }

    /// One master record and its linked deployments. Needs `catalog_read`.
    pub async fn master(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: &str,
    ) -> Result<MasterView, ModelMasterError> {
        if !valid_master_id(id) {
            return Err(ModelMasterError::InvalidMaster);
        }
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        let mut view = self
            .view(organization_id, id)
            .await?
            .ok_or(ModelMasterError::NotFound)?;
        if let Some(release_id) = self.cards.active_release(organization_id).await? {
            let links = self.masters.every_link(organization_id).await?;
            view.deployments = self
                .resolved(organization_id, release_id, &links)
                .await?
                .into_iter()
                .filter(|d| d.master.as_deref() == Some(id))
                .collect();
            view.suggestions = route_suggestions(&view.id, &view.deployments);
        }
        Ok(view)
    }

    /// The card or organization record `id`, without deployments.
    async fn view(
        &self,
        organization_id: OrganizationId,
        id: &str,
    ) -> Result<Option<MasterView>, ModelMasterError> {
        if let Some(slug) = org_slug(id) {
            return Ok(self
                .masters
                .record(organization_id, slug)
                .await?
                .map(org_view));
        }
        Ok(self
            .library
            .card_by_id(id)
            .map(|(card, origin)| MasterView {
                id: card.id.clone(),
                name: card.name.clone(),
                kind: "card",
                vendor: Some(card.vendor.clone()),
                origin: Some(origin),
                modality: card.modality,
                version: None,
                deployments: Vec::new(),
                suggestions: Vec::new(),
            }))
    }

    /// The organization's link for `family` and `model`, cleared or not
    /// (`cleared`: its version is the one to set it again at). Needs
    /// `catalog_read`; one never set is not found.
    pub async fn link(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
    ) -> Result<MasterLink, ModelMasterError> {
        check_family_and_model(family, model)?;
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        self.masters
            .link(organization_id, family, model)
            .await?
            .ok_or(ModelMasterError::NotFound)
    }

    /// Links `family` and `model` to `master` (`None`: to none). Needs
    /// `catalog_write`; idempotent, versioned and audited
    /// (`model_master_link.set`). The active release must serve the model on
    /// that family, in the master's modality.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_link(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
        master: Option<&str>,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(MasterLink, bool, bool), ModelMasterError> {
        check_family_and_model(family, model)?;
        if master.is_some_and(|master| !valid_master_id(master)) {
            return Err(ModelMasterError::InvalidMaster);
        }
        let target_id = link_id(organization_id, family, model);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_MASTER_LINK.into(),
            method: "PUT".into(),
            operation: operation("model_master_link.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelMasterLink,
            audit_action: audit_action("model_master_link.set"),
            if_match,
            request_id,
            body: serde_json::json!({
                "provider_family": family,
                "model": model,
                "master": master,
            }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(record) => {
                let link: MasterLink = replayed(record.response_metadata)?;
                return Ok((link, false, true));
            }
            MutationStart::New(mutation) => mutation,
        };
        // Checked once the request is not a replay, so a retry of a write
        // that went through replays it whatever changed since. Errors from
        // here on drop the mutation, which rolls it back.
        // What the active release serves of the model on the family, read
        // in the mutation's transaction: a second connection could wait
        // forever on a pool of one.
        let operations = ModelMasterStore::served_operations_in(
            mutation.transaction(),
            organization_id,
            family,
            model,
        )
        .await?;
        if operations.is_empty() {
            return Err(ModelMasterError::DeploymentNotFound);
        }
        let served: Vec<Modality> = operations
            .iter()
            .filter_map(|operation| Modality::of_operation(operation))
            .collect();
        let card_modality = match master {
            Some(master) if org_slug(master).is_none() => Some(
                self.library
                    .card_by_id(master)
                    .ok_or(ModelMasterError::UnknownMaster)?
                    .0
                    .modality,
            ),
            _ => None,
        };
        let tx = mutation.transaction();
        let modality = match master {
            Some(master) => match org_slug(master) {
                // Locked, so the record is not deleted under the link.
                Some(slug) => ModelMasterStore::record_live_for_link(tx, organization_id, slug)
                    .await?
                    .ok_or(ModelMasterError::UnknownMaster)?,
                None => card_modality.ok_or(ModelMasterError::UnknownMaster)?,
            }
            .into(),
            None => None,
        };
        if let Some(modality) = modality
            && !served.contains(&modality)
        {
            return Err(ModelMasterError::ModalityMismatch(modality));
        }
        // The link in use before (a cleared one is none).
        let before = ModelMasterStore::link_in(tx, organization_id, family, model)
            .await?
            .filter(|link| !link.cleared);
        let created = ModelMasterStore::set_link(
            tx,
            organization_id,
            target_id,
            family,
            model,
            master,
            actor.0,
        )
        .await?;
        let next = mutation.next_version();
        let tx = mutation.transaction();
        let stored = MasterLink {
            version: next.get(),
            ..ModelMasterStore::link_in(tx, organization_id, family, model)
                .await?
                .ok_or(ModelMasterError::NotFound)?
        };
        let snapshot = snapshot_of(&stored)?;
        mutation
            .commit(
                before.map(|before| AuditSnapshot {
                    // Whether the link named a master (false: to none).
                    enabled: Some(before.master.is_some()),
                    version: previous(next.get()),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    // Whether the link names a master (false: to none).
                    enabled: Some(stored.master.is_some()),
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: Some(snapshot),
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok((stored, created, false))
    }

    /// Clears the organization's link for `family` and `model` (soft), so
    /// the card that matches decides again. Needs `catalog_write`;
    /// idempotent, versioned and audited (`model_master_link.clear`).
    /// Returns whether this was a replay.
    #[allow(clippy::too_many_arguments)]
    pub async fn clear_link(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        family: &str,
        model: &str,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<bool, ModelMasterError> {
        check_family_and_model(family, model)?;
        let target_id = link_id(organization_id, family, model);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_MASTER_LINK.into(),
            method: "DELETE".into(),
            operation: operation("model_master_link.clear"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelMasterLink,
            audit_action: audit_action("model_master_link.clear"),
            if_match,
            request_id,
            body: serde_json::json!({ "provider_family": family, "model": model }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(_) => return Ok(true),
            MutationStart::New(mutation) => mutation,
        };
        let next = mutation.next_version();
        let tx = mutation.transaction();
        if !ModelMasterStore::clear_link(tx, organization_id, family, model, actor.0).await? {
            return Err(ModelMasterError::NotFound);
        }
        mutation
            .commit(
                Some(AuditSnapshot {
                    enabled: Some(true),
                    version: previous(next.get()),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    // No link in use any more.
                    enabled: Some(false),
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: None,
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok(false)
    }

    /// Creates the organization record `id` (`org/<slug>`), renames it or
    /// changes its modality, or brings back a deleted one. Needs
    /// `catalog_write`; idempotent, versioned and audited
    /// (`model_master.set`). Changing the modality of a record that links
    /// name is refused, as a link to it would then be of another modality.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_record(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: &str,
        name: &str,
        modality: Modality,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(OrgMaster, bool, bool), ModelMasterError> {
        let slug = writable_slug(id)?;
        let name = crate::domain::model_master::normalized_name(name)
            .ok_or(ModelMasterError::InvalidName)?;
        let target_id = record_id(organization_id, slug);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_MASTER.into(),
            method: "PUT".into(),
            operation: operation("model_master.set"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelMaster,
            audit_action: audit_action("model_master.set"),
            if_match,
            request_id,
            body: serde_json::json!({ "id": id, "name": name, "modality": modality }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(record) => {
                let record: OrgMaster = replayed(record.response_metadata)?;
                return Ok((record, false, true));
            }
            MutationStart::New(mutation) => mutation,
        };
        let tx = mutation.transaction();
        let before = ModelMasterStore::record_in(tx, organization_id, slug).await?;
        if let Some(before) = &before
            && before.modality != modality
        {
            let links = ModelMasterStore::links_to(tx, organization_id, id).await?;
            if links > 0 {
                return Err(ModelMasterError::MasterInUse(links));
            }
        }
        let created = ModelMasterStore::put_record(
            tx,
            organization_id,
            target_id,
            slug,
            &name,
            modality,
            actor.0,
        )
        .await?;
        let next = mutation.next_version();
        let tx = mutation.transaction();
        let stored = OrgMaster {
            version: next.get(),
            ..ModelMasterStore::record_in(tx, organization_id, slug)
                .await?
                .ok_or(ModelMasterError::NotFound)?
        };
        let snapshot = snapshot_of(&stored)?;
        mutation
            .commit(
                before.map(|_| AuditSnapshot {
                    enabled: Some(true),
                    version: previous(next.get()),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    enabled: Some(true),
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: Some(snapshot),
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok((stored, created, false))
    }

    /// Deletes the organization record `id` (soft; its revisions are kept).
    /// Refused while links name it: clear or change them first. Needs
    /// `catalog_write`; idempotent, versioned and audited
    /// (`model_master.delete`). Returns whether this was a replay.
    pub async fn delete_record(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: &str,
        if_match: IfMatch,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<bool, ModelMasterError> {
        let slug = writable_slug(id)?;
        let target_id = record_id(organization_id, slug);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_MASTER.into(),
            method: "DELETE".into(),
            operation: operation("model_master.delete"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelMaster,
            audit_action: audit_action("model_master.delete"),
            if_match,
            request_id,
            body: serde_json::json!({ "id": id }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(_) => return Ok(true),
            MutationStart::New(mutation) => mutation,
        };
        let next = mutation.next_version();
        let tx = mutation.transaction();
        let links = ModelMasterStore::links_to(tx, organization_id, id).await?;
        if links > 0 {
            return Err(ModelMasterError::MasterInUse(links));
        }
        if !ModelMasterStore::delete_record(tx, organization_id, slug, actor.0).await? {
            return Err(ModelMasterError::NotFound);
        }
        mutation
            .commit(
                Some(AuditSnapshot {
                    enabled: Some(true),
                    version: previous(next.get()),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    enabled: Some(false),
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: None,
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok(false)
    }

    /// A route proposal for master `id`: for each of `operations`, every
    /// linked deployment of the active release that serves it, at weight
    /// and priority 1, and each one left out with why (see
    /// [`crate::domain::model_master::propose`]). The routes are checked as
    /// the active release with them would be when published, by the
    /// manifest's own validation. Read-only: publishing is a release, made
    /// and activated through the catalog API. Needs `catalog_read`.
    pub async fn propose(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        id: &str,
        operations: &[String],
        tools: bool,
        alias: Option<&str>,
    ) -> Result<RouteProposal, ModelMasterError> {
        if !valid_master_id(id) {
            return Err(ModelMasterError::InvalidMaster);
        }
        let mut wanted: Vec<String> = Vec::new();
        for operation in operations {
            // Streaming is a mode of chat (D-020): it takes the chat route.
            let parsed = operation
                .parse::<CatalogOperation>()
                .ok()
                .filter(|parsed| CatalogOperation::DECLARED.contains(parsed))
                .ok_or(ModelMasterError::InvalidOperation)?;
            if !wanted.iter().any(|w| w == parsed.as_str()) {
                wanted.push(parsed.as_str().to_owned());
            }
        }
        if wanted.is_empty() {
            return Err(ModelMasterError::InvalidOperation);
        }
        let alias = alias.unwrap_or_else(|| default_alias(id)).to_owned();
        if alias.trim().is_empty() || alias.len() > MAX_ALIAS_LEN {
            return Err(ModelMasterError::InvalidAlias);
        }
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        if self.view(organization_id, id).await?.is_none() {
            return Err(ModelMasterError::NotFound);
        }
        let release_id = self
            .cards
            .active_release(organization_id)
            .await?
            .ok_or(ModelMasterError::NoActiveRelease)?;
        let links = self.masters.every_link(organization_id).await?;
        let linked: Vec<ProposalDeployment> = self
            .resolved(organization_id, release_id, &links)
            .await?
            .into_iter()
            .filter(|d| d.master.as_deref() == Some(id))
            .map(|d| ProposalDeployment {
                operations: folded_operations(&d.operations),
                availability: d.availability.as_ref().map(|a| a.state.clone()),
                deployment: d.deployment,
                provider_family: d.provider_family,
                model: d.model,
                location: d.location,
            })
            .collect();
        let policy = self.routing_policy.policy(organization_id).await?;
        let (routes, excluded) = propose(
            &wanted,
            tools,
            &linked,
            PolicyLists {
                providers: policy.providers.as_deref(),
                locations: policy.locations.as_deref(),
            },
        );
        let mut manifest = self
            .catalog
            .release_manifest(organization_id, release_id)
            .await
            .map_err(|error| match error {
                crate::persistence::catalog_admin::CatalogAdminStoreError::Database(error) => {
                    ModelMasterError::Store(error)
                }
                crate::persistence::catalog_admin::CatalogAdminStoreError::InvalidState => {
                    ModelMasterError::Store(sqlx::Error::Protocol("release manifest".into()))
                }
            })?
            .ok_or(ModelMasterError::NoActiveRelease)?;
        // As the console builds the next release: streaming folded into
        // chat, then the alias's routes of these operations replaced.
        manifest.fold_streaming_into_chat();
        let alias_exists = manifest.routes.iter().any(|route| route.alias == alias);
        let existing: Vec<ProposedRoute> = manifest
            .routes
            .iter()
            .filter(|route| route.alias == alias && wanted.contains(&route.operation))
            .map(|route| ProposedRoute {
                operation: route.operation.clone(),
                deployment: route.deployment.clone(),
                weight: route.weight,
                priority: route.priority,
            })
            .collect();
        let problem = apply_routes(&mut manifest, &alias, &wanted, &routes)
            .validate()
            .err()
            .map(|error| error.to_string());
        Ok(RouteProposal {
            master: id.to_owned(),
            release_id,
            changes: diff(&existing, &routes),
            alias,
            alias_exists,
            operations: wanted,
            tools,
            routes,
            excluded,
            existing,
            problem,
        })
    }
}

/// `manifest` with `alias`'s routes of `operations` replaced by `routes`:
/// the release a proposal publishes as.
pub fn apply_routes<'a>(
    manifest: &'a mut CatalogManifest,
    alias: &str,
    operations: &[String],
    routes: &[ProposedRoute],
) -> &'a mut CatalogManifest {
    manifest
        .routes
        .retain(|route| route.alias != alias || !operations.contains(&route.operation));
    manifest
        .routes
        .extend(routes.iter().map(|route| RouteManifest {
            weight: route.weight,
            priority: route.priority,
            ..RouteManifest::new(alias, &route.operation, &route.deployment)
        }));
    manifest
}

/// A deployment's operations as a proposal sees them: streaming folded
/// into chat as the manifest folds it (D-020), so a release from before
/// that, with only `chat_stream`, is proposed (or left out with a reason)
/// for chat.
fn folded_operations(operations: &[CardOperation]) -> Vec<ProposalOperation> {
    let has_chat = operations.iter().any(|o| o.operation == "chat");
    operations
        .iter()
        .filter_map(|operation| {
            let name = match operation.operation.as_str() {
                "chat_stream" if has_chat => return None,
                "chat_stream" => "chat".to_owned(),
                other => other.to_owned(),
            };
            Some(ProposalOperation {
                operation: name,
                tools: operation.tools,
                currency: operation
                    .price
                    .as_ref()
                    .and_then(|p| p.release_currency.clone()),
                billed_currency: operation.price.as_ref().map(|p| p.currency.clone()),
            })
        })
        .collect()
}

/// The deployments of master `master` its aliases leave out. Its aliases,
/// per operation, are those that already route two or more of its
/// deployments (it balances them across providers), and its default alias
/// (`claude-sonnet-4-6`, as a proposal names it) where the release routes
/// one of them by it. An alias that routes one deployment under another
/// name (each discovered model gets its own) is that deployment's, not the
/// master's. For each, every other deployment of the master that serves
/// the operation with a price and is not on it. `chat_stream` of a release
/// from before D-020 is chat.
pub fn route_suggestions(master: &str, deployments: &[MasterDeployment]) -> Vec<RouteSuggestion> {
    let fold = |operation: &str| {
        if operation == "chat_stream" {
            "chat".to_owned()
        } else {
            operation.to_owned()
        }
    };
    let routed: std::collections::BTreeSet<(String, String, String)> = deployments
        .iter()
        .flat_map(|d| {
            d.routes.iter().map(|route| {
                (
                    route.alias.clone(),
                    fold(&route.operation),
                    d.deployment.clone(),
                )
            })
        })
        .collect();
    let mut routing: std::collections::BTreeMap<(String, String), usize> =
        std::collections::BTreeMap::new();
    for (alias, operation, _) in &routed {
        *routing
            .entry((alias.clone(), operation.clone()))
            .or_default() += 1;
    }
    let default = default_alias(master);
    let aliases: Vec<(String, String)> = routing
        .into_iter()
        .filter(|((alias, _), count)| *count >= 2 || alias == default)
        .map(|(key, _)| key)
        .collect();
    let mut suggestions = Vec::new();
    for (alias, operation) in &aliases {
        for deployment in deployments {
            let priced = deployment
                .operations
                .iter()
                .any(|served| fold(&served.operation) == *operation && served.price.is_some());
            if priced
                && !routed.contains(&(
                    alias.clone(),
                    operation.clone(),
                    deployment.deployment.clone(),
                ))
            {
                suggestions.push(RouteSuggestion {
                    alias: alias.clone(),
                    operation: operation.clone(),
                    deployment: deployment.deployment.clone(),
                });
            }
        }
    }
    suggestions.sort_by(|a, b| {
        (&a.alias, &a.operation, &a.deployment).cmp(&(&b.alias, &b.operation, &b.deployment))
    });
    suggestions
}

fn org_view(record: OrgMaster) -> MasterView {
    MasterView {
        id: record.id,
        name: record.name,
        kind: "organization",
        vendor: None,
        origin: None,
        modality: record.modality,
        version: Some(record.version),
        deployments: Vec::new(),
        suggestions: Vec::new(),
    }
}

fn check_family_and_model(family: &str, model: &str) -> Result<(), ModelMasterError> {
    if family.parse::<ProviderFamily>().is_err() {
        return Err(ModelMasterError::InvalidFamily);
    }
    if !valid_model(model) {
        return Err(ModelMasterError::InvalidModel);
    }
    Ok(())
}

/// The slug of an organization record id; a card's id cannot be written.
fn writable_slug(id: &str) -> Result<&str, ModelMasterError> {
    id.strip_prefix(crate::domain::model_master::ORG_PREFIX)
        .filter(|slug| valid_slug(slug))
        .ok_or(ModelMasterError::InvalidMaster)
}

/// A link's resource id: one per organization, family and model.
pub fn link_id(organization_id: OrganizationId, family: &str, model: &str) -> Uuid {
    deterministic_target_id(&[
        "model_master_link",
        &organization_id.0.to_string(),
        family,
        model,
    ])
}

/// An organization record's resource id: one per organization and slug.
pub fn record_id(organization_id: OrganizationId, slug: &str) -> Uuid {
    deterministic_target_id(&["model_master", &organization_id.0.to_string(), slug])
}

fn previous(next: u64) -> Option<u64> {
    next.checked_sub(1).filter(|version| *version > 0)
}

fn snapshot_of<T: Serialize>(value: &T) -> Result<serde_json::Value, ModelMasterError> {
    serde_json::to_value(value).map_err(|_| {
        ModelMasterError::Store(sqlx::Error::Protocol(
            "response snapshot is not serializable".into(),
        ))
    })
}

fn replayed<T: serde::de::DeserializeOwned>(
    metadata: Option<IdempotencyResultMetadata>,
) -> Result<T, ModelMasterError> {
    let snapshot = metadata
        .and_then(|metadata| metadata.snapshot)
        .ok_or(ModelMasterError::NotFound)?;
    serde_json::from_value(snapshot).map_err(|_| {
        ModelMasterError::Store(sqlx::Error::Protocol(
            "recorded response snapshot is invalid".into(),
        ))
    })
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> ModelMasterError {
    match error {
        AdminStoreError::Authorization(_) => ModelMasterError::Forbidden,
        AdminStoreError::ResourceNotFound => ModelMasterError::NotFound,
        AdminStoreError::VersionConflict(conflict) => ModelMasterError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => ModelMasterError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => ModelMasterError::IdempotencyInProgress,
        other => ModelMasterError::Admin(other),
    }
}

/// When the API writes the card files' exact matches (`model_card_match`,
/// migration 0141) that usage is stamped with: retried after a failure,
/// doubling from `first_retry` to `max_retry`, and written again every
/// `resync` after a success. A failed write leaves the previous matches
/// (none on a new install, or before 0141 is applied) and card-matched
/// requests are stamped with no master until one succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CardMatchSchedule {
    pub first_retry: std::time::Duration,
    pub max_retry: std::time::Duration,
    pub resync: std::time::Duration,
}

impl Default for CardMatchSchedule {
    fn default() -> Self {
        Self {
            first_retry: std::time::Duration::from_secs(1),
            max_retry: std::time::Duration::from_secs(60),
            resync: std::time::Duration::from_secs(300),
        }
    }
}

impl CardMatchSchedule {
    /// The wait after `failures` failed writes in a row (0: after a success).
    pub fn delay(&self, failures: u32) -> std::time::Duration {
        if failures == 0 {
            return self.resync;
        }
        self.first_retry
            .saturating_mul(2_u32.saturating_pow(failures - 1))
            .min(self.max_retry)
    }
}

/// Writes the matches once, logs and counts the outcome, and returns the
/// failures in a row after it.
pub async fn sync_card_matches_once(
    store: &ModelMasterStore,
    matches: &crate::persistence::model_masters::CardMatches,
    failures: u32,
) -> u32 {
    use crate::metrics::{MODEL_CARD_MATCH_SYNCS, global as metrics};
    match store.store_card_matches(matches).await {
        Ok(stored) => {
            metrics().inc(&MODEL_CARD_MATCH_SYNCS, &["ok"]);
            if failures > 0 {
                tracing::info!(
                    matches = stored,
                    failures,
                    "stored the model cards' exact matches"
                );
            } else {
                tracing::debug!(matches = stored, "stored the model cards' exact matches");
            }
            0
        }
        Err(error) => {
            metrics().inc(&MODEL_CARD_MATCH_SYNCS, &["error"]);
            tracing::error!(
                error = %error,
                failures = failures + 1,
                "storing the model cards' exact matches failed; until it succeeds, requests \
                 a card would match are recorded with the matches stored before (none on a \
                 new install), and retried"
            );
            failures.saturating_add(1)
        }
    }
}

/// Keeps the matches written in the background: after `failures` failed
/// writes, waits as `schedule` says, writes again, and so on, for as long
/// as the runtime runs. Each API process runs one.
pub fn spawn_card_match_sync(
    store: ModelMasterStore,
    matches: crate::persistence::model_masters::CardMatches,
    schedule: CardMatchSchedule,
    mut failures: u32,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(schedule.delay(failures)).await;
            failures = sync_card_matches_once(&store, &matches, failures).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_and_actions_are_valid() {
        for value in [
            "model_master_link.set",
            "model_master_link.clear",
            "model_master.set",
            "model_master.delete",
        ] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }

    #[test]
    fn card_match_writes_back_off_then_resync() {
        let schedule = CardMatchSchedule::default();
        let secs = |failures| schedule.delay(failures).as_secs();
        assert_eq!(
            [secs(1), secs(2), secs(3), secs(7), secs(8), secs(40)],
            [1, 2, 4, 60, 60, 60]
        );
        assert_eq!(secs(0), 300);
    }

    #[test]
    fn suggestions_name_deployments_an_alias_of_the_master_leaves_out() {
        use crate::domain::model_card::CardPrice;
        let price = || CardPrice {
            currency: "USD".into(),
            release_currency: Some("USD".into()),
            input_minor_per_token: 1,
            output_minor_per_token: 1,
            cached_input_minor_per_token: 0,
            reasoning_minor_per_token: 0,
            other_minor_per_token: 0,
            billed_by: "release",
            source: None,
            source_version: None,
            confidence: "known".into(),
        };
        let deployment =
            |name: &str, ops: &[(&str, bool)], routes: &[(&str, &str)]| MasterDeployment {
                deployment: name.into(),
                model: name.into(),
                provider_family: "bedrock".into(),
                adapter_instance_key: "bedrock-main".into(),
                location: None,
                master: Some("anthropic/claude-sonnet-4-6".into()),
                resolved_by: ResolvedBy::Card,
                card: Some("anthropic/claude-sonnet-4-6".into()),
                link: None,
                operations: ops
                    .iter()
                    .map(|(operation, priced)| CardOperation {
                        operation: (*operation).into(),
                        max_input_bytes: 1,
                        max_output_tokens: 1,
                        tools: false,
                        price: priced.then(price),
                    })
                    .collect(),
                availability: None,
                routes: routes
                    .iter()
                    .map(|(alias, operation)| CardRoute {
                        alias: (*alias).into(),
                        operation: (*operation).into(),
                    })
                    .collect(),
            };
        let deployments = [
            // Routed as chat_stream by an older release: that is chat.
            deployment(
                "a",
                &[("chat", true), ("embeddings", true)],
                &[("sonnet", "chat_stream")],
            ),
            deployment("b", &[("chat", true)], &[]),
            // Not priced for chat: no route could take it.
            deployment("c", &[("chat", false)], &[]),
            deployment(
                "d",
                &[("chat", true), ("embeddings", true)],
                &[("sonnet", "chat"), ("claude-sonnet-4-6", "embeddings")],
            ),
        ];
        let suggest = |deployments: &[MasterDeployment]| -> Vec<(String, String, String)> {
            route_suggestions("anthropic/claude-sonnet-4-6", deployments)
                .into_iter()
                .map(|s| (s.alias, s.operation, s.deployment))
                .collect()
        };
        let found = suggest(&deployments);
        let row = |a: &str, o: &str, d: &str| (a.to_owned(), o.to_owned(), d.to_owned());
        // `sonnet` balances two of them; the default alias routes one.
        assert_eq!(
            found,
            [
                row("claude-sonnet-4-6", "embeddings", "a"),
                row("sonnet", "chat", "b"),
            ]
        );
        // Every deployment on the alias: nothing to suggest.
        assert!(suggest(&deployments[..1]).is_empty());

        // As discovery adds them, each deployment has its own alias (its
        // model id): none is the master's, so nothing is suggested...
        let mut own: Vec<MasterDeployment> =
            ["anthropic-sonnet", "us.anthropic.sonnet", "foundry-sonnet"]
                .into_iter()
                .map(|name| deployment(name, &[("chat", true)], &[(name, "chat")]))
                .collect();
        assert!(suggest(&own).is_empty());
        // ...until an alias balances two: then only it names the third.
        own[0].routes.push(CardRoute {
            alias: "balanced".into(),
            operation: "chat".into(),
        });
        own[1].routes.push(CardRoute {
            alias: "balanced".into(),
            operation: "chat".into(),
        });
        assert_eq!(suggest(&own), [row("balanced", "chat", "foundry-sonnet")]);
    }

    #[test]
    fn streaming_is_proposed_as_chat_as_the_manifest_folds_it() {
        use crate::domain::model_card::CardPrice;
        let operation = |name: &str, currency: Option<(&str, &str)>| CardOperation {
            operation: name.into(),
            max_input_bytes: 1,
            max_output_tokens: 1,
            tools: false,
            price: currency.map(|(release, billed)| CardPrice {
                currency: billed.into(),
                release_currency: Some(release.into()),
                input_minor_per_token: 1,
                output_minor_per_token: 1,
                cached_input_minor_per_token: 0,
                reasoning_minor_per_token: 0,
                other_minor_per_token: 0,
                billed_by: "release",
                source: None,
                source_version: None,
                confidence: "known".into(),
            }),
        };
        // A release from before D-020, with only chat_stream: it is chat.
        let folded = folded_operations(&[operation("chat_stream", Some(("USD", "EUR")))]);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].operation, "chat");
        assert_eq!(folded[0].currency.as_deref(), Some("USD"));
        assert_eq!(folded[0].billed_currency.as_deref(), Some("EUR"));
        // With chat as well, chat's own entry stands.
        let folded = folded_operations(&[
            operation("chat", None),
            operation("chat_stream", Some(("USD", "USD"))),
            operation("embeddings", Some(("USD", "USD"))),
        ]);
        let names: Vec<&str> = folded.iter().map(|o| o.operation.as_str()).collect();
        assert_eq!(names, ["chat", "embeddings"]);
        assert_eq!(folded[0].currency, None);
    }

    #[test]
    fn the_contract_documents_every_route_and_shape() {
        use crate::application::model_cards::tests::check;
        use crate::domain::model_card::{CardPrice, GovernanceActor};
        use crate::domain::model_master::{ExclusionReason, RouteChangeKind, RouteWeight};
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-model-cards-v1.json")).unwrap();
        for route in [
            ROUTE_MODEL_MASTERS,
            ROUTE_MODEL_MASTER,
            ROUTE_ROUTE_PROPOSAL,
            ROUTE_MODEL_MASTER_LINK,
        ] {
            assert!(
                contract["paths"][route].is_object(),
                "{route} is not documented"
            );
        }
        let now = chrono::Utc::now();
        let actor = Some(GovernanceActor {
            actor_type: "user".into(),
            id: Some(Uuid::nil()),
            name: Some("Ada".into()),
        });
        let deployment = MasterDeployment {
            deployment: "bedrock-sonnet".into(),
            model: "us.anthropic.claude-sonnet-4-6".into(),
            provider_family: "bedrock".into(),
            adapter_instance_key: "bedrock-main".into(),
            location: Some("us-east-1".into()),
            master: Some("anthropic/claude-sonnet-4-6".into()),
            resolved_by: ResolvedBy::Link,
            card: Some("anthropic/claude-sonnet-4-6".into()),
            link: Some(LinkRef {
                master: Some("anthropic/claude-sonnet-4-6".into()),
                version: 2,
                cleared: false,
            }),
            operations: vec![CardOperation {
                operation: "chat".into(),
                max_input_bytes: 1,
                max_output_tokens: 1,
                tools: true,
                price: Some(CardPrice {
                    currency: "USD".into(),
                    release_currency: Some("USD".into()),
                    input_minor_per_token: 1,
                    output_minor_per_token: 1,
                    cached_input_minor_per_token: 0,
                    reasoning_minor_per_token: 0,
                    other_minor_per_token: 0,
                    billed_by: "release",
                    source: Some("aws".into()),
                    source_version: Some("v1".into()),
                    confidence: "known".into(),
                }),
            }],
            availability: Some(CardAvailability {
                state: "verified".into(),
                reason: None,
                source: "traffic".into(),
                observed_at: now,
                verified_at: Some(now),
            }),
            routes: vec![CardRoute {
                alias: "claude-sonnet-4-6".into(),
                operation: "chat".into(),
            }],
        };
        let record = OrgMaster {
            id: "org/qwen".into(),
            name: "Qwen".into(),
            modality: Modality::Chat,
            version: 1,
            created_at: now,
            updated_at: now,
            updated_by: actor.clone(),
            deleted: false,
        };
        let link = MasterLink {
            provider_family: "bedrock".into(),
            model: "m".into(),
            master: None,
            version: 1,
            updated_at: now,
            updated_by: actor,
            cleared: true,
        };
        let list = MasterList {
            release_id: Some(Uuid::nil()),
            items: vec![
                MasterView {
                    id: "anthropic/claude-sonnet-4-6".into(),
                    name: "Claude Sonnet 4.6".into(),
                    kind: "card",
                    vendor: Some("Anthropic".into()),
                    origin: Some(FileOrigin::Builtin),
                    modality: Modality::Chat,
                    version: None,
                    deployments: vec![deployment.clone()],
                    suggestions: vec![RouteSuggestion {
                        alias: "sonnet".into(),
                        operation: "chat".into(),
                        deployment: "bedrock-sonnet".into(),
                    }],
                },
                org_view(record.clone()),
            ],
            unlinked: vec![MasterDeployment {
                master: None,
                resolved_by: ResolvedBy::None,
                card: None,
                link: None,
                availability: None,
                ..deployment
            }],
            links: vec![link.clone()],
            deleted_records: vec![OrgMaster {
                deleted: true,
                ..record.clone()
            }],
        };
        let route = ProposedRoute {
            operation: "chat".into(),
            deployment: "bedrock-sonnet".into(),
            weight: 1,
            priority: 1,
        };
        let proposal = RouteProposal {
            master: "anthropic/claude-sonnet-4-6".into(),
            release_id: Uuid::nil(),
            alias: "claude-sonnet-4-6".into(),
            alias_exists: true,
            operations: vec!["chat".into()],
            tools: false,
            routes: vec![route.clone()],
            excluded: vec![Exclusion {
                operation: "chat".into(),
                deployment: "x".into(),
                reason: ExclusionReason::Currency,
                detail: "d".into(),
            }],
            existing: vec![route.clone()],
            changes: vec![RouteChange {
                operation: "chat".into(),
                deployment: "bedrock-sonnet".into(),
                change: RouteChangeKind::Unchanged,
                before: Some(RouteWeight {
                    weight: 1,
                    priority: 1,
                }),
                after: None,
            }],
            problem: Some("p".into()),
        };
        for (schema, value) in [
            ("MasterList", serde_json::to_value(&list).unwrap()),
            ("RouteProposal", serde_json::to_value(&proposal).unwrap()),
            ("MasterLink", serde_json::to_value(&link).unwrap()),
            ("OrganizationMaster", serde_json::to_value(&record).unwrap()),
        ] {
            check(
                &contract,
                &serde_json::json!({ "$ref": format!("#/components/schemas/{schema}") }),
                &value,
                schema,
            );
        }
        // Every exclusion reason is documented.
        let documented =
            &contract["components"]["schemas"]["RouteExclusion"]["properties"]["reason"]["enum"];
        for reason in [
            ExclusionReason::NoPrice,
            ExclusionReason::ProviderNotPermitted,
            ExclusionReason::LocationNotPermitted,
            ExclusionReason::AvailabilityRefused,
            ExclusionReason::AvailabilityRequiresAction,
            ExclusionReason::NoTools,
            ExclusionReason::MixedToolCallIds,
            ExclusionReason::Currency,
            ExclusionReason::TooManyDeployments,
        ] {
            let name = serde_json::to_value(reason).unwrap();
            assert!(
                documented.as_array().unwrap().contains(&name),
                "{name} is not documented"
            );
        }
    }

    #[test]
    fn resource_ids_are_stable_and_distinct() {
        let org = OrganizationId(Uuid::nil());
        assert_eq!(link_id(org, "bedrock", "m"), link_id(org, "bedrock", "m"));
        assert_ne!(link_id(org, "bedrock", "m"), link_id(org, "anthropic", "m"));
        assert_ne!(record_id(org, "m"), link_id(org, "bedrock", "m"));
        assert!(writable_slug("org/qwen").is_ok());
        assert!(writable_slug("anthropic/claude").is_err());
        assert!(writable_slug("org/Qwen").is_err());
    }
}
