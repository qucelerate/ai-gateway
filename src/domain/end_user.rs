//! Budgets per end user of an app (D-028, plan §6).
//!
//! An app's service account names the user behind a request; each user is on
//! one of the service account's plans, an allowance per period (a month,
//! week, day or number of days) with optional per-user rate limits, and may
//! hold prepaid credit grants. A request spends the allowance first, then credits,
//! soonest-expiring first. This module holds the rules that need no
//! database: what an end-user ID may be, how a reservation is split across
//! a user's allowance and credits, and how a settled charge is split back.

use chrono::DurationRound;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// The request header naming the end user.
pub const END_USER_HEADER: &str = "gateway-end-user";

/// The longest end-user ID, in bytes.
pub const MAX_END_USER_ID_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum EndUserIdError {
    #[error("end-user ID must be 1 to 256 bytes of visible UTF-8")]
    Invalid,
}

/// An end-user ID as the app sent it: 1–256 bytes of UTF-8 with no control
/// characters and no leading or trailing whitespace. It is the app's own and
/// opaque to the gateway, and stored as sent.
pub fn parse_end_user_id(value: &str) -> Result<String, EndUserIdError> {
    if value.is_empty()
        || value.len() > MAX_END_USER_ID_BYTES
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        return Err(EndUserIdError::Invalid);
    }
    Ok(value.to_owned())
}

/// Whether a service account's requests must name their end user.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndUserRequirement {
    #[default]
    Optional,
    Required,
}

impl EndUserRequirement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Optional => "optional",
            Self::Required => "required",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "optional" => Some(Self::Optional),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

/// What remains of one end-user bucket: limit less consumed and reserved.
/// The allowance can be negative after usage past a reservation; a credit
/// grant never is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Remaining {
    pub bucket: Uuid,
    pub remaining: i128,
}

/// One request's share of an end user's buckets: `rank` 0 is the allowance,
/// 1.. the credit grants in draw order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Share {
    pub bucket: Uuid,
    pub rank: i16,
    pub amount: i64,
}

/// What the end user can still spend: the allowance (which may be negative,
/// a debt credits must cover first) plus every credit grant.
pub fn available(allowance: &Remaining, credits: &[Remaining]) -> i128 {
    credits.iter().fold(allowance.remaining, |sum, credit| {
        sum + credit.remaining.max(0)
    })
}

/// Split a reservation of `amount` across the allowance, then the credits in
/// the order given (the caller orders them soonest-expiring first). The
/// allowance always gets a share, possibly zero, since usage past the
/// reservation is charged to it. `None` when the user cannot cover it.
pub fn split_reservation(
    amount: i64,
    allowance: &Remaining,
    credits: &[Remaining],
) -> Option<Vec<Share>> {
    if amount < 0 || available(allowance, credits) < i128::from(amount) {
        return None;
    }
    let mut left = i128::from(amount);
    let from_allowance = left.min(allowance.remaining.max(0));
    left -= from_allowance;
    let mut shares = vec![Share {
        bucket: allowance.bucket,
        rank: 0,
        amount: i64::try_from(from_allowance).ok()?,
    }];
    for (index, credit) in credits.iter().enumerate() {
        if left == 0 {
            break;
        }
        let take = left.min(credit.remaining.max(0));
        if take > 0 {
            shares.push(Share {
                bucket: credit.bucket,
                rank: i16::try_from(index + 1).ok()?,
                amount: i64::try_from(take).ok()?,
            });
            left -= take;
        }
    }
    (left == 0).then_some(shares)
}

/// Split a settled `charge` back over a reservation's shares, given in rank
/// order (`reserved[0]` is the allowance's). Each share is charged up to what
/// it reserved, allowance first, so a request that used less than it
/// reserved gives credits back before allowance; anything past the whole
/// reservation is charged to the allowance, which can go over, so credits
/// never go below zero.
pub fn settle_shares(charge: i64, reserved: &[i64]) -> Vec<i64> {
    let mut left = charge.max(0);
    let mut charged: Vec<i64> = reserved
        .iter()
        .map(|&share| {
            let take = left.min(share.max(0));
            left -= take;
            take
        })
        .collect();
    if let Some(allowance) = charged.first_mut() {
        *allowance = allowance.saturating_add(left);
    }
    charged
}

/// The UTC calendar month containing `now`: an allowance's window.
pub fn month_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    use chrono::{Datelike, TimeZone};
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .expect("the first of a month at midnight UTC exists");
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let end = Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("the first of a month at midnight UTC exists");
    (start, end)
}

/// How a plan's allowance repeats, as the admin API names it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodKind {
    /// The UTC calendar month.
    #[default]
    Monthly,
    /// The ISO week, from Monday 00:00 UTC.
    Weekly,
    /// The UTC day.
    Daily,
    /// Windows of `period_days` days from [`PERIOD_DAYS_ANCHOR`].
    Days,
}

/// The longest fixed-day period.
pub const MAX_PERIOD_DAYS: u32 = 366;

/// A plan's allowance period: the window an allowance bucket covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanPeriod {
    Monthly,
    Weekly,
    Daily,
    /// 1 to [`MAX_PERIOD_DAYS`] days.
    Days(u32),
}

/// Where fixed-day windows are counted from: a Monday, so 7-day windows are
/// ISO weeks and 1-day windows UTC days.
pub fn period_days_anchor() -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(1970, 1, 5, 0, 0, 0)
        .single()
        .expect("a valid timestamp")
}

impl PlanPeriod {
    /// From the admin API's `period` and `period_days`: the day count only
    /// with `days`, and then 1 to 366.
    pub fn new(kind: PeriodKind, days: Option<u32>) -> Result<Self, EndUserInputError> {
        match (kind, days) {
            (PeriodKind::Monthly, None) => Ok(Self::Monthly),
            (PeriodKind::Weekly, None) => Ok(Self::Weekly),
            (PeriodKind::Daily, None) => Ok(Self::Daily),
            (PeriodKind::Days, Some(days)) if (1..=MAX_PERIOD_DAYS).contains(&days) => {
                Ok(Self::Days(days))
            }
            _ => Err(EndUserInputError::Period),
        }
    }

    /// From a stored plan; the migration checks the same rules, so an
    /// unexpected row falls back to the month, the period every plan had.
    pub fn from_row(period: &str, days: Option<i32>) -> Self {
        let days = days.and_then(|days| u32::try_from(days).ok());
        let kind = match period {
            "weekly" => PeriodKind::Weekly,
            "daily" => PeriodKind::Daily,
            "days" => PeriodKind::Days,
            _ => PeriodKind::Monthly,
        };
        Self::new(kind, days).unwrap_or(Self::Monthly)
    }

    pub fn kind(self) -> PeriodKind {
        match self {
            Self::Monthly => PeriodKind::Monthly,
            Self::Weekly => PeriodKind::Weekly,
            Self::Daily => PeriodKind::Daily,
            Self::Days(_) => PeriodKind::Days,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Weekly => "weekly",
            Self::Daily => "daily",
            Self::Days(_) => "days",
        }
    }

    pub fn days(self) -> Option<u32> {
        match self {
            Self::Days(days) => Some(days),
            _ => None,
        }
    }

    /// The window containing `now`, `[start, end)`; the same as the
    /// database's `gateway.end_user_period_start` and `_end`.
    pub fn window(self, now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
        use chrono::{Datelike, TimeDelta, Timelike};
        let midnight = now
            .with_hour(0)
            .and_then(|at| at.with_minute(0))
            .and_then(|at| at.with_second(0))
            .and_then(|at| at.with_nanosecond(0))
            .expect("midnight UTC exists");
        match self {
            Self::Monthly => month_window(now),
            Self::Daily => (midnight, midnight + TimeDelta::days(1)),
            Self::Weekly => {
                let start =
                    midnight - TimeDelta::days(i64::from(now.weekday().num_days_from_monday()));
                (start, start + TimeDelta::days(7))
            }
            Self::Days(days) => {
                let anchor = period_days_anchor();
                let length = TimeDelta::days(i64::from(days));
                let micros = length.num_microseconds().expect("366 days fit");
                let elapsed = (now - anchor)
                    .num_microseconds()
                    .expect("within the timestamp range");
                let start = anchor + TimeDelta::microseconds(elapsed.div_euclid(micros) * micros);
                (start, start + length)
            }
        }
    }
}

/// The rate window of an end user's limits, in seconds (as policies').
pub const END_USER_RATE_WINDOW_SECONDS: i64 = 60;

/// The highest `requests_per_minute` of a plan (as rate-limit policies').
pub const MAX_PLAN_REQUESTS_PER_MINUTE: u32 = 1_000_000;

/// The highest `tokens_per_minute` of a plan.
pub const MAX_PLAN_TOKENS_PER_MINUTE: u32 = 1_000_000_000;

/// The highest `max_concurrent` of a plan (as rate-limit policies').
pub const MAX_PLAN_CONCURRENT: u32 = 1_000_000;

/// A plan's limits on each of its users; `None` is no limit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlanRateLimits {
    pub requests_per_minute: Option<u32>,
    pub tokens_per_minute: Option<u32>,
    /// The user's requests in flight (reserved or claimed), as a
    /// rate-limit policy's `max_concurrent`.
    pub max_concurrent: Option<u32>,
}

impl PlanRateLimits {
    pub fn is_empty(self) -> bool {
        self.requests_per_minute.is_none()
            && self.tokens_per_minute.is_none()
            && self.max_concurrent.is_none()
    }
}

/// Which of a plan's limits refused a request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndUserRateLimitKind {
    RequestsPerMinute,
    TokensPerMinute,
    /// `max_concurrent`.
    Concurrency,
}

impl EndUserRateLimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequestsPerMinute => "requests_per_minute",
            Self::TokensPerMinute => "tokens_per_minute",
            Self::Concurrency => "max_concurrent",
        }
    }
}

/// A request refused by its end user's plan limits, for the problem the app
/// sees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndUserRateRefusal {
    pub end_user: String,
    pub limit: EndUserRateLimitKind,
    /// Whole seconds until the request could fit, 1 to the window.
    pub retry_after_seconds: u32,
    /// The request alone is more than the limit: it never fits.
    pub never_fits: bool,
}

/// One request in an end user's token window: how many seconds until it
/// leaves the window, and the tokens it counts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowEntry {
    pub leaves_in_seconds: f64,
    pub tokens: u64,
}

/// Whole seconds until a request could be admitted: at least 1, at most the
/// window.
pub fn rate_retry_after(wait: f64) -> u32 {
    wait.ceil().clamp(1.0, END_USER_RATE_WINDOW_SECONDS as f64) as u32
}

/// Whether a request needing `needed` tokens fits a token window of `limit`
/// whose requests, oldest first, are `entries`: `Ok` with the tokens left
/// for it, or `Err` with the seconds until enough of the window has passed
/// (`None` when it never fits, being larger than the limit itself).
pub fn token_admission(
    limit: u64,
    needed: u64,
    entries: &[WindowEntry],
) -> Result<u64, Option<u32>> {
    let used: u64 = entries
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.tokens));
    let room = limit.saturating_sub(used);
    if needed <= room {
        return Ok(room);
    }
    if needed > limit {
        return Err(None);
    }
    // Free the oldest requests until what is left, plus this one, fits.
    let mut left = used;
    for entry in entries {
        left = left.saturating_sub(entry.tokens);
        if left.saturating_add(needed) <= limit {
            return Err(Some(rate_retry_after(entry.leaves_in_seconds)));
        }
    }
    Err(Some(END_USER_RATE_WINDOW_SECONDS as u32))
}

/// The end of a credit grant with no expiry: far enough that no request
/// reaches it, and a value every client decodes (unlike `infinity`).
pub fn never_expires() -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(9999, 12, 31, 0, 0, 0)
        .single()
        .expect("a valid timestamp")
}

/// Why a request was refused on its end user's account, for the problem the
/// app sees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndUserRefusal {
    pub end_user: String,
    /// This period's allowance left, never negative.
    pub allowance_remaining: i64,
    pub credit_balance: i64,
    pub currency: String,
}

/// The longest plan name, as `[a-z0-9][a-z0-9_-]{0,62}`.
pub const MAX_PLAN_NAME_BYTES: usize = 63;

/// A plan name: lowercase letters, digits, `_` and `-`, starting with a
/// letter or digit, up to 63 bytes.
pub fn valid_plan_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && name.len() <= MAX_PLAN_NAME_BYTES
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

fn stable_id(parts: &[&str]) -> Uuid {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b":");
    }
    Uuid::from_bytes(hasher.finalize().into())
}

/// Ids are stable per name, so a request and an admin write that create the
/// same row agree on it, and the admin lane's versions follow the resource.
pub fn setting_id(service_account_id: Uuid) -> Uuid {
    stable_id(&["end_user_setting", &service_account_id.to_string()])
}

pub fn plan_id(service_account_id: Uuid, name: &str) -> Uuid {
    stable_id(&["end_user_plan", &service_account_id.to_string(), name])
}

pub fn end_user_id(service_account_id: Uuid, external_id: &str) -> Uuid {
    stable_id(&["end_user", &service_account_id.to_string(), external_id])
}

pub fn grant_id(service_account_id: Uuid, reference: &str) -> Uuid {
    stable_id(&[
        "end_user_credit_grant",
        &service_account_id.to_string(),
        reference,
    ])
}

pub fn reversal_id(grant_id: Uuid, reference: &str) -> Uuid {
    stable_id(&["end_user_credit_reversal", &grant_id.to_string(), reference])
}

/// Why an admin write's body is refused, before any lookup.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum EndUserInputError {
    #[error("currency must be three uppercase letters")]
    Currency,
    #[error("plan names are 1 to 63 of a-z, 0-9, '_' and '-', starting with a letter or digit")]
    PlanName,
    #[error("allowance_minor must be 0 or more")]
    Allowance,
    #[error(
        "period is monthly, weekly, daily or days; period_days is 1 to 366, and only with days"
    )]
    Period,
    #[error(
        "requests_per_minute and max_concurrent are 1 to 1000000 and tokens_per_minute 1 to 1000000000, or absent"
    )]
    RateLimit,
    #[error("reference must be 1 to 255 characters")]
    Reference,
    #[error("amount_minor must be 1 or more")]
    Amount,
    #[error("expires_at must be in the future")]
    Expiry,
    #[error("reason must be 1 to 500 characters")]
    Reason,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndUserSettingsInput {
    #[serde(default)]
    pub requirement: EndUserRequirement,
    pub currency: String,
    #[serde(default)]
    pub default_plan: Option<String>,
}

impl EndUserSettingsInput {
    pub fn validate(self) -> Result<Self, EndUserInputError> {
        if self.currency.len() != 3 || !self.currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(EndUserInputError::Currency);
        }
        if self
            .default_plan
            .as_deref()
            .is_some_and(|name| !valid_plan_name(name))
        {
            return Err(EndUserInputError::PlanName);
        }
        Ok(self)
    }
}

/// A plan as `PUT` sets it: the whole plan, so a field left out is its
/// default (a monthly period, no rate limits).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanInput {
    pub allowance_minor: i64,
    #[serde(default)]
    pub period: PeriodKind,
    #[serde(default)]
    pub period_days: Option<u32>,
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

/// A checked [`PlanInput`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlanSpec {
    pub allowance_minor: i64,
    pub period: PlanPeriod,
    pub limits: PlanRateLimits,
}

impl PlanInput {
    pub fn validate(self) -> Result<PlanSpec, EndUserInputError> {
        if self.allowance_minor < 0 {
            return Err(EndUserInputError::Allowance);
        }
        let period = PlanPeriod::new(self.period, self.period_days)?;
        let within = |value: Option<u32>, max: u32| value.is_none_or(|v| (1..=max).contains(&v));
        if !within(self.requests_per_minute, MAX_PLAN_REQUESTS_PER_MINUTE)
            || !within(self.tokens_per_minute, MAX_PLAN_TOKENS_PER_MINUTE)
            || !within(self.max_concurrent, MAX_PLAN_CONCURRENT)
        {
            return Err(EndUserInputError::RateLimit);
        }
        Ok(PlanSpec {
            allowance_minor: self.allowance_minor,
            period,
            limits: PlanRateLimits {
                requests_per_minute: self.requests_per_minute,
                tokens_per_minute: self.tokens_per_minute,
                max_concurrent: self.max_concurrent,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EndUserInput {
    pub plan: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreditGrantInput {
    pub reference: String,
    pub amount_minor: i64,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl CreditGrantInput {
    /// The reference and amount; the expiry is checked only for a new
    /// grant ([`Self::expired`]), so a retried purchase whose credits have
    /// since expired still returns its grant.
    /// The expiry keeps the microseconds the ledger stores, so a retry with
    /// a finer timestamp still matches the grant it made.
    pub fn validate(mut self) -> Result<Self, EndUserInputError> {
        if !valid_reference(&self.reference) {
            return Err(EndUserInputError::Reference);
        }
        if self.amount_minor < 1 {
            return Err(EndUserInputError::Amount);
        }
        self.expires_at = self
            .expires_at
            .map(|at| at.duration_trunc(chrono::TimeDelta::microseconds(1)))
            .transpose()
            .map_err(|_| EndUserInputError::Expiry)?;
        Ok(self)
    }

    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreditReversalInput {
    pub reference: String,
    pub amount_minor: i64,
    pub reason: String,
}

impl CreditReversalInput {
    pub fn validate(self) -> Result<Self, EndUserInputError> {
        if !valid_reference(&self.reference) {
            return Err(EndUserInputError::Reference);
        }
        if self.amount_minor < 1 {
            return Err(EndUserInputError::Amount);
        }
        let reason = self.reason.chars().count();
        if !(1..=500).contains(&reason) || self.reason.trim().is_empty() {
            return Err(EndUserInputError::Reason);
        }
        Ok(self)
    }
}

fn valid_reference(reference: &str) -> bool {
    let length = reference.chars().count();
    (1..=255).contains(&length)
        && reference.trim() == reference
        && !reference.chars().any(char::is_control)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EndUserSettingsView {
    pub service_account_id: Uuid,
    pub requirement: EndUserRequirement,
    pub currency: String,
    pub default_plan: Option<String>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlanView {
    pub name: String,
    /// Per period.
    pub allowance_minor: i64,
    // Defaults let a response recorded before these fields replay.
    #[serde(default)]
    pub period: PeriodKind,
    #[serde(default)]
    pub period_days: Option<u32>,
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    pub currency: String,
    /// The end users it is in force for: assigned to it, or unassigned
    /// while it is the default.
    #[serde(default)]
    pub users: u64,
    /// What those users' requests settled in `period_start..period_end`
    /// were charged, in `currency`.
    #[serde(default)]
    pub spent_minor: i64,
    /// The plan's current period.
    #[serde(default)]
    pub period_start: DateTime<Utc>,
    #[serde(default)]
    pub period_end: DateTime<Utc>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EndUserView {
    /// The app's ID, as sent.
    pub id: String,
    pub plan: Option<String>,
    pub effective_plan: Option<String>,
    pub currency: Option<String>,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub allowance_minor: i64,
    pub allowance_spent_minor: i64,
    pub allowance_reserved_minor: i64,
    pub allowance_remaining_minor: i64,
    pub credit_balance_minor: i64,
    /// What its requests settled this period (of the plan in force; the UTC
    /// month without one) were charged, from allowance or credits, in
    /// `spent_currency`; none when, without end-user settings, they used
    /// several currencies.
    pub spent_minor: Option<i64>,
    /// The app's currency; without end-user settings, the one its requests
    /// used (none before any request this period).
    pub spent_currency: Option<String>,
    /// 0 for a user only requests have seen.
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreditGrantView {
    pub id: Uuid,
    pub end_user: String,
    pub reference: String,
    pub amount_minor: i64,
    pub currency: String,
    pub reversed_minor: i64,
    pub spent_minor: i64,
    pub reserved_minor: i64,
    pub remaining_minor: i64,
    pub expires_at: Option<DateTime<Utc>>,
    pub expired: bool,
    pub granted_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreditReversalView {
    pub id: Uuid,
    pub grant_id: Uuid,
    pub reference: String,
    pub requested_minor: i64,
    pub reversed_minor: i64,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(remaining: i128) -> Remaining {
        Remaining {
            bucket: Uuid::new_v4(),
            remaining,
        }
    }

    #[test]
    fn end_user_ids_are_visible_utf8_up_to_256_bytes() {
        assert!(parse_end_user_id("user-42").is_ok());
        assert!(parse_end_user_id("Zoë 7").is_ok());
        assert!(parse_end_user_id(&"a".repeat(256)).is_ok());
        assert!(parse_end_user_id(&"a".repeat(257)).is_err());
        assert!(parse_end_user_id("").is_err());
        assert!(parse_end_user_id(" padded").is_err());
        assert!(parse_end_user_id("tab\there").is_err());
        assert!(parse_end_user_id("nul\0").is_err());
    }

    #[test]
    fn a_reservation_draws_allowance_then_credits_in_order() {
        let allowance = bucket(100);
        let (first, second) = (bucket(50), bucket(500));
        let shares = split_reservation(180, &allowance, &[first, second]).unwrap();
        assert_eq!(
            shares,
            vec![
                Share {
                    bucket: allowance.bucket,
                    rank: 0,
                    amount: 100
                },
                Share {
                    bucket: first.bucket,
                    rank: 1,
                    amount: 50
                },
                Share {
                    bucket: second.bucket,
                    rank: 2,
                    amount: 30
                },
            ]
        );
        // Within the allowance, credits are not touched.
        let shares = split_reservation(60, &allowance, &[first]).unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].amount, 60);
        // More than allowance and credits together is refused.
        assert!(split_reservation(651, &allowance, &[first, second]).is_none());
    }

    #[test]
    fn an_allowance_overrun_is_a_debt_credits_cover_first() {
        let allowance = bucket(-40);
        let credit = bucket(100);
        assert_eq!(available(&allowance, &[credit]), 60);
        let shares = split_reservation(60, &allowance, &[credit]).unwrap();
        assert_eq!(shares[0].amount, 0);
        assert_eq!(shares[1].amount, 60);
        assert!(split_reservation(61, &allowance, &[credit]).is_none());
    }

    #[test]
    fn a_zero_allowance_plan_spends_credits_only() {
        let allowance = bucket(0);
        assert!(split_reservation(1, &allowance, &[]).is_none());
        let credit = bucket(10);
        let shares = split_reservation(10, &allowance, &[credit]).unwrap();
        assert_eq!(shares.iter().map(|s| s.amount).collect::<Vec<_>>(), [0, 10]);
    }

    #[test]
    fn settlement_charges_allowance_first_and_overruns_to_the_allowance() {
        // Used less: credits are given back before allowance.
        assert_eq!(settle_shares(120, &[100, 50, 30]), [100, 20, 0]);
        assert_eq!(settle_shares(0, &[100, 50]), [0, 0]);
        // Used exactly what was reserved.
        assert_eq!(settle_shares(180, &[100, 50, 30]), [100, 50, 30]);
        // Used more (a bound breach or an operator's resolution): the
        // allowance takes the rest and credits stay whole.
        assert_eq!(settle_shares(200, &[0, 60]), [140, 60]);
    }

    #[test]
    fn months_are_utc_calendar_months() {
        use chrono::TimeZone;
        let (start, end) = month_window(Utc.with_ymd_and_hms(2026, 12, 31, 23, 59, 59).unwrap());
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn periods_are_utc_days_iso_weeks_and_fixed_day_windows() {
        use chrono::TimeZone;
        let at = |y, m, d, h| Utc.with_ymd_and_hms(y, m, d, h, 30, 0).unwrap();
        let midnight = |y, m, d| Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap();
        // 2026-11-19 is a Thursday.
        let now = at(2026, 11, 19, 15);
        assert_eq!(
            PlanPeriod::Daily.window(now),
            (midnight(2026, 11, 19), midnight(2026, 11, 20))
        );
        assert_eq!(
            PlanPeriod::Weekly.window(now),
            (midnight(2026, 11, 16), midnight(2026, 11, 23))
        );
        assert_eq!(PlanPeriod::Monthly.window(now), month_window(now));
        // Fixed-day windows count from Monday 1970-01-05, so 7 days is the
        // ISO week and 1 day the UTC day.
        assert_eq!(
            PlanPeriod::Days(7).window(now),
            PlanPeriod::Weekly.window(now)
        );
        assert_eq!(
            PlanPeriod::Days(1).window(now),
            PlanPeriod::Daily.window(now)
        );
        let (start, end) = PlanPeriod::Days(30).window(now);
        assert!(start <= now && now < end);
        assert_eq!(end - start, chrono::TimeDelta::days(30));
        assert_eq!((start - period_days_anchor()).num_days() % 30, 0);
        // A window's end starts the next.
        assert_eq!(PlanPeriod::Days(30).window(end).0, end);
        assert_eq!(
            PlanPeriod::Weekly.window(midnight(2026, 11, 23)).0,
            midnight(2026, 11, 23)
        );
    }

    #[test]
    fn plan_inputs_check_periods_and_limits() {
        let input = |period, period_days, requests_per_minute, tokens_per_minute| PlanInput {
            allowance_minor: 10,
            period,
            period_days,
            requests_per_minute,
            tokens_per_minute,
            max_concurrent: None,
        };
        let plain = input(PeriodKind::Monthly, None, None, None)
            .validate()
            .unwrap();
        assert_eq!(plain.period, PlanPeriod::Monthly);
        assert!(plain.limits.is_empty());
        assert_eq!(
            input(PeriodKind::Days, Some(30), Some(60), Some(1_000))
                .validate()
                .unwrap(),
            PlanSpec {
                allowance_minor: 10,
                period: PlanPeriod::Days(30),
                limits: PlanRateLimits {
                    requests_per_minute: Some(60),
                    tokens_per_minute: Some(1_000),
                    max_concurrent: None,
                },
            }
        );
        for bad in [
            input(PeriodKind::Days, None, None, None),
            input(PeriodKind::Days, Some(0), None, None),
            input(PeriodKind::Days, Some(367), None, None),
            input(PeriodKind::Weekly, Some(7), None, None),
        ] {
            assert_eq!(bad.validate(), Err(EndUserInputError::Period));
        }
        for bad in [
            input(PeriodKind::Daily, None, Some(0), None),
            input(
                PeriodKind::Daily,
                None,
                Some(MAX_PLAN_REQUESTS_PER_MINUTE + 1),
                None,
            ),
            input(PeriodKind::Daily, None, None, Some(0)),
            input(
                PeriodKind::Daily,
                None,
                None,
                Some(MAX_PLAN_TOKENS_PER_MINUTE + 1),
            ),
        ] {
            assert_eq!(bad.validate(), Err(EndUserInputError::RateLimit));
        }
        let negative = PlanInput {
            allowance_minor: -1,
            ..input(PeriodKind::Monthly, None, None, None)
        };
        assert_eq!(negative.validate(), Err(EndUserInputError::Allowance));
        let concurrent = |max_concurrent| PlanInput {
            max_concurrent,
            ..input(PeriodKind::Monthly, None, None, None)
        };
        let limited = concurrent(Some(MAX_PLAN_CONCURRENT)).validate().unwrap();
        assert_eq!(limited.limits.max_concurrent, Some(MAX_PLAN_CONCURRENT));
        assert!(!limited.limits.is_empty());
        for bad in [Some(0), Some(MAX_PLAN_CONCURRENT + 1)] {
            assert_eq!(
                concurrent(bad).validate(),
                Err(EndUserInputError::RateLimit)
            );
        }
        // Old bodies still parse, and a stored row round-trips.
        let old: PlanInput = serde_json::from_str(r#"{"allowance_minor": 5}"#).unwrap();
        assert_eq!(old.validate().unwrap().period, PlanPeriod::Monthly);
        assert_eq!(PlanPeriod::from_row("days", Some(30)), PlanPeriod::Days(30));
        assert_eq!(PlanPeriod::from_row("weekly", None), PlanPeriod::Weekly);
        assert_eq!(PlanPeriod::from_row("days", None), PlanPeriod::Monthly);
    }

    #[test]
    fn a_token_window_admits_what_fits_and_says_when_more_will() {
        let entry = |leaves_in_seconds, tokens| WindowEntry {
            leaves_in_seconds,
            tokens,
        };
        let window = [entry(10.0, 40), entry(30.5, 30), entry(55.0, 20)];
        // 90 used of 100: 10 left.
        assert_eq!(token_admission(100, 10, &window), Ok(10));
        // 25 needs the oldest (40) to leave: in 10 seconds.
        assert_eq!(token_admission(100, 25, &window), Err(Some(10)));
        // 60 needs the two oldest gone: 30.5 rounds up to 31.
        assert_eq!(token_admission(100, 60, &window), Err(Some(31)));
        // 100 needs everything gone.
        assert_eq!(token_admission(100, 100, &window), Err(Some(55)));
        // More than the limit never fits.
        assert_eq!(token_admission(100, 101, &window), Err(None));
        assert_eq!(token_admission(100, 1, &[]), Ok(100));
        // Waits are whole seconds within the window.
        assert_eq!(rate_retry_after(0.01), 1);
        assert_eq!(rate_retry_after(-1.0), 1);
        assert_eq!(rate_retry_after(75.0), 60);
    }

    #[test]
    fn plan_names_and_inputs_are_checked() {
        assert!(valid_plan_name("free"));
        assert!(valid_plan_name("pro-2026_q4"));
        assert!(!valid_plan_name(""));
        assert!(!valid_plan_name("-pro"));
        assert!(!valid_plan_name("Pro"));
        assert!(!valid_plan_name(&"a".repeat(64)));
        let now = Utc::now();
        let grant = |amount, expires_at| CreditGrantInput {
            reference: "order-1".into(),
            amount_minor: amount,
            expires_at,
        };
        assert!(grant(1, None).validate().is_ok());
        assert_eq!(grant(0, None).validate(), Err(EndUserInputError::Amount));
        assert!(grant(1, Some(now)).expired(now));
        assert!(!grant(1, None).expired(now));
        let settings = EndUserSettingsInput {
            requirement: EndUserRequirement::Required,
            currency: "usd".into(),
            default_plan: None,
        };
        assert_eq!(settings.validate(), Err(EndUserInputError::Currency));
    }

    #[test]
    fn ids_are_stable_per_service_account_and_name() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(end_user_id(a, "u"), end_user_id(a, "u"));
        assert_ne!(end_user_id(a, "u"), end_user_id(b, "u"));
        assert_ne!(plan_id(a, "u"), end_user_id(a, "u"));
        assert_ne!(grant_id(a, "r"), grant_id(a, "s"));
    }
}
