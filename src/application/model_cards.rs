// Model cards (Phase 8, D-022): one catalog model's card, built from what the
// gateway already has.
//
// The card needs `catalog_read` (organization-wide, as every catalog read
// does) and describes a model the active release serves. Its usage section
// needs `usage_read` and is clamped to the teams the caller may read, as the
// usage report is; its governance section needs `audit_read`. A section the
// caller cannot read is left out and named in `omitted`. Usage is financial
// data, so a card that includes it records a `usage.read` audit event, and
// is not returned if that cannot be written. Curated public facts come from
// the card library (shipped card files and the operator's directory); the
// gateway's own trace settings need `tracing_read`.

use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use crate::application::admin_identities::partition_scopes;
use crate::domain::{
    admin::{AdminPermission, AdminScope},
    audit::{AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditTargetResource},
    identity::{OrganizationId, TeamId, UserAccountId},
    model_card::{
        CardGovernance, CardImport, CardJoin, CardNote, CardOverview, CardRef, CardUsage,
        GatewayDataHandling, GovernanceEntry, MAX_GOVERNANCE_ENTRIES, MAX_GOVERNANCE_RELEASES,
        MODEL_CARD_SCHEMA, ModelCard, ModelCardSummary, OmittedSection, PublicCard,
        REQUEST_WINDOW_DAYS, USAGE_WINDOW_DAYS, UsageWindow, normalized_note, release_changes,
        valid_model,
    },
    model_card_files::{CardLibrary, SERVICE_AGENT_PLATFORM, SERVICE_GEMINI_API},
    reporting::{UsageGroupBy, UsageQuery},
};
use crate::persistence::{
    admin::{AdminStore, AdminStoreError},
    model_cards::{ActivationRow, ListingRow, ModelCardStore},
    reporting::{ReportingStore, ReportingStoreError},
    trace_policy::TracePolicyStore,
};

pub const ROUTE_MODEL_CARD: &str = "/api/admin/v1/organizations/{org}/catalog/models/{model}/card";
pub const ROUTE_MODEL_CARD_NOTE: &str =
    "/api/admin/v1/organizations/{org}/catalog/models/{model}/card/note";
pub const ROUTE_MODEL_CARD_IMPORT: &str =
    "/api/admin/v1/organizations/{org}/catalog/models/{model}/card/import";
pub const ROUTE_MODEL_CARDS: &str = "/api/admin/v1/organizations/{org}/catalog/model-cards";

#[derive(Debug)]
pub enum ModelCardError {
    InvalidModel,
    InvalidNote,
    Forbidden,
    VersionConflict(crate::domain::version::VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    /// No active release, or the active release does not serve the model.
    NotFound,
    InvalidRepository,
    ImportDisabled,
    RepositoryNotFound,
    HubUnavailable,
    ImportUnusable(String),
    Store(sqlx::Error),
    Reporting(ReportingStoreError),
    Admin(AdminStoreError),
}

impl From<sqlx::Error> for ModelCardError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone)]
pub struct ModelCardService {
    admin: AdminStore,
    store: ModelCardStore,
    reporting: ReportingStore,
    trace_policy: TracePolicyStore,
    library: Arc<CardLibrary>,
    hub: Option<HubSource>,
}

/// Where Hugging Face imports read model cards
/// (`GATEWAY_MODEL_CARD_HUB_URL`); unset, imports are off.
#[derive(Clone, Debug)]
pub struct HubSource {
    base: url::Url,
    policy: crate::jobs::egress::DeliveryPolicy,
}

impl HubSource {
    /// HTTPS to public addresses only, through the egress checks webhooks
    /// use: DNS resolved and pinned, no redirects, no proxy.
    pub fn new(base: url::Url) -> Self {
        Self {
            base,
            policy: crate::jobs::egress::DeliveryPolicy::public_only(),
        }
    }

    /// Also reaches a local listener over plain HTTP. Never use outside
    /// tests.
    #[doc(hidden)]
    pub fn for_tests(base: url::Url) -> Self {
        Self {
            base,
            policy: crate::jobs::egress::DeliveryPolicy::allow_private_targets_for_tests(),
        }
    }

    /// `GET {base}/api/models/{repository}`, at most 1 MiB, in 10 s.
    async fn model(&self, repository: &str) -> Result<serde_json::Value, ModelCardError> {
        let url = self
            .base
            .join(&format!("api/models/{repository}"))
            .map_err(|_| ModelCardError::HubUnavailable)?;
        let builder = reqwest::Client::builder()
            .user_agent(concat!("gateway/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10));
        let client = crate::jobs::egress::pinned_client(&url, self.policy, builder)
            .await
            .map_err(|_| ModelCardError::HubUnavailable)?;
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| ModelCardError::HubUnavailable)?;
        match response.status() {
            status if status.is_success() => {}
            // The Hub answers 401 for a repository that does not exist or is
            // private, as for a gated one without a token.
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::UNAUTHORIZED => {
                return Err(ModelCardError::RepositoryNotFound);
            }
            _ => return Err(ModelCardError::HubUnavailable),
        }
        let body = crate::jobs::egress::read_limited(
            response,
            crate::domain::model_card_import::MAX_HUB_RESPONSE_BYTES,
        )
        .await
        .map_err(|_| ModelCardError::HubUnavailable)?
        .ok_or_else(|| ModelCardError::ImportUnusable("the Hub's response is too large".into()))?;
        serde_json::from_slice(&body)
            .map_err(|_| ModelCardError::ImportUnusable("the Hub's response is not JSON".into()))
    }
}

impl ModelCardService {
    pub fn new(
        admin: AdminStore,
        store: ModelCardStore,
        reporting: ReportingStore,
        trace_policy: TracePolicyStore,
        library: Arc<CardLibrary>,
    ) -> Self {
        Self {
            admin,
            store,
            reporting,
            trace_policy,
            library,
            hub: None,
        }
    }

    /// With Hugging Face imports read from `hub`.
    pub fn with_hub(mut self, hub: Option<HubSource>) -> Self {
        self.hub = hub;
        self
    }

    pub async fn card(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        model: &str,
        request_id: Uuid,
    ) -> Result<ModelCard, ModelCardError> {
        if !valid_model(model) {
            return Err(ModelCardError::InvalidModel);
        }
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(|error| match error {
                AdminStoreError::Authorization(_) => ModelCardError::Forbidden,
                AdminStoreError::ResourceNotFound => ModelCardError::NotFound,
                other => ModelCardError::Admin(other),
            })?;
        let organization = AdminScope::Organization(organization_id);
        if !context.allows(AdminPermission::CatalogRead, organization) {
            return Err(ModelCardError::Forbidden);
        }
        let release_id = self
            .store
            .active_release(organization_id)
            .await?
            .ok_or(ModelCardError::NotFound)?;
        let deployments = self
            .store
            .deployments(organization_id, release_id, model)
            .await?;
        if deployments.is_empty() {
            return Err(ModelCardError::NotFound);
        }
        let mut overview = CardOverview {
            display_name: self.store.display_name(organization_id, model).await?,
            ..CardOverview::default()
        };
        for deployment in &deployments {
            overview
                .provider_families
                .push(deployment.provider_family.clone());
            overview.locations.extend(deployment.location.clone());
            for operation in &deployment.operations {
                overview.operations.push(operation.operation.clone());
                overview.max_input_bytes = overview
                    .max_input_bytes
                    .max(Some(operation.max_input_bytes));
                overview.max_output_tokens = overview
                    .max_output_tokens
                    .max(Some(operation.max_output_tokens));
            }
        }
        for list in [
            &mut overview.provider_families,
            &mut overview.operations,
            &mut overview.locations,
        ] {
            list.sort();
            list.dedup();
        }

        let mut omitted = Vec::new();
        let (org_wide, teams) =
            partition_scopes(context.authorized_scopes(AdminPermission::UsageRead));
        let usage = if org_wide || !teams.is_empty() {
            let teams = (!org_wide).then_some(teams);
            let usage = self.usage(organization_id, teams.as_deref(), model).await?;
            self.audit_usage_read(actor, organization_id, request_id)
                .await?;
            Some(usage)
        } else {
            omitted.push(OmittedSection {
                section: "usage",
                requires: "usage_read",
            });
            None
        };
        let governance = if context.allows(AdminPermission::AuditRead, organization) {
            let activations = self
                .store
                .activations(organization_id, model, MAX_GOVERNANCE_RELEASES)
                .await?;
            // Earlier history matters only if an earlier release could
            // have served the model; an active release with no activation
            // read leaves it unknown.
            let older_unread = match activations.first() {
                Some(oldest) => {
                    self.store
                        .older_release_serves(organization_id, oldest.release_id, model)
                        .await?
                }
                None => true,
            };
            Some(governance(activations, older_unread))
        } else {
            omitted.push(OmittedSection {
                section: "governance",
                requires: "audit_read",
            });
            None
        };
        let joined = self
            .joins(
                organization_id,
                deployments.iter().map(|d| {
                    (
                        d.deployment.as_str(),
                        d.provider_family.as_str(),
                        d.adapter_instance_key.as_str(),
                        model,
                        d.provider_listing.as_ref().and_then(|l| l.model.as_deref()),
                        d.provider_listing
                            .as_ref()
                            .and_then(|l| l.format.as_deref()),
                    )
                }),
            )
            .await?;
        let mut public_card = PublicCard::from_library(&self.library, &joined);
        // An imported card stands in only where no file covers the model.
        if public_card.card.is_none()
            && let Some(import) = self.store.import(organization_id, model).await?
        {
            let mut names: Vec<String> = deployments.iter().map(|d| d.deployment.clone()).collect();
            names.sort();
            public_card.card = Some(import.public_card(names));
            public_card.status = crate::domain::model_card::PublicCardStatus::OnFile;
        }
        let gateway_data_handling = if context.allows(AdminPermission::TracingRead, organization) {
            let policy = self.trace_policy.policy(organization_id).await?;
            Some(GatewayDataHandling {
                trace_mode: policy.mode.as_str(),
                trace_content: policy.content,
                trace_content_retention_days: policy.content_retention_days,
            })
        } else {
            omitted.push(OmittedSection {
                section: "gateway_data_handling",
                requires: "tracing_read",
            });
            None
        };
        Ok(ModelCard {
            schema: MODEL_CARD_SCHEMA,
            model: model.to_owned(),
            generated_at: Utc::now(),
            release_id,
            overview,
            public_card,
            gateway_data_handling,
            deployments,
            usage,
            governance,
            // A cleared note (empty text) is returned with its version, so
            // a client can set it again without overwriting a newer one.
            organization_note: self.store.note(organization_id, model).await?,
            organization_evaluations: self
                .store
                .organization_evaluations(organization_id, model)
                .await?,
            omitted,
        })
    }

    /// How each deployment joins card files: by its model and the model its
    /// listing names, and the service its family's terms depend on. Each
    /// item is `(deployment, family, adapter key, model, listed model,
    /// listed format)`.
    async fn joins<'a>(
        &self,
        organization_id: OrganizationId,
        deployments: impl Iterator<
            Item = (
                &'a str,
                &'a str,
                &'a str,
                &'a str,
                Option<&'a str>,
                Option<&'a str>,
            ),
        >,
    ) -> Result<Vec<CardJoin>, ModelCardError> {
        let deployments: Vec<_> = deployments.collect();
        let gemini_keys: Vec<String> = deployments
            .iter()
            .filter(|d| d.1 == "gemini")
            .map(|d| d.2.to_owned())
            .collect();
        let kinds = if gemini_keys.is_empty() {
            Vec::new()
        } else {
            self.store
                .credential_kinds(organization_id, "gemini", &gemini_keys)
                .await?
        };
        Ok(deployments
            .into_iter()
            .map(|(name, family, key, model, listed_model, format)| {
                let service = match family {
                    // A stored credential decides the service: a Google
                    // service account calls Agent Platform, a key the Gemini
                    // API. Without one, the process's own adapter.
                    "gemini" => match kinds.iter().find(|k| k.adapter_instance_key == key) {
                        Some(kind) if kind.has_principal => Some(SERVICE_AGENT_PLATFORM),
                        Some(kind) if kind.recorded => Some(SERVICE_GEMINI_API),
                        Some(_) => None,
                        None => self.library.process_service(key),
                    }
                    .map(str::to_owned),
                    // Foundry's terms differ by who sells the model, which
                    // its format says (Anthropic's are Marketplace offers).
                    "azure-ai-foundry" | "azure-openai" => format.map(str::to_ascii_lowercase),
                    _ => None,
                };
                let mut models = vec![model.to_owned()];
                models.extend(
                    listed_model
                        .filter(|listed| *listed != model)
                        .map(str::to_owned),
                );
                CardJoin {
                    deployment: name.to_owned(),
                    family: family.to_owned(),
                    models,
                    service,
                }
            })
            .collect())
    }

    /// Every model the active release serves, with the card facts the
    /// Models table shows. Needs `catalog_read`.
    pub async fn summaries(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
    ) -> Result<(Option<Uuid>, Vec<ModelCardSummary>), ModelCardError> {
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        let Some(release_id) = self.store.active_release(organization_id).await? else {
            return Ok((None, Vec::new()));
        };
        let (rows, listings) = self
            .store
            .release_deployments(organization_id, release_id)
            .await?;
        let mut models: Vec<String> = rows.iter().map(|row| row.model.clone()).collect();
        models.dedup();
        let imports = self.store.imports(organization_id, &models).await?;
        let listing = |key: &str, family: &str, model: &str| -> Option<&ListingRow> {
            listings
                .iter()
                .find(|l| l.adapter_instance_key == key && l.family == family && l.model == model)
        };
        let mut summaries = Vec::new();
        let mut start = 0;
        while start < rows.len() {
            let model = rows[start].model.clone();
            let end = rows[start..]
                .iter()
                .position(|row| row.model != model)
                .map_or(rows.len(), |offset| start + offset);
            let group = &rows[start..end];
            start = end;
            let joins = self
                .joins(
                    organization_id,
                    group.iter().map(|row| {
                        let listed = listing(&row.adapter_instance_key, &row.family, &row.model);
                        (
                            row.deployment.as_str(),
                            row.family.as_str(),
                            row.adapter_instance_key.as_str(),
                            row.model.as_str(),
                            listed.and_then(|l| l.deployment_model.as_deref()),
                            listed.and_then(|l| l.format.as_deref()),
                        )
                    }),
                )
                .await?;
            let card = PublicCard::from_library(&self.library, &joins)
                .card
                .or_else(|| {
                    imports
                        .iter()
                        .find(|import| import.model == model)
                        .map(|import| import.public_card(Vec::new()))
                });
            let mut families: Vec<String> = group.iter().map(|row| row.family.clone()).collect();
            families.sort();
            families.dedup();
            let fact = |key: &str| card.as_ref().and_then(|c| c.overview.get(key).cloned());
            summaries.push(ModelCardSummary {
                retires_at: group
                    .iter()
                    .filter_map(|row| {
                        listing(&row.adapter_instance_key, &row.family, &row.model)
                            .and_then(|l| l.retires_at)
                    })
                    .min(),
                context_window_tokens: fact("context_window_tokens"),
                max_output_tokens: fact("max_output_tokens"),
                retirement: fact("retirement"),
                card: card.as_ref().map(|c| CardRef {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    vendor: c.vendor.clone(),
                    origin: c.origin,
                }),
                provider_families: families,
                model,
            });
        }
        Ok((Some(release_id), summaries))
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), ModelCardError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(ModelCardError::Forbidden)
        }
    }

    /// The organization's imported card for `model`. Needs `catalog_read`.
    pub async fn import(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<CardImport, ModelCardError> {
        if !valid_model(model) {
            return Err(ModelCardError::InvalidModel);
        }
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        self.store
            .import(organization_id, model)
            .await?
            .ok_or(ModelCardError::NotFound)
    }

    /// Imports `repository`'s Hugging Face model card as `model`'s card
    /// (`repository` Some), or removes the import (None, soft). Needs
    /// `catalog_write`; idempotent, versioned and audited
    /// (`model_card_import.set`). The card is shown only while no card file
    /// matches the model.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_import(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        model: &str,
        repository: Option<(&str, bool)>,
        if_match: crate::domain::version::IfMatch,
        idempotency_key: crate::domain::admin::AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(Option<CardImport>, bool, bool), ModelCardError> {
        use crate::domain::admin::{AdminOperation, IdempotencyResultMetadata};
        use crate::domain::audit::AuditSnapshot;
        use crate::persistence::admin::{AdminMutationRequest, MutationStart};
        if !valid_model(model) {
            return Err(ModelCardError::InvalidModel);
        }
        if let Some((repository, _)) = repository
            && !crate::domain::model_card_import::valid_repository(repository)
        {
            return Err(ModelCardError::InvalidRepository);
        }
        // Checked before the Hub is called, so a caller who may not import
        // cannot make the gateway fetch.
        self.authorize(actor, organization_id, AdminPermission::CatalogWrite)
            .await?;
        // Read before the transaction opens, so no database transaction
        // waits on the Hub; a failed read is answered after the
        // idempotency check below, so a retry of a recorded import replays
        // even while the Hub is unreachable.
        let fetched = async {
            Ok::<_, ModelCardError>(match repository {
                Some((repository, provider_repository)) => {
                    let hub = self.hub.as_ref().ok_or(ModelCardError::ImportDisabled)?;
                    let body = hub.model(repository).await?;
                    let card = crate::domain::model_card_import::card_from_hub(
                        repository,
                        model,
                        &body,
                        provider_repository,
                        Utc::now().date_naive(),
                    )
                    .map_err(
                        |crate::domain::model_card_import::ImportError::Unusable(why)| {
                            ModelCardError::ImportUnusable(why)
                        },
                    )?;
                    Some((
                        repository,
                        provider_repository,
                        serde_json::to_value(&card).map_err(|_| {
                            ModelCardError::ImportUnusable("the card is not serializable".into())
                        })?,
                    ))
                }
                None => None,
            })
        }
        .await;
        let target_id = import_id(organization_id, model);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_CARD_IMPORT.into(),
            method: if repository.is_some() {
                "PUT"
            } else {
                "DELETE"
            }
            .into(),
            operation: AdminOperation::new("model_card_import.set")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelCardImport,
            audit_action: AuditAction::new("model_card_import.set")
                .expect("audit action constant is valid"),
            if_match,
            request_id,
            body: serde_json::json!({
                "model": model,
                "repository": repository.map(|r| r.0),
                "provider_repository": repository.map(|r| r.1),
            }),
        };
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator.begin(request).await.map_err(map_admin_error)? {
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .unwrap_or(serde_json::Value::Null);
                let import: Option<CardImport> =
                    serde_json::from_value(snapshot).map_err(|_| {
                        ModelCardError::Store(sqlx::Error::Protocol(
                            "recorded response snapshot is invalid".into(),
                        ))
                    })?;
                return Ok((import, false, true));
            }
            MutationStart::New(mutation) => mutation,
        };
        // Not a replay: a failed Hub read fails the request, and dropping
        // the mutation rolls its transaction back.
        let card = match fetched {
            Ok(card) => card,
            Err(error) => {
                drop(mutation);
                return Err(error);
            }
        };
        let tx = mutation.transaction();
        let before = ModelCardStore::import_in(tx, organization_id, model, false).await?;
        if repository.is_none() && before.is_none() {
            return Err(ModelCardError::NotFound);
        }
        let next = mutation.next_version();
        let tx = mutation.transaction();
        let created = ModelCardStore::set_import(
            tx,
            organization_id,
            target_id,
            model,
            card.as_ref().map(|(r, p, c)| (*r, *p, c)),
            actor.0,
        )
        .await?;
        let stored = match card {
            Some(_) => Some(CardImport {
                version: next.get(),
                ..ModelCardStore::import_in(tx, organization_id, model, false)
                    .await?
                    .ok_or(ModelCardError::NotFound)?
            }),
            None => None,
        };
        // A removal records no snapshot: its replay answers 204 again.
        let snapshot = stored
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| {
                ModelCardError::Store(sqlx::Error::Protocol(
                    "response snapshot is not serializable".into(),
                ))
            })?;
        mutation
            .commit(
                before.map(|_| AuditSnapshot {
                    enabled: Some(true),
                    version: next.get().checked_sub(1).filter(|v| *v > 0),
                    ..Default::default()
                }),
                Some(AuditSnapshot {
                    // Whether an import is in place.
                    enabled: Some(stored.is_some()),
                    version: Some(next.get()),
                    ..Default::default()
                }),
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot,
                }),
            )
            .await
            .map_err(map_admin_error)?;
        Ok((stored, created, false))
    }

    /// The organization's note on `model`. Needs `catalog_read`; a model
    /// with no note is not found.
    pub async fn note(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        model: &str,
    ) -> Result<CardNote, ModelCardError> {
        if !valid_model(model) {
            return Err(ModelCardError::InvalidModel);
        }
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        self.store
            .note(organization_id, model)
            .await?
            .ok_or(ModelCardError::NotFound)
    }

    /// Sets the organization's note on `model` (empty clears it). Needs
    /// `catalog_write`; idempotent, versioned and audited
    /// (`model_card_note.set`). A note may be set on any model identifier,
    /// served now or not, so it survives a release that drops the model.
    pub async fn set_note(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        model: &str,
        note: &str,
        if_match: crate::domain::version::IfMatch,
        idempotency_key: crate::domain::admin::AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<(CardNote, bool, bool), ModelCardError> {
        use crate::domain::admin::{AdminOperation, IdempotencyResultMetadata};
        use crate::domain::audit::AuditSnapshot;
        use crate::persistence::admin::{AdminMutationRequest, MutationStart};
        if !valid_model(model) {
            return Err(ModelCardError::InvalidModel);
        }
        let note = normalized_note(note).ok_or(ModelCardError::InvalidNote)?;
        let target_id = note_id(organization_id, model);
        let request = AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_MODEL_CARD_NOTE.into(),
            method: "PUT".into(),
            operation: AdminOperation::new("model_card_note.set")
                .expect("operation constant is valid"),
            idempotency_key,
            target_id,
            parent_id: None,
            target: AuditTargetResource::ModelCardNote,
            audit_action: AuditAction::new("model_card_note.set")
                .expect("audit action constant is valid"),
            if_match,
            request_id,
            // The text itself stays out of the audit log and idempotency
            // fingerprint's readable fields: only its digest.
            body: serde_json::json!({
                "model": model,
                "note_sha256": format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(note.as_bytes())),
            }),
        };
        match self
            .admin
            .coordinator()
            .begin(request)
            .await
            .map_err(map_admin_error)?
        {
            MutationStart::Replay(record) => {
                let snapshot = record
                    .response_metadata
                    .and_then(|metadata| metadata.snapshot)
                    .ok_or(ModelCardError::NotFound)?;
                let note: CardNote = serde_json::from_value(snapshot).map_err(|_| {
                    ModelCardError::Store(sqlx::Error::Protocol(
                        "recorded response snapshot is invalid".into(),
                    ))
                })?;
                Ok((note, false, true))
            }
            MutationStart::New(mut mutation) => {
                let tx = mutation.transaction();
                let before = ModelCardStore::note_in(tx, organization_id, model).await?;
                let created =
                    ModelCardStore::set_note(tx, organization_id, target_id, model, &note, actor.0)
                        .await?;
                let stored = ModelCardStore::note_in(tx, organization_id, model)
                    .await?
                    .ok_or(ModelCardError::NotFound)?;
                let next = mutation.next_version();
                let stored = CardNote {
                    version: next.get(),
                    ..stored
                };
                let snapshot = serde_json::to_value(&stored).map_err(|_| {
                    ModelCardError::Store(sqlx::Error::Protocol(
                        "response snapshot is not serializable".into(),
                    ))
                })?;
                mutation
                    .commit(
                        before.map(|before| AuditSnapshot {
                            enabled: Some(!before.note.is_empty()),
                            version: next.get().checked_sub(1).filter(|v| *v > 0),
                            ..Default::default()
                        }),
                        Some(AuditSnapshot {
                            // Whether a note is set.
                            enabled: Some(!stored.note.is_empty()),
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
        }
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        teams: Option<&[TeamId]>,
        model: &str,
    ) -> Result<CardUsage, ModelCardError> {
        let to = Utc::now();
        let mut windows = Vec::with_capacity(USAGE_WINDOW_DAYS.len());
        for days in USAGE_WINDOW_DAYS {
            let from = to - Duration::days(days);
            // The usage report's own query, filtered to the model.
            let query = UsageQuery {
                from,
                to,
                group_by: vec![UsageGroupBy::Model],
                team_ids: Vec::new(),
                service_account_ids: Vec::new(),
                models: vec![model.to_owned()],
                deployments: Vec::new(),
                provider_families: Vec::new(),
                locations: Vec::new(),
                batch_ids: Vec::new(),
                model_masters: Vec::new(),
                currency: None,
            };
            let totals = self
                .reporting
                .usage_totals(organization_id, teams, &query)
                .await
                .map_err(ModelCardError::Reporting)?;
            windows.push(UsageWindow {
                days,
                from,
                to,
                totals,
            });
        }
        let requests = self
            .store
            .request_stats(
                organization_id,
                teams,
                model,
                to - Duration::days(REQUEST_WINDOW_DAYS),
                to,
            )
            .await?;
        Ok(CardUsage { windows, requests })
    }

    async fn audit_usage_read(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        request_id: Uuid,
    ) -> Result<(), ModelCardError> {
        self.admin
            .record_audit(AdminAuditEvent::new(
                AuditActor::User(actor),
                organization_id,
                None,
                AuditTargetResource::UsageReport,
                AuditAction::new("usage.read").expect("audit action constant is valid"),
                organization_id.0,
                AuditOutcome::Succeeded,
                request_id,
                None,
                None,
            ))
            .await
            .map_err(ModelCardError::Admin)
    }
}

/// An import's resource id: one per organization and model.
pub fn import_id(organization_id: OrganizationId, model: &str) -> Uuid {
    crate::application::admin_identities::deterministic_target_id(&[
        "model_card_import",
        &organization_id.0.to_string(),
        model,
    ])
}

/// A note's resource id: one per organization and model.
pub fn note_id(organization_id: OrganizationId, model: &str) -> Uuid {
    crate::application::admin_identities::deterministic_target_id(&[
        "model_card_note",
        &organization_id.0.to_string(),
        model,
    ])
}

fn map_admin_error(error: AdminStoreError) -> ModelCardError {
    match error {
        AdminStoreError::Authorization(_) => ModelCardError::Forbidden,
        AdminStoreError::ResourceNotFound => ModelCardError::NotFound,
        AdminStoreError::VersionConflict(conflict) => ModelCardError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => ModelCardError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => ModelCardError::IdempotencyInProgress,
        other => ModelCardError::Admin(other),
    }
}

/// The model's history from release activations, oldest first: an entry for
/// each activation that added, removed or changed one of its deployments.
/// `older_unread`: a release older than the oldest activation read had a
/// deployment of the model, so activations before it may have served the
/// model; the oldest read is then a baseline and the model's first release
/// is not known.
pub fn governance(rows: Vec<ActivationRow>, older_unread: bool) -> CardGovernance {
    // Group the rows into activations, each with its deployments of the model.
    let mut activations: Vec<(ActivationRow, Vec<(String, String)>)> = Vec::new();
    for row in rows {
        let entry = match (row.deployment.clone(), row.fingerprint.clone()) {
            (Some(name), Some(fingerprint)) => Some((name, fingerprint)),
            _ => None,
        };
        match activations.last_mut() {
            Some((last, deployments))
                if last.activated_at == row.activated_at && last.release_id == row.release_id =>
            {
                deployments.extend(entry);
            }
            _ => activations.push((row, entry.into_iter().collect())),
        }
    }
    let complete = !older_unread;
    let mut history = Vec::new();
    let mut first_release = None;
    let mut previous: Option<&[(String, String)]> = None;
    for (activation, deployments) in &activations {
        // The model first appears where nothing before served it, and only
        // a complete history shows where that is.
        let appears = complete && previous.is_none_or(<[_]>::is_empty) && !deployments.is_empty();
        let (added, removed, changed) = match previous {
            Some(before) => release_changes(before, deployments),
            // The oldest activation read: everything in it is new only if
            // nothing older exists.
            None if complete => release_changes(&[], deployments),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        previous = Some(deployments);
        if added.is_empty() && removed.is_empty() && changed.is_empty() {
            continue;
        }
        let entry = GovernanceEntry {
            release_id: activation.release_id,
            release_version: activation.release_version.clone(),
            created_at: activation.activated_at,
            actor: Some(activation.actor()),
            added,
            removed,
            changed,
        };
        if first_release.is_none() && appears {
            first_release = Some(entry.clone());
        }
        history.push(entry);
    }
    history.reverse();
    let truncated = !complete || history.len() > MAX_GOVERNANCE_ENTRIES;
    history.truncate(MAX_GOVERNANCE_ENTRIES);
    CardGovernance {
        first_release,
        history,
        truncated,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone};

    fn row(at: i64, release: u128, deployment: Option<(&str, &str)>) -> ActivationRow {
        ActivationRow {
            activated_at: DateTime::<Utc>::from_timestamp(at, 0).unwrap(),
            release_id: Uuid::from_u128(release),
            release_version: format!("v{release}"),
            actor_type: "user".into(),
            actor_id: None,
            actor_name: Some("Ada".into()),
            deployment: deployment.map(|(d, _)| d.to_owned()),
            fingerprint: deployment.map(|(_, f)| f.to_owned()),
        }
    }

    #[test]
    fn governance_lists_what_each_activation_changed_newest_first() {
        let rows = vec![
            row(1, 1, None),
            row(2, 2, Some(("bedrock-x", "a"))),
            row(3, 3, Some(("bedrock-x", "a"))),
            row(3, 3, Some(("azure-x", "a"))),
            row(4, 4, Some(("bedrock-x", "b"))),
            row(4, 4, Some(("azure-x", "a"))),
            row(5, 5, Some(("azure-x", "a"))),
        ];
        let card = governance(rows, false);
        let first = card.first_release.unwrap();
        assert_eq!(first.release_id, Uuid::from_u128(2));
        assert_eq!(first.added, ["bedrock-x"]);
        let summary: Vec<_> = card
            .history
            .iter()
            .map(|e| {
                (
                    e.release_version.as_str(),
                    e.added.clone(),
                    e.removed.clone(),
                    e.changed.clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("v5", vec![], vec!["bedrock-x".to_owned()], vec![]),
                ("v4", vec![], vec![], vec!["bedrock-x".to_owned()]),
                ("v3", vec!["azure-x".to_owned()], vec![], vec![]),
                ("v2", vec!["bedrock-x".to_owned()], vec![], vec![]),
            ]
        );
        assert!(!card.truncated);
        assert_eq!(card.history[0].created_at, Utc.timestamp_opt(5, 0).unwrap());
    }

    #[test]
    fn unread_older_activations_leave_the_first_release_unknown() {
        // The model was already served before the oldest activation read;
        // a later addition is not its first release.
        let rows = vec![
            row(1, 1, Some(("bedrock-x", "a"))),
            row(2, 2, Some(("bedrock-x", "a"))),
            row(2, 2, Some(("azure-x", "a"))),
        ];
        let card = governance(rows, true);
        assert!(card.first_release.is_none());
        assert!(card.truncated);
        assert_eq!(card.history.len(), 1);
        assert_eq!(card.history[0].added, ["azure-x"]);
    }

    #[test]
    fn a_truncated_history_does_not_claim_the_first_release() {
        let rows = vec![
            row(1, 1, Some(("bedrock-x", "a"))),
            row(2, 2, Some(("bedrock-x", "b"))),
        ];
        let card = governance(rows, true);
        assert!(card.first_release.is_none());
        assert_eq!(card.history.len(), 1);
        assert_eq!(card.history[0].changed, ["bedrock-x"]);
        assert!(card.truncated);
    }

    /// Checks `value` against `schema` in the contract: every key is a
    /// property, every required property is present, recursively through
    /// `$ref`, arrays and `oneOf`.
    pub(crate) fn check(
        contract: &serde_json::Value,
        schema: &serde_json::Value,
        value: &serde_json::Value,
        at: &str,
    ) {
        if let Some(reference) = schema["$ref"].as_str() {
            let name = reference.trim_start_matches("#/components/schemas/");
            return check(
                contract,
                &contract["components"]["schemas"][name],
                value,
                name,
            );
        }
        if let Some(options) = schema["oneOf"].as_array() {
            if value.is_null() {
                return;
            }
            let option = options.iter().find(|o| o["type"] != "null").unwrap();
            return check(contract, option, value, at);
        }
        if let Some(items) = schema.get("items") {
            for item in value
                .as_array()
                .unwrap_or_else(|| panic!("{at}: not an array"))
            {
                check(contract, items, item, at);
            }
            return;
        }
        let Some(properties) = schema["properties"].as_object() else {
            return;
        };
        let object = value
            .as_object()
            .unwrap_or_else(|| panic!("{at}: not an object"));
        for (key, field) in object {
            let property = properties
                .get(key)
                .unwrap_or_else(|| panic!("{at}.{key} is not in the contract"));
            check(contract, property, field, &format!("{at}.{key}"));
        }
        for name in schema["required"].as_array().into_iter().flatten() {
            let name = name.as_str().unwrap();
            assert!(
                object.contains_key(name),
                "{at}.{name} is required by the contract"
            );
        }
    }

    #[test]
    fn cards_match_the_contract() {
        use crate::domain::model_card::*;
        use crate::domain::reporting::{ReportConfidence, TokenTotals, UsageAggregate};
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-model-cards-v1.json")).unwrap();
        let now = Utc::now();
        let entry = GovernanceEntry {
            release_id: Uuid::nil(),
            release_version: "v1".into(),
            created_at: now,
            actor: Some(GovernanceActor {
                actor_type: "user".into(),
                id: Some(Uuid::nil()),
                name: Some("Ada".into()),
            }),
            added: vec!["bedrock-x".into()],
            removed: vec![],
            changed: vec![],
        };
        let full = ModelCard {
            schema: MODEL_CARD_SCHEMA,
            model: "m".into(),
            generated_at: now,
            release_id: Uuid::nil(),
            overview: CardOverview {
                display_name: Some("M".into()),
                provider_families: vec!["bedrock".into()],
                operations: vec!["chat".into()],
                locations: vec!["us-east-1".into()],
                max_input_bytes: Some(1),
                max_output_tokens: Some(1),
            },
            public_card: PublicCard::from_library(
                &crate::domain::model_card_files::CardLibrary::builtin().unwrap(),
                &[CardJoin {
                    deployment: "bedrock-x".into(),
                    family: "bedrock".into(),
                    models: vec!["openai.gpt-oss-120b-1:0".into()],
                    service: None,
                }],
            ),
            gateway_data_handling: Some(GatewayDataHandling {
                trace_mode: "organization",
                trace_content: true,
                trace_content_retention_days: 30,
            }),
            deployments: vec![CardDeployment {
                deployment: "bedrock-x".into(),
                adapter_instance_key: "k".into(),
                provider_family: "bedrock".into(),
                location: Some("us-east-1".into()),
                routes: vec![CardRoute {
                    alias: "x".into(),
                    operation: "chat".into(),
                }],
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
                        source: Some("s".into()),
                        source_version: Some("1".into()),
                        confidence: "known".into(),
                    }),
                }],
                availability: Some(CardAvailability {
                    state: "verified".into(),
                    reason: None,
                    source: "probe".into(),
                    observed_at: now,
                    verified_at: Some(now),
                }),
                provider_listing: Some(CardListing {
                    format: Some("OpenAI".into()),
                    model: Some("gpt-x".into()),
                    version: Some("1".into()),
                    sku: Some("GlobalStandard".into()),
                    lifecycle_status: Some("Deprecating".into()),
                    retires_at: Some(now),
                    listed_at: now,
                    content_filter: Some("Microsoft.DefaultV2".into()),
                    content_filter_policy: Some(crate::domain::discovery::ContentFilterPolicy {
                        name: "Microsoft.DefaultV2".into(),
                        base_policy: None,
                        mode: Some("Default".into()),
                        system_policy: Some(true),
                        filters: vec![crate::domain::discovery::ContentFilterRule {
                            name: "Hate".into(),
                            source: Some("Prompt".into()),
                            enabled: Some(true),
                            blocking: Some(true),
                            severity_threshold: Some("Medium".into()),
                        }],
                    }),
                }),
            }],
            usage: Some(CardUsage {
                windows: vec![UsageWindow {
                    days: 7,
                    from: now,
                    to: now,
                    totals: vec![UsageAggregate {
                        currency: "USD".into(),
                        confidence: ReportConfidence::Exact,
                        request_count: 1,
                        amount_minor: 1,
                        unpriced_request_count: 0,
                        incomplete_request_count: 0,
                        tokens: TokenTotals {
                            input: 1,
                            output: 1,
                            cached_input: 0,
                            reasoning: 0,
                            other: 0,
                        },
                    }],
                }],
                requests: RequestStats {
                    days: 30,
                    request_count: 1,
                    errors: vec![CountBy {
                        key: "rate_limited".into(),
                        count: 1,
                    }],
                    timed_since: Some(now),
                    latency_p50_ms: Some(1),
                    finish_reasons: vec![FinishReasonCount {
                        reason: "stop".into(),
                        count: 1,
                        safety: false,
                    }],
                    ..RequestStats::default()
                },
            }),
            governance: Some(CardGovernance {
                first_release: Some(entry.clone()),
                history: vec![entry.clone()],
                truncated: false,
            }),
            organization_evaluations: vec![
                crate::domain::model_evaluation::OrganizationEvaluation {
                    set_id: Uuid::nil(),
                    set_name: "Support answers".into(),
                    run_id: Uuid::nil(),
                    batch_id: Uuid::nil(),
                    scored_at: now,
                    items: 10,
                    answered: 9,
                    correct: 7,
                },
            ],
            organization_note: Some(CardNote {
                note: "Approved for internal tools.".into(),
                version: 1,
                updated_at: now,
                updated_by: entry.actor.clone(),
            }),
            omitted: vec![],
        };
        let root = serde_json::json!({"$ref": "#/components/schemas/ModelCard"});
        check(
            &contract,
            &root,
            &serde_json::to_value(&full).unwrap(),
            "ModelCard",
        );
        assert_eq!(full.public_card.status, PublicCardStatus::OnFile);
        let restricted = ModelCard {
            usage: None,
            governance: None,
            gateway_data_handling: None,
            public_card: PublicCard::from_library(
                &crate::domain::model_card_files::CardLibrary::default(),
                &[],
            ),
            omitted: vec![
                OmittedSection {
                    section: "usage",
                    requires: "usage_read",
                },
                OmittedSection {
                    section: "governance",
                    requires: "audit_read",
                },
                OmittedSection {
                    section: "gateway_data_handling",
                    requires: "tracing_read",
                },
            ],
            ..full
        };
        check(
            &contract,
            &root,
            &serde_json::to_value(&restricted).unwrap(),
            "ModelCard",
        );
        let summary = ModelCardSummary {
            model: "m".into(),
            provider_families: vec!["bedrock".into()],
            card: Some(CardRef {
                id: "v/m".into(),
                name: "M".into(),
                vendor: "V".into(),
                origin: crate::domain::model_card_files::FileOrigin::Builtin,
            }),
            context_window_tokens: full
                .public_card
                .card
                .as_ref()
                .and_then(|c| c.overview.get("context_window_tokens").cloned()),
            max_output_tokens: None,
            retirement: None,
            retires_at: Some(now),
        };
        check(
            &contract,
            &serde_json::json!({"$ref": "#/components/schemas/ModelCardSummaryList"}),
            &serde_json::json!({"release_id": Uuid::nil(), "items": [summary]}),
            "ModelCardSummaryList",
        );
    }

    #[test]
    fn imports_match_the_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-model-cards-v1.json")).unwrap();
        let card = crate::domain::model_card_import::card_from_hub(
            "openai/gpt-oss-x",
            "gpt-oss-x",
            &serde_json::json!({"author": "openai", "safetensors": {"total": 5},
                "cardData": {"license": "apache-2.0", "model-index": [{"results": [
                    {"dataset": {"name": "MMLU"}, "task": {"name": "QA"},
                     "metrics": [{"type": "acc", "value": 1.5}]}]}]}}),
            false,
            chrono::NaiveDate::from_ymd_opt(2026, 11, 17).unwrap(),
        )
        .unwrap();
        let import = CardImport {
            model: "gpt-oss-x".into(),
            repository: "openai/gpt-oss-x".into(),
            provider_repository: false,
            card,
            version: 1,
            imported_at: Utc::now(),
            imported_by: None,
        };
        check(
            &contract,
            &serde_json::json!({"$ref": "#/components/schemas/CardImport"}),
            &serde_json::to_value(&import).unwrap(),
            "CardImport",
        );
        assert_eq!(
            serde_json::to_value(import.public_card(vec![]).origin).unwrap(),
            serde_json::json!("imported")
        );
        assert_ne!(
            import_id(OrganizationId::new(), "m"),
            note_id(OrganizationId::new(), "m")
        );
        let item = crate::domain::model_evaluation::EvaluationItem {
            id: "q1".into(),
            expected: "Paris".into(),
            match_kind: crate::domain::model_evaluation::MatchKind::Exact,
        };
        let set = crate::domain::model_evaluation::EvaluationSet {
            id: Uuid::nil(),
            name: "Capitals".into(),
            description: Some("d".into()),
            items: vec![item],
            created_at: Utc::now(),
            created_by: None,
            version: 1,
        };
        check(
            &contract,
            &serde_json::json!({"$ref": "#/components/schemas/EvaluationSet"}),
            &serde_json::to_value(&set).unwrap(),
            "EvaluationSet",
        );
        let run = crate::domain::model_evaluation::EvaluationRun {
            id: Uuid::nil(),
            set_id: Uuid::nil(),
            batch_id: Uuid::nil(),
            created_at: Utc::now(),
            created_by: None,
            unmatched_lines: 0,
            results: vec![crate::domain::model_evaluation::ModelScore {
                model: "m".into(),
                items: 1,
                answered: 1,
                correct: 1,
            }],
        };
        check(
            &contract,
            &serde_json::json!({"$ref": "#/components/schemas/EvaluationRun"}),
            &serde_json::to_value(&run).unwrap(),
            "EvaluationRun",
        );
    }

    #[test]
    fn notes_are_one_per_organization_and_model() {
        let org = OrganizationId::new();
        assert_eq!(note_id(org, "m"), note_id(org, "m"));
        assert_ne!(note_id(org, "m"), note_id(org, "n"));
        assert_ne!(note_id(org, "m"), note_id(OrganizationId::new(), "m"));
        assert!(crate::domain::admin::AdminOperation::new("model_card_note.set").is_ok());
        assert!(AuditAction::new("model_card_note.set").is_ok());
    }
}
