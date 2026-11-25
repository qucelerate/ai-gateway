//! Price sync: provider prices observed from price sources, and how they
//! compare with the prices an organization's active release bills.
//!
//! Observations keep each published decimal exactly as the source states it,
//! per 1, 1 000 or 1 000 000 tokens. Comparison converts it to money units
//! per token (10⁻⁶ of the currency's smallest unit, see
//! [`crate::domain::accounting::MONEY_SCALE`]) only for currencies whose
//! minor-unit exponent is known, and never across currencies. Nothing here
//! changes a catalog price.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use super::{accounting::Currency, catalog::ProviderFamily, public_prices::AWS_PRICE_LIST_SOURCE};

/// The `format` an operator price sheet declares.
pub const PRICE_SHEET_FORMAT: &str = "qucelerate.price-sheet.v1";
/// Most observations one fetch may record. A public price list prices
/// every item in every region, so it may list many.
pub const MAX_OBSERVATIONS: usize = 50_000;
/// Most prices an operator price sheet may list.
pub const MAX_SHEET_PRICES: usize = 10_000;
/// Appended to the item of a batch price: `gpt 4.1 · global · batch` is the
/// batch price of `gpt 4.1 · global`. Batch operations (`chat_batch`,
/// `embeddings_batch`) are compared only with batch items, and the others
/// never are.
pub const BATCH_ITEM_SUFFIX: &str = " · batch";

/// Whether `item` is a batch price.
pub fn is_batch_item(item: &str) -> bool {
    item.ends_with(BATCH_ITEM_SUFFIX)
}

/// The batch item of `item`, which may already be one.
pub fn batch_item(item: &str) -> String {
    if is_batch_item(item) {
        item.to_owned()
    } else {
        format!("{item}{BATCH_ITEM_SUFFIX}")
    }
}
const MAX_INTEGER_DIGITS: usize = 12;
const MAX_FRACTION_DIGITS: usize = 12;
/// Money units per smallest currency unit, as a power of ten.
const MONEY_SCALE_EXPONENT: u32 = 6;

/// Fixed fetch error codes. A recorded error is one of these, never a URL,
/// response body or provider text.
pub const ERROR_INVALID_URL: &str = "invalid_url";
pub const ERROR_DESTINATION_NOT_ALLOWED: &str = "destination_not_allowed";
pub const ERROR_DNS_FAILED: &str = "dns_failed";
pub const ERROR_CONNECTION_FAILED: &str = "connection_failed";
pub const ERROR_TIMED_OUT: &str = "timed_out";
pub const ERROR_HTTP_STATUS: &str = "http_status";
pub const ERROR_RESPONSE_TOO_LARGE: &str = "response_too_large";
pub const ERROR_REQUEST_FAILED: &str = "request_failed";
pub const ERROR_INVALID_PRICE_SHEET: &str = "invalid_price_sheet";
pub const ERROR_INVALID_PRICE_LIST: &str = "invalid_price_list";

/// Digits after the decimal point of `currency`'s smallest unit (ISO 4217),
/// or `None` for a code not listed here, such as internal credits. Prices in
/// such a currency are recorded but never compared.
pub fn minor_exponent(currency: &str) -> Option<u32> {
    const ZERO: &[&str] = &[
        "BIF", "CLP", "DJF", "GNF", "ISK", "JPY", "KMF", "KRW", "PYG", "RWF", "UGX", "UYI", "VND",
        "VUV", "XAF", "XOF", "XPF",
    ];
    const THREE: &[&str] = &["BHD", "IQD", "JOD", "KWD", "LYD", "OMR", "TND"];
    const FOUR: &[&str] = &["CLF", "UYW"];
    const TWO: &[&str] = &[
        "AED", "ARS", "AUD", "BGN", "BRL", "CAD", "CHF", "CNY", "COP", "CZK", "DKK", "EGP", "EUR",
        "GBP", "HKD", "HUF", "IDR", "ILS", "INR", "KES", "MAD", "MXN", "MYR", "NGN", "NOK", "NZD",
        "PEN", "PHP", "PKR", "PLN", "QAR", "RON", "RSD", "SAR", "SEK", "SGD", "THB", "TRY", "TWD",
        "UAH", "USD", "ZAR",
    ];
    [(ZERO, 0), (TWO, 2), (THREE, 3), (FOUR, 4)]
        .into_iter()
        .find_map(|(codes, exponent)| codes.contains(&currency).then_some(exponent))
}

/// A non-negative decimal price as published, in canonical form: no leading
/// zeros before the point, no trailing zeros after it.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PriceAmount(String);

impl PriceAmount {
    /// Accepts up to 12 digits before and 12 after the point. Exponents,
    /// signs and separators are refused, since a float round trip would
    /// change the price.
    pub fn parse(value: &str) -> Option<Self> {
        let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
        if integer.is_empty()
            || integer.len() > MAX_INTEGER_DIGITS
            || fraction.len() > MAX_FRACTION_DIGITS
            || (value.contains('.') && fraction.is_empty())
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let integer = integer.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        let integer = if integer.is_empty() { "0" } else { integer };
        Some(Self(if fraction.is_empty() {
            integer.to_owned()
        } else {
            format!("{integer}.{fraction}")
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A price published as the plain decimal `value` per `per_tokens`
    /// tokens, restated per million tokens. Public price lists state prices
    /// with more fraction digits than [`PriceAmount::parse`] accepts, and in
    /// different units for one item; per million they fit and compare.
    pub fn per_million(value: &str, per_tokens: PerTokens) -> Option<Self> {
        let (integer, fraction) = value.split_once('.').unwrap_or((value, ""));
        if integer.is_empty()
            || value.len() > 40
            || (value.contains('.') && fraction.is_empty())
            || !integer
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Self::parse(&Self(value.to_owned()).shifted(6 - per_tokens.exponent()))
    }

    /// The price shifted `places` digits left of the point: `2.5` shifted 2
    /// is `250`. Exact, in canonical form.
    fn shifted(&self, places: u32) -> String {
        let (integer, fraction) = self.0.split_once('.').unwrap_or((&self.0, ""));
        let places = places as usize;
        let mut digits = format!("{integer}{fraction}");
        let point = integer.len() + places;
        if digits.len() < point {
            digits.push_str(&"0".repeat(point - digits.len()));
        }
        let (integer, fraction) = digits.split_at(point);
        let integer = integer.trim_start_matches('0');
        let fraction = fraction.trim_end_matches('0');
        let integer = if integer.is_empty() { "0" } else { integer };
        if fraction.is_empty() {
            integer.to_owned()
        } else {
            format!("{integer}.{fraction}")
        }
    }
}

impl<'de> Deserialize<'de> for PriceAmount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| serde::de::Error::custom("invalid price amount"))
    }
}

/// How many tokens a published price is for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(into = "u32", try_from = "u32")]
pub enum PerTokens {
    One,
    Thousand,
    Million,
}

impl PerTokens {
    pub fn new(value: u64) -> Option<Self> {
        match value {
            1 => Some(Self::One),
            1_000 => Some(Self::Thousand),
            1_000_000 => Some(Self::Million),
            _ => None,
        }
    }

    pub fn count(self) -> u32 {
        match self {
            Self::One => 1,
            Self::Thousand => 1_000,
            Self::Million => 1_000_000,
        }
    }

    fn exponent(self) -> u32 {
        match self {
            Self::One => 0,
            Self::Thousand => 3,
            Self::Million => 6,
        }
    }
}

impl TryFrom<u32> for PerTokens {
    type Error = &'static str;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value.into()).ok_or("per_tokens must be 1, 1000 or 1000000")
    }
}

impl From<PerTokens> for u32 {
    fn from(value: PerTokens) -> Self {
        value.count()
    }
}

/// Money units per token of `amount` published per `per_tokens` tokens in a
/// currency with `minor_exponent` digits: exact, and fractional when the
/// price is finer than one unit.
pub fn money_units_per_token(
    amount: &PriceAmount,
    per_tokens: PerTokens,
    minor_exponent: u32,
) -> String {
    amount.shifted(minor_exponent + MONEY_SCALE_EXPONENT - per_tokens.exponent())
}

/// The usage dimensions a catalog price charges per token.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceDimension {
    Input,
    Output,
    CachedInput,
    Reasoning,
    Other,
}

impl PriceDimension {
    pub const ALL: [Self; 5] = [
        Self::Input,
        Self::Output,
        Self::CachedInput,
        Self::Reasoning,
        Self::Other,
    ];

    /// Whether `family`'s adapter reports usage in this dimension, so a
    /// catalog price for it can be billed.
    pub fn reported_by(self, family: ProviderFamily) -> bool {
        match self {
            Self::Input | Self::Output | Self::CachedInput | Self::Other => true,
            Self::Reasoning => {
                !matches!(family, ProviderFamily::Anthropic | ProviderFamily::Bedrock)
            }
        }
    }
}

/// Published per-dimension prices; `None` where the source states none.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Serialize)]
pub struct DimensionPrices {
    pub input: Option<PriceAmount>,
    pub output: Option<PriceAmount>,
    pub cached_input: Option<PriceAmount>,
    pub reasoning: Option<PriceAmount>,
    pub other: Option<PriceAmount>,
}

impl DimensionPrices {
    pub fn get(&self, dimension: PriceDimension) -> Option<&PriceAmount> {
        match dimension {
            PriceDimension::Input => self.input.as_ref(),
            PriceDimension::Output => self.output.as_ref(),
            PriceDimension::CachedInput => self.cached_input.as_ref(),
            PriceDimension::Reasoning => self.reasoning.as_ref(),
            PriceDimension::Other => self.other.as_ref(),
        }
    }

    fn is_empty(&self) -> bool {
        PriceDimension::ALL.iter().all(|d| self.get(*d).is_none())
    }
}

/// One published price. `item` is the source's own identifier for it;
/// `model` is the provider model identifier it prices, or `None` when the
/// source could not be mapped to one with confidence. Unmapped prices are
/// kept and reported, never matched by guesswork.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ObservedPrice {
    pub item: String,
    pub model: Option<String>,
    /// `None` for a price that applies in every region.
    pub region: Option<String>,
    pub currency: String,
    pub per_tokens: PerTokens,
    pub prices: DimensionPrices,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ObservationError {
    #[error("price source lists more than {MAX_OBSERVATIONS} prices")]
    TooMany,
    #[error("price has an invalid identifier, model, region or currency")]
    InvalidField,
    #[error("price states no dimension")]
    Empty,
    #[error("price is listed twice for the same item, region and currency")]
    Duplicate,
    #[error("price source version is invalid")]
    InvalidVersion,
}

/// Everything one fetch of a source observed. Two fetches with the same
/// digest observed the same prices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationSet {
    pub source_version: Option<String>,
    pub observations: Vec<ObservedPrice>,
}

impl ObservationSet {
    pub fn new(
        source_version: Option<String>,
        mut observations: Vec<ObservedPrice>,
    ) -> Result<Self, ObservationError> {
        if observations.len() > MAX_OBSERVATIONS {
            return Err(ObservationError::TooMany);
        }
        if source_version
            .as_deref()
            .is_some_and(|version| !valid_text(version, 128))
        {
            return Err(ObservationError::InvalidVersion);
        }
        let mut items = HashSet::new();
        let mut models = HashSet::new();
        for price in &observations {
            if !valid_text(&price.item, 255)
                || price.model.as_deref().is_some_and(|m| !valid_text(m, 255))
                || price.region.as_deref().is_some_and(|r| !valid_text(r, 255))
                || Currency::new(price.currency.clone()).is_err()
            {
                return Err(ObservationError::InvalidField);
            }
            if price.prices.is_empty() {
                return Err(ObservationError::Empty);
            }
            let fresh_item = items.insert((&price.item, &price.region, &price.currency));
            let fresh_model = price.model.as_ref().is_none_or(|model| {
                models.insert((
                    model,
                    is_batch_item(&price.item),
                    &price.region,
                    &price.currency,
                ))
            });
            if !fresh_item || !fresh_model {
                return Err(ObservationError::Duplicate);
            }
        }
        observations.sort_by(|a, b| {
            (&a.item, &a.region, &a.currency).cmp(&(&b.item, &b.region, &b.currency))
        });
        Ok(Self {
            source_version,
            observations,
        })
    }

    /// SHA-256 of the canonical, ordered observations. The version label is
    /// left out: a relabelled sheet with the same prices is the same snapshot.
    pub fn digest(&self) -> [u8; 32] {
        let canonical = serde_json::json!({ "observations": self.observations });
        Sha256::digest(canonical.to_string().as_bytes()).into()
    }

    pub fn unmapped(&self) -> usize {
        self.observations
            .iter()
            .filter(|p| p.model.is_none())
            .count()
    }
}

/// Identifiers and names: 1..=`max` bytes, no surrounding whitespace, no
/// control characters.
fn valid_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceSheetDocument {
    format: String,
    version: Option<String>,
    prices: Vec<PriceSheetEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceSheetEntry {
    model: String,
    /// The model's batch price rather than its on-demand one.
    #[serde(default)]
    batch: bool,
    region: Option<String>,
    currency: String,
    per_tokens: u64,
    input: Option<PriceAmount>,
    output: Option<PriceAmount>,
    cached_input: Option<PriceAmount>,
    reasoning: Option<PriceAmount>,
    other: Option<PriceAmount>,
}

/// Parses an operator price sheet: a JSON document of `format`
/// [`PRICE_SHEET_FORMAT`], an optional `version`, and `prices`, each naming
/// the provider `model`, an optional `region`, `currency`, `per_tokens`
/// (1, 1000 or 1000000) and decimal strings for any of `input`, `output`,
/// `cached_input`, `reasoning` and `other`. An entry with `"batch": true`
/// is the model's batch price, recorded as its batch item
/// ([`BATCH_ITEM_SUFFIX`]).
pub fn parse_price_sheet(body: &[u8]) -> Result<ObservationSet, ObservationError> {
    let document: PriceSheetDocument =
        serde_json::from_slice(body).map_err(|_| ObservationError::InvalidField)?;
    if document.format != PRICE_SHEET_FORMAT {
        return Err(ObservationError::InvalidField);
    }
    if document.prices.len() > MAX_SHEET_PRICES {
        return Err(ObservationError::TooMany);
    }
    let observations = document
        .prices
        .into_iter()
        .map(|entry| {
            Ok(ObservedPrice {
                item: if entry.batch {
                    batch_item(&entry.model)
                } else {
                    entry.model.clone()
                },
                model: Some(entry.model),
                region: entry.region,
                currency: entry.currency,
                per_tokens: PerTokens::new(entry.per_tokens)
                    .ok_or(ObservationError::InvalidField)?,
                prices: DimensionPrices {
                    input: entry.input,
                    output: entry.output,
                    cached_input: entry.cached_input,
                    reasoning: entry.reasoning,
                    other: entry.other,
                },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    ObservationSet::new(document.version, observations)
}

/// A configured price source and the state of its fetches.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceSourceView {
    pub source: String,
    pub kind: String,
    pub provider_family: String,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error_code: Option<String>,
    pub next_fetch_at: DateTime<Utc>,
    pub snapshot: Option<PriceSnapshotView>,
}

/// The prices a source currently publishes. `fetched_at` is when they were
/// first seen, `confirmed_at` the latest fetch that saw them unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceSnapshotView {
    pub id: Uuid,
    pub source_version: Option<String>,
    pub fetched_at: DateTime<Utc>,
    pub confirmed_at: DateTime<Utc>,
    pub observations: u32,
    pub unmapped: u32,
}

/// How the active release's prices compare with observed prices.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceDrift {
    pub release_id: Uuid,
    pub sources: Vec<PriceSourceView>,
    pub prices: Vec<PriceDriftEntry>,
}

/// Whether a catalog price is the release's own or a price override's.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogPriceSource {
    Release,
    Override,
}

/// One priced deployment operation, as billed now, compared with each
/// source of its provider family.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceDriftEntry {
    pub deployment: String,
    pub operation: String,
    pub model: String,
    pub location: Option<String>,
    pub provider_family: String,
    pub currency: String,
    pub price_source: CatalogPriceSource,
    pub comparisons: Vec<PriceComparison>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonStatus {
    /// Every published, billable dimension equals the catalog price (one
    /// finer than a money unit rounded up, as adopting it bills).
    Matches,
    /// At least one published, billable dimension differs.
    Differs,
    /// The source has no price for the model in the deployment's region or
    /// globally.
    NoObservation,
    /// The source prices the model only in other currencies.
    CurrencyMismatch,
    /// The currency's minor-unit exponent is unknown, so money units cannot
    /// be derived.
    NotComparable,
    /// The source publishes no billable dimension for the model.
    Unpublished,
}

/// How a comparison looks up the published price.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchedBy {
    /// The organization maps the deployment to a published item.
    Mapping,
    /// The source names the deployment's model.
    Model,
    /// The AWS Price List item named after the name the account's listing
    /// gives the Bedrock model (`Claude Sonnet 4.6`, with ` · global` for a
    /// global inference profile), as automatic offers price it; compared
    /// at the higher of its standard and long-context prices.
    ListingName,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceComparison {
    pub source: String,
    pub snapshot_id: Uuid,
    pub status: ComparisonStatus,
    pub matched_by: MatchedBy,
    /// The compared price's item; `null` when none was compared.
    pub observed_item: Option<String>,
    /// The matched price's region; `null` for a global price or no match.
    pub observed_region: Option<String>,
    /// Currencies the source prices the model in, when none is the catalog's.
    pub observed_currencies: Vec<String>,
    /// How many tokens the compared published prices are for.
    pub per_tokens: Option<PerTokens>,
    pub dimensions: Vec<DimensionDrift>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionStatus {
    Matches,
    Differs,
    /// The source states no price for this dimension.
    Unpublished,
    /// The provider family never reports usage in this dimension, so the
    /// gateway cannot bill it.
    NotReported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DimensionDrift {
    pub dimension: PriceDimension,
    /// Money units per token the catalog bills.
    pub catalog: i64,
    /// Observed money units per token, as an exact decimal string; a
    /// fraction means the published price is finer than one unit.
    pub observed: Option<String>,
    /// The published decimal and the tokens it is for.
    pub published: Option<PriceAmount>,
    pub status: DimensionStatus,
}

/// A catalog price as the comparison needs it.
#[derive(Clone, Debug)]
pub struct CatalogPriceRow {
    pub deployment: String,
    pub operation: String,
    pub model: String,
    pub location: Option<String>,
    pub family: ProviderFamily,
    pub currency: String,
    pub price_source: CatalogPriceSource,
    pub per_token: [i64; 5],
    /// For a Bedrock deployment, the AWS Price List items named after its
    /// listing name (standard, long context), as automatic offers price it
    /// (`auto_offer::price_list_items`); compared when it has no mapping at
    /// that source and the source does not name its model.
    pub listing_items: Option<(String, String)>,
}

/// The item a deployment is mapped to at one source, as set.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceMappingInput {
    /// The source's item, as the observation listing shows it.
    pub item: String,
    /// The item's region. Absent prefers the deployment's own region, then
    /// a global price.
    #[serde(default)]
    pub region: Option<String>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PriceMappingError {
    #[error("item must be 1 to 255 bytes without surrounding whitespace or control characters")]
    Item,
    #[error("region must be 1 to 255 bytes without surrounding whitespace or control characters")]
    Region,
}

impl PriceMappingInput {
    pub fn validate(self) -> Result<Self, PriceMappingError> {
        if !valid_text(&self.item, 255) {
            return Err(PriceMappingError::Item);
        }
        if self.region.as_deref().is_some_and(|r| !valid_text(r, 255)) {
            return Err(PriceMappingError::Region);
        }
        Ok(self)
    }
}

/// Whether `name` could name a deployment or a price source.
pub fn valid_name(name: &str) -> bool {
    valid_text(name, 255)
}

/// Which published item prices a deployment at one source. Drift compares
/// the deployment with that item instead of looking its model up.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PriceMappingView {
    pub id: Uuid,
    pub deployment: String,
    pub source: String,
    pub item: String,
    pub region: Option<String>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One price a source currently publishes.
#[derive(Clone, Debug, Serialize)]
pub struct PriceObservationView {
    pub source: String,
    pub snapshot_id: Uuid,
    pub item: String,
    /// `null` when the source does not name the model, as public price
    /// lists do not; map a deployment to the item instead.
    pub model: Option<String>,
    pub region: Option<String>,
    pub currency: String,
    pub per_tokens: PerTokens,
    #[serde(flatten)]
    pub prices: DimensionPrices,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PriceQueryError {
    #[error("source must name a price source")]
    Source,
    #[error("q must be 1 to 255 bytes without surrounding whitespace or control characters")]
    Text,
    #[error("region must be 1 to 255 bytes without surrounding whitespace or control characters")]
    Region,
}

/// Filters for the observation listing: one source, text the item or model
/// contains (ignoring case), and one region.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PriceObservationQuery {
    pub source: Option<String>,
    pub text: Option<String>,
    pub region: Option<String>,
}

impl PriceObservationQuery {
    pub fn new(
        source: Option<String>,
        text: Option<String>,
        region: Option<String>,
    ) -> Result<Self, PriceQueryError> {
        if source.as_deref().is_some_and(|s| !valid_text(s, 65)) {
            return Err(PriceQueryError::Source);
        }
        if text.as_deref().is_some_and(|t| !valid_text(t, 255)) {
            return Err(PriceQueryError::Text);
        }
        if region.as_deref().is_some_and(|r| !valid_text(r, 255)) {
            return Err(PriceQueryError::Region);
        }
        Ok(Self {
            source,
            text,
            region,
        })
    }

    /// The filters as page tokens bind them.
    pub fn canonical(&self) -> String {
        serde_json::json!([self.source, self.text, self.region]).to_string()
    }
}

/// The published item an organization says prices a deployment.
#[derive(Clone, Debug)]
pub struct PriceMappingRow {
    pub deployment: String,
    pub source: String,
    pub item: String,
    pub region: Option<String>,
}

/// A source's current snapshot, with the observations of models the release
/// deploys and of items mapped to its deployments.
#[derive(Clone, Debug)]
pub struct SourceObservations {
    pub source: String,
    pub family: ProviderFamily,
    pub snapshot_id: Uuid,
    pub observations: Vec<ObservedPrice>,
}

/// Compares every catalog price with every source of its family: with the
/// item the deployment is mapped to at that source, otherwise with the
/// source's price for the deployment's model.
pub fn compare(
    prices: &[CatalogPriceRow],
    sources: &[SourceObservations],
    mappings: &[PriceMappingRow],
) -> Vec<PriceDriftEntry> {
    prices
        .iter()
        .map(|price| PriceDriftEntry {
            deployment: price.deployment.clone(),
            operation: price.operation.clone(),
            model: price.model.clone(),
            location: price.location.clone(),
            provider_family: price.family.as_str().to_owned(),
            currency: price.currency.clone(),
            price_source: price.price_source,
            comparisons: sources
                .iter()
                .filter(|source| source.family == price.family)
                .map(|source| {
                    let mapping = mappings.iter().find(|mapping| {
                        mapping.deployment == price.deployment && mapping.source == source.source
                    });
                    match (mapping, listing_name(price, source)) {
                        (None, Some((standard, long_context))) => {
                            compare_listed(price, source, &standard, &long_context)
                        }
                        _ => compare_one(price, source, mapping, None),
                    }
                })
                .collect(),
        })
        .collect()
}

/// What `source` publishes for a deployment of `model` in `location`: the
/// item the deployment is mapped to, otherwise the source's price for the
/// model. With `batch`, the batch price of that item or model
/// ([`BATCH_ITEM_SUFFIX`]); without, never a batch price. A mapping may pin
/// the region; otherwise a price for the deployment's own region wins over a
/// global one. Returns the region the prices are for and one price per
/// currency.
pub fn published_for<'a>(
    source: &'a SourceObservations,
    mapping: Option<&'a PriceMappingRow>,
    model: &str,
    location: Option<&'a str>,
    batch: bool,
) -> (Option<&'a str>, Vec<&'a ObservedPrice>) {
    let mapped = mapping.map(|mapping| {
        if batch {
            batch_item(&mapping.item)
        } else {
            mapping.item.clone()
        }
    });
    let in_region = |region: Option<&str>| -> Vec<&'a ObservedPrice> {
        source
            .observations
            .iter()
            .filter(|o| {
                // A batch price never prices an on-demand operation, even
                // one mapped to the batch item, nor the other way round.
                let priced = is_batch_item(&o.item) == batch
                    && match &mapped {
                        Some(item) => o.item == *item,
                        None => o.model.as_deref() == Some(model),
                    };
                priced && o.region.as_deref() == region
            })
            .collect()
    };
    let region = match mapping.and_then(|mapping| mapping.region.as_deref()) {
        Some(pinned) => Some(pinned),
        None => location.filter(|location| !in_region(Some(location)).is_empty()),
    };
    (region, in_region(region))
}

/// The whole money units per token a catalog bills for an observed price:
/// the price itself, or, finer than one unit, rounded up (as adopting a
/// published price does), so the gateway never charges less than published.
pub fn billable_units(observed: &str) -> Option<i64> {
    let (whole, fraction) = observed.split_once('.').unwrap_or((observed, ""));
    let whole: i64 = whole.parse().ok()?;
    if fraction.bytes().all(|digit| digit == b'0') {
        Some(whole)
    } else {
        whole.checked_add(1)
    }
}

/// The listing-name items to compare `price` with at `source`: only at the
/// AWS Price List, only for a deployment with listing items, and only when
/// the source does not name its model (a model price wins, as it does for
/// automatic offers).
fn listing_name(price: &CatalogPriceRow, source: &SourceObservations) -> Option<(String, String)> {
    let items = price.listing_items.as_ref()?;
    if source.source != AWS_PRICE_LIST_SOURCE {
        return None;
    }
    let batch = price
        .operation
        .parse::<super::catalog::CatalogOperation>()
        .is_ok_and(super::catalog::CatalogOperation::is_batch);
    let (_, by_model) = published_for(source, None, &price.model, price.location.as_deref(), batch);
    by_model.is_empty().then(|| items.clone())
}

/// [`compare_one`] by listing name: the standard item, raised in each
/// dimension to the long-context item's price where that is higher, since a
/// flat price must cover both (as automatic offers price it).
fn compare_listed(
    price: &CatalogPriceRow,
    source: &SourceObservations,
    standard: &str,
    long_context: &str,
) -> PriceComparison {
    let item = |item: &str| PriceMappingRow {
        deployment: price.deployment.clone(),
        source: source.source.clone(),
        item: item.to_owned(),
        region: None,
    };
    let long_context = item(long_context);
    let mut comparison = compare_one(price, source, Some(&item(standard)), Some(&long_context));
    comparison.matched_by = MatchedBy::ListingName;
    comparison
}

/// Whether non-negative decimal `a` is greater than `b`.
fn decimal_gt(a: &str, b: &str) -> bool {
    let split = |value: &str| -> (String, String) {
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let whole = whole.trim_start_matches('0');
        (whole.to_owned(), fraction.trim_end_matches('0').to_owned())
    };
    let ((aw, af), (bw, bf)) = (split(a), split(b));
    if aw.len() != bw.len() {
        return aw.len() > bw.len();
    }
    if aw != bw {
        return aw > bw;
    }
    let width = af.len().max(bf.len());
    format!("{af:0<width$}") > format!("{bf:0<width$}")
}

fn compare_one(
    price: &CatalogPriceRow,
    source: &SourceObservations,
    mapping: Option<&PriceMappingRow>,
    long_context: Option<&PriceMappingRow>,
) -> PriceComparison {
    let mut comparison = PriceComparison {
        source: source.source.clone(),
        snapshot_id: source.snapshot_id,
        status: ComparisonStatus::NoObservation,
        matched_by: if mapping.is_some() {
            MatchedBy::Mapping
        } else {
            MatchedBy::Model
        },
        observed_item: None,
        observed_region: None,
        observed_currencies: Vec::new(),
        per_tokens: None,
        dimensions: Vec::new(),
    };
    // A batch price is compared only with a batch price: against an
    // on-demand one it would always differ, and adopting that would replace
    // the batch discount.
    let batch = price
        .operation
        .parse::<super::catalog::CatalogOperation>()
        .is_ok_and(super::catalog::CatalogOperation::is_batch);
    let (region, candidates) = published_for(
        source,
        mapping,
        &price.model,
        price.location.as_deref(),
        batch,
    );
    if candidates.is_empty() {
        return comparison;
    }
    comparison.observed_region = region.map(str::to_owned);
    let Some(observed) = candidates.iter().find(|o| o.currency == price.currency) else {
        comparison.status = ComparisonStatus::CurrencyMismatch;
        comparison.observed_currencies = candidates.iter().map(|o| o.currency.clone()).collect();
        comparison.observed_currencies.sort();
        comparison.observed_currencies.dedup();
        return comparison;
    };
    comparison.observed_item = Some(observed.item.clone());
    comparison.per_tokens = Some(observed.per_tokens);
    let Some(exponent) = minor_exponent(&price.currency) else {
        comparison.status = ComparisonStatus::NotComparable;
        return comparison;
    };
    // The long-context tier, in the same currency, region and token unit
    // (so `published` stays in the comparison's unit), raises a dimension it
    // prices higher.
    let higher_tier = long_context.and_then(|long_context| {
        let (_, candidates) = published_for(
            source,
            Some(long_context),
            &price.model,
            price.location.as_deref(),
            batch,
        );
        candidates.into_iter().find(|o| {
            o.currency == price.currency
                && o.region == observed.region
                && o.per_tokens == observed.per_tokens
        })
    });
    let mut differs = false;
    let mut compared = false;
    for (index, dimension) in PriceDimension::ALL.into_iter().enumerate() {
        let catalog = price.per_token[index];
        let mut published = observed.prices.get(dimension).cloned();
        let mut units = published
            .as_ref()
            .map(|amount| money_units_per_token(amount, observed.per_tokens, exponent));
        if let Some(tier) = higher_tier {
            let tier_published = tier.prices.get(dimension).cloned();
            let tier_units = tier_published
                .as_ref()
                .map(|amount| money_units_per_token(amount, tier.per_tokens, exponent));
            // Higher, or published only for long context: a flat price must
            // cover it either way.
            let raise = match (&tier_units, &units) {
                (Some(tier), Some(base)) => decimal_gt(tier, base),
                (Some(_), None) => true,
                _ => false,
            };
            if raise {
                units = tier_units;
                published = tier_published;
            }
        }
        let status = match &units {
            _ if !dimension.reported_by(price.family) => DimensionStatus::NotReported,
            None => DimensionStatus::Unpublished,
            Some(units) if billable_units(units) == Some(catalog) => DimensionStatus::Matches,
            Some(_) => DimensionStatus::Differs,
        };
        compared |= matches!(status, DimensionStatus::Matches | DimensionStatus::Differs);
        differs |= status == DimensionStatus::Differs;
        comparison.dimensions.push(DimensionDrift {
            dimension,
            catalog,
            observed: units,
            published,
            status,
        });
    }
    comparison.status = match (compared, differs) {
        (false, _) => ComparisonStatus::Unpublished,
        (true, true) => ComparisonStatus::Differs,
        (true, false) => ComparisonStatus::Matches,
    };
    comparison
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amount(value: &str) -> PriceAmount {
        PriceAmount::parse(value).unwrap()
    }

    #[test]
    fn amounts_are_canonical_and_bounded() {
        assert_eq!(amount("002.500").as_str(), "2.5");
        assert_eq!(amount("0.0").as_str(), "0");
        assert_eq!(amount("10").as_str(), "10");
        for invalid in [
            "", ".", ".5", "1.", "-1", "+1", "1e3", "1,5", " 1", "1 ", "0x1",
        ] {
            assert!(PriceAmount::parse(invalid).is_none(), "{invalid:?}");
        }
        assert!(PriceAmount::parse(&"9".repeat(12)).is_some());
        assert!(PriceAmount::parse(&"9".repeat(13)).is_none());
        assert!(PriceAmount::parse(&format!("0.{}", "1".repeat(13))).is_none());
    }

    #[test]
    fn published_prices_restate_per_million_exactly() {
        let per_million = |value, per| PriceAmount::per_million(value, per).map(|a| a.0);
        assert_eq!(
            per_million("0.0012500000", PerTokens::Thousand).as_deref(),
            Some("1.25")
        );
        assert_eq!(
            per_million("2", PerTokens::Thousand).as_deref(),
            Some("2000")
        );
        assert_eq!(
            per_million("0.0000000125", PerTokens::One).as_deref(),
            Some("0.0125")
        );
        assert_eq!(
            per_million("2.5", PerTokens::Million).as_deref(),
            Some("2.5")
        );
        for invalid in ["", "1.", "-1", "1e-7", "0x1", " 1"] {
            assert!(PriceAmount::per_million(invalid, PerTokens::One).is_none());
        }
        // Too fine to state per million within 12 fraction digits.
        assert!(PriceAmount::per_million("0.0000000000000000001", PerTokens::One).is_none());
    }

    #[test]
    fn money_units_follow_the_currency_exponent() {
        let usd = minor_exponent("USD").unwrap();
        assert_eq!(
            money_units_per_token(&amount("2.50"), PerTokens::Million, usd),
            "250"
        );
        assert_eq!(
            money_units_per_token(&amount("0.15"), PerTokens::Million, usd),
            "15"
        );
        assert_eq!(
            money_units_per_token(&amount("0.075"), PerTokens::Million, usd),
            "7.5"
        );
        assert_eq!(
            money_units_per_token(&amount("0.003"), PerTokens::Thousand, usd),
            "300"
        );
        assert_eq!(
            money_units_per_token(&amount("0.0000025"), PerTokens::One, usd),
            "250"
        );
        assert_eq!(
            money_units_per_token(&amount("0"), PerTokens::Million, usd),
            "0"
        );
        let jpy = minor_exponent("JPY").unwrap();
        assert_eq!(
            money_units_per_token(&amount("375"), PerTokens::Million, jpy),
            "375"
        );
        let kwd = minor_exponent("KWD").unwrap();
        assert_eq!(
            money_units_per_token(&amount("0.77"), PerTokens::Million, kwd),
            "770"
        );
        assert_eq!(minor_exponent("CRD"), None);
    }

    fn sheet(prices: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "format": PRICE_SHEET_FORMAT, "version": "2026-09", "prices": prices,
        }))
        .unwrap()
    }

    #[test]
    fn price_sheets_parse_strictly() {
        let set = parse_price_sheet(&sheet(serde_json::json!([
            {"model": "gpt-4o", "currency": "USD", "per_tokens": 1000000, "input": "2.50", "output": "10"},
            {"model": "gpt-4o", "region": "eastus", "currency": "EUR", "per_tokens": 1000000, "input": "2.30"},
        ])))
        .unwrap();
        assert_eq!(set.source_version.as_deref(), Some("2026-09"));
        assert_eq!(set.observations.len(), 2);
        assert_eq!(set.unmapped(), 0);
        let reordered = parse_price_sheet(&sheet(serde_json::json!([
            {"model": "gpt-4o", "region": "eastus", "currency": "EUR", "per_tokens": 1000000, "input": "2.3"},
            {"model": "gpt-4o", "currency": "USD", "per_tokens": 1000000, "output": "10.0", "input": "2.5"},
        ])))
        .unwrap();
        assert_eq!(set.digest(), reordered.digest());

        for (prices, error) in [
            (
                serde_json::json!([{"model": "m", "currency": "USD", "per_tokens": 100, "input": "1"}]),
                ObservationError::InvalidField,
            ),
            (
                serde_json::json!([{"model": "m", "currency": "usd", "per_tokens": 1, "input": "1"}]),
                ObservationError::InvalidField,
            ),
            (
                serde_json::json!([{"model": "m", "currency": "USD", "per_tokens": 1, "input": 1.5}]),
                ObservationError::InvalidField,
            ),
            (
                serde_json::json!([{"model": "m", "currency": "USD", "per_tokens": 1, "extra": "1"}]),
                ObservationError::InvalidField,
            ),
            (
                serde_json::json!([{"model": "m", "currency": "USD", "per_tokens": 1}]),
                ObservationError::Empty,
            ),
            (
                serde_json::json!([
                    {"model": "m", "currency": "USD", "per_tokens": 1, "input": "1"},
                    {"model": "m", "currency": "USD", "per_tokens": 1000, "input": "1"},
                ]),
                ObservationError::Duplicate,
            ),
        ] {
            assert_eq!(parse_price_sheet(&sheet(prices)).unwrap_err(), error);
        }
        let wrong_format = serde_json::to_vec(&serde_json::json!({
            "format": "other", "prices": [],
        }))
        .unwrap();
        assert!(parse_price_sheet(&wrong_format).is_err());
    }

    fn unmapped(
        prices: &[CatalogPriceRow],
        sources: &[SourceObservations],
    ) -> Vec<PriceDriftEntry> {
        compare(prices, sources, &[])
    }

    fn catalog(
        family: ProviderFamily,
        location: Option<&str>,
        per_token: [i64; 5],
    ) -> CatalogPriceRow {
        CatalogPriceRow {
            deployment: "chat".into(),
            operation: "chat".into(),
            model: "m".into(),
            location: location.map(str::to_owned),
            family,
            currency: "USD".into(),
            price_source: CatalogPriceSource::Release,
            per_token,
            listing_items: None,
        }
    }

    fn observed(
        region: Option<&str>,
        currency: &str,
        input: &str,
        output: Option<&str>,
    ) -> ObservedPrice {
        ObservedPrice {
            item: format!("m/{region:?}/{currency}"),
            model: Some("m".into()),
            region: region.map(str::to_owned),
            currency: currency.into(),
            per_tokens: PerTokens::Million,
            prices: DimensionPrices {
                input: Some(amount(input)),
                output: output.map(amount),
                ..DimensionPrices::default()
            },
        }
    }

    fn source(family: ProviderFamily, observations: Vec<ObservedPrice>) -> SourceObservations {
        SourceObservations {
            source: "price-sheet:x".into(),
            family,
            snapshot_id: Uuid::nil(),
            observations,
        }
    }

    #[test]
    fn bedrock_prices_compare_by_listing_name_at_the_higher_tier() {
        let family = ProviderFamily::Bedrock;
        let item = |item: &str, input: &str, output: &str| ObservedPrice {
            item: item.into(),
            model: None,
            region: None,
            currency: "USD".into(),
            per_tokens: PerTokens::Million,
            prices: DimensionPrices {
                input: Some(amount(input)),
                output: Some(amount(output)),
                ..DimensionPrices::default()
            },
        };
        let aws = |observations| SourceObservations {
            source: AWS_PRICE_LIST_SOURCE.into(),
            ..source(family, observations)
        };
        let named = CatalogPriceRow {
            listing_items: Some((
                "Claude Sonnet 4.6 · global".into(),
                "Claude Sonnet 4.6 · global · long context".into(),
            )),
            ..catalog(family, None, [300, 1500, 0, 0, 0])
        };
        // Long context costs more input, the same output: input compares
        // at the long-context price.
        let sources = [aws(vec![
            item("Claude Sonnet 4.6 · global", "3", "15"),
            item("Claude Sonnet 4.6 · global · long context", "6", "15"),
            item("Claude Sonnet 4.6", "3.3", "16.5"),
        ])];
        let comparison = &unmapped(std::slice::from_ref(&named), &sources)[0].comparisons[0];
        assert_eq!(comparison.matched_by, MatchedBy::ListingName);
        assert_eq!(
            comparison.observed_item.as_deref(),
            Some("Claude Sonnet 4.6 · global")
        );
        assert_eq!(comparison.status, ComparisonStatus::Differs);
        assert_eq!(comparison.dimensions[0].observed.as_deref(), Some("600"));
        assert_eq!(comparison.dimensions[0].status, DimensionStatus::Differs);
        assert_eq!(comparison.dimensions[1].status, DimensionStatus::Matches);

        // A dimension only the long-context tier publishes is compared at
        // it, so a change to it is still adopted.
        let tiered = |cached: Option<&str>| {
            let mut long = item("Claude Sonnet 4.6 · global · long context", "3", "15");
            long.prices.cached_input = cached.map(amount);
            [aws(vec![
                item("Claude Sonnet 4.6 · global", "3", "15"),
                long,
            ])]
        };
        let cached_catalog = CatalogPriceRow {
            per_token: [300, 1500, 60, 0, 0],
            ..named.clone()
        };
        let comparison = &unmapped(std::slice::from_ref(&cached_catalog), &tiered(Some("0.6")))[0]
            .comparisons[0];
        assert_eq!(comparison.dimensions[2].status, DimensionStatus::Matches);
        let comparison = &unmapped(std::slice::from_ref(&cached_catalog), &tiered(Some("0.75")))[0]
            .comparisons[0];
        assert_eq!(comparison.dimensions[2].observed.as_deref(), Some("75"));
        assert_eq!(comparison.dimensions[2].status, DimensionStatus::Differs);

        // Without a long-context tier, the standard price.
        let sources = [aws(vec![item("Claude Sonnet 4.6 · global", "3", "15")])];
        let comparison = &unmapped(std::slice::from_ref(&named), &sources)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Matches);

        // A price for the model itself wins, as it does for offers.
        let sources = [aws(vec![
            item("Claude Sonnet 4.6 · global", "9", "99"),
            ObservedPrice {
                model: Some("m".into()),
                ..item("m", "3", "15")
            },
        ])];
        let comparison = &unmapped(std::slice::from_ref(&named), &sources)[0].comparisons[0];
        assert_eq!(comparison.matched_by, MatchedBy::Model);
        assert_eq!(comparison.status, ComparisonStatus::Matches);

        // So does a mapping.
        let mapping = PriceMappingRow {
            deployment: "chat".into(),
            source: AWS_PRICE_LIST_SOURCE.into(),
            item: "Claude Sonnet 4.6".into(),
            region: None,
        };
        let sources = [aws(vec![
            item("Claude Sonnet 4.6 · global", "3", "15"),
            item("Claude Sonnet 4.6", "3.3", "16.5"),
        ])];
        let comparison =
            &compare(std::slice::from_ref(&named), &sources, &[mapping])[0].comparisons[0];
        assert_eq!(comparison.matched_by, MatchedBy::Mapping);
        assert_eq!(comparison.dimensions[0].observed.as_deref(), Some("330"));

        // Other sources never compare by listing name.
        let sources = [source(
            family,
            vec![item("Claude Sonnet 4.6 · global", "3", "15")],
        )];
        let comparison = &unmapped(std::slice::from_ref(&named), &sources)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::NoObservation);
        assert_eq!(comparison.matched_by, MatchedBy::Model);
    }

    #[test]
    fn batch_prices_compare_by_listing_name_only_with_batch_items() {
        let family = ProviderFamily::Bedrock;
        let item = |item: &str, input: &str, output: &str| ObservedPrice {
            item: item.into(),
            model: None,
            region: None,
            currency: "USD".into(),
            per_tokens: PerTokens::Million,
            prices: DimensionPrices {
                input: Some(amount(input)),
                output: Some(amount(output)),
                ..DimensionPrices::default()
            },
        };
        let sources = [SourceObservations {
            source: AWS_PRICE_LIST_SOURCE.into(),
            ..source(
                family,
                vec![
                    item("Claude Sonnet 4.6", "3", "15"),
                    item("Claude Sonnet 4.6 · batch", "1.5", "7.5"),
                    item("Claude Sonnet 4.6 · long context · batch", "3", "7.5"),
                ],
            )
        }];
        let row = |operation: &str, per_token| CatalogPriceRow {
            operation: operation.into(),
            listing_items: Some((
                "Claude Sonnet 4.6".into(),
                "Claude Sonnet 4.6 · long context".into(),
            )),
            ..catalog(family, None, per_token)
        };
        let entries = unmapped(
            &[
                row("chat", [300, 1500, 0, 0, 0]),
                row("chat_batch", [150, 750, 0, 0, 0]),
            ],
            &sources,
        );
        // On demand: the on-demand item, never the batch one.
        let on_demand = &entries[0].comparisons[0];
        assert_eq!(on_demand.matched_by, MatchedBy::ListingName);
        assert_eq!(
            on_demand.observed_item.as_deref(),
            Some("Claude Sonnet 4.6")
        );
        assert_eq!(on_demand.status, ComparisonStatus::Matches);
        // Batch: the batch item, raised to its long-context batch price.
        let batch = &entries[1].comparisons[0];
        assert_eq!(batch.matched_by, MatchedBy::ListingName);
        assert_eq!(
            batch.observed_item.as_deref(),
            Some("Claude Sonnet 4.6 · batch")
        );
        assert_eq!(batch.dimensions[0].observed.as_deref(), Some("300"));
        assert_eq!(batch.dimensions[0].status, DimensionStatus::Differs);
        assert_eq!(batch.dimensions[1].status, DimensionStatus::Matches);
    }

    #[test]
    fn decimals_compare_by_value() {
        assert!(decimal_gt("600", "300"));
        assert!(decimal_gt("1000", "999.99"));
        assert!(decimal_gt("0.5", "0.25"));
        assert!(!decimal_gt("3.30", "3.3"));
        assert!(!decimal_gt("007", "7"));
        assert!(decimal_gt("10", "9"));
    }

    #[test]
    fn comparisons_match_region_currency_and_dimensions() {
        let family = ProviderFamily::OpenAi;
        let global = observed(None, "USD", "2.50", Some("10"));
        let regional = observed(Some("eastus"), "USD", "2.75", Some("10"));
        let sources = [source(family, vec![global, regional])];

        let entry = &unmapped(&[catalog(family, None, [250, 1000, 0, 0, 0])], &sources)[0];
        let comparison = &entry.comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Matches);
        assert_eq!(comparison.observed_region, None);
        assert_eq!(
            comparison.dimensions[2].status,
            DimensionStatus::Unpublished
        );

        let entry = &unmapped(
            &[catalog(family, Some("eastus"), [250, 1000, 0, 0, 0])],
            &sources,
        )[0];
        let comparison = &entry.comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Differs);
        assert_eq!(comparison.observed_region.as_deref(), Some("eastus"));
        assert_eq!(comparison.dimensions[0].observed.as_deref(), Some("275"));
        assert_eq!(comparison.dimensions[0].status, DimensionStatus::Differs);
        assert_eq!(comparison.per_tokens, Some(PerTokens::Million));

        // Another region falls back to the global price.
        let entry = &unmapped(
            &[catalog(family, Some("westus"), [250, 1000, 0, 0, 0])],
            &sources,
        )[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::Matches);

        // A price finer than one unit matches only its rounding up, which is
        // what adopting it bills: rounded down, it would charge too little.
        let fine = [source(family, vec![observed(None, "USD", "0.075", None)])];
        let entry = &unmapped(&[catalog(family, None, [7, 0, 0, 0, 0])], &fine)[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::Differs);
        assert_eq!(
            entry.comparisons[0].dimensions[0].observed.as_deref(),
            Some("7.5")
        );
        let entry = &unmapped(&[catalog(family, None, [8, 0, 0, 0, 0])], &fine)[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::Matches);
        assert_eq!(
            entry.comparisons[0].dimensions[0].observed.as_deref(),
            Some("7.5")
        );
        let entry = &unmapped(&[catalog(family, None, [9, 0, 0, 0, 0])], &fine)[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::Differs);
    }

    #[test]
    fn batch_prices_are_never_compared_with_on_demand_ones() {
        let family = ProviderFamily::OpenAi;
        let sources = [source(
            family,
            vec![observed(None, "USD", "2.50", Some("10"))],
        )];
        for operation in ["chat_batch", "embeddings_batch"] {
            let mut batch = catalog(family, None, [125, 500, 0, 0, 0]);
            batch.operation = operation.into();
            let entry = &unmapped(&[batch], &sources)[0];
            assert_eq!(entry.comparisons[0].status, ComparisonStatus::NoObservation);
            assert!(entry.comparisons[0].dimensions.is_empty());
        }
    }

    #[test]
    fn batch_prices_are_compared_with_published_batch_prices() {
        let family = ProviderFamily::OpenAi;
        let on_demand = observed(None, "USD", "2.50", Some("10"));
        let batch_price = ObservedPrice {
            item: batch_item("m"),
            ..observed(None, "USD", "1.25", Some("5"))
        };
        let sources = [source(family, vec![on_demand, batch_price])];
        let mut batch = catalog(family, None, [125, 500, 0, 0, 0]);
        batch.operation = "chat_batch".into();
        let comparison = &unmapped(std::slice::from_ref(&batch), &sources)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Matches);
        assert_eq!(comparison.observed_item.as_deref(), Some("m · batch"));
        // The on-demand price is still compared with the on-demand item.
        let chat = catalog(family, None, [125, 500, 0, 0, 0]);
        let comparison = &unmapped(&[chat], &sources)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Differs);
        assert_eq!(comparison.observed_item.as_deref(), Some("m/None/USD"));

        // A mapping to the on-demand item prices the batch operation from
        // its batch item, and so does a mapping to the batch item itself.
        let items = [
            ObservedPrice {
                item: "gpt 4o · global".into(),
                model: None,
                ..observed(None, "USD", "2.50", Some("10"))
            },
            ObservedPrice {
                item: "gpt 4o · global · batch".into(),
                model: None,
                ..observed(None, "USD", "1.25", Some("5"))
            },
        ];
        let sources = [source(family, items.to_vec())];
        for item in ["gpt 4o · global", "gpt 4o · global · batch"] {
            let mapped = [PriceMappingRow {
                deployment: "chat".into(),
                source: "price-sheet:x".into(),
                item: item.into(),
                region: None,
            }];
            let comparison =
                &compare(std::slice::from_ref(&batch), &sources, &mapped)[0].comparisons[0];
            assert_eq!(comparison.status, ComparisonStatus::Matches, "{item}");
            assert_eq!(
                comparison.observed_item.as_deref(),
                Some("gpt 4o · global · batch")
            );
        }
        // An on-demand price mapped to the batch item is not compared with
        // the discount.
        let mapped = [PriceMappingRow {
            deployment: "chat".into(),
            source: "price-sheet:x".into(),
            item: "gpt 4o · global · batch".into(),
            region: None,
        }];
        let chat = catalog(family, None, [125, 500, 0, 0, 0]);
        let comparison = &compare(&[chat], &sources, &mapped)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::NoObservation);
    }

    #[test]
    fn price_sheets_publish_batch_prices_beside_on_demand_ones() {
        let body = serde_json::to_vec(&serde_json::json!({
            "format": PRICE_SHEET_FORMAT,
            "prices": [
                {"model": "m", "currency": "USD", "per_tokens": 1000000, "input": "3"},
                {"model": "m", "batch": true, "currency": "USD", "per_tokens": 1000000, "input": "1.5"},
            ],
        }))
        .unwrap();
        let set = parse_price_sheet(&body).unwrap();
        let items: Vec<_> = set.observations.iter().map(|o| o.item.as_str()).collect();
        assert_eq!(items, ["m", "m · batch"]);
        assert!(
            set.observations
                .iter()
                .all(|o| o.model.as_deref() == Some("m"))
        );
    }

    #[test]
    fn comparisons_report_what_cannot_be_compared() {
        let family = ProviderFamily::Anthropic;
        let eur = [source(family, vec![observed(None, "EUR", "3", None)])];
        let entry = &unmapped(&[catalog(family, None, [300, 0, 0, 0, 0])], &eur)[0];
        assert_eq!(
            entry.comparisons[0].status,
            ComparisonStatus::CurrencyMismatch
        );
        assert_eq!(entry.comparisons[0].observed_currencies, ["EUR"]);

        let none = [source(family, vec![])];
        let entry = &unmapped(&[catalog(family, None, [300, 0, 0, 0, 0])], &none)[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::NoObservation);

        let other_family = [source(ProviderFamily::OpenAi, vec![])];
        assert!(
            unmapped(&[catalog(family, None, [0; 5])], &other_family)[0]
                .comparisons
                .is_empty()
        );

        let mut credits = catalog(family, None, [300, 0, 0, 0, 0]);
        credits.currency = "CRD".into();
        let crd = [source(family, vec![observed(None, "CRD", "3", None)])];
        let entry = &unmapped(&[credits], &crd)[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::NotComparable);

        // Anthropic never reports reasoning (thinking is output), so a
        // published reasoning price is shown but does not count as drift.
        let mut reasoning = observed(None, "USD", "3", Some("15"));
        reasoning.prices.reasoning = Some(amount("15"));
        let entry = &unmapped(
            &[catalog(family, None, [300, 1500, 0, 0, 0])],
            &[source(family, vec![reasoning])],
        )[0];
        assert_eq!(entry.comparisons[0].status, ComparisonStatus::Matches);
        assert_eq!(
            entry.comparisons[0].dimensions[3].status,
            DimensionStatus::NotReported
        );
        assert_eq!(
            entry.comparisons[0].dimensions[3].observed.as_deref(),
            Some("1500")
        );
    }

    #[test]
    fn mappings_select_the_published_item() {
        let family = ProviderFamily::AzureOpenAi;
        let item = |name: &str, region: Option<&str>, input: &str| ObservedPrice {
            item: name.into(),
            model: None,
            region: region.map(str::to_owned),
            currency: "USD".into(),
            per_tokens: PerTokens::Million,
            prices: DimensionPrices {
                input: Some(amount(input)),
                ..DimensionPrices::default()
            },
        };
        let sources = [SourceObservations {
            source: "azure-retail:azure-openai".into(),
            ..source(
                family,
                vec![
                    item("gpt 4o · global", Some("eastus"), "2.5"),
                    item("gpt 4o · global", Some("westeurope"), "2.75"),
                    item("gpt 4o · regional", Some("eastus"), "2.75"),
                ],
            )
        }];
        let price = catalog(family, Some("eastus"), [250, 0, 0, 0, 0]);
        // Unmapped, nothing names the model.
        let comparison = &unmapped(std::slice::from_ref(&price), &sources)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::NoObservation);
        assert_eq!(comparison.matched_by, MatchedBy::Model);

        let mapping = |item: &str, region: Option<&str>| PriceMappingRow {
            deployment: "chat".into(),
            source: "azure-retail:azure-openai".into(),
            item: item.into(),
            region: region.map(str::to_owned),
        };
        // The deployment's own region.
        let mapped = [mapping("gpt 4o · global", None)];
        let comparison =
            &compare(std::slice::from_ref(&price), &sources, &mapped)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Matches);
        assert_eq!(comparison.matched_by, MatchedBy::Mapping);
        assert_eq!(comparison.observed_item.as_deref(), Some("gpt 4o · global"));
        assert_eq!(comparison.observed_region.as_deref(), Some("eastus"));
        // A pinned region.
        let pinned = [mapping("gpt 4o · global", Some("westeurope"))];
        let comparison =
            &compare(std::slice::from_ref(&price), &sources, &pinned)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::Differs);
        assert_eq!(comparison.observed_region.as_deref(), Some("westeurope"));
        // No global price to fall back on.
        let global = catalog(family, None, [250, 0, 0, 0, 0]);
        let comparison = &compare(&[global], &sources, &mapped)[0].comparisons[0];
        assert_eq!(comparison.status, ComparisonStatus::NoObservation);
        assert_eq!(comparison.observed_item, None);
        // A mapping for another source or deployment does not apply.
        let elsewhere = [PriceMappingRow {
            deployment: "other".into(),
            ..mapping("gpt 4o · global", None)
        }];
        let comparison = &compare(&[price], &sources, &elsewhere)[0].comparisons[0];
        assert_eq!(comparison.matched_by, MatchedBy::Model);
    }
}
