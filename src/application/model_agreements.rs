// Model agreements application service (D-035): reading a provider
// account's models that need an agreement, with their offers, and accepting
// them. Only for an account the organization connected with its own
// credential, never the operator's process credential.
//
// Reading needs `catalog_read`; accepting needs `catalog_write` and is
// idempotent and audited through the admin mutation coordinator. Accepting
// takes explicit model and offer id pairs only. It calls the provider, which
// can take seconds per model, so the calls are not made inside the
// coordinator's transaction (it locks the organization): the key is first
// checked for a recorded result; every named offer is read; each model to
// accept is recorded `accepting` (audited) and committed; the provider is
// called; and the results are then settled, audited per model and completed
// under the key in one short transaction. A request that dies after the
// calls leaves its attempts `accepting`, which the worker settles from the
// provider's availability. Two identical requests sent at once may both
// reach the provider; an agreement that already exists is reported
// `already_accepted`, so neither double-accepts. A model whose agreement is
// already pending or available when read is not accepted again; one whose
// call fails is read again, and recorded `accepted` (pending) or
// `already_accepted` when AWS shows the agreement in place after all, as it
// does for models one Marketplace subscription covers together.
//
// Nothing is accepted when any named model's current offer is not the one
// the administrator saw (`agreement_offer_changed`), or when the
// connection's role may read no model's offers
// (`agreement_permission_missing`); both refusals are audited. The gateway
// never falls back to a billed call to accept terms.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chrono::Utc;
use futures::StreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::admin_identities::{MutationOutcome, deterministic_target_id};
use crate::domain::{
    admin::{
        AdminIdempotencyKey, AdminOperation, AdminPermission, AdminScope, IdempotencyResultMetadata,
    },
    audit::{AuditAction, AuditActor, AuditTargetResource},
    catalog::ProviderFamily,
    credentials::validate_adapter_instance_key,
    identity::{OrganizationId, UserAccountId},
    inference::{ProviderError, ProviderErrorCategory},
    model_agreement::{
        AcceptInput, AcceptInputError, AcceptOutcome, AcceptResponse, AcceptResult,
        AgreementModelView, AgreementOffer, AgreementPermission, AgreementReason, AgreementStatus,
        AgreementsView, OfferView, ProviderAgreementStatus,
    },
    model_evidence::EvidenceReason,
    version::{IfMatch, VersionConflict},
};
use crate::jobs::discovery::ProviderAccounts;
use crate::persistence::{
    admin::{
        AdminMutationRequest, AdminStore, AdminStoreError, HeldClaim, HoldStart, MutationStart,
    },
    auto_offer::AutoOfferStore,
    catalog_sync::CatalogSyncStore,
    model_agreement::{Acceptance, AttemptContext, ModelAgreementStore, StoredAgreement},
    model_evidence::ModelEvidenceStore,
};
use crate::provider::{InferenceProviderAdapter, ModelAgreements};

pub const ROUTE_AGREEMENTS: &str =
    "/api/admin/v1/organizations/{org}/catalog/agreements/{adapter_instance_key}";
pub const ROUTE_AGREEMENTS_ACCEPT: &str =
    "/api/admin/v1/organizations/{org}/catalog/agreements/{adapter_instance_key}/accept";

/// Provider calls made at once for one request: AWS allows the agreement
/// APIs only a request or two a second per account, and the adapter waits
/// out its throttling, so more at once only queues.
const CONCURRENCY: usize = 2;
/// The longest reading every model's offers may take: enough for 100
/// models' reads (and, accepting, their availability) at AWS's rate.
const READ_BUDGET: Duration = Duration::from_secs(90);
/// The longest accepting every model may take: well under the store's
/// `SETTLE_AFTER`, so the worker never settles an attempt a live request
/// is still making.
const CREATE_BUDGET: Duration = Duration::from_secs(120);
/// How long an acceptance's key is held while AWS is called: past every
/// budget (reads, creates and the listing before them).
const CLAIM_HOLD: Duration = Duration::from_secs(300);
/// The longest one accepting call may take before it is unconfirmed.
const CREATE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub enum AgreementError {
    Forbidden,
    NotFound,
    InvalidKey,
    Invalid(AcceptInputError),
    /// The active release declares no provider account with the key.
    UnknownAccount,
    /// The account's family has no model agreements.
    Unsupported,
    /// The gateway cannot reach the account: no credential, or none
    /// configured in this process.
    Unavailable,
    /// The account is called with the gateway operator's own credential,
    /// not one the organization stored: agreements would subscribe the
    /// operator's account.
    ProcessCredential,
    /// The connection's role may not read offers or make agreements.
    PermissionMissing,
    /// A named model's current offers do not include the one named.
    OfferChanged(Vec<OfferChange>),
    /// The provider failed a call every model needed.
    Provider(ProviderError),
    VersionConflict(VersionConflict),
    IdempotencyConflict,
    IdempotencyInProgress,
    Store(String),
    Admin(AdminStoreError),
}

/// A model whose offer changed since the administrator saw it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct OfferChange {
    pub model: String,
    pub requested_offer_id: String,
    pub current_offer_ids: Vec<String>,
}

impl From<sqlx::Error> for AgreementError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(format!("{error:?}"))
    }
}

#[derive(Clone)]
pub struct ModelAgreementService {
    pool: PgPool,
    admin: AdminStore,
    store: ModelAgreementStore,
    evidence: ModelEvidenceStore,
    offers: AutoOfferStore,
    accounts: Option<Arc<dyn ProviderAccounts>>,
}

/// One model the account needs (or was given) an agreement for.
struct Candidate {
    unblocks: Vec<String>,
    /// The evidence of one of its identifiers still says so.
    required: bool,
    agreement: Option<StoredAgreement>,
}

impl ModelAgreementService {
    pub fn new(
        pool: PgPool,
        admin: AdminStore,
        accounts: Option<Arc<dyn ProviderAccounts>>,
    ) -> Self {
        Self {
            store: ModelAgreementStore::new(pool.clone()),
            evidence: ModelEvidenceStore::new(pool.clone()),
            offers: AutoOfferStore::new(pool.clone()),
            pool,
            admin,
            accounts,
        }
    }

    async fn authorize(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        permission: AdminPermission,
    ) -> Result<(), AgreementError> {
        let context = self
            .admin
            .authorization_context(actor, organization_id)
            .await
            .map_err(map_admin_error)?;
        if context.allows(permission, AdminScope::Organization(organization_id)) {
            Ok(())
        } else {
            Err(AgreementError::Forbidden)
        }
    }

    /// The account's family and adapter.
    async fn account(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<(String, Arc<dyn InferenceProviderAdapter>), AgreementError> {
        let mut conn = self.pool.acquire().await?;
        let family = CatalogSyncStore::active_account_family(
            &mut conn,
            organization_id,
            adapter_instance_key,
        )
        .await?
        .ok_or(AgreementError::UnknownAccount)?;
        drop(conn);
        let parsed = family
            .parse::<ProviderFamily>()
            .map_err(|_| AgreementError::Unsupported)?;
        let accounts = self.accounts.as_ref().ok_or(AgreementError::Unavailable)?;
        let unavailable = |error: ProviderError| {
            tracing::warn!(error = ?error, "resolving a provider account for agreements failed");
            AgreementError::Unavailable
        };
        // Only the organization's own credential: an agreement subscribes
        // the account the gateway calls, which for the operator's process
        // credential is the operator's, not the tenant's.
        let adapter = match accounts
            .tenant_adapter(organization_id, adapter_instance_key, parsed)
            .await
            .map_err(unavailable)?
        {
            Some(adapter) => adapter,
            None => {
                let process = accounts
                    .adapter(organization_id, adapter_instance_key, parsed)
                    .await
                    .map_err(unavailable)?;
                return Err(if process.is_some() {
                    AgreementError::ProcessCredential
                } else {
                    AgreementError::Unavailable
                });
            }
        };
        if adapter.model_agreements().is_none() {
            return Err(AgreementError::Unsupported);
        }
        Ok((family, adapter))
    }

    /// Every foundation model of the account whose evidence says an
    /// agreement is required, or that has an agreement on record, with the
    /// listed identifiers each is for.
    async fn candidates(
        &self,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        family: &str,
        agreements: &dyn ModelAgreements,
    ) -> Result<BTreeMap<String, Candidate>, AgreementError> {
        let required: Vec<String> = self
            .evidence
            .for_account(organization_id, adapter_instance_key, family)
            .await?
            .into_iter()
            .filter(|(_, view)| view.reason == Some(EvidenceReason::AgreementRequired))
            .map(|(model, _)| model)
            .collect();
        let mut stored = self
            .store
            .for_account(organization_id, adapter_instance_key, family)
            .await?;
        let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
        if !required.is_empty() {
            let bases = agreements
                .agreement_bases()
                .await
                .map_err(AgreementError::Provider)?;
            for model in required {
                let base = bases.get(&model).cloned().unwrap_or_else(|| model.clone());
                // Only a foundation model has an agreement; an identifier
                // whose base is not one (an ARN) cannot be accepted here.
                if !crate::domain::model_agreement::foundation_model_id(&base) {
                    continue;
                }
                let candidate = candidates.entry(base).or_insert_with(|| Candidate {
                    unblocks: Vec::new(),
                    required: true,
                    agreement: None,
                });
                candidate.unblocks.push(model);
            }
        }
        for (model, candidate) in &mut candidates {
            candidate.agreement = stored.remove(model);
        }
        for (model, agreement) in stored {
            candidates.insert(
                model,
                Candidate {
                    unblocks: agreement.unblocks.clone(),
                    required: false,
                    agreement: Some(agreement),
                },
            );
        }
        for candidate in candidates.values_mut() {
            candidate.unblocks.sort_unstable();
            candidate.unblocks.dedup();
        }
        Ok(candidates)
    }

    pub async fn get(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
    ) -> Result<AgreementsView, AgreementError> {
        validate_adapter_instance_key(adapter_instance_key)
            .map_err(|_| AgreementError::InvalidKey)?;
        self.authorize(actor, organization_id, AdminPermission::CatalogRead)
            .await?;
        let (family, adapter) = self.account(organization_id, adapter_instance_key).await?;
        let agreements = adapter.model_agreements().expect("checked by account");
        let candidates = self
            .candidates(organization_id, adapter_instance_key, &family, agreements)
            .await?;
        let deployments = self
            .offers
            .view(organization_id, adapter_instance_key)
            .await?
            .map(|view| view.models)
            .unwrap_or_default();
        let wanted: Vec<String> = candidates
            .iter()
            .filter(|(_, candidate)| candidate.required)
            .map(|(model, _)| model.clone())
            .collect();
        let (permission, mut offers) = read_offers(agreements, wanted).await;
        let models = candidates
            .into_iter()
            .map(|(model, candidate)| {
                let (offers, offers_error) = match offers.remove(&model) {
                    Some(Ok(found)) => (found.iter().map(OfferView::from).collect(), None),
                    Some(Err(reason)) => (Vec::new(), Some(reason)),
                    None => (Vec::new(), None),
                };
                let mut serving: Vec<String> = deployments
                    .iter()
                    .filter(|offered| candidate.unblocks.contains(&offered.model))
                    .map(|offered| offered.deployment.clone())
                    .collect();
                serving.sort_unstable();
                AgreementModelView {
                    model,
                    deployments: serving,
                    agreement_required: candidate.required,
                    offers,
                    offers_error,
                    agreement: candidate.agreement.map(|stored| stored.view),
                    unblocks: candidate.unblocks,
                }
            })
            .collect();
        Ok(AgreementsView {
            adapter_instance_key: adapter_instance_key.to_owned(),
            family,
            permission,
            models,
        })
    }

    /// Accepts the named models' offers, recording each result.
    pub async fn accept(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: String,
        input: AcceptInput,
        idempotency_key: AdminIdempotencyKey,
        request_id: Uuid,
    ) -> Result<MutationOutcome<AcceptResponse>, AgreementError> {
        validate_adapter_instance_key(&adapter_instance_key)
            .map_err(|_| AgreementError::InvalidKey)?;
        input.validate().map_err(AgreementError::Invalid)?;
        let mut named = input.models.clone();
        named.sort_by(|a, b| a.model.cmp(&b.model));
        let request = || AdminMutationRequest {
            actor_id: actor,
            organization_id,
            scope: AdminScope::Organization(organization_id),
            permission: AdminPermission::CatalogWrite,
            route: ROUTE_AGREEMENTS_ACCEPT.into(),
            method: "POST".into(),
            operation: operation("model_agreement.accept"),
            idempotency_key: idempotency_key.clone(),
            target_id: account_id(organization_id, &adapter_instance_key),
            parent_id: None,
            target: AuditTargetResource::ModelAgreement,
            audit_action: audit_action("model_agreement.accept"),
            if_match: IfMatch::Any,
            request_id,
            body: serde_json::json!({
                "adapter_instance_key": adapter_instance_key,
                "models": named,
                "verify": input.verify,
            }),
        };
        // A key that already has a result replays it without calling the
        // provider. Otherwise the key is claimed and the claim committed
        // before AWS is called, with no lock held: another request with the
        // key meanwhile is refused (`idempotency_conflict` for another body,
        // `idempotency_in_progress` for the same one).
        let coordinator = self.admin.coordinator();
        let claim = match coordinator
            .hold(request(), CLAIM_HOLD)
            .await
            .map_err(map_admin_error)?
        {
            HoldStart::Replay(record) => return replay(*record),
            HoldStart::Held(claim) => claim,
        };
        let outcome = self
            .accept_claimed(
                actor,
                organization_id,
                &adapter_instance_key,
                &named,
                input.verify,
                request_id,
                &request,
                claim,
            )
            .await;
        // A request that failed lets its key go, so a retry is decided
        // afresh (each attempt it recorded is settled by the worker).
        if outcome.is_err()
            && let Err(error) = coordinator.release(&request(), claim).await
        {
            tracing::warn!(error = %error, "releasing an agreement request's key failed");
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn accept_claimed(
        &self,
        actor: UserAccountId,
        organization_id: OrganizationId,
        adapter_instance_key: &str,
        named: &[crate::domain::model_agreement::AcceptModel],
        verify: bool,
        request_id: Uuid,
        request: &(dyn Fn() -> AdminMutationRequest + Sync),
        claim: HeldClaim,
    ) -> Result<MutationOutcome<AcceptResponse>, AgreementError> {
        let adapter_instance_key = adapter_instance_key.to_owned();
        let (family, adapter) = self.account(organization_id, &adapter_instance_key).await?;
        let agreements = adapter.model_agreements().expect("checked by account");
        let candidates = self
            .candidates(organization_id, &adapter_instance_key, &family, agreements)
            .await?;
        let context = AttemptContext {
            organization_id,
            adapter_instance_key: &adapter_instance_key,
            family: &family,
            actor,
            request_id,
        };

        // A named model the gateway has no evidence for is refused without
        // calling the provider.
        let mut results: BTreeMap<String, Acceptance> = BTreeMap::new();
        let mut wanted: Vec<(String, String)> = Vec::new();
        for one in named {
            if candidates.contains_key(&one.model) {
                wanted.push((one.model.clone(), one.offer_id.clone()));
            } else {
                results.insert(
                    one.model.clone(),
                    failure(
                        &one.model,
                        Some(one.offer_id.clone()),
                        AgreementReason::NotAgreementRequired,
                        None,
                    ),
                );
            }
        }

        // Read every named model's offers and agreement first, within a
        // budget: nothing is accepted unless every named offer is still the
        // model's.
        let deadline = tokio::time::Instant::now() + READ_BUDGET;
        let checked = futures::stream::iter(wanted)
            .map(|(model, offer)| async move {
                let read = async {
                    let offers = agreements.agreement_offers(&model).await;
                    let availability = match &offers {
                        Ok(_) => Some(agreements.agreement_availability(&model).await),
                        Err(_) => None,
                    };
                    (offers, availability)
                };
                let (offers, availability) = tokio::time::timeout_at(deadline, read)
                    .await
                    .unwrap_or_else(|_| {
                        (
                            Err(ProviderError::new(ProviderErrorCategory::Timeout)),
                            None,
                        )
                    });
                (model, offer, offers, availability)
            })
            .buffered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let refused = checked
            .iter()
            .filter(|(_, _, offers, _)| matches!(offers, Err(error) if permission_denied(error)))
            .count();
        if !checked.is_empty() && refused == checked.len() {
            self.store
                .audit_refusal(context, account_id(organization_id, &adapter_instance_key))
                .await?;
            return Err(AgreementError::PermissionMissing);
        }
        let changed: Vec<OfferChange> = checked
            .iter()
            .filter_map(|(model, offer, offers, _)| {
                let Ok(offers) = offers else {
                    return None;
                };
                (!offers.iter().any(|current| &current.offer_id == offer)).then(|| OfferChange {
                    model: model.clone(),
                    requested_offer_id: offer.clone(),
                    current_offer_ids: offers.iter().map(|o| o.offer_id.clone()).collect(),
                })
            })
            .collect();
        if !changed.is_empty() {
            self.store
                .audit_refusal(context, account_id(organization_id, &adapter_instance_key))
                .await?;
            return Err(AgreementError::OfferChanged(changed));
        }

        // What each model needs: a failure or an agreement already in place
        // is recorded at the end; an offer to accept is first recorded as
        // being accepted, and committed, so a subscription the call makes
        // is on record even if this request never finishes.
        let mut to_create = Vec::new();
        for (model, offer, offers, availability) in checked {
            let offers = match offers {
                Ok(offers) => offers,
                Err(error) => {
                    let reason = if permission_denied(&error) {
                        AgreementReason::AgreementPermissionMissing
                    } else {
                        AgreementReason::ProviderError
                    };
                    results.insert(
                        model.clone(),
                        failure(&model, Some(offer), reason, Some(&error)),
                    );
                    continue;
                }
            };
            let chosen = offers
                .into_iter()
                .find(|current| current.offer_id == offer)
                .expect("every named offer is current");
            let acceptance = Acceptance {
                model: model.clone(),
                unblocks: candidates
                    .get(&model)
                    .map(|candidate| candidate.unblocks.clone())
                    .unwrap_or_default(),
                offer_id: Some(chosen.offer_id.clone()),
                rate_card: chosen.terms.rate_card.clone(),
                rate_card_region: chosen.terms.rate_card_region.clone(),
                outcome: AcceptOutcome::Accepted,
                reason: None,
                provider_code: None,
                probe: verify,
            };
            // An agreement already in place (or on its way) is not made
            // again.
            if let Some(Ok(Some(current))) = &availability
                && matches!(
                    current.agreement,
                    ProviderAgreementStatus::Available | ProviderAgreementStatus::Pending
                )
            {
                results.insert(
                    model,
                    Acceptance {
                        outcome: AcceptOutcome::AlreadyAccepted,
                        ..acceptance
                    },
                );
                continue;
            }
            to_create.push((acceptance, chosen.token));
        }
        let mut attempts = Vec::with_capacity(to_create.len());
        if !to_create.is_empty() {
            let mut tx = self.pool.begin().await?;
            let now = Utc::now();
            for (acceptance, token) in to_create {
                let attempt =
                    ModelAgreementStore::begin_accepting(&mut tx, context, &acceptance, now)
                        .await?;
                attempts.push((attempt, acceptance, token));
            }
            tx.commit().await?;
        }

        // Accept each in turn (a few at once), within a budget: a model the
        // budget runs out on before its call is not sent at all; one whose
        // call does not answer in time is unconfirmed, for the worker to
        // settle from AWS's availability.
        let deadline = tokio::time::Instant::now() + CREATE_BUDGET;
        let created = futures::stream::iter(attempts)
            .map(|(attempt, mut acceptance, token)| async move {
                if tokio::time::Instant::now() >= deadline {
                    acceptance.outcome = AcceptOutcome::Failed;
                    acceptance.reason = Some(AgreementReason::NotAttempted);
                    return (attempt, acceptance);
                }
                let call = agreements.create_agreement(&acceptance.model, &token);
                match tokio::time::timeout(CREATE_TIMEOUT, call).await {
                    Ok(Ok(())) => {}
                    // An assumption: Bedrock's conflict means an agreement
                    // exists. The worker's check corrects it if not.
                    Ok(Err(error)) if error.error_type() == Some("conflictException") => {
                        acceptance.outcome = AcceptOutcome::AlreadyAccepted;
                    }
                    // A model sold under one Marketplace subscription with
                    // others (Bedrock's Stability image models share one):
                    // accepting one makes the rest's agreements pending,
                    // and their own calls then fail. AWS's availability
                    // says whether the agreement is in place after all.
                    Ok(Err(error))
                        if !permission_denied(&error)
                            && error.category() != ProviderErrorCategory::RateLimited =>
                    {
                        let read = agreements.agreement_availability(&acceptance.model);
                        match tokio::time::timeout(CREATE_TIMEOUT, read).await {
                            Ok(Ok(Some(current)))
                                if current.agreement == ProviderAgreementStatus::Pending =>
                            {
                                acceptance.outcome = AcceptOutcome::Accepted;
                            }
                            Ok(Ok(Some(current)))
                                if current.agreement == ProviderAgreementStatus::Available =>
                            {
                                acceptance.outcome = AcceptOutcome::AlreadyAccepted;
                            }
                            _ => {
                                acceptance.outcome = AcceptOutcome::Failed;
                                acceptance.reason = Some(AgreementReason::ProviderError);
                                acceptance.provider_code = provider_code(&error);
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        acceptance.outcome = AcceptOutcome::Failed;
                        acceptance.reason = Some(if permission_denied(&error) {
                            AgreementReason::AgreementPermissionMissing
                        } else {
                            AgreementReason::ProviderError
                        });
                        acceptance.provider_code = provider_code(&error);
                    }
                    Err(_) => acceptance.outcome = AcceptOutcome::Unconfirmed,
                }
                (attempt, acceptance)
            })
            .buffered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        // Settle and audit every result, and complete the key.
        let coordinator = self.admin.coordinator();
        let mut mutation = match coordinator
            .resume(request(), claim)
            .await
            .map_err(map_admin_error)?
        {
            // An identical request finished first.
            MutationStart::Replay(record) => return replay(record),
            MutationStart::New(mutation) => mutation,
        };
        let now = Utc::now();
        let tx = mutation.transaction();
        for acceptance in results.values() {
            ModelAgreementStore::record(tx, context, acceptance, now).await?;
        }
        for (attempt, acceptance) in created {
            ModelAgreementStore::settle(
                tx,
                organization_id,
                attempt,
                AuditActor::User(actor),
                request_id,
                acceptance.outcome,
                acceptance.reason,
                acceptance.provider_code.as_deref(),
                now,
            )
            .await?;
            results.insert(acceptance.model.clone(), acceptance);
        }
        let response = AcceptResponse {
            adapter_instance_key: adapter_instance_key.clone(),
            results: results
                .into_values()
                .map(|acceptance| AcceptResult {
                    model: acceptance.model,
                    offer_id: acceptance.offer_id,
                    outcome: acceptance.outcome,
                    status: if acceptance.outcome == AcceptOutcome::Failed {
                        AgreementStatus::Failed
                    } else {
                        AgreementStatus::Pending
                    },
                    reason: acceptance.reason,
                    detail: acceptance.reason.map(|reason| reason.detail().to_owned()),
                    provider_code: acceptance.provider_code,
                })
                .collect(),
        };
        let snapshot = serde_json::to_value(&response)
            .map_err(|_| AgreementError::Store("response snapshot is not serializable".into()))?;
        let next = mutation.next_version();
        let target_id = mutation.target_id();
        mutation
            .commit(
                None,
                None,
                Some(IdempotencyResultMetadata {
                    resource_id: target_id,
                    version: Some(next.get()),
                    secret_unavailable_on_replay: false,
                    snapshot: Some(snapshot),
                }),
            )
            .await
            .map_err(map_admin_error)?;
        for result in &response.results {
            crate::metrics::global().inc(
                &crate::metrics::MODEL_AGREEMENTS,
                &[result.outcome.as_str()],
            );
        }
        Ok(MutationOutcome {
            item: response,
            replayed: false,
        })
    }
}

/// Every wanted model's offers, a few at once within [`READ_BUDGET`]. One
/// model's refused read fails that model only: the role is `missing` the
/// permission only when every read was refused, as accepting decides.
async fn read_offers(
    agreements: &dyn ModelAgreements,
    models: Vec<String>,
) -> (
    AgreementPermission,
    BTreeMap<String, Result<Vec<AgreementOffer>, AgreementReason>>,
) {
    let mut found = BTreeMap::new();
    if models.is_empty() {
        return (AgreementPermission::Unknown, found);
    }
    let deadline = tokio::time::Instant::now() + READ_BUDGET;
    let reason = |error: &ProviderError| {
        if permission_denied(error) {
            AgreementReason::AgreementPermissionMissing
        } else {
            AgreementReason::ProviderError
        }
    };
    let mut reads = futures::stream::iter(models.clone())
        .map(|model| async move {
            let result = agreements.agreement_offers(&model).await;
            (model, result)
        })
        .buffer_unordered(CONCURRENCY);
    while let Ok(Some((model, result))) = tokio::time::timeout_at(deadline, reads.next()).await {
        found.insert(model, result.map_err(|error| reason(&error)));
    }
    // Those the budget ran out on.
    for model in models {
        found
            .entry(model)
            .or_insert(Err(AgreementReason::ProviderError));
    }
    let permission = if found.values().any(Result::is_ok) {
        AgreementPermission::Granted
    } else if found
        .values()
        .all(|read| matches!(read, Err(AgreementReason::AgreementPermissionMissing)))
    {
        AgreementPermission::Missing
    } else {
        AgreementPermission::Unknown
    };
    (permission, found)
}

/// Whether the provider refused the call for the role's permissions.
fn permission_denied(error: &ProviderError) -> bool {
    error.category() == ProviderErrorCategory::Authorization
        || error.error_type() == Some("accessDeniedException")
}

fn provider_code(error: &ProviderError) -> Option<String> {
    error
        .error_type()
        .filter(|code| !code.is_empty() && code.len() <= 128)
        .map(str::to_owned)
}

fn failure(
    model: &str,
    offer_id: Option<String>,
    reason: AgreementReason,
    error: Option<&ProviderError>,
) -> Acceptance {
    Acceptance {
        model: model.to_owned(),
        unblocks: Vec::new(),
        offer_id,
        rate_card: Vec::new(),
        rate_card_region: None,
        outcome: AcceptOutcome::Failed,
        reason: Some(reason),
        provider_code: error.and_then(provider_code),
        probe: false,
    }
}

fn replay(
    record: crate::persistence::admin::AdminIdempotencyRecord,
) -> Result<MutationOutcome<AcceptResponse>, AgreementError> {
    let snapshot = record
        .response_metadata
        .and_then(|metadata| metadata.snapshot)
        .ok_or(AgreementError::NotFound)?;
    let item = serde_json::from_value(snapshot)
        .map_err(|_| AgreementError::Store("recorded response snapshot is invalid".into()))?;
    Ok(MutationOutcome {
        item,
        replayed: true,
    })
}

/// Stable per account: the version resource every acceptance for the
/// account advances.
fn account_id(organization_id: OrganizationId, adapter_instance_key: &str) -> Uuid {
    deterministic_target_id(&[
        "model_agreements",
        &organization_id.0.to_string(),
        adapter_instance_key,
    ])
}

fn operation(value: &str) -> AdminOperation {
    AdminOperation::new(value).expect("operation constant is valid")
}

fn audit_action(value: &str) -> AuditAction {
    AuditAction::new(value).expect("audit action constant is valid")
}

fn map_admin_error(error: AdminStoreError) -> AgreementError {
    match error {
        AdminStoreError::Authorization(_) => AgreementError::Forbidden,
        AdminStoreError::ResourceNotFound => AgreementError::NotFound,
        AdminStoreError::VersionConflict(conflict) => AgreementError::VersionConflict(conflict),
        AdminStoreError::IdempotencyConflict => AgreementError::IdempotencyConflict,
        AdminStoreError::IdempotencyInProgress => AgreementError::IdempotencyInProgress,
        other => AgreementError::Admin(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_and_actions_are_valid() {
        for value in [
            "model_agreement.accept",
            "model_agreement.accepting",
            "model_agreement.accepted",
            "model_agreement.unconfirmed",
            "model_agreement.already_accepted",
            "model_agreement.failed",
        ] {
            assert!(AdminOperation::new(value).is_ok(), "{value}");
            assert!(AuditAction::new(value).is_ok(), "{value}");
        }
    }

    /// The contract's schemas list exactly the fields a response carries.
    #[test]
    fn responses_match_the_contract() {
        use crate::domain::model_agreement::{AgreementView, OfferTerms, Rate};
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../../openapi/admin-catalog-v1.json")).unwrap();
        let fields = |value: &serde_json::Value| {
            let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort_unstable();
            keys
        };
        let required = |schema: &str| {
            let mut names: Vec<String> = contract["components"]["schemas"][schema]["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
            names.sort_unstable();
            names
        };
        let rate = Rate {
            dimension: "USE2_input_tokens_global_standard".into(),
            price: Some("0.1".into()),
            unit: Some("Units".into()),
            description: None,
        };
        let offer = OfferView {
            offer_id: "offer-1".into(),
            terms: OfferTerms {
                rate_card: vec![rate.clone()],
                ..OfferTerms::default()
            },
        };
        let state = AgreementView {
            status: AgreementStatus::Pending,
            reason: None,
            detail: None,
            provider_code: None,
            offer_id: "offer-1".into(),
            accepted_by: None,
            accepted_at: Utc::now(),
            checked_at: None,
            verified_at: None,
            probe: false,
            probed_model: None,
            checking: true,
        };
        let model = AgreementModelView {
            model: "anthropic.claude-haiku-5-5".into(),
            unblocks: vec![],
            deployments: vec![],
            agreement_required: true,
            offers: vec![offer.clone()],
            offers_error: None,
            agreement: Some(state.clone()),
        };
        let view = AgreementsView {
            adapter_instance_key: "bedrock".into(),
            family: "bedrock".into(),
            permission: AgreementPermission::Granted,
            models: vec![model.clone()],
        };
        let result = AcceptResult {
            model: "anthropic.claude-haiku-5-5".into(),
            offer_id: None,
            outcome: AcceptOutcome::Failed,
            status: AgreementStatus::Failed,
            reason: None,
            detail: None,
            provider_code: None,
        };
        let results = AcceptResponse {
            adapter_instance_key: "bedrock".into(),
            results: vec![result.clone()],
        };
        for (value, schema) in [
            (serde_json::to_value(&rate).unwrap(), "ModelAgreementRate"),
            (serde_json::to_value(&offer).unwrap(), "ModelAgreementOffer"),
            (serde_json::to_value(&state).unwrap(), "ModelAgreementState"),
            (serde_json::to_value(&model).unwrap(), "ModelAgreementModel"),
            (serde_json::to_value(&view).unwrap(), "ModelAgreements"),
            (
                serde_json::to_value(&result).unwrap(),
                "ModelAgreementResult",
            ),
            (
                serde_json::to_value(&results).unwrap(),
                "ModelAgreementResults",
            ),
        ] {
            assert_eq!(fields(&value), required(schema), "{schema}");
        }
        let reasons = contract["components"]["schemas"]["ModelAgreementState"]["properties"]
            ["reason"]["enum"]
            .as_array()
            .unwrap()
            .clone();
        assert!(reasons.contains(&serde_json::json!("access_request_required")));
    }

    #[test]
    fn agreements_are_per_account() {
        let org = OrganizationId::new();
        assert_eq!(account_id(org, "bedrock"), account_id(org, "bedrock"));
        assert_ne!(account_id(org, "bedrock"), account_id(org, "other"));
    }
}
