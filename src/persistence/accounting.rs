use crate::domain::{
    accounting::{
        AccountingError, BudgetAmount, CatalogProvenance, Currency, PriceSnapshot,
        PricingConfidence, QuotaScope, Reservation, ReservationId, ReservationState, TraceId,
        TraceMetadata, UsageCalculation, UsageConfidence, calculate_usage,
    },
    identity::{OrganizationId, ServiceAccountId, TeamId, UserAccountId},
    inference::{ProviderError, Usage, UsageCompleteness, UsageQuality},
    outbox::{
        InferenceFinalized, OutboxPayload, ReconciliationOpened, ReconciliationResolved,
        ReconciliationUpdated,
    },
};
use crate::persistence::leases::BudgetLeases;
use crate::persistence::outbox::{self, OutboxError};
use crate::persistence::rate_leases::LeaseUse;
use crate::persistence::rate_leases::RateLimitLeases;
use crate::persistence::rate_limits::{Admission, RateLimitBreach, RateLimitStore, TokenDemand};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool, Postgres, Transaction};
use std::{collections::HashSet, num::NonZeroU32, sync::Arc, time::Duration};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AccountingStoreError {
    #[error("accounting database error")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Domain(#[from] AccountingError),
    #[error("idempotency key reused with a different request")]
    IdempotencyConflict,
    #[error("quota exceeded")]
    QuotaExceeded,
    /// The API key's own budget cannot cover the request, though every other
    /// budget covering it could.
    #[error("the API key's budget is exceeded")]
    ApiKeyBudgetExceeded,
    /// No budget covers the request (the organization, its team or its
    /// service account): an organization serves nothing until it has one.
    #[error("no budget covers the request")]
    NoBudget,
    #[error("rate limit exceeded")]
    RateLimited(RateLimitBreach),
    #[error("reservation is not claimable")]
    NotClaimable,
    #[error("reservation is not settleable")]
    NotSettleable,
    #[error("settlement conflicts with an already recorded settlement")]
    SettlementConflict,
    #[error("observed usage exceeded the reserved hard budget")]
    BoundExceeded,
    /// The service account requires every request to name its end user.
    #[error("the request names no end user")]
    EndUserRequired,
    /// The end user has no plan and the service account has no default.
    #[error("the end user is not registered")]
    EndUserNotRegistered,
    /// The end user's allowance and credits cannot cover the request.
    #[error("the end user's allowance and credits are used up")]
    EndUserQuotaExceeded(crate::domain::end_user::EndUserRefusal),
    /// The end user's plan limits their requests or tokens per minute, and
    /// this request is over one.
    #[error("the end user's rate limit is exceeded")]
    EndUserRateLimited(crate::domain::end_user::EndUserRateRefusal),
}

/// Result of an operator resolving a reconciliation item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatorResolution {
    Resolved,
    NotFound,
    /// The item is already resolved, or its reservation is not unknown.
    NotOpen,
    /// The reservation or one of its buckets is locked by in-flight
    /// accounting; the caller may retry.
    Busy,
}

#[derive(Clone)]
pub struct AccountingStore {
    pool: PgPool,
    /// This process's budget leases, when `GATEWAY_BUDGET_LEASES` is on.
    leases: Option<Arc<BudgetLeases>>,
    /// This process's rate-limit leases, when `GATEWAY_RATE_LIMIT_LEASES`
    /// is on.
    rate_leases: Option<Arc<RateLimitLeases>>,
}
impl AccountingStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            leases: None,
            rate_leases: None,
        }
    }
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
    /// Reserves against budget leases this process takes (D-030).
    pub fn with_leases(mut self, leases: Arc<BudgetLeases>) -> Self {
        self.leases = Some(leases);
        self
    }
    pub fn leases(&self) -> Option<&Arc<BudgetLeases>> {
        self.leases.as_ref()
    }
    /// Admits against rate-limit leases this process takes (D-030).
    pub fn with_rate_leases(mut self, leases: Arc<RateLimitLeases>) -> Self {
        self.rate_leases = Some(leases);
        self
    }
    pub fn rate_leases(&self) -> Option<&Arc<RateLimitLeases>> {
        self.rate_leases.as_ref()
    }
    /// Gives back to their leases what committed finalizations no longer
    /// hold: (bucket, lease, amount).
    fn finished(&self, credits: &[(Uuid, Uuid, i64)]) {
        if let Some(leases) = &self.leases {
            for &(bucket, lease, amount) in credits {
                leases.give_back(bucket, lease, amount, true);
            }
        }
    }

    /// The budgets (organization, bucket) covering `scope` that a lease may
    /// cover, remembered for a few seconds. Only steers which leases to
    /// take: the reservation's own transaction finds the budgets that apply.
    async fn lease_candidates(
        &self,
        leases: &BudgetLeases,
        scope: &QuotaScope,
        currency: &str,
        now: DateTime<Utc>,
    ) -> Vec<(Uuid, Uuid)> {
        let key = format!(
            "{}:{:?}:{:?}:{:?}:{currency}",
            scope.organization_id.0,
            scope.team_id.map(|v| v.0),
            scope.service_account_id.map(|v| v.0),
            scope.api_key_id.map(|v| v.0),
        );
        if let Some(buckets) = leases.cached_scope(&key) {
            return buckets;
        }
        let sql = format!(
            "SELECT organization_id, id FROM gateway.quota_bucket WHERE {} AND end_user_id IS NULL",
            scope_sql("$5")
        );
        match sqlx::query_as::<_, (Uuid, Uuid)>(&sql)
            .bind(scope.organization_id.0)
            .bind(scope.team_id.map(|v| v.0))
            .bind(scope.service_account_id.map(|v| v.0))
            .bind(currency)
            .bind(now)
            .bind(scope.api_key_id.map(|v| v.0))
            .fetch_all(&self.pool)
            .await
        {
            Ok(buckets) => {
                leases.cache_scope(key, buckets.clone());
                buckets
            }
            Err(_) => Vec::new(),
        }
    }
}

/// What one new reservation holds on budget leases: given back to them if
/// its transaction is dropped before it could have committed.
struct LeaseHolds<'a> {
    leases: Option<&'a BudgetLeases>,
    /// (bucket, lease, amount).
    held: Vec<(Uuid, Uuid, i64)>,
    committing: bool,
}

impl<'a> LeaseHolds<'a> {
    fn new(leases: Option<&'a BudgetLeases>) -> Self {
        Self {
            leases,
            held: Vec::new(),
            committing: false,
        }
    }

    fn leases(&self) -> Vec<Uuid> {
        self.held.iter().map(|&(_, lease, _)| lease).collect()
    }

    /// Keeps only holds on leases the database says are usable; the others
    /// are given back and their leases drain.
    fn keep_usable(&mut self, usable: &HashSet<Uuid>) {
        let Some(leases) = self.leases else {
            return;
        };
        self.held.retain(|&(bucket, lease, amount)| {
            if usable.contains(&lease) {
                return true;
            }
            leases.give_back(bucket, lease, amount, true);
            leases.retire(bucket, lease);
            false
        });
    }

    fn lease_for(&self, bucket: Uuid) -> Option<Uuid> {
        self.held
            .iter()
            .find(|&&(held, _, _)| held == bucket)
            .map(|&(_, lease, _)| lease)
    }

    /// The reservation holds `amount`: what each lease held past it goes
    /// back.
    fn trim(&mut self, amount: i64) {
        let Some(leases) = self.leases else {
            return;
        };
        for (bucket, lease, held) in &mut self.held {
            if *held > amount {
                leases.give_back(*bucket, *lease, *held - amount, false);
                *held = amount;
            }
        }
    }
}

impl Drop for LeaseHolds<'_> {
    fn drop(&mut self) {
        // Once the commit was sent its outcome may be unknown, so what was
        // held stays used until the lease returns.
        if self.committing {
            return;
        }
        if let Some(leases) = self.leases {
            for &(bucket, lease, amount) in &self.held {
                leases.give_back(bucket, lease, amount, true);
            }
        }
    }
}

/// The admission one new reservation holds on a rate-limit lease: given back
/// to it if the reservation's transaction is dropped before it could have
/// committed.
struct RateHold<'a> {
    leases: Option<&'a RateLimitLeases>,
    scope: &'a QuotaScope,
    /// The lease and the admission's ticket.
    lease: Option<(Uuid, i32)>,
    committing: bool,
}

impl<'a> RateHold<'a> {
    fn new(leases: Option<&'a RateLimitLeases>, scope: &'a QuotaScope) -> Self {
        Self {
            leases,
            scope,
            lease: None,
            committing: false,
        }
    }

    /// The database says the lease is no longer usable: the request is
    /// admitted on the policies' rows instead, and the lease takes no more.
    fn retire(&mut self) {
        if let (Some(leases), Some((lease, _))) = (self.leases, self.lease.take()) {
            leases.retire(self.scope, lease);
        }
    }

    /// The request's key has a policy of its own: the request is admitted
    /// on the policies' rows, its ticket goes back to the lease, and the
    /// key's requests skip leases for a while.
    fn keyed(&mut self) {
        if let (Some(leases), Some((lease, ticket))) = (self.leases, self.lease.take()) {
            leases.give_back(self.scope, lease, ticket);
            leases.keyed(self.scope);
        }
    }
}

impl Drop for RateHold<'_> {
    fn drop(&mut self) {
        // Once the commit was sent its outcome may be unknown, so the
        // admission stays used; the lease's return counts what committed.
        if self.committing {
            return;
        }
        if let (Some(leases), Some((lease, ticket))) = (self.leases, self.lease) {
            leases.give_back(self.scope, lease, ticket);
        }
    }
}

/// The active leases among those `allocations` were made under, with their
/// advisory locks taken shared: an allocation under one moves nothing on
/// its bucket when it is finalized.
async fn active_leases(
    conn: &mut PgConnection,
    allocations: &[AllocationRow],
) -> Result<HashSet<Uuid>, sqlx::Error> {
    let leases: Vec<Uuid> = allocations.iter().filter_map(|a| a.lease_id).collect();
    crate::persistence::leases::lock_active(conn, &leases).await
}

#[derive(Clone, Debug)]
pub(crate) struct ReservationRequest {
    pub scope: QuotaScope,
    pub idempotency_key: String,
    pub fingerprint: [u8; 32],
    pub price: PriceSnapshot,
    pub currency: Currency,
    pub deployment: String,
    pub model: String,
    pub input_bound: u64,
    pub output_token_cap: Option<NonZeroU32>,
    pub reserve_output: bool,
    /// Reasoning tokens reserved beyond the output cap, for a model whose
    /// reasoning the cap does not bound; zero otherwise.
    pub reasoning_allowance: u32,
    pub ttl: Duration,
    pub trace_id: TraceId,
    pub correlation_id: Uuid,
    pub trace_metadata: TraceMetadata,
    pub catalog_provenance: Option<CatalogProvenance>,
    /// The API key that made the request, recorded on its trace.
    pub api_key_id: Option<Uuid>,
    /// The app's end user the request is for (D-028), as the app named it.
    pub end_user: Option<String>,
    /// What the request is held at when it may be served at a higher price
    /// than `price`: the ceiling of a route's deployments, so a failover
    /// stays inside the reservation (D-023). Settlement charges `price`.
    pub hold_price: Option<PriceSnapshot>,
    /// The input tokens held when another deployment the request may fail
    /// over to could count more than `input_bound`: the route's strict
    /// byte bound, above an exact provider count (D-023).
    pub hold_input_bound: Option<u64>,
}

/// Where a failover attempt came from: the claimed attempt it replaces.
#[derive(Clone, Copy, Debug)]
struct Lineage {
    failover_of: ReservationId,
    failed_trace: TraceId,
    attempt: i16,
}

#[derive(Clone, Debug)]
pub enum ReserveResult {
    Created(Reservation),
    Existing(Reservation),
}
#[derive(Clone, Debug)]
pub enum ClaimOutcome {
    Won(Reservation),
    AlreadyClaimed(Reservation),
}
#[derive(Clone, Debug)]
pub struct Settlement {
    /// Chosen once per finalization and reused by every retry of it, so a
    /// retry after a lost commit reply replays rather than settles twice.
    pub finalization_id: Uuid,
    pub usage: Option<Usage>,
    pub trace_id: TraceId,
    pub provider_request_id: Option<String>,
    /// Why the provider request failed, recorded on the trace.
    pub provider_error: Option<ProviderError>,
    /// How the request went, recorded on its trace in the same commit.
    pub timings: crate::domain::model_card::RequestTimings,
}

#[derive(Clone, Debug, FromRow)]
struct ReservationRow {
    id: Uuid,
    organization_id: Uuid,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    idempotency_key: String,
    fingerprint: Vec<u8>,
    state: String,
    reserved_minor: i64,
    expires_at: DateTime<Utc>,
    output_token_cap: Option<i64>,
    input_price_minor: i64,
    output_price_minor: i64,
    cached_input_price_minor: i64,
    reasoning_price_minor: i64,
    other_price_minor: i64,
    price_version: String,
    pricing_confidence: String,
    currency: String,
    deployment: String,
    model: String,
    trace_id: Uuid,
    catalog_release_id: Option<Uuid>,
    catalog_route_id: Option<Uuid>,
    catalog_deployment_id: Option<Uuid>,
    catalog_price_id: Option<Uuid>,
    catalog_digest: Option<Vec<u8>>,
    catalog_operation: Option<String>,
    catalog_family: Option<String>,
    catalog_price_source: Option<String>,
    catalog_chat_input_count_mode: Option<String>,
    catalog_price_override_id: Option<Uuid>,
    _created_at: DateTime<Utc>,
    settlement_fingerprint: Option<Vec<u8>>,
    api_key_id: Option<Uuid>,
}
impl ReservationRow {
    fn price(&self) -> Result<PriceSnapshot, AccountingStoreError> {
        let confidence = match self.pricing_confidence.as_str() {
            "known" => PricingConfidence::Known,
            "conservative" => PricingConfidence::Conservative,
            _ => return Err(AccountingStoreError::NotSettleable),
        };
        Ok(PriceSnapshot::configured_with_breakdown(
            self.input_price_minor,
            self.output_price_minor,
            self.cached_input_price_minor,
            self.reasoning_price_minor,
            self.other_price_minor,
            self.price_version.clone(),
            confidence,
        )?)
    }

    fn domain(self) -> Result<Reservation, AccountingStoreError> {
        let state = match self.state.as_str() {
            "reserved" => ReservationState::Reserved,
            "claimed" => ReservationState::Claimed,
            "settled" => ReservationState::Settled,
            "released" => ReservationState::Released,
            "unknown" => ReservationState::Unknown,
            "expired" => ReservationState::Expired,
            _ => return Err(AccountingStoreError::NotSettleable),
        };
        let cap = self
            .output_token_cap
            .map(|value| {
                NonZeroU32::new(u32::try_from(value).map_err(|_| AccountingError::Overflow)?)
                    .ok_or(AccountingError::Overflow)
            })
            .transpose()?;
        let pricing_confidence = match self.pricing_confidence.as_str() {
            "known" => PricingConfidence::Known,
            "conservative" => PricingConfidence::Conservative,
            _ => return Err(AccountingStoreError::NotSettleable),
        };
        Ok(Reservation {
            id: ReservationId(self.id),
            scope: QuotaScope {
                organization_id: OrganizationId(self.organization_id),
                team_id: self.team_id.map(crate::domain::identity::TeamId),
                service_account_id: self
                    .service_account_id
                    .map(crate::domain::identity::ServiceAccountId),
                api_key_id: self.api_key_id.map(crate::domain::identity::ApiKeyId),
            },
            idempotency_key: self.idempotency_key,
            fingerprint: self.fingerprint,
            reserved: BudgetAmount::new(self.reserved_minor)?,
            expires_at: self.expires_at,
            state,
            price: PriceSnapshot::configured_with_breakdown(
                self.input_price_minor,
                self.output_price_minor,
                self.cached_input_price_minor,
                self.reasoning_price_minor,
                self.other_price_minor,
                self.price_version,
                pricing_confidence,
            )?,
            currency: Currency::new(self.currency)?,
            output_token_cap: cap,
            deployment: self.deployment,
            model: self.model,
            trace_id: TraceId(self.trace_id),
            catalog_provenance: match (
                self.catalog_release_id,
                self.catalog_route_id,
                self.catalog_deployment_id,
                self.catalog_price_id,
                self.catalog_digest,
                self.catalog_operation,
                self.catalog_family,
                self.catalog_price_source,
                self.catalog_chat_input_count_mode,
            ) {
                (None, None, None, None, None, None, None, None, None)
                    if self.catalog_price_override_id.is_none() =>
                {
                    None
                }
                (
                    Some(release_id),
                    Some(route_id),
                    Some(deployment_id),
                    Some(price_id),
                    Some(digest),
                    Some(operation),
                    Some(family),
                    Some(price_source),
                    Some(chat_input_count_mode),
                ) => Some(CatalogProvenance {
                    release_id,
                    route_id,
                    deployment_id,
                    price_id,
                    digest: digest
                        .try_into()
                        .map_err(|_| AccountingStoreError::NotSettleable)?,
                    operation,
                    family,
                    price_source,
                    chat_input_count_mode,
                    price_override_id: self.catalog_price_override_id,
                }),
                _ => return Err(AccountingStoreError::NotSettleable),
            },
        })
    }
}

const RESERVATION_COLUMNS: &str = "id,organization_id,team_id,service_account_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id,created_at AS _created_at,settlement_fingerprint,api_key_id";
fn scope_sql(time: &str) -> String {
    format!(
        "organization_id=$1 AND currency=$4 AND ((scope_kind='organization' AND team_id IS NULL AND service_account_id IS NULL) OR (scope_kind='team' AND team_id=$2 AND service_account_id IS NULL) OR (scope_kind='service-account' AND service_account_id=$3 AND ((team_id=$2) OR (team_id IS NULL AND $2 IS NULL))) OR (scope_kind='api-key' AND api_key_id=$6)) AND period_start <= {time} AND period_end > {time} AND retired_at IS NULL"
    )
}
impl AccountingStore {
    pub async fn create_quota_bucket(
        &self,
        scope: &QuotaScope,
        limit: BudgetAmount,
        currency: &Currency,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Uuid, AccountingStoreError> {
        scope.validate()?;
        // A key's bucket names only the key; the others name what they cover.
        let (kind, team_id, service_account_id) = if scope.api_key_id.is_some() {
            ("api-key", None, None)
        } else if scope.service_account_id.is_some() {
            ("service-account", scope.team_id, scope.service_account_id)
        } else if scope.team_id.is_some() {
            ("team", scope.team_id, None)
        } else {
            ("organization", None, None)
        };
        Ok(sqlx::query_scalar("INSERT INTO gateway.quota_bucket (id,organization_id,team_id,service_account_id,api_key_id,scope_kind,period_start,period_end,currency,limit_minor) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING id").bind(Uuid::new_v4()).bind(scope.organization_id.0).bind(team_id.map(|v|v.0)).bind(service_account_id.map(|v|v.0)).bind(scope.api_key_id.map(|v|v.0)).bind(kind).bind(start).bind(end).bind(currency.as_str()).bind(limit.value()).fetch_one(&self.pool).await?)
    }

    pub async fn find_reservation(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
    ) -> Result<Option<Reservation>, AccountingStoreError> {
        let row = sqlx::query_as::<_, ReservationRow>(&format!("SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND id=$2")).bind(organization_id.0).bind(id.0).fetch_optional(&self.pool).await?;
        row.map(ReservationRow::domain).transpose()
    }

    pub async fn find_reservation_by_idempotency(
        &self,
        organization_id: OrganizationId,
        idempotency_key: &str,
    ) -> Result<Option<Reservation>, AccountingStoreError> {
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND service_account_id IS NULL AND idempotency_key=$2"
        ))
        .bind(organization_id.0)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ReservationRow::domain).transpose()
    }

    pub async fn find_reservation_by_service_account_idempotency(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        idempotency_key: &str,
    ) -> Result<Option<Reservation>, AccountingStoreError> {
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND service_account_id=$2 AND idempotency_key=$3"
        ))
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ReservationRow::domain).transpose()
    }

    /// Admits `request` and holds its amount on every budget that covers
    /// it, in one transaction with its trace. With `claim`, a new
    /// reservation is written claimed, as the caller would claim it next
    /// (one commit instead of two); a replay is returned as it stands.
    pub(crate) async fn reserve(
        &self,
        request: ReservationRequest,
        now: DateTime<Utc>,
        claim: bool,
    ) -> Result<ReserveResult, AccountingStoreError> {
        request.scope.validate()?;
        if request.idempotency_key.trim().is_empty() {
            return Err(AccountingError::InvalidIdempotency.into());
        }
        let ttl =
            chrono::Duration::from_std(request.ttl).map_err(|_| AccountingError::InvalidTtl)?;
        now.checked_add_signed(ttl)
            .ok_or(AccountingError::Overflow)?;
        // With budget leases on: the most this request can hold, which a
        // lease must have room for to admit it without the budget's row, and
        // leases taken (in their own transactions) where they are worth it.
        let leasing = match &self.leases {
            Some(leases) => match admit(&request, i128::from(i64::MAX), None) {
                Ok((_, most)) => {
                    let buckets = self
                        .lease_candidates(leases, &request.scope, request.currency.as_str(), now)
                        .await;
                    leases.prepare(&buckets, most.value()).await;
                    Some((leases.as_ref(), most.value()))
                }
                Err(_) => None,
            },
            None => None,
        };
        let mut holds = LeaseHolds::new(leasing.map(|(leases, _)| leases));
        // With rate-limit leases on: a new lease for the scope where one is
        // worth it, taken before this request holds a connection (taking it
        // needs one of its own).
        let mut rate_hold = RateHold::new(self.rate_leases.as_deref(), &request.scope);
        if let Some(leases) = &self.rate_leases {
            leases.prepare(&request.scope).await;
        }
        let mut tx = self.pool.begin().await?;
        // A missing row cannot be locked by SELECT ... FOR UPDATE. Serialize
        // first use of each tenant/idempotency-key pair before checking for
        // an existing reservation so only one transaction can create it.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "{}:{}:{}",
                request.scope.organization_id.0,
                request
                    .scope
                    .service_account_id
                    .map_or_else(|| "-".to_owned(), |id| id.0.to_string()),
                request.idempotency_key
            ))
            .execute(&mut *tx)
            .await?;
        let existing = sqlx::query_as::<_, ReservationRow>(&format!("SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND service_account_id IS NOT DISTINCT FROM $2 AND idempotency_key=$3 FOR UPDATE")).bind(request.scope.organization_id.0).bind(request.scope.service_account_id.map(|id| id.0)).bind(&request.idempotency_key).fetch_optional(&mut *tx).await?;
        if let Some(row) = existing {
            if row.fingerprint != request.fingerprint {
                return Err(AccountingStoreError::IdempotencyConflict);
            }
            let result = ReserveResult::Existing(row.domain()?);
            tx.commit().await?;
            return Ok(result);
        }
        // Only a new reservation is admitted, so a replay above is never
        // limited. Policies (or a rate-limit lease's lock) lock before quota
        // buckets on every path.
        // The admission is taken from the lease only now, with the
        // connection in hand, just before the database checks it: taken
        // before waiting for a connection, it could outlive the lease's
        // one second and be wasted.
        if let Some(leases) = &self.rate_leases {
            rate_hold.lease = leases.hold(&request.scope);
        }
        let leased_admission = match rate_hold.lease {
            Some((lease, ticket)) => {
                match crate::persistence::rate_leases::use_lease(
                    &mut tx,
                    lease,
                    ticket,
                    &request.scope,
                )
                .await?
                {
                    LeaseUse::Admitted(at) => Some((lease, ticket, at)),
                    // The key has a policy of its own, which leases do not
                    // cover: admitted on the policies' rows, and the lease
                    // keeps its ticket for another key's request.
                    LeaseUse::Keyed => {
                        rate_hold.keyed();
                        None
                    }
                    LeaseUse::Unusable => {
                        rate_hold.retire();
                        None
                    }
                }
            }
            None => None,
        };
        // A policy limiting tokens is never leased, so a leased admission
        // has no token room to keep to.
        let policy_token_room = match leased_admission {
            Some(_) => None,
            None => match RateLimitStore::check_admission(
                &mut tx,
                &request.scope,
                TokenDemand::Request {
                    min_tokens: min_tokens(&request),
                },
            )
            .await?
            {
                Admission::Refused(breach) => {
                    return Err(AccountingStoreError::RateLimited(breach));
                }
                Admission::Admitted { token_room } => token_room,
            },
        };
        // Admission may have waited on locks; the reservation's lifetime
        // starts when it is admitted, not when the request arrived.
        let now = now.max(Utc::now());
        // A budget's bucket for this window exists once its first request
        // needs it; a request no budget covers still finds no bucket.
        crate::persistence::budgets::BudgetStore::materialize(
            &mut tx,
            &request.scope,
            request.currency.as_str(),
            now,
        )
        .await?;
        // The end user's allowance and credits, if the app limits its users;
        // locked with the budgets' buckets, in id order, as settlement locks
        // them.
        let end_user = crate::persistence::end_users::admit(
            &mut tx,
            &request.scope,
            request.end_user.as_deref(),
            request.currency.as_str(),
            now,
        )
        .await?;
        // The plan's per-user rate limits, with the user's row locked; the
        // tokens left, here and under the policies' limits, cap the output
        // as a budget does.
        let user_token_room = match &end_user {
            Some(admission) => {
                crate::persistence::end_users::check_rate(
                    &mut tx,
                    admission,
                    crate::persistence::end_users::RateDemand::Request {
                        min_tokens: min_tokens(&request),
                    },
                )
                .await?
            }
            None => None,
        };
        let token_room = min_room(policy_token_room, user_token_room);
        let draw = end_user
            .as_ref()
            .and_then(|admission| admission.draw.clone());
        let draw_ids = draw.as_ref().map(|d| d.bucket_ids()).unwrap_or_default();
        let sql = format!(
            "SELECT id,scope_kind,limit_minor,reserved_minor,consumed_minor,end_user_id FROM gateway.quota_bucket WHERE ({}) OR id = ANY($7) ORDER BY id{}",
            scope_sql("$5"),
            if leasing.is_some() { "" } else { " FOR UPDATE" }
        );
        let mut locked = sqlx::query_as::<_, LockedBucketRow>(&sql)
            .bind(request.scope.organization_id.0)
            .bind(request.scope.team_id.map(|v| v.0))
            .bind(request.scope.service_account_id.map(|v| v.0))
            .bind(request.currency.as_str())
            .bind(now)
            .bind(request.scope.api_key_id.map(|v| v.0))
            .bind(&draw_ids)
            .fetch_all(&mut *tx)
            .await?;
        if let Some((leases, most)) = leasing {
            // Read unlocked above: a budget a lease covers is never locked.
            // Its row stands in for the lease, with exactly the lease's hold
            // as its room. The lease locks come first, then the other rows,
            // locked now and read again.
            for bucket in locked.iter().filter(|b| b.end_user_id.is_none()) {
                if let Some(lease) = leases.hold(bucket.id, most) {
                    holds.held.push((bucket.id, lease, most));
                }
            }
            let usable = crate::persistence::leases::lock_usable(&mut tx, &holds.leases()).await?;
            holds.keep_usable(&usable);
            let rest: Vec<Uuid> = locked
                .iter()
                .map(|bucket| bucket.id)
                .filter(|&id| holds.lease_for(id).is_none())
                .collect();
            let mut rows = sqlx::query_as::<_, LockedBucketRow>(
                "SELECT id,scope_kind,limit_minor,reserved_minor,consumed_minor,end_user_id FROM gateway.quota_bucket WHERE id = ANY($1) ORDER BY id FOR UPDATE",
            )
            .bind(&rest)
            .fetch_all(&mut *tx)
            .await?;
            rows.extend(
                locked
                    .into_iter()
                    .filter(|bucket| holds.lease_for(bucket.id).is_some())
                    .map(|bucket| LockedBucketRow {
                        limit_minor: most,
                        reserved_minor: 0,
                        consumed_minor: 0,
                        ..bucket
                    }),
            );
            locked = rows;
        }
        let (buckets, end_user_buckets): (Vec<LockedBucketRow>, Vec<LockedBucketRow>) = locked
            .into_iter()
            .partition(|bucket| bucket.end_user_id.is_none());
        let buckets: Vec<BucketRow> = buckets.into_iter().map(LockedBucketRow::budget).collect();
        if buckets.is_empty() {
            return Err(AccountingStoreError::NoBudget);
        }
        let end_user_draw = match (&end_user, &draw) {
            (Some(admission), Some(draw)) => {
                let state = EndUserDrawState::new(admission, draw, &end_user_buckets)?;
                state.sync_allowance(&mut tx).await?;
                Some(state)
            }
            _ => None,
        };
        let (budget_available, others_available) = available(&buckets)?;
        let user_available = end_user_draw.as_ref().map(EndUserDrawState::available);
        let available = user_available.map_or(budget_available, |user| budget_available.min(user));
        // Refused on the end user's account when theirs is what runs out.
        let quota_exceeded = || match &end_user_draw {
            Some(draw) if draw.available() <= budget_available => {
                AccountingStoreError::EndUserQuotaExceeded(draw.refusal())
            }
            _ => AccountingStoreError::QuotaExceeded,
        };
        let (output_token_cap, amount) = match admit(&request, available, token_room) {
            Ok(admitted) => admitted,
            Err(AccountingStoreError::QuotaExceeded) => {
                let error = quota_exceeded();
                // Refused only because of the key's own budget: the caller
                // says so, since the service account's budgets still have
                // room.
                if matches!(error, AccountingStoreError::QuotaExceeded)
                    && buckets.iter().any(BucketRow::is_api_key)
                    && admit(
                        &request,
                        user_available.map_or(others_available, |user| others_available.min(user)),
                        token_room,
                    )
                    .is_ok()
                {
                    return Err(AccountingStoreError::ApiKeyBudgetExceeded);
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        holds.trim(amount.value());
        for bucket in buckets.iter().filter(|b| holds.lease_for(b.id).is_none()) {
            sqlx::query(
                "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor+$1 WHERE id=$2",
            )
            .bind(amount.value())
            .bind(bucket.id)
            .execute(&mut *tx)
            .await?;
        }
        let end_user_shares = match &end_user_draw {
            Some(draw) => {
                let shares = draw.split(amount.value()).ok_or_else(quota_exceeded)?;
                for share in &shares {
                    sqlx::query(
                        "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor+$1 WHERE id=$2",
                    )
                    .bind(share.amount)
                    .bind(share.bucket)
                    .execute(&mut *tx)
                    .await?;
                }
                shares
            }
            None => Vec::new(),
        };
        let end_user_id = end_user.as_ref().map(|admission| admission.end_user_id);
        let expires = now
            .max(Utc::now())
            .checked_add_signed(ttl)
            .ok_or(AccountingError::Overflow)?;
        let id = ReservationId::new();
        insert_reservation(
            &mut tx,
            &request,
            id,
            amount,
            output_token_cap,
            expires,
            end_user_id,
            None,
            claim,
            leased_admission,
        )
        .await?;
        for bucket in &buckets {
            sqlx::query("INSERT INTO gateway.accounting_allocation (id,reservation_id,quota_bucket_id,organization_id,reserved_minor,state,lease_id) VALUES ($1,$2,$3,$4,$5,'held',$6)").bind(Uuid::new_v4()).bind(id.0).bind(bucket.id).bind(request.scope.organization_id.0).bind(amount.value()).bind(holds.lease_for(bucket.id)).execute(&mut *tx).await?;
        }
        for share in &end_user_shares {
            sqlx::query("INSERT INTO gateway.accounting_allocation (id,reservation_id,quota_bucket_id,organization_id,reserved_minor,state,draw_rank) VALUES ($1,$2,$3,$4,$5,'held',$6)").bind(Uuid::new_v4()).bind(id.0).bind(share.bucket).bind(request.scope.organization_id.0).bind(share.amount).bind(share.rank).execute(&mut *tx).await?;
        }
        holds.committing = true;
        rate_hold.committing = true;
        tx.commit().await?;
        drop(rate_hold);
        if self.rate_leases.is_some() {
            crate::metrics::global().inc(
                &crate::metrics::RATE_LIMIT_ADMISSIONS,
                &[if leased_admission.is_some() {
                    "lease"
                } else {
                    "policy"
                }],
            );
        }
        if leasing.is_some() {
            let leased = holds.held.len();
            let metrics = crate::metrics::global();
            metrics.add(
                &crate::metrics::BUDGET_LEASE_RESERVATIONS,
                &["lease"],
                leased as u64,
            );
            metrics.add(
                &crate::metrics::BUDGET_LEASE_RESERVATIONS,
                &["bucket"],
                buckets.len().saturating_sub(leased) as u64,
            );
        }
        Ok(ReserveResult::Created(Reservation {
            id,
            scope: request.scope,
            idempotency_key: request.idempotency_key,
            fingerprint: request.fingerprint.to_vec(),
            reserved: amount,
            expires_at: expires,
            state: if claim {
                ReservationState::Claimed
            } else {
                ReservationState::Reserved
            },
            price: request.price.clone(),
            currency: request.currency,
            output_token_cap,
            deployment: request.deployment,
            model: request.model,
            trace_id: request.trace_id,
            catalog_provenance: request.catalog_provenance,
        }))
    }

    /// Reserves every line of a batch in `tx`, the transaction that creates
    /// the batch, with the batch row already written. Each line is an
    /// ordinary reservation naming the batch, held for its full output cap:
    /// a batch is never cut to what a budget affords, so if any currency's
    /// lines together exceed what its budgets have left, nothing is held.
    /// The caller admits the batch against rate limits once, before it
    /// writes the batch row; its lines are not counted. Line idempotency keys are the batch's own, so no
    /// line can already exist.
    pub(crate) async fn reserve_batch_lines(
        tx: &mut PgConnection,
        scope: &QuotaScope,
        batch_id: Uuid,
        lines: &[ReservationRequest],
        now: DateTime<Utc>,
    ) -> Result<Vec<ReservationId>, AccountingStoreError> {
        scope.validate()?;
        let now = now.max(Utc::now());
        // Each line's full hold: the input upper bound, the whole output
        // cap and any reasoning allowance.
        let mut amounts = Vec::with_capacity(lines.len());
        for line in lines {
            let output = if line.reserve_output {
                line.output_token_cap
                    .map(|cap| u64::from(cap.get()))
                    .ok_or(AccountingStoreError::QuotaExceeded)?
            } else {
                0
            };
            let allowance = if line.reserve_output {
                i128::from(line.reasoning_allowance)
                    .checked_mul(i128::from(line.price.reasoning_rate()))
                    .ok_or(AccountingError::Overflow)?
            } else {
                0
            };
            let amount = line.price.reserve_tokens(line.input_bound, output)?;
            let amount = i64::try_from(i128::from(amount.value()) + allowance)
                .map_err(|_| AccountingError::Overflow)
                .and_then(BudgetAmount::new)?;
            amounts.push(amount);
        }
        // Amounts in different currencies are never summed: each currency's
        // total is reserved against that currency's budgets.
        let mut currencies: Vec<&str> = lines.iter().map(|l| l.currency.as_str()).collect();
        currencies.sort_unstable();
        currencies.dedup();
        let mut buckets_by_currency = std::collections::HashMap::new();
        let mut key_refused = false;
        // A batch is for one end user, named when it was created.
        let end_user_name = lines.first().and_then(|line| line.end_user.clone());
        let mut end_user_id = None;
        let mut line_shares: Vec<Vec<crate::domain::end_user::Share>> =
            vec![Vec::new(); lines.len()];
        for currency in currencies {
            crate::persistence::budgets::BudgetStore::materialize(&mut *tx, scope, currency, now)
                .await?;
            let end_user = crate::persistence::end_users::admit(
                &mut *tx,
                scope,
                end_user_name.as_deref(),
                currency,
                now,
            )
            .await?;
            // A batch is one admission against its user's plan limits,
            // checked once, with the user's row locked.
            if end_user_id.is_none()
                && let Some(admission) = end_user.as_ref()
            {
                crate::persistence::end_users::check_rate(
                    &mut *tx,
                    admission,
                    crate::persistence::end_users::RateDemand::Batch,
                )
                .await?;
            }
            end_user_id = end_user_id.or(end_user.as_ref().map(|a| a.end_user_id));
            let draw = end_user
                .as_ref()
                .and_then(|admission| admission.draw.clone());
            let draw_ids = draw.as_ref().map(|d| d.bucket_ids()).unwrap_or_default();
            let sql = format!(
                "SELECT id,scope_kind,limit_minor,reserved_minor,consumed_minor,end_user_id FROM gateway.quota_bucket WHERE ({}) OR id = ANY($7) ORDER BY id FOR UPDATE",
                scope_sql("$5")
            );
            let locked = sqlx::query_as::<_, LockedBucketRow>(&sql)
                .bind(scope.organization_id.0)
                .bind(scope.team_id.map(|v| v.0))
                .bind(scope.service_account_id.map(|v| v.0))
                .bind(currency)
                .bind(now)
                .bind(scope.api_key_id.map(|v| v.0))
                .bind(&draw_ids)
                .fetch_all(&mut *tx)
                .await?;
            let (buckets, end_user_buckets): (Vec<LockedBucketRow>, Vec<LockedBucketRow>) = locked
                .into_iter()
                .partition(|bucket| bucket.end_user_id.is_none());
            let buckets: Vec<BucketRow> =
                buckets.into_iter().map(LockedBucketRow::budget).collect();
            if buckets.is_empty() {
                return Err(AccountingStoreError::NoBudget);
            }
            let total = lines
                .iter()
                .zip(&amounts)
                .filter(|(line, _)| line.currency.as_str() == currency)
                .try_fold(0i64, |total, (_, amount)| total.checked_add(amount.value()))
                .ok_or(AccountingError::Overflow)?;
            let mut end_user_draw = match (&end_user, &draw) {
                (Some(admission), Some(draw)) => {
                    let state = EndUserDrawState::new(admission, draw, &end_user_buckets)?;
                    state.sync_allowance(&mut *tx).await?;
                    Some(state)
                }
                _ => None,
            };
            // The same reasons as one request's: the user's account when
            // theirs is what runs out, otherwise the key's or another budget.
            let (budget_available, others_available) = available(&buckets)?;
            let user_available = end_user_draw.as_ref().map(EndUserDrawState::available);
            let limit = user_available.map_or(budget_available, |user| budget_available.min(user));
            if i128::from(total) > limit {
                if let Some(draw) = end_user_draw.as_ref()
                    && draw.available() <= budget_available
                {
                    return Err(AccountingStoreError::EndUserQuotaExceeded(draw.refusal()));
                }
                let others =
                    user_available.map_or(others_available, |user| others_available.min(user));
                if i128::from(total) > others || !buckets.iter().any(BucketRow::is_api_key) {
                    return Err(AccountingStoreError::QuotaExceeded);
                }
                // Only the key's budget refuses this currency; another
                // currency's other budgets may still refuse the batch, and
                // then the key is not the whole reason.
                key_refused = true;
                continue;
            }
            if let Some(state) = end_user_draw.as_mut() {
                // Each line takes its share in turn, so the lines together
                // draw allowance first and then credits, as one request would.
                for (index, (line, amount)) in lines.iter().zip(&amounts).enumerate() {
                    if line.currency.as_str() != currency {
                        continue;
                    }
                    let shares = state.split(amount.value()).ok_or_else(|| {
                        AccountingStoreError::EndUserQuotaExceeded(state.refusal())
                    })?;
                    state.take(&shares);
                    for share in &shares {
                        sqlx::query(
                            "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor+$1 WHERE id=$2",
                        )
                        .bind(share.amount)
                        .bind(share.bucket)
                        .execute(&mut *tx)
                        .await?;
                    }
                    line_shares[index] = shares;
                }
            }
            for bucket in &buckets {
                sqlx::query(
                    "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor+$1 WHERE id=$2",
                )
                .bind(total)
                .bind(bucket.id)
                .execute(&mut *tx)
                .await?;
            }
            buckets_by_currency.insert(
                currency.to_owned(),
                buckets.into_iter().map(|b| b.id).collect::<Vec<_>>(),
            );
        }
        // Nothing is held: the caller's transaction rolls back what earlier
        // currencies reserved.
        if key_refused {
            return Err(AccountingStoreError::ApiKeyBudgetExceeded);
        }
        let ids: Vec<ReservationId> = lines.iter().map(|_| ReservationId::new()).collect();
        let mut expires = Vec::with_capacity(lines.len());
        for line in lines {
            let ttl =
                chrono::Duration::from_std(line.ttl).map_err(|_| AccountingError::InvalidTtl)?;
            expires.push(
                now.checked_add_signed(ttl)
                    .ok_or(AccountingError::Overflow)?,
            );
        }
        let confidence = |price: &PriceSnapshot| match price.confidence {
            PricingConfidence::Known => "known",
            PricingConfidence::Conservative => "conservative",
        };
        // Postgres takes at most 65,535 parameters a statement.
        const CHUNK: usize = 500;
        let rows: Vec<usize> = (0..lines.len()).collect();
        for chunk in rows.chunks(CHUNK) {
            let mut insert = sqlx::QueryBuilder::<Postgres>::new(
                "INSERT INTO gateway.accounting_reservation (id,organization_id,team_id,service_account_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id,batch_id,end_user_id,created_at) ",
            );
            insert.push_values(chunk, |mut row, &i| {
                let line = &lines[i];
                let provenance = line.catalog_provenance.as_ref();
                row.push_bind(ids[i].0)
                    .push_bind(scope.organization_id.0)
                    .push_bind(scope.team_id.map(|v| v.0))
                    .push_bind(scope.service_account_id.map(|v| v.0))
                    .push_bind(line.idempotency_key.clone())
                    .push_bind(line.fingerprint.to_vec())
                    .push("'reserved'")
                    .push_bind(amounts[i].value())
                    .push_bind(expires[i])
                    .push_bind(line.output_token_cap.map(|cap| i64::from(cap.get())))
                    .push_bind(line.price.input_minor_per_token)
                    .push_bind(line.price.output_minor_per_token)
                    .push_bind(line.price.cached_input_minor_per_token)
                    .push_bind(line.price.reasoning_minor_per_token)
                    .push_bind(line.price.other_minor_per_token)
                    .push_bind(line.price.version.clone())
                    .push_bind(confidence(&line.price))
                    .push_bind(line.currency.as_str().to_owned())
                    .push_bind(line.deployment.clone())
                    .push_bind(line.model.clone())
                    .push_bind(line.trace_id.0)
                    .push_bind(provenance.map(|p| p.release_id))
                    .push_bind(provenance.map(|p| p.route_id))
                    .push_bind(provenance.map(|p| p.deployment_id))
                    .push_bind(provenance.map(|p| p.price_id))
                    .push_bind(provenance.map(|p| p.digest.to_vec()))
                    .push_bind(provenance.map(|p| p.operation.clone()))
                    .push_bind(provenance.map(|p| p.family.clone()))
                    .push_bind(provenance.map(|p| p.price_source.clone()))
                    .push_bind(provenance.map(|p| p.chat_input_count_mode.clone()))
                    .push_bind(provenance.and_then(|p| p.price_override_id))
                    .push_bind(batch_id)
                    .push_bind(end_user_id)
                    .push("clock_timestamp()");
            });
            insert.build().execute(&mut *tx).await?;
            let mut snapshot = sqlx::QueryBuilder::<Postgres>::new(
                "INSERT INTO gateway.accounting_price_snapshot (organization_id,reservation_id,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency) ",
            );
            snapshot.push_values(chunk, |mut row, &i| {
                let line = &lines[i];
                row.push_bind(scope.organization_id.0)
                    .push_bind(ids[i].0)
                    .push_bind(line.price.input_minor_per_token)
                    .push_bind(line.price.output_minor_per_token)
                    .push_bind(line.price.cached_input_minor_per_token)
                    .push_bind(line.price.reasoning_minor_per_token)
                    .push_bind(line.price.other_minor_per_token)
                    .push_bind(line.price.version.clone())
                    .push_bind(confidence(&line.price))
                    .push_bind(line.currency.as_str().to_owned());
            });
            snapshot.build().execute(&mut *tx).await?;
            let mut metadata = Vec::with_capacity(chunk.len());
            let mut input_bounds = Vec::with_capacity(chunk.len());
            for &i in chunk {
                metadata.push(
                    serde_json::to_value(&lines[i].trace_metadata)
                        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                );
                input_bounds.push(
                    i64::try_from(lines[i].input_bound).map_err(|_| AccountingError::Overflow)?,
                );
            }
            let mut trace = sqlx::QueryBuilder::<Postgres>::new(
                "INSERT INTO gateway.inference_trace (id,organization_id,team_id,service_account_id,reservation_id,correlation_id,operation,status,model,metadata,input_bound,output_cap,price_version,pricing_confidence,currency,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id,end_user_id) ",
            );
            trace.push_values(chunk.iter().enumerate(), |mut row, (n, &i)| {
                let line = &lines[i];
                let provenance = line.catalog_provenance.as_ref();
                row.push_bind(line.trace_id.0)
                    .push_bind(scope.organization_id.0)
                    .push_bind(scope.team_id.map(|v| v.0))
                    .push_bind(scope.service_account_id.map(|v| v.0))
                    .push_bind(ids[i].0)
                    .push_bind(line.correlation_id)
                    .push("'inference'")
                    .push("'pending'")
                    .push_bind(line.model.clone())
                    .push_bind(metadata[n].clone())
                    .push_bind(input_bounds[n])
                    .push_bind(line.trace_metadata.output_cap.map(i64::from))
                    .push_bind(line.price.version.clone())
                    .push_bind(confidence(&line.price))
                    .push_bind(line.currency.as_str().to_owned())
                    .push_bind(provenance.map(|p| p.release_id))
                    .push_bind(provenance.map(|p| p.route_id))
                    .push_bind(provenance.map(|p| p.deployment_id))
                    .push_bind(provenance.map(|p| p.price_id))
                    .push_bind(provenance.map(|p| p.digest.to_vec()))
                    .push_bind(provenance.map(|p| p.operation.clone()))
                    .push_bind(provenance.map(|p| p.family.clone()))
                    .push_bind(provenance.map(|p| p.price_source.clone()))
                    .push_bind(provenance.map(|p| p.chat_input_count_mode.clone()))
                    .push_bind(provenance.and_then(|p| p.price_override_id))
                    .push_bind(end_user_id);
            });
            trace.build().execute(&mut *tx).await?;
            let allocations: Vec<(usize, Uuid)> = chunk
                .iter()
                .flat_map(|&i| {
                    buckets_by_currency[lines[i].currency.as_str()]
                        .iter()
                        .map(move |bucket| (i, *bucket))
                })
                .collect();
            for allocation_chunk in allocations.chunks(CHUNK * 2) {
                let mut allocation = sqlx::QueryBuilder::<Postgres>::new(
                    "INSERT INTO gateway.accounting_allocation (id,reservation_id,quota_bucket_id,organization_id,reserved_minor,state) ",
                );
                allocation.push_values(allocation_chunk, |mut row, &(i, bucket)| {
                    row.push_bind(Uuid::new_v4())
                        .push_bind(ids[i].0)
                        .push_bind(bucket)
                        .push_bind(scope.organization_id.0)
                        .push_bind(amounts[i].value())
                        .push("'held'");
                });
                allocation.build().execute(&mut *tx).await?;
            }
            let shares: Vec<(usize, crate::domain::end_user::Share)> = chunk
                .iter()
                .flat_map(|&i| line_shares[i].iter().map(move |share| (i, *share)))
                .collect();
            for share_chunk in shares.chunks(CHUNK * 2) {
                let mut allocation = sqlx::QueryBuilder::<Postgres>::new(
                    "INSERT INTO gateway.accounting_allocation (id,reservation_id,quota_bucket_id,organization_id,reserved_minor,state,draw_rank) ",
                );
                allocation.push_values(share_chunk, |mut row, &(i, share)| {
                    row.push_bind(Uuid::new_v4())
                        .push_bind(ids[i].0)
                        .push_bind(share.bucket)
                        .push_bind(scope.organization_id.0)
                        .push_bind(share.amount)
                        .push("'held'")
                        .push_bind(share.rank);
                });
                allocation.build().execute(&mut *tx).await?;
            }
        }
        Ok(ids)
    }

    pub async fn claim_exclusive(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
        now: DateTime<Utc>,
    ) -> Result<ClaimOutcome, AccountingStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND id=$2 FOR UPDATE"
        ))
        .bind(organization_id.0)
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AccountingStoreError::NotClaimable)?;
        if row.state == "claimed" {
            let reservation = row.domain()?;
            tx.commit().await?;
            return Ok(ClaimOutcome::AlreadyClaimed(reservation));
        }
        if row.state != "reserved" || row.expires_at <= now {
            return Err(AccountingStoreError::NotClaimable);
        }
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "UPDATE gateway.accounting_reservation SET state='claimed',claimed_at=CURRENT_TIMESTAMP WHERE organization_id=$1 AND id=$2 AND state='reserved' AND expires_at > $3 RETURNING {RESERVATION_COLUMNS}"
        ))
        .bind(organization_id.0)
        .bind(id.0)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        let reservation = row.domain()?;
        tx.commit().await?;
        Ok(ClaimOutcome::Won(reservation))
    }

    pub async fn claim(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
        now: DateTime<Utc>,
    ) -> Result<ClaimOutcome, AccountingStoreError> {
        self.claim_exclusive(organization_id, id, now).await
    }

    /// The last attempt of the request reserved under `idempotency_key`:
    /// its own reservation, or the failover attempt that replaced it, and so
    /// on (D-023). A retry of the request replays that attempt.
    pub async fn latest_attempt_by_service_account_idempotency(
        &self,
        organization_id: OrganizationId,
        service_account_id: ServiceAccountId,
        idempotency_key: &str,
    ) -> Result<Option<Reservation>, AccountingStoreError> {
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "WITH RECURSIVE attempt_chain AS (
               SELECT id, attempt FROM gateway.accounting_reservation
               WHERE organization_id=$1 AND service_account_id=$2 AND idempotency_key=$3
               UNION ALL
               SELECT r.id, r.attempt FROM gateway.accounting_reservation r
               JOIN attempt_chain c ON r.organization_id=$1 AND r.failover_of=c.id
             )
             SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation
             WHERE organization_id=$1
               AND id=(SELECT id FROM attempt_chain ORDER BY attempt DESC LIMIT 1)"
        ))
        .bind(organization_id.0)
        .bind(service_account_id.0)
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(ReservationRow::domain).transpose()
    }

    /// Replaces `failed`, a claimed attempt the provider refused before it
    /// can have started any work, with `next` on another deployment of the
    /// route, in one transaction (D-023): `failed` is released, holding and
    /// charging nothing, and its trace closes as `failed-over` with why the
    /// provider refused it; `next` is claimed at once, from what `failed`
    /// held, on the same budget buckets, until the same deadline. Nothing is
    /// admitted again: it is the same request. `None` when `failed` is no
    /// longer claimed, its deadline has passed, it is the last attempt, or
    /// `next` would fit in what `failed` held only with a lower output cap.
    pub(crate) async fn fail_over(
        &self,
        failed: &Reservation,
        error: &ProviderError,
        next: ReservationRequest,
        max_attempts: u8,
        now: DateTime<Utc>,
    ) -> Result<Option<Reservation>, AccountingStoreError> {
        let organization_id = failed.scope.organization_id;
        let mut tx = self.pool.begin().await?;
        let row: Option<(
            String,
            i64,
            DateTime<Utc>,
            i16,
            Uuid,
            Option<Uuid>,
            Option<i64>,
        )> = sqlx::query_as(
            "SELECT state,reserved_minor,expires_at,attempt,trace_id,end_user_id,token_bound
                 FROM gateway.accounting_reservation
                 WHERE organization_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(failed.id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, reserved, expires_at, attempt, trace_id, end_user_id, token_bound)) = row
        else {
            return Ok(None);
        };
        if state != "claimed"
            || trace_id != failed.trace_id.0
            || expires_at <= now.max(Utc::now())
            || attempt >= i16::from(max_attempts)
        {
            return Ok(None);
        }
        // Under a policy's tokens-per-minute limit the next attempt is
        // counted at the failed one's second, as the same request, so it
        // holds no more tokens than the failed one, which leaves the window
        // as it enters it. The organization's row is key-share locked, as
        // admission locks it, so no such policy changes meanwhile.
        let policy_token_room = match token_bound {
            Some(bound) if RateLimitStore::tokens_limited(&mut tx, &failed.scope).await? => {
                Some(u64::try_from(bound).unwrap_or(0))
            }
            _ => None,
        };
        // Under a plan's tokens-per-minute limit the next attempt holds no
        // more tokens than the failed one, nor than the user's window has
        // left now. A reservation from before token bounds were recorded
        // constrains nothing.
        let user_token_room = match (end_user_id, token_bound) {
            (Some(user), Some(bound)) => {
                crate::persistence::end_users::failover_token_room(
                    &mut tx,
                    user,
                    failed.id.0,
                    u64::try_from(bound).unwrap_or(0),
                )
                .await?
            }
            _ => None,
        };
        let token_room = min_room(policy_token_room, user_token_room);
        let (output_token_cap, amount) = match admit(&next, i128::from(reserved), token_room) {
            Ok(admitted) => admitted,
            Err(AccountingStoreError::QuotaExceeded) => return Ok(None),
            Err(error) => return Err(error),
        };
        // The next attempt may answer at most what it asked for, which is
        // the request's own cap, and never less than the failed one could.
        if output_token_cap < failed.output_token_cap {
            return Ok(None);
        }
        let allocations = sqlx::query_as::<_, AllocationRow>(
            "SELECT id,quota_bucket_id,reserved_minor,draw_rank,lease_id FROM gateway.accounting_allocation
             WHERE reservation_id=$1 AND state='held' ORDER BY quota_bucket_id FOR UPDATE",
        )
        .bind(failed.id.0)
        .fetch_all(&mut *tx)
        .await?;
        let leased = active_leases(&mut tx, &allocations).await?;
        // Each budget holds the next attempt's whole amount; the end user's
        // shares hold it in draw order, allowance first, within what each
        // held for the failed attempt (it never needs more). A hold under an
        // active lease stays under it, smaller or the same, so nothing moves
        // on its bucket; what it no longer needs goes back to the lease.
        let holds = allocation_charges(&allocations, amount.value());
        let mut credits = Vec::new();
        for (allocation, hold) in allocations.iter().zip(&holds) {
            if let Some(lease) = allocation.leased(&leased) {
                credits.push((
                    allocation.quota_bucket_id,
                    lease,
                    allocation.reserved_minor - hold,
                ));
            } else {
                if let Some(lease) = allocation.lease_id {
                    // Its lease was returned: the next attempt is held on the
                    // bucket and the old lease is done with this request.
                    credits.push((allocation.quota_bucket_id, lease, 0));
                }
                let result = sqlx::query(
                    "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor-$1+$2
                 WHERE id=$3 AND reserved_minor >= $1",
                )
                .bind(allocation.reserved_minor)
                .bind(hold)
                .bind(allocation.quota_bucket_id)
                .execute(&mut *tx)
                .await?;
                if result.rows_affected() != 1 {
                    return Err(AccountingStoreError::NotSettleable);
                }
            }
            sqlx::query(
                "UPDATE gateway.accounting_allocation SET state='released',released_at=CURRENT_TIMESTAMP
                 WHERE id=$1 AND state='held'",
            )
            .bind(allocation.id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE gateway.accounting_reservation SET state='released'
             WHERE organization_id=$1 AND id=$2 AND state='claimed'",
        )
        .bind(organization_id.0)
        .bind(failed.id.0)
        .execute(&mut *tx)
        .await?;
        finalize_trace(
            &mut tx,
            failed.trace_id,
            "failed-over",
            None,
            error.provider_request_id(),
        )
        .await?;
        record_provider_error(&mut tx, failed.trace_id, Some(error)).await?;
        let id = ReservationId::new();
        insert_reservation(
            &mut tx,
            &next,
            id,
            amount,
            output_token_cap,
            expires_at,
            end_user_id,
            Some(Lineage {
                failover_of: failed.id,
                failed_trace: failed.trace_id,
                attempt: attempt + 1,
            }),
            true,
            None,
        )
        .await?;
        for (allocation, hold) in allocations.iter().zip(&holds) {
            sqlx::query("INSERT INTO gateway.accounting_allocation (id,reservation_id,quota_bucket_id,organization_id,reserved_minor,state,draw_rank,lease_id) VALUES ($1,$2,$3,$4,$5,'held',$6,$7)")
                .bind(Uuid::new_v4())
                .bind(id.0)
                .bind(allocation.quota_bucket_id)
                .bind(organization_id.0)
                .bind(hold)
                .bind(allocation.draw_rank)
                .bind(allocation.leased(&leased))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        // The next attempt keeps its place on each lease: only the change
        // comes back, and a returned lease is done with the request.
        if let Some(leases) = &self.leases {
            for &(bucket, lease, amount) in &credits {
                leases.give_back(bucket, lease, amount, !leased.contains(&lease));
            }
        }
        Ok(Some(Reservation {
            id,
            scope: next.scope,
            idempotency_key: next.idempotency_key,
            fingerprint: next.fingerprint.to_vec(),
            reserved: amount,
            expires_at,
            state: ReservationState::Claimed,
            price: next.price,
            currency: next.currency,
            output_token_cap,
            deployment: next.deployment,
            model: next.model,
            trace_id: next.trace_id,
            catalog_provenance: next.catalog_provenance,
        }))
    }

    /// Releases a claimed reservation whose provider refused the request
    /// before doing any work (a 429 from the last deployment tried, or a
    /// status its adapter documents as a refusal): it used nothing, so its
    /// holds return to every budget, nothing is charged, and its trace closes
    /// as `refused` with why. `settlement.usage` must be `None`. Replays a
    /// committed attempt of the same finalization. A reservation the expiry
    /// worker already made unknown keeps its reconciliation item (`Unknown`).
    pub async fn release_refused(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
        settlement: Settlement,
    ) -> Result<ReservationState, AccountingStoreError> {
        if settlement.usage.is_some() {
            return Err(AccountingStoreError::NotSettleable);
        }
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, Uuid)> = sqlx::query_as(
            "SELECT state,trace_id FROM gateway.accounting_reservation
             WHERE organization_id=$1 AND id=$2 FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, trace_id)) = row else {
            return Err(AccountingStoreError::NotSettleable);
        };
        if trace_id != settlement.trace_id.0 {
            return Err(AccountingStoreError::NotSettleable);
        }
        let finalization_id = settlement.finalization_id;
        if let Some(outcome) = replayed_finalization(&mut tx, finalization_id, id).await? {
            tx.commit().await?;
            return match outcome {
                FinalizationOutcome::Refused => Ok(ReservationState::Released),
                _ => Err(AccountingStoreError::NotSettleable),
            };
        }
        record_timings(
            &mut tx,
            organization_id,
            settlement.trace_id,
            &settlement.timings,
        )
        .await?;
        match state.as_str() {
            "claimed" => {}
            "unknown" => {
                tx.commit().await?;
                return Ok(ReservationState::Unknown);
            }
            _ => return Err(AccountingStoreError::NotSettleable),
        }
        let allocations = sqlx::query_as::<_, AllocationRow>(
            "SELECT id,quota_bucket_id,reserved_minor,draw_rank,lease_id FROM gateway.accounting_allocation
             WHERE reservation_id=$1 AND state='held' ORDER BY quota_bucket_id FOR UPDATE",
        )
        .bind(id.0)
        .fetch_all(&mut *tx)
        .await?;
        let leased = active_leases(&mut tx, &allocations).await?;
        let mut credits = Vec::new();
        for allocation in &allocations {
            if let Some(lease) = allocation.leased(&leased) {
                credits.push((allocation.quota_bucket_id, lease, allocation.reserved_minor));
            } else {
                let result = sqlx::query(
                    "UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor-$1
                     WHERE id=$2 AND reserved_minor >= $1",
                )
                .bind(allocation.reserved_minor)
                .bind(allocation.quota_bucket_id)
                .execute(&mut *tx)
                .await?;
                if result.rows_affected() != 1 {
                    return Err(AccountingStoreError::NotSettleable);
                }
            }
            sqlx::query(
                "UPDATE gateway.accounting_allocation SET state='released',released_at=CURRENT_TIMESTAMP
                 WHERE id=$1 AND state='held'",
            )
            .bind(allocation.id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE gateway.accounting_reservation SET state='released'
             WHERE organization_id=$1 AND id=$2 AND state='claimed'",
        )
        .bind(organization_id.0)
        .bind(id.0)
        .execute(&mut *tx)
        .await?;
        finalize_trace(
            &mut tx,
            settlement.trace_id,
            "refused",
            Some(0),
            settlement.provider_request_id.as_deref(),
        )
        .await?;
        record_provider_error(
            &mut tx,
            settlement.trace_id,
            settlement.provider_error.as_ref(),
        )
        .await?;
        record_finalization(
            &mut tx,
            finalization_id,
            organization_id,
            id,
            FinalizationOutcome::Refused,
        )
        .await?;
        tx.commit().await?;
        self.finished(&credits);
        Ok(ReservationState::Released)
    }

    pub async fn settle(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
        settlement: Settlement,
        _now: DateTime<Utc>,
    ) -> Result<ReservationState, AccountingStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, ReservationRow>(&format!("SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND id=$2 FOR UPDATE")).bind(organization_id.0).bind(id.0).fetch_optional(&mut *tx).await?.ok_or(AccountingStoreError::NotSettleable)?;
        if row.trace_id != settlement.trace_id.0 {
            return Err(AccountingStoreError::NotSettleable);
        }
        let finalization_id = settlement.finalization_id;
        if let Some(outcome) = replayed_finalization(&mut tx, finalization_id, id).await? {
            tx.commit().await?;
            return match outcome {
                FinalizationOutcome::Settled => Ok(ReservationState::Settled),
                FinalizationOutcome::Unknown => Ok(ReservationState::Unknown),
                FinalizationOutcome::BoundBreach => Err(AccountingStoreError::BoundExceeded),
                // Pricing is deterministic, so the retry's usage (the same
                // as the committed attempt's) reproduces the original error.
                FinalizationOutcome::SettlementFailed => Err(AccountingStoreError::Domain(
                    settlement
                        .usage
                        .as_ref()
                        .and_then(|usage| calculate_usage(row.price().ok()?, usage).err())
                        .unwrap_or(AccountingError::InvalidUsage),
                )),
                FinalizationOutcome::InputCountDrift | FinalizationOutcome::Refused => {
                    Err(AccountingStoreError::NotSettleable)
                }
            };
        }
        record_timings(
            &mut tx,
            organization_id,
            settlement.trace_id,
            &settlement.timings,
        )
        .await?;
        if row.state == "settled" {
            if let Some(usage) = settlement.usage.as_ref() {
                if usage.completeness() != UsageCompleteness::Complete
                    || usage.quality() != UsageQuality::Exact
                {
                    return Err(AccountingStoreError::SettlementConflict);
                }
                let fingerprint = usage_fingerprint(usage)?;
                if row.settlement_fingerprint.as_deref() != Some(fingerprint.as_slice()) {
                    return Err(AccountingStoreError::SettlementConflict);
                }
            }
            tx.commit().await?;
            return Ok(ReservationState::Settled);
        }
        if row.state != "claimed" && row.state != "unknown" {
            return Err(AccountingStoreError::NotSettleable);
        }
        let Some(usage) = settlement.usage else {
            sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state='claimed'")
                .bind(organization_id.0)
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            record_reconciliation(&mut tx, organization_id, id, "provider-unknown").await?;
            finalize_trace(
                &mut tx,
                settlement.trace_id,
                "unknown",
                None,
                settlement.provider_request_id.as_deref(),
            )
            .await?;
            record_provider_error(
                &mut tx,
                settlement.trace_id,
                settlement.provider_error.as_ref(),
            )
            .await?;
            record_finalization(
                &mut tx,
                finalization_id,
                organization_id,
                id,
                FinalizationOutcome::Unknown,
            )
            .await?;
            tx.commit().await?;
            return Ok(ReservationState::Unknown);
        };
        if usage.completeness() != UsageCompleteness::Complete
            || usage.quality() != UsageQuality::Exact
        {
            insert_usage(
                &mut tx,
                Uuid::new_v4(),
                id,
                organization_id,
                &usage,
                None,
                &row.currency,
            )
            .await?;
            sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state='claimed'")
                .bind(organization_id.0)
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            record_reconciliation(&mut tx, organization_id, id, "provider-unknown").await?;
            finalize_trace(
                &mut tx,
                settlement.trace_id,
                "unknown",
                None,
                settlement.provider_request_id.as_deref(),
            )
            .await?;
            record_provider_error(
                &mut tx,
                settlement.trace_id,
                settlement.provider_error.as_ref(),
            )
            .await?;
            record_finalization(
                &mut tx,
                finalization_id,
                organization_id,
                id,
                FinalizationOutcome::Unknown,
            )
            .await?;
            tx.commit().await?;
            return Ok(ReservationState::Unknown);
        }
        let output_exceeded = row
            .output_token_cap
            .and_then(|cap| u64::try_from(cap).ok())
            .zip(usage.output_tokens().and_then(|counter| counter.value()))
            .is_some_and(|(cap, observed)| observed > cap);
        if output_exceeded {
            insert_usage(
                &mut tx,
                Uuid::new_v4(),
                id,
                organization_id,
                &usage,
                None,
                &row.currency,
            )
            .await?;
            sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state IN ('claimed','unknown')")
                .bind(organization_id.0)
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            record_reconciliation(&mut tx, organization_id, id, "bound-breach").await?;
            finalize_trace(
                &mut tx,
                settlement.trace_id,
                "bound-breach",
                None,
                settlement.provider_request_id.as_deref(),
            )
            .await?;
            record_finalization(
                &mut tx,
                finalization_id,
                organization_id,
                id,
                FinalizationOutcome::BoundBreach,
            )
            .await?;
            tx.commit().await?;
            return Err(AccountingStoreError::BoundExceeded);
        }
        let calculation = match calculate_usage(row.price()?, &usage) {
            Ok(calculation) => calculation,
            Err(error) => {
                insert_usage(
                    &mut tx,
                    Uuid::new_v4(),
                    id,
                    organization_id,
                    &usage,
                    None,
                    &row.currency,
                )
                .await?;
                sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state IN ('claimed','unknown')")
                    .bind(organization_id.0)
                    .bind(id.0)
                    .execute(&mut *tx)
                    .await?;
                record_reconciliation(&mut tx, organization_id, id, "settlement-failed").await?;
                finalize_trace(
                    &mut tx,
                    settlement.trace_id,
                    "unknown",
                    None,
                    settlement.provider_request_id.as_deref(),
                )
                .await?;
                record_finalization(
                    &mut tx,
                    finalization_id,
                    organization_id,
                    id,
                    FinalizationOutcome::SettlementFailed,
                )
                .await?;
                tx.commit().await?;
                return Err(AccountingStoreError::Domain(error));
            }
        };
        let settlement_fingerprint = usage_fingerprint(&usage)?;
        if calculation.amount.value() > row.reserved_minor {
            insert_usage(
                &mut tx,
                Uuid::new_v4(),
                id,
                organization_id,
                &usage,
                Some(&calculation),
                &row.currency,
            )
            .await?;
            sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state IN ('claimed','unknown')")
                .bind(organization_id.0)
                .bind(id.0)
                .execute(&mut *tx)
                .await?;
            record_reconciliation(&mut tx, organization_id, id, "bound-breach").await?;
            finalize_trace(
                &mut tx,
                settlement.trace_id,
                "bound-breach",
                Some(calculation.amount.value()),
                settlement.provider_request_id.as_deref(),
            )
            .await?;
            record_finalization(
                &mut tx,
                finalization_id,
                organization_id,
                id,
                FinalizationOutcome::BoundBreach,
            )
            .await?;
            tx.commit().await?;
            return Err(AccountingStoreError::BoundExceeded);
        }
        let usage_id = Uuid::new_v4();
        insert_usage(
            &mut tx,
            usage_id,
            id,
            organization_id,
            &usage,
            Some(&calculation),
            &row.currency,
        )
        .await?;
        let allocations = sqlx::query_as::<_, AllocationRow>("SELECT id,quota_bucket_id,reserved_minor,draw_rank,lease_id FROM gateway.accounting_allocation WHERE reservation_id=$1 AND state='held' ORDER BY quota_bucket_id FOR UPDATE").bind(id.0).fetch_all(&mut *tx).await?;
        if allocations.is_empty() {
            return Err(AccountingStoreError::NotSettleable);
        }
        let charges = allocation_charges(&allocations, calculation.amount.value());
        let leased = active_leases(&mut tx, &allocations).await?;
        let mut credits = Vec::new();
        for (allocation, charge) in allocations.into_iter().zip(charges) {
            // Under an active lease the charge stays inside the lease until
            // it returns; the change comes back to it.
            if let Some(lease) = allocation.leased(&leased) {
                credits.push((
                    allocation.quota_bucket_id,
                    lease,
                    allocation.reserved_minor - charge,
                ));
            } else {
                let result = sqlx::query("UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor-$1,consumed_minor=consumed_minor+$2 WHERE id=$3 AND reserved_minor >= $1")
                    .bind(allocation.reserved_minor)
                    .bind(charge)
                    .bind(allocation.quota_bucket_id)
                    .execute(&mut *tx)
                    .await?;
                if result.rows_affected() != 1 {
                    return Err(AccountingStoreError::NotSettleable);
                }
            }
            let result = sqlx::query("UPDATE gateway.accounting_allocation SET state='settled',settled_minor=$1,usage_id=$2 WHERE id=$3 AND state='held'")
                .bind(charge)
                .bind(usage_id)
                .bind(allocation.id)
                .execute(&mut *tx)
                .await?;
            if result.rows_affected() != 1 {
                return Err(AccountingStoreError::NotSettleable);
            }
        }
        sqlx::query("UPDATE gateway.accounting_reservation SET state='settled',settled_at=CURRENT_TIMESTAMP,settlement_fingerprint=$3 WHERE organization_id=$1 AND id=$2 AND state IN ('claimed','unknown') AND settlement_fingerprint IS NULL")
            .bind(organization_id.0)
            .bind(id.0)
            .bind(settlement_fingerprint.as_slice())
            .execute(&mut *tx)
            .await?;
        resolve_reconciliation(&mut tx, id).await?;
        finalize_trace(
            &mut tx,
            settlement.trace_id,
            "settled",
            Some(calculation.amount.value()),
            settlement.provider_request_id.as_deref(),
        )
        .await?;
        // A request the provider failed but still reported usage for (a
        // stream cut short, a batch line refused at zero) settles with why.
        record_provider_error(
            &mut tx,
            settlement.trace_id,
            settlement.provider_error.as_ref(),
        )
        .await?;
        record_finalization(
            &mut tx,
            finalization_id,
            organization_id,
            id,
            FinalizationOutcome::Settled,
        )
        .await?;
        tx.commit().await?;
        self.finished(&credits);
        Ok(ReservationState::Settled)
    }

    /// Preserve provider usage and leave the reservation unresolved when the
    /// provider's exact input count differs from the count used for admission.
    /// The reconciliation row is intentionally durable so an operator can
    /// resolve the billing difference without silently settling it.
    pub async fn reconcile_input_drift(
        &self,
        organization_id: OrganizationId,
        id: ReservationId,
        finalization_id: Uuid,
        trace_id: TraceId,
        usage: Option<Usage>,
        provider_request_id: Option<&str>,
        provider_error: Option<&ProviderError>,
    ) -> Result<(), AccountingStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, ReservationRow>(&format!(
            "SELECT {RESERVATION_COLUMNS} FROM gateway.accounting_reservation WHERE organization_id=$1 AND id=$2 FOR UPDATE"
        ))
        .bind(organization_id.0)
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AccountingStoreError::NotSettleable)?;
        if row.trace_id != trace_id.0 {
            return Err(AccountingStoreError::NotSettleable);
        }
        if let Some(outcome) = replayed_finalization(&mut tx, finalization_id, id).await? {
            tx.commit().await?;
            return match outcome {
                FinalizationOutcome::InputCountDrift => Ok(()),
                _ => Err(AccountingStoreError::NotSettleable),
            };
        }
        if row.state == "settled" {
            return Err(AccountingStoreError::SettlementConflict);
        }
        if row.state != "claimed" && row.state != "unknown" {
            return Err(AccountingStoreError::NotSettleable);
        }
        if let Some(usage) = usage.as_ref() {
            insert_usage(
                &mut tx,
                Uuid::new_v4(),
                id,
                organization_id,
                usage,
                None,
                &row.currency,
            )
            .await?;
        }
        sqlx::query(
            "UPDATE gateway.accounting_reservation SET state='unknown' WHERE organization_id=$1 AND id=$2 AND state IN ('claimed','unknown')",
        )
        .bind(organization_id.0)
        .bind(id.0)
        .execute(&mut *tx)
        .await?;
        record_reconciliation(&mut tx, organization_id, id, "input-count-drift").await?;
        finalize_trace(
            &mut tx,
            trace_id,
            "input-count-drift",
            None,
            provider_request_id,
        )
        .await?;
        record_provider_error(&mut tx, trace_id, provider_error).await?;
        record_finalization(
            &mut tx,
            finalization_id,
            organization_id,
            id,
            FinalizationOutcome::InputCountDrift,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Bounded recovery: pre-claim reservations release quota; post-claim
    /// reservations become unknown and retain their holds for reconciliation.
    pub async fn recover_expired(
        &self,
        now: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, AccountingStoreError> {
        let max_rows = max_rows.clamp(1, 1_000);
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, RecoveryRow>("SELECT id,organization_id AS _organization_id,state,trace_id FROM gateway.accounting_reservation WHERE state IN ('reserved','claimed') AND expires_at <= $1 ORDER BY expires_at,id LIMIT $2 FOR UPDATE SKIP LOCKED").bind(now).bind(max_rows).fetch_all(&mut *tx).await?;
        // Every allocation first, then every lease lock, then the buckets:
        // lease locks come before bucket rows on every path.
        let mut held = Vec::with_capacity(rows.len());
        for row in &rows {
            let allocations = if row.state == "reserved" {
                sqlx::query_as::<_, AllocationRow>("SELECT id,quota_bucket_id,reserved_minor,lease_id FROM gateway.accounting_allocation WHERE reservation_id=$1 AND state='held' FOR UPDATE").bind(row.id).fetch_all(&mut *tx).await?
            } else {
                Vec::new()
            };
            held.push(allocations);
        }
        let leases: Vec<Uuid> = held.iter().flatten().filter_map(|a| a.lease_id).collect();
        let leased = crate::persistence::leases::lock_active(&mut tx, &leases).await?;
        let mut count = 0;
        for (row, allocations) in rows.into_iter().zip(held) {
            if row.state == "reserved" {
                for a in allocations {
                    // Under an active lease the hold just ends: the lease's
                    // return gives it back to the bucket.
                    if a.leased(&leased).is_none() {
                        let result = sqlx::query("UPDATE gateway.quota_bucket SET reserved_minor=reserved_minor-$1 WHERE id=$2 AND reserved_minor >= $1")
                            .bind(a.reserved_minor)
                            .bind(a.quota_bucket_id)
                            .execute(&mut *tx)
                            .await?;
                        if result.rows_affected() != 1 {
                            return Err(AccountingStoreError::NotSettleable);
                        }
                    }
                    let result = sqlx::query("UPDATE gateway.accounting_allocation SET state='released',released_at=CURRENT_TIMESTAMP WHERE id=$1 AND state='held'")
                        .bind(a.id)
                        .execute(&mut *tx)
                        .await?;
                    if result.rows_affected() != 1 {
                        return Err(AccountingStoreError::NotSettleable);
                    }
                }
                sqlx::query("UPDATE gateway.accounting_reservation SET state='expired' WHERE id=$1 AND state='reserved'").bind(row.id).execute(&mut *tx).await?;
                finalize_trace(&mut tx, TraceId(row.trace_id), "expired", None, None).await?;
            } else {
                sqlx::query("UPDATE gateway.accounting_reservation SET state='unknown' WHERE id=$1 AND state='claimed'").bind(row.id).execute(&mut *tx).await?;
                record_reconciliation(
                    &mut tx,
                    OrganizationId(row._organization_id),
                    ReservationId(row.id),
                    RECOVERY_REASON,
                )
                .await?;
                finalize_trace(&mut tx, TraceId(row.trace_id), "unknown", None, None).await?;
            }
            count += 1;
        }
        tx.commit().await?;
        Ok(count)
    }

    /// Deletes up to `max_rows` finalization records created before `cutoff`.
    /// Retries finish long before a record is old enough to be swept.
    pub async fn sweep_finalizations(
        &self,
        cutoff: DateTime<Utc>,
        max_rows: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM gateway.accounting_finalization
             WHERE id IN (
                 SELECT id FROM gateway.accounting_finalization
                 WHERE created_at < $1 ORDER BY created_at LIMIT $2
             )",
        )
        .bind(cutoff)
        .bind(max_rows.max(1))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

impl AccountingStore {
    /// Settle an open reconciliation item's unknown reservation at the amount
    /// an operator determined, which may exceed the reservation. Held quota
    /// is released and `amount_minor` is consumed in every bucket the
    /// reservation held, as a provider settlement would. The charge is
    /// recorded as a final estimated usage row that keeps the latest observed
    /// token counts, so reports show the charged amount without losing them,
    /// and the trace is finalized with the charged amount.
    ///
    /// The caller holds the organization row lock, which reserve and settle
    /// need (as a foreign-key share lock) while holding reservation and
    /// bucket locks. Waiting on those rows here could deadlock, so they are
    /// taken with NOWAIT and contention is reported as `Busy`. The
    /// reservation is locked before the item, as settlement does.
    pub async fn resolve_by_operator(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        item_id: Uuid,
        amount_minor: i64,
        actor: UserAccountId,
    ) -> Result<OperatorResolution, AccountingStoreError> {
        if amount_minor < 0 {
            return Err(AccountingStoreError::Domain(AccountingError::InvalidUsage));
        }
        // An item never changes reservation, so this read needs no lock.
        let Some(reservation_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT reservation_id FROM gateway.accounting_reconciliation
             WHERE organization_id = $1 AND id = $2",
        )
        .bind(organization_id.0)
        .bind(item_id)
        .fetch_optional(&mut *conn)
        .await?
        else {
            return Ok(OperatorResolution::NotFound);
        };
        let reservation = sqlx::query_as::<_, (String, String)>(
            "SELECT state, currency FROM gateway.accounting_reservation
             WHERE organization_id = $1 AND id = $2 FOR UPDATE NOWAIT",
        )
        .bind(organization_id.0)
        .bind(reservation_id)
        .fetch_one(&mut *conn)
        .await;
        let (state, currency) = match reservation {
            Ok(row) => row,
            Err(error) if lock_unavailable(&error) => return Ok(OperatorResolution::Busy),
            Err(error) => return Err(error.into()),
        };
        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM gateway.accounting_reconciliation
             WHERE organization_id = $1 AND id = $2 FOR UPDATE",
        )
        .bind(organization_id.0)
        .bind(item_id)
        .fetch_one(&mut *conn)
        .await?;
        if status != "open" || state != "unknown" {
            return Ok(OperatorResolution::NotOpen);
        }
        let usage_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO gateway.accounting_usage
                 (id, reservation_id, organization_id, currency, confidence, input_tokens,
                  output_tokens, cached_input_tokens, reasoning_tokens, other_tokens,
                  amount_minor, completeness, finality)
             SELECT $1, $2, $3, $4, 'estimated', l.input_tokens, l.output_tokens,
                    l.cached_input_tokens, l.reasoning_tokens, l.other_tokens, $5,
                    COALESCE(l.completeness, 'unavailable'), 'final'
             FROM (SELECT 1) AS one
             LEFT JOIN LATERAL (
                 SELECT * FROM gateway.accounting_usage u
                 WHERE u.organization_id = $3 AND u.reservation_id = $2
                 ORDER BY u.recorded_at DESC, u.id DESC
                 LIMIT 1
             ) AS l ON true",
        )
        .bind(usage_id)
        .bind(reservation_id)
        .bind(organization_id.0)
        .bind(&currency)
        .bind(amount_minor)
        .execute(&mut *conn)
        .await?;
        let allocations = sqlx::query_as::<_, AllocationRow>(
            "SELECT id, quota_bucket_id, reserved_minor, draw_rank, lease_id FROM gateway.accounting_allocation
             WHERE reservation_id = $1 AND state = 'held' ORDER BY quota_bucket_id FOR UPDATE",
        )
        .bind(reservation_id)
        .fetch_all(&mut *conn)
        .await?;
        // An operator's charge may pass what was held, which a lease's
        // amount cannot take: a lease the reservation was held under is
        // returned first (its hold moves to the bucket), unless a request is
        // using it right now. Its lock first, as on every path, then the
        // buckets, without waiting.
        let leases: Vec<Uuid> = allocations.iter().filter_map(|a| a.lease_id).collect();
        for &lease in &leases {
            if !crate::persistence::leases::try_lock(conn, lease).await? {
                return Ok(OperatorResolution::Busy);
            }
        }
        let buckets: Vec<Uuid> = allocations
            .iter()
            .map(|allocation| allocation.quota_bucket_id)
            .collect();
        let locked = sqlx::query(
            "SELECT id FROM gateway.quota_bucket WHERE id = ANY($1) ORDER BY id FOR UPDATE NOWAIT",
        )
        .bind(&buckets)
        .execute(&mut *conn)
        .await;
        match locked {
            Ok(_) => {}
            Err(error) if lock_unavailable(&error) => return Ok(OperatorResolution::Busy),
            Err(error) => return Err(error.into()),
        }
        for lease in leases {
            let reason = crate::persistence::leases::ReturnReason::Operator;
            if let Some(returned) =
                crate::persistence::leases::return_lease(conn, lease, reason).await?
            {
                crate::persistence::leases::record_return(reason, returned);
            }
        }
        let charges = allocation_charges(&allocations, amount_minor);
        for (allocation, charge) in allocations.into_iter().zip(charges) {
            let result = sqlx::query(
                "UPDATE gateway.quota_bucket
                 SET reserved_minor = reserved_minor - $1, consumed_minor = consumed_minor + $2
                 WHERE id = $3 AND reserved_minor >= $1",
            )
            .bind(allocation.reserved_minor)
            .bind(charge)
            .bind(allocation.quota_bucket_id)
            .execute(&mut *conn)
            .await?;
            if result.rows_affected() != 1 {
                return Err(AccountingStoreError::NotSettleable);
            }
            sqlx::query(
                "UPDATE gateway.accounting_allocation
                 SET state = 'settled', settled_minor = $1, usage_id = $2
                 WHERE id = $3 AND state = 'held'",
            )
            .bind(charge)
            .bind(usage_id)
            .bind(allocation.id)
            .execute(&mut *conn)
            .await?;
        }
        sqlx::query(
            "UPDATE gateway.accounting_reservation
             SET state = 'settled', settled_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND id = $2 AND state = 'unknown'",
        )
        .bind(organization_id.0)
        .bind(reservation_id)
        .execute(&mut *conn)
        .await?;
        let traces: Vec<FinalizedTraceRow> = sqlx::query_as(&format!(
            "UPDATE gateway.inference_trace
             SET status = 'finalized', outcome = 'operator-resolved', amount_minor = $3,
                 finalized_at = CURRENT_TIMESTAMP
             WHERE organization_id = $1 AND reservation_id = $2
             RETURNING {FINALIZED_TRACE_COLUMNS}"
        ))
        .bind(organization_id.0)
        .bind(reservation_id)
        .bind(amount_minor)
        .fetch_all(&mut *conn)
        .await?;
        for trace in traces {
            emit_trace_finalized(conn, trace).await?;
        }
        let resolved: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE gateway.accounting_reconciliation
             SET status = 'resolved', resolved_at = CURRENT_TIMESTAMP, resolution = 'operator',
                 resolved_amount_minor = $3, resolved_by = $4
             WHERE organization_id = $1 AND id = $2
             RETURNING reservation_id, resolved_at",
        )
        .bind(organization_id.0)
        .bind(item_id)
        .bind(amount_minor)
        .bind(actor.0)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some((reservation_id, resolved_at)) = resolved {
            outbox::emit(
                conn,
                organization_id,
                &OutboxPayload::ReconciliationResolved(ReconciliationResolved {
                    item_id,
                    reservation_id,
                    resolution: "operator".to_owned(),
                    resolved_amount_minor: Some(amount_minor),
                    resolved_by: Some(actor),
                    resolved_at,
                }),
            )
            .await?;
        }
        Ok(OperatorResolution::Resolved)
    }
}

#[derive(FromRow)]
struct BucketRow {
    id: Uuid,
    scope_kind: String,
    limit_minor: i64,
    reserved_minor: i64,
    consumed_minor: i64,
}

impl BucketRow {
    fn remaining(&self) -> Result<i128, AccountingError> {
        (self.limit_minor as i128)
            .checked_sub(self.consumed_minor as i128)
            .and_then(|value| value.checked_sub(self.reserved_minor as i128))
            .ok_or(AccountingError::Overflow)
    }

    fn is_api_key(&self) -> bool {
        self.scope_kind == "api-key"
    }
}

/// The input tokens a request holds: its own bound, or the larger one of
/// the deployments it may fail over to.
fn held_input_bound(request: &ReservationRequest) -> u64 {
    request
        .hold_input_bound
        .map_or(request.input_bound, |bound| bound.max(request.input_bound))
}

/// The reasoning tokens held beyond the output cap.
fn held_reasoning(request: &ReservationRequest) -> u64 {
    if request.reserve_output {
        u64::from(request.reasoning_allowance)
    } else {
        0
    }
}

/// The smaller of two token rooms, either of which may be unlimited.
fn min_room(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The fewest tokens a request can be served with, against a
/// tokens-per-minute limit: its input and reasoning, and one output token.
fn min_tokens(request: &ReservationRequest) -> u64 {
    held_input_bound(request)
        .saturating_add(held_reasoning(request))
        .saturating_add(u64::from(request.reserve_output))
}

/// The tokens a reservation counts against its end user's tokens per minute
/// while in flight: what it holds, input, output cap and reasoning.
fn token_bound(request: &ReservationRequest, output_token_cap: Option<NonZeroU32>) -> i64 {
    let output = if request.reserve_output {
        output_token_cap.map_or(0, |cap| u64::from(cap.get()))
    } else {
        0
    };
    i64::try_from(min_tokens(request) - u64::from(request.reserve_output) + output)
        .unwrap_or(i64::MAX)
}

/// The output cap and amount a request is reserved at, given what its
/// budgets have left (`available`) and, under an end user's tokens per
/// minute, the tokens left for it (`token_room`), each of which lowers the
/// cap. Refused with `QuotaExceeded` when they cannot cover the input, the
/// reasoning allowance and at least one output token.
fn admit(
    request: &ReservationRequest,
    available: i128,
    token_room: Option<u64>,
) -> Result<(Option<NonZeroU32>, BudgetAmount), AccountingStoreError> {
    let price = request.hold_price.as_ref().unwrap_or(&request.price);
    let input_bound = held_input_bound(request);
    // What the token room leaves for output, after input and reasoning;
    // a request whose input alone does not fit is refused.
    let token_output = match token_room {
        Some(room) => Some(
            room.checked_sub(input_bound)
                .and_then(|room| room.checked_sub(held_reasoning(request)))
                .ok_or(AccountingStoreError::QuotaExceeded)?,
        ),
        None => None,
    };
    let input_amount = price.reserve_tokens(input_bound, 0)?;
    // Reasoning the output cap does not bound is reserved first, in full:
    // the cap can be cut to what the budget affords, the allowance cannot.
    let allowance_amount = if request.reserve_output {
        i128::from(request.reasoning_allowance)
            .checked_mul(i128::from(price.reasoning_rate()))
            .ok_or(AccountingError::Overflow)?
    } else {
        0
    };
    let available_output = available
        .checked_sub(input_amount.value() as i128)
        .and_then(|value| value.checked_sub(allowance_amount))
        .ok_or(AccountingStoreError::QuotaExceeded)?;
    // A negative bucket balance must reject every operation, including
    // embeddings, which do not reserve output and can have a zero output
    // price.
    if available_output < 0 {
        return Err(AccountingStoreError::QuotaExceeded);
    }
    let reserve_output_rate = i128::from(price.output_minor_per_token)
        .checked_add(i128::from(price.reasoning_rate()))
        .and_then(|value| value.checked_add(i128::from(price.other_minor_per_token)))
        .ok_or(AccountingError::Overflow)?;
    let affordable_output = if reserve_output_rate == 0 {
        request
            .output_token_cap
            .map_or(0, |cap| u64::from(cap.get()))
    } else {
        u64::try_from(available_output / reserve_output_rate).unwrap_or(0)
    };
    let output_cap = if request.reserve_output {
        let requested = request
            .output_token_cap
            .ok_or(AccountingStoreError::QuotaExceeded)?;
        Some(
            requested
                .get()
                .min(u32::try_from(affordable_output).unwrap_or(u32::MAX))
                .min(token_output.map_or(u32::MAX, |t| u32::try_from(t).unwrap_or(u32::MAX))),
        )
    } else {
        None
    };
    let output_token_cap = output_cap
        .map(|cap| NonZeroU32::new(cap).ok_or(AccountingStoreError::QuotaExceeded))
        .transpose()?;
    let amount = price.reserve_tokens(
        input_bound,
        output_token_cap.map_or(0, |cap| u64::from(cap.get())),
    )?;
    let amount = i64::try_from(i128::from(amount.value()) + allowance_amount)
        .map_err(|_| AccountingError::Overflow)
        .and_then(BudgetAmount::new)?;
    Ok((output_token_cap, amount))
}

/// The least any of `buckets` has left, and the least those other than API
/// key buckets have left (`i128::MAX` when there are none).
fn available(buckets: &[BucketRow]) -> Result<(i128, i128), AccountingError> {
    buckets
        .iter()
        .try_fold((i128::MAX, i128::MAX), |(all, others), bucket| {
            let remaining = bucket.remaining()?;
            Ok((
                all.min(remaining),
                if bucket.is_api_key() {
                    others
                } else {
                    others.min(remaining)
                },
            ))
        })
}
#[derive(FromRow)]
struct AllocationRow {
    id: Uuid,
    quota_bucket_id: Uuid,
    reserved_minor: i64,
    /// A share of an end user's allowance (0) or credits (1..); `None` for a
    /// budget, which is charged the whole amount.
    #[sqlx(default)]
    draw_rank: Option<i16>,
    /// The budget lease it was made under (D-030). Every query selects it:
    /// finalizing a leased allocation as if it were not would charge its
    /// bucket twice.
    lease_id: Option<Uuid>,
}

impl AllocationRow {
    /// The lease, among the `active` ones, its hold and charge stay inside.
    fn leased(&self, active: &HashSet<Uuid>) -> Option<Uuid> {
        self.lease_id.filter(|lease| active.contains(lease))
    }
}

/// What each allocation of one reservation is charged when it settles at
/// `charge`: a budget the whole amount; an end user's shares split in draw
/// order, allowance first (`domain::end_user::settle_shares`).
fn allocation_charges(allocations: &[AllocationRow], charge: i64) -> Vec<i64> {
    let mut ranked: Vec<(usize, i16, i64)> = allocations
        .iter()
        .enumerate()
        .filter_map(|(index, a)| a.draw_rank.map(|rank| (index, rank, a.reserved_minor)))
        .collect();
    ranked.sort_by_key(|&(_, rank, _)| rank);
    let shares = crate::domain::end_user::settle_shares(
        charge,
        &ranked
            .iter()
            .map(|&(_, _, reserved)| reserved)
            .collect::<Vec<_>>(),
    );
    let mut charges = vec![charge; allocations.len()];
    for ((index, _, _), share) in ranked.iter().zip(shares) {
        charges[*index] = share;
    }
    charges
}

/// A bucket as a reservation locks it: a budget's, or one of the end
/// user's.
#[derive(FromRow)]
struct LockedBucketRow {
    id: Uuid,
    scope_kind: String,
    limit_minor: i64,
    reserved_minor: i64,
    consumed_minor: i64,
    end_user_id: Option<Uuid>,
}

impl LockedBucketRow {
    fn budget(self) -> BucketRow {
        BucketRow {
            id: self.id,
            scope_kind: self.scope_kind,
            limit_minor: self.limit_minor,
            reserved_minor: self.reserved_minor,
            consumed_minor: self.consumed_minor,
        }
    }

    fn remaining(&self) -> i128 {
        i128::from(self.limit_minor)
            - i128::from(self.consumed_minor)
            - i128::from(self.reserved_minor)
    }
}

/// An end user's locked allowance and credits, in draw order.
struct EndUserDrawState {
    external_id: String,
    currency: String,
    allowance: crate::domain::end_user::Remaining,
    allowance_limit: i64,
    limit_changed: bool,
    credits: Vec<crate::domain::end_user::Remaining>,
}

impl EndUserDrawState {
    fn new(
        admission: &crate::persistence::end_users::EndUserAdmission,
        draw: &crate::persistence::end_users::EndUserDraw,
        locked: &[LockedBucketRow],
    ) -> Result<Self, AccountingStoreError> {
        let remaining = |id: Uuid| {
            locked.iter().find(|bucket| bucket.id == id).map(|bucket| {
                crate::domain::end_user::Remaining {
                    bucket: id,
                    remaining: bucket.remaining(),
                }
            })
        };
        let locked_allowance = locked
            .iter()
            .find(|bucket| bucket.id == draw.allowance)
            .ok_or(AccountingStoreError::NotSettleable)?;
        // The plan in force sets the month's allowance; the bucket is
        // brought in line by `sync_allowance` before anything is reserved.
        let allowance = crate::domain::end_user::Remaining {
            bucket: draw.allowance,
            remaining: i128::from(draw.allowance_limit)
                - i128::from(locked_allowance.consumed_minor)
                - i128::from(locked_allowance.reserved_minor),
        };
        let credits = draw
            .credits
            .iter()
            .filter_map(|id| remaining(*id))
            .collect();
        Ok(Self {
            external_id: admission.external_id.clone(),
            currency: draw.currency.clone(),
            allowance,
            allowance_limit: draw.allowance_limit,
            limit_changed: locked_allowance.limit_minor != draw.allowance_limit,
            credits,
        })
    }

    /// Write the plan's allowance to the month's bucket, which this
    /// transaction has locked, if the plan or its allowance changed.
    async fn sync_allowance(&self, conn: &mut PgConnection) -> Result<(), sqlx::Error> {
        if self.limit_changed {
            sqlx::query("UPDATE gateway.quota_bucket SET limit_minor = $1 WHERE id = $2")
                .bind(self.allowance_limit)
                .bind(self.allowance.bucket)
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }

    fn available(&self) -> i128 {
        crate::domain::end_user::available(&self.allowance, &self.credits)
    }

    /// Count `shares` as reserved, for the next line of a batch.
    fn take(&mut self, shares: &[crate::domain::end_user::Share]) {
        for share in shares {
            let amount = i128::from(share.amount);
            if share.bucket == self.allowance.bucket {
                self.allowance.remaining -= amount;
            } else if let Some(credit) = self.credits.iter_mut().find(|c| c.bucket == share.bucket)
            {
                credit.remaining -= amount;
            }
        }
    }

    fn split(&self, amount: i64) -> Option<Vec<crate::domain::end_user::Share>> {
        crate::domain::end_user::split_reservation(amount, &self.allowance, &self.credits)
    }

    fn refusal(&self) -> crate::domain::end_user::EndUserRefusal {
        let clamp = |value: i128| i64::try_from(value.max(0)).unwrap_or(i64::MAX);
        crate::domain::end_user::EndUserRefusal {
            end_user: self.external_id.clone(),
            allowance_remaining: clamp(self.allowance.remaining),
            credit_balance: clamp(self.credits.iter().map(|c| c.remaining.max(0)).sum()),
            currency: self.currency.clone(),
        }
    }
}
#[derive(FromRow)]
struct RecoveryRow {
    id: Uuid,
    _organization_id: Uuid,
    state: String,
    trace_id: Uuid,
}

async fn insert_usage(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    reservation_id: ReservationId,
    organization_id: OrganizationId,
    usage: &Usage,
    calculation: Option<&UsageCalculation>,
    currency: &str,
) -> Result<(), AccountingStoreError> {
    let c = calculation
        .map(|value| value.counters)
        .unwrap_or_else(|| crate::domain::accounting::UsageCounters::from_usage(usage));
    let confidence = match calculation
        .map(|value| value.confidence)
        .unwrap_or_else(|| crate::domain::accounting::UsageCounters::confidence(usage))
    {
        UsageConfidence::Exact => "exact",
        UsageConfidence::Estimated => "estimated",
        UsageConfidence::Unknown => "unknown",
    };
    let completeness = match usage.completeness() {
        UsageCompleteness::Complete => "complete",
        UsageCompleteness::Partial => "partial",
        UsageCompleteness::Unavailable => "unavailable",
    };
    let finality = match usage.completeness() {
        UsageCompleteness::Complete => "final",
        UsageCompleteness::Partial => "partial",
        UsageCompleteness::Unavailable => "interim",
    };
    let i = |v: Option<u64>| {
        v.map(i64::try_from)
            .transpose()
            .map_err(|_| AccountingError::Overflow)
    };
    sqlx::query("INSERT INTO gateway.accounting_usage (id,reservation_id,organization_id,currency,confidence,input_tokens,output_tokens,cached_input_tokens,reasoning_tokens,other_tokens,amount_minor,completeness,finality) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)").bind(id).bind(reservation_id.0).bind(organization_id.0).bind(currency).bind(confidence).bind(i(c.input)?).bind(i(c.output)?).bind(i(c.cached_input)?).bind(i(c.reasoning)?).bind(i(c.other)?).bind(calculation.map(|value| value.amount.value())).bind(completeness).bind(finality).execute(&mut **tx).await?;
    Ok(())
}

fn usage_fingerprint(usage: &Usage) -> Result<Vec<u8>, AccountingStoreError> {
    let bytes = serde_json::to_vec(usage)
        .map_err(|_| AccountingStoreError::Domain(AccountingError::InvalidUsage))?;
    Ok(Sha256::digest(bytes).to_vec())
}

/// What a finalization committed, recorded under its id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalizationOutcome {
    Settled,
    Unknown,
    BoundBreach,
    SettlementFailed,
    InputCountDrift,
    /// Released, charging nothing: the provider refused before any work.
    Refused,
}

impl FinalizationOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Unknown => "unknown",
            Self::BoundBreach => "bound-breach",
            Self::SettlementFailed => "settlement-failed",
            Self::InputCountDrift => "input-count-drift",
            Self::Refused => "refused",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            Self::Settled,
            Self::Unknown,
            Self::BoundBreach,
            Self::SettlementFailed,
            Self::InputCountDrift,
            Self::Refused,
        ]
        .into_iter()
        .find(|outcome| outcome.as_str() == value)
    }
}

/// The outcome of an earlier attempt of this finalization that committed.
/// The caller holds the reservation lock, so an earlier attempt has either
/// committed or rolled back.
async fn replayed_finalization(
    tx: &mut Transaction<'_, Postgres>,
    finalization_id: Uuid,
    reservation_id: ReservationId,
) -> Result<Option<FinalizationOutcome>, AccountingStoreError> {
    let outcome: Option<String> = sqlx::query_scalar(
        "SELECT outcome FROM gateway.accounting_finalization WHERE id=$1 AND reservation_id=$2",
    )
    .bind(finalization_id)
    .bind(reservation_id.0)
    .fetch_optional(&mut **tx)
    .await?;
    outcome
        .map(|outcome| {
            FinalizationOutcome::parse(&outcome).ok_or(AccountingStoreError::NotSettleable)
        })
        .transpose()
}

async fn record_finalization(
    tx: &mut Transaction<'_, Postgres>,
    finalization_id: Uuid,
    organization_id: OrganizationId,
    reservation_id: ReservationId,
    outcome: FinalizationOutcome,
) -> Result<(), AccountingStoreError> {
    sqlx::query(
        "INSERT INTO gateway.accounting_finalization (id,organization_id,reservation_id,outcome) VALUES ($1,$2,$3,$4)",
    )
    .bind(finalization_id)
    .bind(organization_id.0)
    .bind(reservation_id.0)
    .bind(outcome.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn record_reconciliation(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    reservation_id: ReservationId,
    reason: &str,
) -> Result<(), AccountingStoreError> {
    let item_id = Uuid::new_v4();
    // An existing item for the reservation keeps its reason, except that an
    // open one opened by expiry recovery takes the reason of the
    // finalization that arrived after it.
    let created_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "INSERT INTO gateway.accounting_reconciliation (id,organization_id,reservation_id,reason) VALUES ($1,$2,$3,$4) ON CONFLICT (reservation_id) DO NOTHING RETURNING created_at",
    )
    .bind(item_id)
    .bind(organization_id.0)
    .bind(reservation_id.0)
    .bind(reason)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(created_at) = created_at {
        outbox::emit(
            tx,
            organization_id,
            &OutboxPayload::ReconciliationOpened(ReconciliationOpened {
                item_id,
                reservation_id: reservation_id.0,
                reason: reason.to_owned(),
                created_at,
            }),
        )
        .await?;
    } else if reason != RECOVERY_REASON {
        let updated: Option<(Uuid, DateTime<Utc>)> = sqlx::query_as(
            "UPDATE gateway.accounting_reconciliation SET reason=$2 WHERE reservation_id=$1 AND status='open' AND reason=$3 RETURNING id,CURRENT_TIMESTAMP",
        )
        .bind(reservation_id.0)
        .bind(reason)
        .bind(RECOVERY_REASON)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some((item_id, updated_at)) = updated {
            outbox::emit(
                tx,
                organization_id,
                &OutboxPayload::ReconciliationUpdated(ReconciliationUpdated {
                    item_id,
                    reservation_id: reservation_id.0,
                    previous_reason: RECOVERY_REASON.to_owned(),
                    reason: reason.to_owned(),
                    updated_at,
                }),
            )
            .await?;
        }
    }
    Ok(())
}

/// The reason expiry recovery opens an item with: the request was claimed but
/// its outcome is not known.
const RECOVERY_REASON: &str = "expired-after-claim";

/// A NOWAIT row lock found the row already locked (`lock_not_available`).
fn lock_unavailable(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == "55P03")
}

async fn resolve_reconciliation(
    tx: &mut Transaction<'_, Postgres>,
    reservation_id: ReservationId,
) -> Result<(), AccountingStoreError> {
    let resolved: Option<(Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as(
        "UPDATE gateway.accounting_reconciliation SET status='resolved',resolved_at=CURRENT_TIMESTAMP,resolution='settlement' WHERE reservation_id=$1 AND status='open' RETURNING id,organization_id,resolved_at",
    )
    .bind(reservation_id.0)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((item_id, organization_id, resolved_at)) = resolved {
        outbox::emit(
            tx,
            OrganizationId(organization_id),
            &OutboxPayload::ReconciliationResolved(ReconciliationResolved {
                item_id,
                reservation_id: reservation_id.0,
                resolution: "settlement".to_owned(),
                resolved_amount_minor: None,
                resolved_by: None,
                resolved_at,
            }),
        )
        .await?;
    }
    Ok(())
}

/// Writes a reservation, its price snapshot and its pending trace: a new
/// request's, reserved, or a failover attempt's (`lineage`), claimed at once
/// since it is sent as soon as it exists. A request admitted under a
/// rate-limit lease (`rate_lease`) names it, the ticket it used and when it
/// was admitted (`rate_admitted_at`), which the lease's counts rely on;
/// `created_at` stays the insert's time, which end-user limits count by.
/// The request's API key is written for the key's rate-limit policy; a
/// failover attempt's tokens are counted at the second the request it
/// continues was (`tokens_counted_at`), as the same request.
async fn insert_reservation(
    tx: &mut Transaction<'_, Postgres>,
    request: &ReservationRequest,
    id: ReservationId,
    amount: BudgetAmount,
    output_token_cap: Option<NonZeroU32>,
    expires: DateTime<Utc>,
    end_user_id: Option<Uuid>,
    lineage: Option<Lineage>,
    claimed: bool,
    rate_lease: Option<(Uuid, i32, DateTime<Utc>)>,
) -> Result<(), AccountingStoreError> {
    let pricing_confidence = match request.price.confidence {
        PricingConfidence::Known => "known",
        PricingConfidence::Conservative => "conservative",
    };
    sqlx::query("INSERT INTO gateway.accounting_reservation (id,organization_id,team_id,service_account_id,idempotency_key,fingerprint,state,claimed_at,failover_of,attempt,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id,end_user_id,token_bound,rate_lease_id,rate_admitted_at,rate_lease_ticket,api_key_id,tokens_counted_at,created_at) VALUES ($1,$2,$3,$4,$5,$6,CASE WHEN $35 THEN 'claimed' ELSE 'reserved' END,CASE WHEN $35 THEN CURRENT_TIMESTAMP END,$31,$32,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$33,$34,$36,$37,$38,$39,(SELECT COALESCE(f.tokens_counted_at,f.created_at) FROM gateway.accounting_reservation f WHERE f.organization_id=$2 AND f.id=$31),clock_timestamp())")
            .bind(id.0)
            .bind(request.scope.organization_id.0)
            .bind(request.scope.team_id.map(|v| v.0))
            .bind(request.scope.service_account_id.map(|v| v.0))
            .bind(&request.idempotency_key)
            .bind(request.fingerprint.as_slice())
            .bind(amount.value())
            .bind(expires)
            .bind(output_token_cap.map(|cap| i64::from(cap.get())))
            .bind(request.price.input_minor_per_token)
            .bind(request.price.output_minor_per_token)
            .bind(request.price.cached_input_minor_per_token)
            .bind(request.price.reasoning_minor_per_token)
            .bind(request.price.other_minor_per_token)
            .bind(&request.price.version)
            .bind(pricing_confidence)
            .bind(request.currency.as_str())
            .bind(&request.deployment)
            .bind(&request.model)
            .bind(request.trace_id.0)
            .bind(request.catalog_provenance.as_ref().map(|p| p.release_id))
            .bind(request.catalog_provenance.as_ref().map(|p| p.route_id))
            .bind(request.catalog_provenance.as_ref().map(|p| p.deployment_id))
            .bind(request.catalog_provenance.as_ref().map(|p| p.price_id))
            .bind(request.catalog_provenance.as_ref().map(|p| p.digest.to_vec()))
            .bind(request.catalog_provenance.as_ref().map(|p| p.operation.as_str()))
            .bind(request.catalog_provenance.as_ref().map(|p| p.family.as_str()))
            .bind(request.catalog_provenance.as_ref().map(|p| p.price_source.as_str()))
            .bind(request.catalog_provenance.as_ref().map(|p| p.chat_input_count_mode.as_str()))
            .bind(request.catalog_provenance.as_ref().and_then(|p| p.price_override_id))
            .bind(lineage.map(|l| l.failover_of.0))
            .bind(lineage.map_or(1, |l| l.attempt))
            .bind(end_user_id)
            .bind(token_bound(request, output_token_cap))
            .bind(claimed || lineage.is_some())
            .bind(rate_lease.map(|(lease, _, _)| lease))
            .bind(rate_lease.map(|(_, _, at)| at))
            .bind(rate_lease.map(|(_, ticket, _)| ticket))
            .bind(request.scope.api_key_id.map(|v| v.0))
            .execute(&mut **tx)
            .await?;
    sqlx::query("INSERT INTO gateway.accounting_price_snapshot (organization_id,reservation_id,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
            .bind(request.scope.organization_id.0)
            .bind(id.0)
            .bind(request.price.input_minor_per_token)
            .bind(request.price.output_minor_per_token)
            .bind(request.price.cached_input_minor_per_token)
            .bind(request.price.reasoning_minor_per_token)
            .bind(request.price.other_minor_per_token)
            .bind(&request.price.version)
            .bind(pricing_confidence)
            .bind(request.currency.as_str())
            .execute(&mut **tx)
            .await?;
    let metadata = serde_json::to_value(&request.trace_metadata)
        .map_err(|e| sqlx::Error::Protocol(e.to_string().into()))?;
    sqlx::query("INSERT INTO gateway.inference_trace (id,organization_id,team_id,service_account_id,reservation_id,correlation_id,operation,status,model,metadata,input_bound,output_cap,price_version,pricing_confidence,currency,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode,catalog_price_override_id,api_key_id,failover_of,attempt,end_user_id) VALUES ($1,$2,$3,$4,$5,$6,'inference','pending',$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26,$27)").bind(request.trace_id.0).bind(request.scope.organization_id.0).bind(request.scope.team_id.map(|v|v.0)).bind(request.scope.service_account_id.map(|v|v.0)).bind(id.0).bind(request.correlation_id).bind(&request.model).bind(metadata).bind(i64::try_from(request.input_bound).map_err(|_| AccountingError::Overflow)?).bind(request.trace_metadata.output_cap.map(i64::from)).bind(&request.price.version).bind(pricing_confidence).bind(request.currency.as_str()).bind(request.catalog_provenance.as_ref().map(|p| p.release_id)).bind(request.catalog_provenance.as_ref().map(|p| p.route_id)).bind(request.catalog_provenance.as_ref().map(|p| p.deployment_id)).bind(request.catalog_provenance.as_ref().map(|p| p.price_id)).bind(request.catalog_provenance.as_ref().map(|p| p.digest.to_vec())).bind(request.catalog_provenance.as_ref().map(|p| p.operation.as_str())).bind(request.catalog_provenance.as_ref().map(|p| p.family.as_str())).bind(request.catalog_provenance.as_ref().map(|p| p.price_source.as_str()))
            .bind(request.catalog_provenance.as_ref().map(|p| p.chat_input_count_mode.as_str())).bind(request.catalog_provenance.as_ref().and_then(|p| p.price_override_id)).bind(request.api_key_id).bind(lineage.map(|l| l.failed_trace.0)).bind(lineage.map_or(1, |l| l.attempt)).bind(end_user_id).execute(&mut **tx).await?;
    Ok(())
}

/// Records `timings` on the trace, keeping values already recorded so a
/// retry never rewrites them.
async fn record_timings(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: OrganizationId,
    trace_id: TraceId,
    timings: &crate::domain::model_card::RequestTimings,
) -> Result<(), AccountingStoreError> {
    if timings.is_empty() {
        return Ok(());
    }
    let ms = |value: Option<u32>| value.map(|value| i32::try_from(value).unwrap_or(i32::MAX));
    sqlx::query(
        "UPDATE gateway.inference_trace
         SET provider_latency_ms = COALESCE(provider_latency_ms, $3),
             first_event_ms = COALESCE(first_event_ms, $4),
             finish_reason = COALESCE(finish_reason, $5)
         WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.0)
    .bind(trace_id.0)
    .bind(ms(timings.provider_latency_ms))
    .bind(ms(timings.first_event_ms))
    .bind(timings.finish_reason.as_deref())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn finalize_trace(
    tx: &mut Transaction<'_, Postgres>,
    trace_id: TraceId,
    outcome: &str,
    amount: Option<i64>,
    provider_request_id: Option<&str>,
) -> Result<(), AccountingStoreError> {
    let trace: Option<FinalizedTraceRow> = sqlx::query_as(&format!("UPDATE gateway.inference_trace SET status=$1,outcome=$2,amount_minor=$3,provider_request_id=COALESCE($4,provider_request_id),finalized_at=CURRENT_TIMESTAMP WHERE id=$5 AND (status='pending' OR (status='finalized' AND outcome='unknown')) RETURNING {FINALIZED_TRACE_COLUMNS}")).bind("finalized").bind(outcome).bind(amount).bind(provider_request_id).bind(trace_id.0).fetch_optional(&mut **tx).await?;
    if let Some(trace) = trace {
        emit_trace_finalized(tx, trace).await?;
    }
    Ok(())
}

impl AccountingStore {
    /// Records how a request went on its trace: provider latency, time to a
    /// stream's first event, and the first choice's finish reason (a short
    /// code; see [`crate::domain::model_card::finish_reason_code`]). Values
    /// already recorded are kept, so a retry never rewrites them.
    pub async fn record_request_timings(
        &self,
        organization_id: OrganizationId,
        trace_id: TraceId,
        timings: &crate::domain::model_card::RequestTimings,
    ) -> Result<(), AccountingStoreError> {
        let mut tx = self.pool.begin().await?;
        record_timings(&mut tx, organization_id, trace_id, timings).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Records why the provider request failed: its category, HTTP status and
/// error type, never a message.
async fn record_provider_error(
    tx: &mut Transaction<'_, Postgres>,
    trace_id: TraceId,
    error: Option<&ProviderError>,
) -> Result<(), AccountingStoreError> {
    let Some(error) = error else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE gateway.inference_trace
         SET provider_error_category = $2, provider_error_status = $3, provider_error_code = $4
         WHERE id = $1",
    )
    .bind(trace_id.0)
    .bind(error.category().as_str())
    .bind(
        error
            .http_status()
            .filter(|status| (100..=599).contains(status))
            .map(|status| status as i16),
    )
    .bind(error.error_type())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

const FINALIZED_TRACE_COLUMNS: &str = "organization_id,id,reservation_id,team_id,service_account_id,model,btrim(currency) AS currency,outcome,amount_minor,finalized_at";

#[derive(FromRow)]
struct FinalizedTraceRow {
    organization_id: Uuid,
    id: Uuid,
    reservation_id: Uuid,
    team_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    model: String,
    currency: String,
    outcome: String,
    amount_minor: Option<i64>,
    finalized_at: DateTime<Utc>,
}

async fn emit_trace_finalized(
    conn: &mut PgConnection,
    trace: FinalizedTraceRow,
) -> Result<(), AccountingStoreError> {
    outbox::emit(
        conn,
        OrganizationId(trace.organization_id),
        &OutboxPayload::InferenceFinalized(InferenceFinalized {
            trace_id: trace.id,
            reservation_id: trace.reservation_id,
            team_id: trace.team_id.map(TeamId),
            service_account_id: trace.service_account_id.map(ServiceAccountId),
            model: trace.model,
            currency: trace.currency,
            outcome: trace.outcome,
            amount_minor: trace.amount_minor,
            finalized_at: trace.finalized_at,
        }),
    )
    .await?;
    Ok(())
}

impl From<OutboxError> for AccountingStoreError {
    fn from(error: OutboxError) -> Self {
        match error {
            OutboxError::Database(error) => Self::Database(error),
            OutboxError::Encode(error) => Self::Database(sqlx::Error::Encode(Box::new(error))),
        }
    }
}
