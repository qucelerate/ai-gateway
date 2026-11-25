//! Hard-budget accounting primitives.
//!
//! This module contains no database or provider concerns.  Values are checked
//! integers, reservation transitions are explicit, and usage never turns an
//! unknown counter into zero.
use crate::domain::{
    identity::{ApiKeyId, OrganizationId, ServiceAccountId, TeamId, VerifiedIdentity},
    inference::{Usage, UsageCompleteness, UsageQuality},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, num::NonZeroU32};
use uuid::Uuid;

/// Money units per smallest unit of a currency. Every monetary integer —
/// catalog per-token prices, budget limits, reservations, charges — counts
/// units of 10⁻⁶ of the currency's smallest unit: micro-cents for USD and
/// EUR, micro-yen for JPY. $2.50 per million tokens is 250 per token, and a
/// $100 budget is 10¹⁰. Amounts in different currencies are never converted
/// or summed.
pub const MONEY_SCALE: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ReservationId(pub Uuid);
impl ReservationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TraceId(pub Uuid);
impl TraceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuotaScope {
    pub organization_id: OrganizationId,
    pub team_id: Option<TeamId>,
    pub service_account_id: Option<ServiceAccountId>,
    /// The key the request was made with: its budget and rate-limit policy,
    /// if it has them, apply too. Stored with a reservation for the key's
    /// rate limits (and before migrations 0150 to 0155, not at all); settlement goes
    /// through the buckets the reservation was allocated.
    #[serde(default)]
    pub api_key_id: Option<ApiKeyId>,
}
impl QuotaScope {
    pub fn organization(id: OrganizationId) -> Self {
        Self {
            organization_id: id,
            team_id: None,
            service_account_id: None,
            api_key_id: None,
        }
    }
    pub fn from_identity(identity: &VerifiedIdentity) -> Result<Self, AccountingError> {
        let scope = Self {
            organization_id: identity.organization_id,
            team_id: identity.team_id,
            service_account_id: Some(identity.service_account_id),
            api_key_id: identity.api_key_id,
        };
        scope.validate().map(|_| scope)
    }
    pub fn validate(&self) -> Result<(), AccountingError> {
        if self.team_id.is_none() && self.service_account_id.is_none() {
            return Ok(());
        }
        if self.team_id.is_some() || self.service_account_id.is_some() {
            return Ok(());
        }
        Err(AccountingError::InvalidScope)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReservationState {
    Reserved,
    Claimed,
    Settled,
    Released,
    Unknown,
    Expired,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AllocationState {
    Held,
    Settled,
    Released,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetAmount(i64);
impl BudgetAmount {
    pub fn new(value: i64) -> Result<Self, AccountingError> {
        if value < 0 {
            Err(AccountingError::NegativeAmount)
        } else {
            Ok(Self(value))
        }
    }
    pub fn zero() -> Self {
        Self(0)
    }
    pub fn value(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Currency(String);
impl Currency {
    pub fn new(value: impl Into<String>) -> Result<Self, AccountingError> {
        let value = value.into();
        if value.len() != 3 || !value.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(AccountingError::InvalidCurrency);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UsageConfidence {
    Exact,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PricingConfidence {
    Known,
    Conservative,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageCounters {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached_input: Option<u64>,
    pub reasoning: Option<u64>,
    pub other: Option<u64>,
}
impl UsageCounters {
    pub fn from_usage(usage: &Usage) -> Self {
        let value = |v: Option<crate::domain::inference::TokenCounter>| v.and_then(|c| c.value());
        Self {
            input: value(usage.input_tokens()),
            output: value(usage.output_tokens()),
            cached_input: value(usage.cached_input_tokens()),
            reasoning: value(usage.reasoning_tokens()),
            other: value(usage.other_tokens()),
        }
    }
    pub fn confidence(usage: &Usage) -> UsageConfidence {
        match usage.quality() {
            UsageQuality::Exact => UsageConfidence::Exact,
            UsageQuality::Estimated => UsageConfidence::Estimated,
            UsageQuality::Unknown => UsageConfidence::Unknown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceSnapshot {
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub version: String,
    pub confidence: PricingConfidence,
}
impl PriceSnapshot {
    pub fn new(input: i64, output: i64, cached_input: i64) -> Result<Self, AccountingError> {
        Self::configured(
            input,
            output,
            cached_input,
            "test",
            PricingConfidence::Known,
        )
    }
    pub fn configured(
        input: i64,
        output: i64,
        cached_input: i64,
        version: impl Into<String>,
        confidence: PricingConfidence,
    ) -> Result<Self, AccountingError> {
        Self::configured_with_breakdown(input, output, cached_input, 0, 0, version, confidence)
    }
    pub fn configured_with_breakdown(
        input: i64,
        output: i64,
        cached_input: i64,
        reasoning: i64,
        other: i64,
        version: impl Into<String>,
        confidence: PricingConfidence,
    ) -> Result<Self, AccountingError> {
        if [input, output, cached_input, reasoning, other]
            .iter()
            .any(|v| *v < 0)
        {
            return Err(AccountingError::NegativePrice);
        }
        let version = version.into();
        if version.trim().is_empty() || version.len() > 128 {
            return Err(AccountingError::InvalidPriceVersion);
        }
        Ok(Self {
            input_minor_per_token: input,
            output_minor_per_token: output,
            cached_input_minor_per_token: cached_input,
            reasoning_minor_per_token: reasoning,
            other_minor_per_token: other,
            version,
            confidence,
        })
    }
    /// The highest of `prices` in every dimension, to reserve a request
    /// that may be served at any of them (a route over several deployments,
    /// D-023), or `None` when there are none. Reasoning is held at the
    /// highest rate a reasoning token may cost, which for a price without a
    /// reasoning price is its output price. Only reserved, never charged.
    pub fn ceiling<'a>(prices: impl IntoIterator<Item = &'a PriceSnapshot>) -> Option<Self> {
        prices
            .into_iter()
            .fold(None, |ceiling: Option<Self>, price| {
                Some(match ceiling {
                    None => Self {
                        reasoning_minor_per_token: price.reasoning_rate(),
                        version: "route-ceiling".into(),
                        ..price.clone()
                    },
                    Some(ceiling) => Self {
                        input_minor_per_token: ceiling
                            .input_minor_per_token
                            .max(price.input_minor_per_token),
                        output_minor_per_token: ceiling
                            .output_minor_per_token
                            .max(price.output_minor_per_token),
                        cached_input_minor_per_token: ceiling
                            .cached_input_minor_per_token
                            .max(price.cached_input_minor_per_token),
                        reasoning_minor_per_token: ceiling
                            .reasoning_minor_per_token
                            .max(price.reasoning_rate()),
                        other_minor_per_token: ceiling
                            .other_minor_per_token
                            .max(price.other_minor_per_token),
                        version: ceiling.version,
                        confidence: ceiling.confidence,
                    },
                })
            })
    }
    pub fn charge_tokens(
        &self,
        input: u64,
        output: u64,
        cached: u64,
    ) -> Result<BudgetAmount, AccountingError> {
        self.charge_tokens_with_breakdown(input, output, cached, 0, 0)
    }
    pub fn charge_tokens_with_breakdown(
        &self,
        input: u64,
        output: u64,
        cached: u64,
        reasoning: u64,
        other: u64,
    ) -> Result<BudgetAmount, AccountingError> {
        let uncached = input
            .checked_sub(cached)
            .ok_or(AccountingError::InvalidUsage)? as i128;
        let total = uncached
            .checked_mul(self.input_minor_per_token as i128)
            .and_then(|v| {
                v.checked_add((output as i128).checked_mul(self.output_minor_per_token as i128)?)
            })
            .and_then(|v| {
                v.checked_add(
                    (cached as i128).checked_mul(self.cached_input_minor_per_token as i128)?,
                )
            })
            .and_then(|v| {
                v.checked_add((reasoning as i128).checked_mul(self.reasoning_rate() as i128)?)
            })
            .and_then(|v| {
                v.checked_add((other as i128).checked_mul(self.other_minor_per_token as i128)?)
            })
            .ok_or(AccountingError::Overflow)?;
        if total > i64::MAX as i128 {
            return Err(AccountingError::Overflow);
        }
        BudgetAmount::new(total as i64)
    }
    /// What a reasoning token costs. Usage counts reasoning apart from
    /// output, and providers bill it as output, so a price that publishes
    /// no reasoning price (zero) charges it at the output price.
    pub fn reasoning_rate(&self) -> i64 {
        if self.reasoning_minor_per_token == 0 {
            self.output_minor_per_token
        } else {
            self.reasoning_minor_per_token
        }
    }
    pub fn reserve_tokens(
        &self,
        input_bound: u64,
        output_cap: u64,
    ) -> Result<BudgetAmount, AccountingError> {
        // Any prompt token may be read from or written to a prompt cache
        // (Anthropic and Bedrock report cache writes as `other`, charged
        // on top of the uncached input), so the input bound is reserved at
        // the dearest of the three: a prompt written to the cache whole
        // stays within what was reserved.
        let input_rate = self
            .input_minor_per_token
            .max(self.cached_input_minor_per_token)
            .max(self.other_minor_per_token);
        let input = (input_bound as i128)
            .checked_mul(input_rate as i128)
            .ok_or(AccountingError::Overflow)?;
        let output_rate = (self.output_minor_per_token as i128)
            .checked_add(self.reasoning_rate() as i128)
            .and_then(|v| v.checked_add(self.other_minor_per_token as i128))
            .ok_or(AccountingError::Overflow)?;
        let total = input
            .checked_add(
                (output_cap as i128)
                    .checked_mul(output_rate)
                    .ok_or(AccountingError::Overflow)?,
            )
            .ok_or(AccountingError::Overflow)?;
        if total > i64::MAX as i128 {
            return Err(AccountingError::Overflow);
        }
        BudgetAmount::new(total as i64)
    }
    pub fn charge(&self, usage: &Usage) -> Result<UsageCalculation, AccountingError> {
        if usage.completeness() != UsageCompleteness::Complete
            || usage.quality() != UsageQuality::Exact
        {
            return Err(AccountingError::UnknownUsage);
        }
        let c = UsageCounters::from_usage(usage);
        let amount = self.charge_tokens_with_breakdown(
            c.input.ok_or(AccountingError::UnknownUsage)?,
            c.output.ok_or(AccountingError::UnknownUsage)?,
            optional_billable_counter(c.cached_input, self.cached_input_minor_per_token)?,
            optional_billable_counter(c.reasoning, self.reasoning_minor_per_token)?,
            optional_billable_counter(c.other, self.other_minor_per_token)?,
        )?;
        Ok(UsageCalculation {
            amount,
            counters: c,
            confidence: UsageCounters::confidence(usage),
        })
    }
}

fn optional_billable_counter(value: Option<u64>, price: i64) -> Result<u64, AccountingError> {
    match value {
        Some(value) => Ok(value),
        None if price == 0 => Ok(0),
        None => Err(AccountingError::UnknownUsage),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageCalculation {
    pub amount: BudgetAmount,
    pub counters: UsageCounters,
    pub confidence: UsageConfidence,
}
pub fn calculate_usage(
    price: PriceSnapshot,
    usage: &Usage,
) -> Result<UsageCalculation, AccountingError> {
    price.charge(usage)
}

/// Safe, fixed-shape trace details. It cannot carry prompts, completions, or
/// arbitrary provider JSON.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceMetadata {
    pub input_bound: u64,
    pub output_cap: Option<u32>,
    pub currency: String,
}
impl TraceMetadata {
    pub fn new(input_bound: u64, output_cap: impl Into<Option<u32>>, currency: Currency) -> Self {
        Self {
            input_bound,
            output_cap: output_cap.into(),
            currency: currency.0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reservation {
    pub id: ReservationId,
    pub scope: QuotaScope,
    pub idempotency_key: String,
    pub fingerprint: Vec<u8>,
    pub reserved: BudgetAmount,
    pub expires_at: DateTime<Utc>,
    pub state: ReservationState,
    pub price: PriceSnapshot,
    pub currency: Currency,
    pub output_token_cap: Option<NonZeroU32>,
    pub deployment: String,
    pub model: String,
    pub trace_id: TraceId,
    pub catalog_provenance: Option<CatalogProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogProvenance {
    pub release_id: uuid::Uuid,
    pub route_id: uuid::Uuid,
    pub deployment_id: uuid::Uuid,
    pub price_id: uuid::Uuid,
    pub digest: [u8; 32],
    pub operation: String,
    pub family: String,
    pub price_source: String,
    pub chat_input_count_mode: String,
    /// The organization price override that replaced the catalog price, if any.
    #[serde(default)]
    pub price_override_id: Option<uuid::Uuid>,
}
impl Reservation {
    pub fn can_claim(&self, now: DateTime<Utc>) -> Result<(), AccountingError> {
        if self.state != ReservationState::Reserved {
            return Err(AccountingError::InvalidTransition);
        }
        if self.expires_at <= now {
            return Err(AccountingError::Expired);
        }
        Ok(())
    }
    pub fn claim_at(&mut self, now: DateTime<Utc>) -> Result<(), AccountingError> {
        if self.state == ReservationState::Claimed {
            return Ok(());
        }
        self.can_claim(now)?;
        self.state = ReservationState::Claimed;
        Ok(())
    }
    pub fn mark_unknown(&mut self) -> Result<(), AccountingError> {
        if self.state != ReservationState::Claimed {
            return Err(AccountingError::InvalidTransition);
        }
        self.state = ReservationState::Unknown;
        Ok(())
    }
    pub fn settle(&mut self, amount: BudgetAmount) -> Result<(), AccountingError> {
        if self.state == ReservationState::Settled {
            if amount.value() <= self.reserved.value() {
                return Ok(());
            }
            return Err(AccountingError::InvalidTransition);
        }
        if !matches!(
            self.state,
            ReservationState::Claimed | ReservationState::Unknown
        ) || amount.value() > self.reserved.value()
        {
            return Err(AccountingError::InvalidTransition);
        }
        self.state = ReservationState::Settled;
        Ok(())
    }
    pub fn expire_at(&mut self, now: DateTime<Utc>) -> Result<(), AccountingError> {
        if self.state != ReservationState::Reserved || self.expires_at > now {
            return Err(AccountingError::InvalidTransition);
        }
        self.state = ReservationState::Expired;
        Ok(())
    }
    pub fn ttl_from(now: DateTime<Utc>, seconds: i64) -> Result<DateTime<Utc>, AccountingError> {
        if seconds <= 0 {
            return Err(AccountingError::InvalidTtl);
        }
        now.checked_add_signed(Duration::seconds(seconds))
            .ok_or(AccountingError::Overflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountingError {
    InvalidScope,
    NegativeAmount,
    NegativePrice,
    InvalidPriceVersion,
    Overflow,
    UnknownUsage,
    InvalidTransition,
    Expired,
    InvalidTtl,
    InvalidFingerprint,
    InvalidIdempotency,
    InvalidUsage,
    InvalidCurrency,
}
impl fmt::Display for AccountingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidScope => "invalid quota scope",
            Self::NegativeAmount => "amount cannot be negative",
            Self::NegativePrice => "price cannot be negative",
            Self::InvalidPriceVersion => "price version is invalid",
            Self::Overflow => "accounting integer overflow",
            Self::UnknownUsage => "provider usage is unknown",
            Self::InvalidTransition => "invalid reservation transition",
            Self::Expired => "reservation expired",
            Self::InvalidTtl => "invalid reservation ttl",
            Self::InvalidFingerprint => "invalid fingerprint",
            Self::InvalidIdempotency => "invalid idempotency key",
            Self::InvalidUsage => "invalid provider usage",
            Self::InvalidCurrency => "invalid currency",
        })
    }
}
impl std::error::Error for AccountingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::inference::{TokenCounter, Usage};
    #[test]
    fn a_prompt_written_to_the_cache_whole_stays_within_its_reservation() {
        // Input 100, output 500, cache reads 10, cache writes 125 per token.
        let price = PriceSnapshot::configured_with_breakdown(
            100,
            500,
            10,
            0,
            125,
            "cache",
            PricingConfidence::Known,
        )
        .unwrap();
        let reserved = price.reserve_tokens(10_000, 1).unwrap().value();
        // Every prompt token written to the cache, one output token.
        let usage = Usage::complete(
            TokenCounter::exact(0),
            TokenCounter::exact(1),
            Some(TokenCounter::exact(0)),
            None,
            Some(TokenCounter::exact(10_000)),
        )
        .unwrap();
        let charged = price.charge(&usage).unwrap().amount.value();
        assert_eq!(charged, 10_000 * 125 + 500);
        assert!(charged <= reserved, "{charged} > {reserved}");
    }

    #[test]
    fn a_route_reserves_at_the_highest_price_of_each_dimension() {
        let a =
            PriceSnapshot::configured_with_breakdown(5, 10, 1, 0, 0, "a", PricingConfidence::Known)
                .unwrap();
        let b =
            PriceSnapshot::configured_with_breakdown(3, 12, 2, 7, 1, "b", PricingConfidence::Known)
                .unwrap();
        let ceiling = PriceSnapshot::ceiling([&a, &b]).unwrap();
        assert_eq!(
            (
                ceiling.input_minor_per_token,
                ceiling.output_minor_per_token,
                ceiling.cached_input_minor_per_token,
                ceiling.other_minor_per_token,
            ),
            (5, 12, 2, 1)
        );
        // `a` charges reasoning at its output price, 10, above `b`'s 7.
        assert_eq!(ceiling.reasoning_rate(), 10);
        assert!(PriceSnapshot::ceiling([]).is_none());
    }
    #[test]
    fn checked_pricing_excludes_cached_tokens_from_uncached_input() {
        let p = PriceSnapshot::new(2, 5, 1).unwrap();
        let u = Usage::complete(
            TokenCounter::exact(10),
            TokenCounter::exact(4),
            Some(TokenCounter::exact(2)),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.charge(&u).unwrap().amount.value(), 38);
    }
    #[test]
    fn unknown_usage_is_not_zero() {
        assert_eq!(
            PriceSnapshot::new(1, 1, 1)
                .unwrap()
                .charge(&Usage::unavailable()),
            Err(AccountingError::UnknownUsage)
        );
    }

    #[test]
    fn positive_optional_prices_require_concrete_counters() {
        let missing_cached = Usage::complete(
            TokenCounter::exact(2),
            TokenCounter::exact(1),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            PriceSnapshot::configured_with_breakdown(
                1,
                1,
                1,
                0,
                0,
                "v1",
                PricingConfidence::Known,
            )
            .unwrap()
            .charge(&missing_cached),
            Err(AccountingError::UnknownUsage)
        );

        let missing_reasoning = Usage::complete(
            TokenCounter::exact(2),
            TokenCounter::exact(1),
            None,
            None,
            Some(TokenCounter::exact(0)),
        )
        .unwrap();
        assert_eq!(
            PriceSnapshot::configured_with_breakdown(
                1,
                1,
                0,
                1,
                0,
                "v1",
                PricingConfidence::Known,
            )
            .unwrap()
            .charge(&missing_reasoning),
            Err(AccountingError::UnknownUsage)
        );

        let missing_other = Usage::complete(
            TokenCounter::exact(2),
            TokenCounter::exact(1),
            None,
            Some(TokenCounter::exact(0)),
            None,
        )
        .unwrap();
        assert_eq!(
            PriceSnapshot::configured_with_breakdown(
                1,
                1,
                0,
                0,
                1,
                "v1",
                PricingConfidence::Known,
            )
            .unwrap()
            .charge(&missing_other),
            Err(AccountingError::UnknownUsage)
        );
    }

    #[test]
    fn zero_priced_optional_counters_default_to_zero() {
        let usage = Usage::complete(
            TokenCounter::exact(4),
            TokenCounter::exact(5),
            None,
            None,
            None,
        )
        .unwrap();
        let price =
            PriceSnapshot::configured_with_breakdown(2, 3, 0, 0, 0, "v1", PricingConfidence::Known)
                .unwrap();
        assert_eq!(price.charge(&usage).unwrap().amount.value(), 23);
    }

    #[test]
    fn embedding_usage_settles_under_a_cached_input_price() {
        let price = PriceSnapshot::new(2, 0, 2).unwrap();
        let usage = Usage::embedding(TokenCounter::exact(20)).unwrap();
        assert_eq!(price.charge(&usage).unwrap().amount.value(), 40);
        // Usage that leaves the cached dimension out is unknown to that price.
        let bare = Usage::complete(
            TokenCounter::exact(20),
            TokenCounter::exact(0),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(price.charge(&bare), Err(AccountingError::UnknownUsage));
    }

    #[test]
    fn estimated_complete_usage_cannot_be_hard_settled() {
        let usage = Usage::complete(
            TokenCounter::estimated(2),
            TokenCounter::estimated(1),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            PriceSnapshot::configured(1, 1, 1, "v1", PricingConfidence::Conservative)
                .unwrap()
                .charge(&usage),
            Err(AccountingError::UnknownUsage)
        );
    }

    #[test]
    fn hard_budget_reserve_covers_all_possible_billed_counters() {
        let price = PriceSnapshot::configured_with_breakdown(
            2,
            3,
            7,
            5,
            11,
            "v1",
            PricingConfidence::Conservative,
        )
        .unwrap();
        // The input bound at the dearest prompt rate (cache writes, 11), the
        // output cap at output, reasoning and other together.
        assert_eq!(
            price.reserve_tokens(10, 4).unwrap().value(),
            10 * 11 + 4 * (3 + 5 + 11)
        );
        let usage = Usage::complete(
            TokenCounter::exact(10),
            TokenCounter::exact(4),
            Some(TokenCounter::exact(2)),
            Some(TokenCounter::exact(1)),
            Some(TokenCounter::exact(1)),
        )
        .unwrap();
        assert_eq!(price.charge(&usage).unwrap().amount.value(), 58);
    }

    #[test]
    fn unpriced_reasoning_is_charged_and_reserved_as_output() {
        // Usage counts reasoning apart from output; a price with no
        // reasoning price charges it at the output price, as providers do.
        let price =
            PriceSnapshot::configured_with_breakdown(2, 3, 1, 0, 0, "v1", PricingConfidence::Known)
                .unwrap();
        assert_eq!(price.reasoning_rate(), 3);
        let usage = Usage::complete(
            TokenCounter::exact(10),
            TokenCounter::exact(4),
            Some(TokenCounter::exact(0)),
            Some(TokenCounter::exact(6)),
            None,
        )
        .unwrap();
        assert_eq!(price.charge(&usage).unwrap().amount.value(), 20 + 12 + 18);
        assert_eq!(price.reserve_tokens(10, 4).unwrap().value(), 20 + 4 * 6);
        // A published reasoning price applies as is.
        let priced =
            PriceSnapshot::configured_with_breakdown(2, 3, 1, 5, 0, "v1", PricingConfidence::Known)
                .unwrap();
        assert_eq!(priced.reasoning_rate(), 5);
        assert_eq!(priced.charge(&usage).unwrap().amount.value(), 20 + 12 + 30);
    }
    #[test]
    fn identity_scope_is_derived() {
        let i = VerifiedIdentity {
            organization_id: OrganizationId::new(),
            service_account_id: ServiceAccountId::new(),
            team_id: None,
            api_key_id: None,
            scopes: vec![],
            end_user: None,
        };
        assert_eq!(
            QuotaScope::from_identity(&i).unwrap().service_account_id,
            Some(i.service_account_id)
        );
    }
}
