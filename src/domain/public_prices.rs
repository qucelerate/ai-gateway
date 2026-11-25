//! Public price lists: the Azure Retail Prices API for Azure OpenAI, and the
//! AWS Price List for Bedrock.
//!
//! Both price products, not models: a meter or usage type prices one
//! dimension of one product, deployment type and region. On-demand and
//! batch token prices are kept, a batch price as the product's batch item
//! ([`BATCH_ITEM_SUFFIX`]); fine-tuning, provisioned and priority capacity
//! are not what the gateway bills. The meters of one product are
//! grouped into an item with a stable name, restated per million tokens,
//! and recorded without a model: an organization maps a deployment to the
//! item that prices it. A product whose meters disagree is left out rather
//! than guessed at.
//!
//! Bedrock publishes two offers. `AmazonBedrock` prices Amazon's own models
//! by usage type (`USE1-NovaPro-input-tokens`); `AmazonBedrockFoundationModels`
//! prices partner models (Anthropic, Cohere, Writer, …) under the name the
//! Bedrock model listing gives them (`Claude Sonnet 4.6`), so automatic
//! offers can find a listed model's item by that name.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, value::RawValue};

use super::pricing::{
    BATCH_ITEM_SUFFIX, DimensionPrices, MAX_OBSERVATIONS, ObservationError, ObservationSet,
    ObservedPrice, PerTokens, PriceAmount, PriceDimension,
};

/// Most regions read from the AWS region index.
pub const MAX_AWS_REGIONS: usize = 64;
/// The source both Bedrock offers are recorded under.
pub const AWS_PRICE_LIST_SOURCE: &str = "aws-price-list:bedrock";
/// Appended to a foundation model's item for global cross-region usage.
pub const AWS_GLOBAL_SUFFIX: &str = " · global";
/// Appended to a foundation model's item for the tier above its standard
/// context length.
pub const AWS_LONG_CONTEXT_SUFFIX: &str = " · long context";
/// Longest model name taken from a foundation model offer.
const MAX_AWS_MODEL_NAME: usize = 200;

/// A Bedrock offer in the AWS Price List.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AwsOffer {
    /// Amazon's own models, priced by usage type.
    Bedrock,
    /// Partner models, priced under their Bedrock model name.
    FoundationModels,
}

impl AwsOffer {
    pub const ALL: [Self; 2] = [Self::Bedrock, Self::FoundationModels];

    fn prefix(self) -> &'static str {
        match self {
            Self::Bedrock => "/offers/v1.0/aws/AmazonBedrock/",
            Self::FoundationModels => "/offers/v1.0/aws/AmazonBedrockFoundationModels/",
        }
    }

    /// The path of the offer's region index.
    pub fn region_index(self) -> String {
        format!("{}current/region_index.json", self.prefix())
    }
}

/// One page of the Azure Retail Prices API.
pub struct AzurePage {
    /// The next page's URL as the API states it; the caller checks where it
    /// points before following it.
    pub next_page: Option<String>,
}

#[derive(Deserialize)]
struct AzurePageDocument {
    #[serde(rename = "Items")]
    items: Vec<AzureMeter>,
    #[serde(rename = "NextPageLink")]
    next_page_link: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureMeter {
    currency_code: String,
    tier_minimum_units: f64,
    /// Kept as sent: a float would not hold every published decimal.
    retail_price: Box<RawValue>,
    #[serde(default)]
    arm_region_name: String,
    meter_name: String,
    unit_of_measure: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    effective_start_date: String,
}

/// Prices collected across pages or regions. The latest effective price of
/// each dimension wins; an item whose dimension has two different prices
/// effective at once is dropped.
#[derive(Default)]
pub struct PriceCollector {
    items: HashMap<(String, Option<String>, String), [Option<Collected>; 4]>,
}

struct Collected {
    amount: PriceAmount,
    effective: String,
    conflicting: bool,
}

impl PriceCollector {
    fn add(
        &mut self,
        item: String,
        region: Option<String>,
        currency: String,
        dimension: PriceDimension,
        amount: PriceAmount,
        effective: String,
    ) -> Result<(), ObservationError> {
        let slot = match dimension {
            PriceDimension::Input => 0,
            PriceDimension::Output => 1,
            PriceDimension::CachedInput => 2,
            PriceDimension::Other => 3,
            PriceDimension::Reasoning => return Ok(()),
        };
        let key = (item, region, currency);
        if !self.items.contains_key(&key) && self.items.len() >= MAX_OBSERVATIONS {
            return Err(ObservationError::TooMany);
        }
        let entry = &mut self.items.entry(key).or_default()[slot];
        match entry {
            Some(current) if current.effective > effective => {}
            Some(current) if current.effective == effective => {
                current.conflicting |= current.amount != amount;
            }
            _ => {
                *entry = Some(Collected {
                    amount,
                    effective,
                    conflicting: false,
                });
            }
        }
        Ok(())
    }

    /// The collected items, labelled `source_version`.
    pub fn finish(
        self,
        source_version: Option<String>,
    ) -> Result<ObservationSet, ObservationError> {
        let observations = self
            .items
            .into_iter()
            .filter(|(_, slots)| !slots.iter().flatten().any(|c| c.conflicting))
            .map(
                |((item, region, currency), [input, output, cached_input, other])| {
                    let amount = |slot: Option<Collected>| slot.map(|c| c.amount);
                    ObservedPrice {
                        item,
                        model: None,
                        region,
                        currency,
                        per_tokens: PerTokens::Million,
                        prices: DimensionPrices {
                            input: amount(input),
                            output: amount(output),
                            cached_input: amount(cached_input),
                            reasoning: None,
                            other: amount(other),
                        },
                    }
                },
            )
            .collect();
        ObservationSet::new(source_version, observations)
    }
}

/// Reads one page of Azure OpenAI meters into `collector`.
pub fn parse_azure_page(
    body: &[u8],
    collector: &mut PriceCollector,
) -> Result<AzurePage, ObservationError> {
    let document: AzurePageDocument =
        serde_json::from_slice(body).map_err(|_| ObservationError::InvalidField)?;
    for meter in document.items {
        let Some((item, dimension)) = azure_item(&meter.meter_name) else {
            continue;
        };
        let per_tokens = match meter.unit_of_measure.as_str() {
            "1" => PerTokens::One,
            "1K" => PerTokens::Thousand,
            "1M" => PerTokens::Million,
            _ => continue,
        };
        if meter.kind != "Consumption"
            || meter.tier_minimum_units != 0.0
            || !is_currency(&meter.currency_code)
        {
            continue;
        }
        let Some(amount) = plain_decimal(meter.retail_price.get())
            .and_then(|price| PriceAmount::per_million(&price, per_tokens))
        else {
            continue;
        };
        let region = Some(meter.arm_region_name).filter(|region| !region.is_empty());
        collector.add(
            item,
            region,
            meter.currency_code,
            dimension,
            amount,
            meter.effective_start_date,
        )?;
    }
    Ok(AzurePage {
        next_page: document.next_page_link.filter(|link| !link.is_empty()),
    })
}

/// The item and dimension an Azure OpenAI meter prices, e.g. `gpt 4.1 Inp
/// regnl Tokens` is the input of `gpt 4.1 · regional`, and `gpt 4.1 Inp
/// Batch glbl Tokens` the input of `gpt 4.1 · global · batch`. `None` for a
/// meter that is not an on-demand or batch token price, or whose name is
/// ambiguous.
fn azure_item(meter_name: &str) -> Option<(String, PriceDimension)> {
    let name = meter_name.strip_suffix(" Tokens")?.to_lowercase();
    let mut tokens: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|token| !token.is_empty())
        .collect();
    // "Data Zone" is one deployment type.
    let mut merged = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "data" && tokens.get(index + 1) == Some(&"zone") {
            merged.push("datazone");
            index += 2;
        } else {
            merged.push(tokens[index]);
            index += 1;
        }
    }
    tokens = merged;
    if tokens.iter().any(|token| {
        matches!(
            *token,
            "ft" | "tuned" | "training" | "grdr" | "grader" | "dev"
        )
    }) {
        return None;
    }
    let batch = tokens.iter().filter(|token| **token == "batch").count();
    if batch > 1
        || tokens
            .iter()
            .any(|token| token.starts_with("batch") && *token != "batch")
    {
        return None;
    }
    tokens.retain(|token| *token != "batch");
    let (mut input, mut output, mut cached) = (false, false, false);
    let mut deployments = Vec::new();
    let mut rest = Vec::new();
    for token in tokens {
        match token {
            "inp" | "input" | "inpt" => input = true,
            "outp" | "output" | "opt" | "outpt" => output = true,
            "cached" | "cchd" => cached = true,
            "glbl" | "global" => deployments.push("global"),
            "regnl" | "regional" | "rgnl" | "regn" => deployments.push("regional"),
            "dzone" | "dz" | "datazone" | "dzn" => deployments.push("data zone"),
            other => rest.push(other),
        }
    }
    let dimension = match (input, output, cached) {
        (true, false, false) => PriceDimension::Input,
        (false, true, false) => PriceDimension::Output,
        (true, false, true) => PriceDimension::CachedInput,
        (false, false, false) if rest.iter().any(|token| token.contains("embedding")) => {
            PriceDimension::Input
        }
        _ => return None,
    };
    if rest.is_empty() || deployments.len() > 1 {
        return None;
    }
    let mut item = rest.join(" ");
    if let Some(deployment) = deployments.first() {
        item = format!("{item} · {deployment}");
    }
    if batch == 1 {
        item.push_str(BATCH_ITEM_SUFFIX);
    }
    Some((item, dimension))
}

#[derive(Deserialize)]
struct RegionIndexDocument {
    #[serde(rename = "publicationDate")]
    publication_date: Option<String>,
    regions: HashMap<String, RegionEntry>,
}

#[derive(Deserialize)]
struct RegionEntry {
    #[serde(rename = "currentVersionUrl")]
    current_version_url: String,
}

/// The AWS region index: its publication date, and the path of each
/// region's current offer, in order.
pub struct RegionIndex {
    pub publication_date: Option<String>,
    pub offer_paths: Vec<String>,
}

/// Reads `offer`'s region index; every path it lists must be one of that
/// offer's.
pub fn parse_aws_region_index(
    body: &[u8],
    offer: AwsOffer,
) -> Result<RegionIndex, ObservationError> {
    let document: RegionIndexDocument =
        serde_json::from_slice(body).map_err(|_| ObservationError::InvalidField)?;
    if document.regions.len() > MAX_AWS_REGIONS {
        return Err(ObservationError::TooMany);
    }
    let mut offer_paths = document
        .regions
        .into_values()
        .map(|region| {
            let path = region.current_version_url;
            let valid = path.starts_with(offer.prefix())
                && path.ends_with("/index.json")
                && !path.contains("..")
                && !path.contains("//")
                && path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_'));
            valid.then_some(path).ok_or(ObservationError::InvalidField)
        })
        .collect::<Result<Vec<_>, _>>()?;
    offer_paths.sort();
    Ok(RegionIndex {
        publication_date: document
            .publication_date
            .filter(|date| !date.is_empty() && date.len() <= 128),
        offer_paths,
    })
}

#[derive(Deserialize)]
struct OfferDocument {
    products: HashMap<String, Product>,
    #[serde(default)]
    terms: Terms,
}

#[derive(Deserialize)]
struct Product {
    #[serde(default)]
    attributes: HashMap<String, Value>,
}

#[derive(Default, Deserialize)]
struct Terms {
    #[serde(rename = "OnDemand", default)]
    on_demand: HashMap<String, HashMap<String, Term>>,
}

#[derive(Deserialize)]
struct Term {
    #[serde(rename = "effectiveDate", default)]
    effective_date: String,
    #[serde(rename = "priceDimensions")]
    price_dimensions: HashMap<String, TermPrice>,
}

#[derive(Deserialize)]
struct TermPrice {
    unit: String,
    #[serde(rename = "beginRange")]
    begin_range: String,
    #[serde(rename = "endRange")]
    end_range: String,
    #[serde(rename = "pricePerUnit")]
    price_per_unit: HashMap<String, String>,
}

/// Reads one region's Bedrock offer into `collector`.
pub fn parse_aws_offer(
    body: &[u8],
    offer: AwsOffer,
    collector: &mut PriceCollector,
) -> Result<(), ObservationError> {
    let document: OfferDocument =
        serde_json::from_slice(body).map_err(|_| ObservationError::InvalidField)?;
    for (sku, product) in document.products {
        let attribute = |name: &str| product.attributes.get(name).and_then(Value::as_str);
        let dimension = match attribute("inferenceType").map(str::to_lowercase).as_deref() {
            Some("input tokens") => PriceDimension::Input,
            Some("output tokens") => PriceDimension::Output,
            Some("prompt cache read input tokens") => PriceDimension::CachedInput,
            // Bedrock reports cache writes as other usage.
            Some("prompt cache write input tokens") if offer == AwsOffer::FoundationModels => {
                PriceDimension::Other
            }
            _ => continue,
        };
        let batch_feature = match attribute("feature") {
            None | Some("On-demand Inference") => false,
            Some("Batch Inference") => true,
            _ => continue,
        };
        if !matches!(
            attribute("service_tier"),
            None | Some("standard") | Some("batch")
        ) {
            continue;
        }
        let item = attribute("usagetype")
            .and_then(|usage_type| match offer {
                AwsOffer::Bedrock => aws_item(usage_type),
                AwsOffer::FoundationModels => {
                    foundation_model_item(attribute("model")?, usage_type)
                }
            })
            .map(|item| {
                // A batch product's usage type need not say so.
                if batch_feature && !item.ends_with(BATCH_ITEM_SUFFIX) {
                    format!("{item}{BATCH_ITEM_SUFFIX}")
                } else {
                    item
                }
            });
        let (Some(item), Some(region)) = (
            item,
            attribute("regionCode").filter(|region| !region.is_empty()),
        ) else {
            continue;
        };
        // One on-demand term with one unbounded price.
        let Some(terms) = document.terms.on_demand.get(&sku) else {
            continue;
        };
        let mut prices = terms.values().flat_map(|term| {
            term.price_dimensions
                .values()
                .map(move |price| (term, price))
        });
        let (Some((term, price)), None) = (prices.next(), prices.next()) else {
            continue;
        };
        let per_tokens = match price.unit.as_str() {
            "1K tokens" => PerTokens::Thousand,
            "1M tokens" => PerTokens::Million,
            _ => continue,
        };
        if price.begin_range != "0" || price.end_range != "Inf" {
            continue;
        }
        for (currency, value) in &price.price_per_unit {
            let Some(amount) = PriceAmount::per_million(value, per_tokens) else {
                continue;
            };
            if !is_currency(currency) {
                continue;
            }
            collector.add(
                item.clone(),
                Some(region.to_owned()),
                currency.clone(),
                dimension,
                amount,
                term.effective_date.clone(),
            )?;
        }
    }
    Ok(())
}

/// Dimension phrases in a usage type, in the order they are tried at each
/// position.
const AWS_DIMENSIONS: &[&[&str]] = &[
    &["cache", "read", "input", "token", "count"],
    &["cache", "read", "input", "tokens"],
    &["input", "tokens"],
    &["output", "tokens"],
    &["cache", "read", "input"],
    &["input"],
    &["output"],
];

/// The item a usage type prices: the usage type without its region prefix
/// and dimension phrase, e.g. `USE1-NovaPro-input-tokens` prices `NovaPro`
/// and `USE1-NovaPro-input-tokens-batch` prices `NovaPro · batch`. `None`
/// unless exactly one dimension phrase follows the model.
fn aws_item(usage_type: &str) -> Option<String> {
    let (_, rest) = usage_type.split_once('-')?;
    let mut segments: Vec<&str> = rest.split('-').collect();
    let before = segments.len();
    segments.retain(|segment| !segment.eq_ignore_ascii_case("batch"));
    let batch = match before - segments.len() {
        0 => false,
        1 => true,
        _ => return None,
    };
    let item = aws_on_demand_item(&segments)?;
    Some(if batch {
        format!("{item}{BATCH_ITEM_SUFFIX}")
    } else {
        item
    })
}

fn aws_on_demand_item(segments: &[&str]) -> Option<String> {
    let mut found = None;
    let mut index = 1;
    while index < segments.len() {
        let matched = AWS_DIMENSIONS.iter().find(|phrase| {
            segments.len() - index >= phrase.len()
                && phrase
                    .iter()
                    .zip(&segments[index..])
                    .all(|(word, segment)| segment.eq_ignore_ascii_case(word))
        });
        match (matched, found) {
            (Some(phrase), None) => {
                found = Some((index, phrase.len()));
                index += phrase.len();
            }
            (Some(_), Some(_)) => return None,
            (None, _) => index += 1,
        }
    }
    let (start, length) = found?;
    let mut kept = segments[..start].to_vec();
    kept.extend_from_slice(&segments[start + length..]);
    Some(kept.join("-")).filter(|item| !item.is_empty())
}

/// The item a foundation model's usage type prices: the model's name, with
/// [`AWS_GLOBAL_SUFFIX`] for global cross-region usage and
/// [`AWS_LONG_CONTEXT_SUFFIX`] for its long-context tier, e.g.
/// `USE2-MP:USE2_InputTokenCount_Global-Units` of `Claude Sonnet 4.6` prices
/// `Claude Sonnet 4.6 · global`, and batch usage the batch item
/// (`Claude Sonnet 4.6 · batch`). `None` for flex, priority, reserved and
/// one-hour cache write usage, which the gateway does not bill.
fn foundation_model_item(model: &str, usage_type: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() || model.len() > MAX_AWS_MODEL_NAME {
        return None;
    }
    // `{region}-MP:{region}_{usage}-Units`: what follows the second region.
    let (_, usage) = usage_type.split_once(':').unwrap_or(("", usage_type));
    let (_, usage) = usage.split_once('_')?;
    let usage = usage.to_ascii_lowercase();
    let words: Vec<&str> = usage.split(['_', '-']).collect();
    let unbilled = ["flex", "priority", "reserved", "provisionedthroughput"];
    if words.iter().any(|word| unbilled.contains(word)) || usage.contains("1h") {
        return None;
    }
    let mut item = model.to_owned();
    if words.contains(&"global") {
        item.push_str(AWS_GLOBAL_SUFFIX);
    }
    if words.windows(2).any(|pair| pair == ["long", "ctx"]) {
        item.push_str(AWS_LONG_CONTEXT_SUFFIX);
    }
    if words.contains(&"batch") {
        item.push_str(BATCH_ITEM_SUFFIX);
    }
    Some(item)
}

/// A non-negative JSON number as a plain decimal, exactly: `1.1E-05` is
/// `0.000011`. `None` for anything else, or an exponent beyond 30.
fn plain_decimal(number: &str) -> Option<String> {
    let (mantissa, exponent) = match number.find(['e', 'E']) {
        Some(at) => (&number[..at], number[at + 1..].parse::<i32>().ok()?),
        None => (number, 0),
    };
    if exponent.abs() > 30 {
        return None;
    }
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        || (mantissa.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    // Where the point falls in `digits`, then padded into range.
    let point = whole.len() as i32 + exponent;
    let digits = if point <= 0 {
        format!("{}{digits}", "0".repeat((1 - point) as usize))
    } else if point as usize > digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        digits
    };
    let point = point.max(1) as usize;
    let (whole, fraction) = digits.split_at(point);
    Some(if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    })
}

fn is_currency(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_numbers_become_plain_decimals() {
        for (number, decimal) in [
            ("0.0025", "0.0025"),
            ("10", "10"),
            ("1.1E-05", "0.000011"),
            ("25e-1", "2.5"),
            ("2.5e+2", "250"),
            ("0.00000000000000000012", "0.00000000000000000012"),
        ] {
            assert_eq!(plain_decimal(number).as_deref(), Some(decimal), "{number}");
        }
        for number in ["-1", "1.", ".5", "1e", "1e99", "\"1\"", "null", ""] {
            assert_eq!(plain_decimal(number), None, "{number}");
        }
    }

    #[test]
    fn azure_meters_group_into_items() {
        for (meter, item, dimension) in [
            (
                "gpt 4.1 Inp regnl Tokens",
                "gpt 4.1 · regional",
                PriceDimension::Input,
            ),
            (
                "gpt 4.1 Outp glbl Tokens",
                "gpt 4.1 · global",
                PriceDimension::Output,
            ),
            (
                "gpt 4.1 cached Inp Data Zone Tokens",
                "gpt 4.1 · data zone",
                PriceDimension::CachedInput,
            ),
            (
                "o3-mini-0131-Input-Global Tokens",
                "o3 mini 0131 · global",
                PriceDimension::Input,
            ),
            (
                "text-embedding-3-large Tokens",
                "text embedding 3 large",
                PriceDimension::Input,
            ),
            (
                "gpt 4.1 Inp Batch glbl Tokens",
                "gpt 4.1 · global · batch",
                PriceDimension::Input,
            ),
            (
                "gpt-4o-mini-0718-Batch-Outp-Data Zone Tokens",
                "gpt 4o mini 0718 · data zone · batch",
                PriceDimension::Output,
            ),
        ] {
            assert_eq!(
                azure_item(meter),
                Some((item.to_owned(), dimension)),
                "{meter}"
            );
        }
        for meter in [
            "gpt 4.1 Inp Batch Batch glbl Tokens",
            "gpt 4.1 Inp Batched glbl Tokens",
            "gpt 4.1 FT Inp regnl Tokens",
            "az davinci 002 fine tuned Inp Tokens",
            "gpt 4.1 Training Tokens",
            "gpt 4.1 Inp Outp glbl Tokens",
            "gpt 4.1 cached Outp glbl Tokens",
            "gpt 4.1 Inp glbl regnl Tokens",
            "Inp glbl Tokens",
            "Code-Interpreter-global Session",
            "gpt 4.1 glbl Tokens",
        ] {
            assert_eq!(azure_item(meter), None, "{meter}");
        }
    }

    fn azure_meter(name: &str, price: f64, unit: &str, region: &str) -> Value {
        serde_json::json!({
            "currencyCode": "USD", "tierMinimumUnits": 0.0, "retailPrice": price,
            "unitPrice": price, "armRegionName": region, "meterName": name,
            "unitOfMeasure": unit, "type": "Consumption",
            "effectiveStartDate": "2025-01-01T00:00:00Z", "productName": "Azure OpenAI",
        })
    }

    #[test]
    fn azure_pages_restate_prices_per_million() {
        let mut collector = PriceCollector::default();
        let page = serde_json::json!({
            "Items": [
                azure_meter("gpt 4o Inp glbl Tokens", 0.0025, "1K", "eastus"),
                azure_meter("gpt 4o Outp glbl Tokens", 10.0, "1M", "eastus"),
                azure_meter("gpt 4o cached Inp glbl Tokens", 0.00000125, "1", "eastus"),
                azure_meter("gpt 4o Inp glbl Tokens", 0.00275, "1K", "westeurope"),
                azure_meter("gpt 4o Inp glbl Tokens", 0.0025, "1 Hour", "eastus"),
                azure_meter("Code-Interpreter-global Session", 0.03, "1", "eastus"),
            ],
            "NextPageLink": "https://prices.azure.com/api/retail/prices?$skip=1000",
        });
        let parsed = parse_azure_page(page.to_string().as_bytes(), &mut collector).unwrap();
        assert_eq!(
            parsed.next_page.as_deref(),
            Some("https://prices.azure.com/api/retail/prices?$skip=1000")
        );
        let set = collector.finish(None).unwrap();
        assert_eq!(set.observations.len(), 2);
        let eastus = &set.observations[0];
        assert_eq!(eastus.item, "gpt 4o · global");
        assert_eq!(eastus.region.as_deref(), Some("eastus"));
        assert_eq!(eastus.model, None);
        assert_eq!(eastus.per_tokens, PerTokens::Million);
        assert_eq!(eastus.prices.input.as_ref().unwrap().as_str(), "2.5");
        assert_eq!(eastus.prices.output.as_ref().unwrap().as_str(), "10");
        assert_eq!(
            eastus.prices.cached_input.as_ref().unwrap().as_str(),
            "1.25"
        );
        assert_eq!(set.observations[1].region.as_deref(), Some("westeurope"));
        assert_eq!(set.unmapped(), 2);
        assert!(parse_azure_page(b"{}", &mut PriceCollector::default()).is_err());
    }

    #[test]
    fn disagreeing_meters_drop_the_item() {
        let mut collector = PriceCollector::default();
        let mut later = azure_meter("gpt 4o Inp glbl Tokens", 0.003, "1K", "eastus");
        later["effectiveStartDate"] = "2025-06-01T00:00:00Z".into();
        let page = serde_json::json!({ "Items": [
            // A later price replaces an earlier one.
            azure_meter("gpt 4o Inp glbl Tokens", 0.0025, "1K", "eastus"),
            later,
            // Two prices effective at once are ambiguous.
            azure_meter("gpt 4o mini Inp glbl Tokens", 0.00015, "1K", "eastus"),
            azure_meter("gpt 4o mini Inp glbl Tokens", 0.0002, "1K", "eastus"),
            azure_meter("gpt 4o mini Outp glbl Tokens", 0.0006, "1K", "eastus"),
        ]});
        parse_azure_page(page.to_string().as_bytes(), &mut collector).unwrap();
        let set = collector.finish(None).unwrap();
        assert_eq!(set.observations.len(), 1);
        assert_eq!(set.observations[0].item, "gpt 4o · global");
        assert_eq!(
            set.observations[0].prices.input.as_ref().unwrap().as_str(),
            "3"
        );
    }

    #[test]
    fn aws_usage_types_name_items() {
        for (usage_type, item) in [
            ("USE1-NovaPro-input-tokens", "NovaPro"),
            ("USE1-NovaPro-output-tokens", "NovaPro"),
            (
                "USE1-openai.gpt-oss-120b-mantle-input-tokens-standard",
                "openai.gpt-oss-120b-mantle-standard",
            ),
            (
                "USE1-Nova2.0Lite-cache-read-input-token-count-cross-region-global",
                "Nova2.0Lite-cross-region-global",
            ),
            ("EUW1-Claude3.5Sonnet-input", "Claude3.5Sonnet"),
            ("USE1-NovaPro-input-tokens-batch", "NovaPro · batch"),
        ] {
            assert_eq!(aws_item(usage_type).as_deref(), Some(item), "{usage_type}");
        }
        for usage_type in [
            "USE1-NovaPro-batch-input-tokens-batch",
            "USE1-input-tokens",
            "USE1-Model-input-output",
            "USE1-NovaPro",
            "NovaPro",
        ] {
            assert_eq!(aws_item(usage_type), None, "{usage_type}");
        }
    }

    fn aws_product(sku: &str, usage_type: &str, inference: &str, extra: Value) -> (Value, Value) {
        let mut attributes = serde_json::json!({
            "usagetype": usage_type, "inferenceType": inference, "regionCode": "us-east-1",
        });
        attributes
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        (
            serde_json::json!({ "sku": sku, "attributes": attributes }),
            serde_json::json!({ format!("{sku}.T"): {
                "effectiveDate": "2025-01-01T00:00:00Z",
                "priceDimensions": { format!("{sku}.T.P"): {
                    "unit": "1K tokens", "beginRange": "0", "endRange": "Inf",
                    "pricePerUnit": { "USD": "0.0008000000" },
                }},
            }}),
        )
    }

    #[test]
    fn aws_offers_keep_on_demand_and_batch_token_prices() {
        let none = serde_json::json!({});
        let products = [
            (
                "A",
                "USE1-NovaPro-input-tokens",
                "Input tokens",
                none.clone(),
            ),
            (
                "B",
                "USE1-NovaPro-output-tokens",
                "Output tokens",
                serde_json::json!({ "feature": "On-demand Inference" }),
            ),
            (
                "C",
                "USE1-NovaPro-cache-read-input-token-count",
                "Prompt cache read input tokens",
                none.clone(),
            ),
            (
                "D",
                "USE1-NovaPro-input-tokens-flex",
                "Input tokens",
                serde_json::json!({ "service_tier": "flex" }),
            ),
            (
                "E",
                "USE1-NovaPro-input-tokens-batch",
                "Input tokens",
                serde_json::json!({ "feature": "Batch Inference" }),
            ),
            (
                "F",
                "USE1-NovaPro-output-tokens-batch",
                "Output tokens",
                serde_json::json!({ "feature": "Batch Inference" }),
            ),
            (
                "G",
                "USE1-NovaPro-input-tokens-provisioned",
                "Input tokens",
                serde_json::json!({ "feature": "Provisioned Throughput" }),
            ),
            ("H", "USE1-NovaPro-image", "Image", none),
        ];
        let mut document = serde_json::json!({ "products": {}, "terms": { "OnDemand": {} } });
        for (sku, usage_type, inference, extra) in products {
            let (product, term) = aws_product(sku, usage_type, inference, extra);
            document["products"][sku] = product;
            document["terms"]["OnDemand"][sku] = term;
        }
        let mut collector = PriceCollector::default();
        parse_aws_offer(
            document.to_string().as_bytes(),
            AwsOffer::Bedrock,
            &mut collector,
        )
        .unwrap();
        let set = collector
            .finish(Some("2025-01-01T00:00:00Z".into()))
            .unwrap();
        assert_eq!(set.observations.len(), 2);
        let observed = &set.observations[0];
        assert_eq!(observed.item, "NovaPro");
        // Batch prices are kept, as the batch item.
        let batch = &set.observations[1];
        assert_eq!(batch.item, "NovaPro · batch");
        assert_eq!(batch.prices.input.as_ref().unwrap().as_str(), "0.8");
        assert_eq!(batch.prices.output.as_ref().unwrap().as_str(), "0.8");
        assert_eq!(batch.prices.cached_input, None);
        assert_eq!(observed.region.as_deref(), Some("us-east-1"));
        assert_eq!(observed.prices.input.as_ref().unwrap().as_str(), "0.8");
        assert_eq!(observed.prices.output.as_ref().unwrap().as_str(), "0.8");
        assert_eq!(
            observed.prices.cached_input.as_ref().unwrap().as_str(),
            "0.8"
        );
        assert_eq!(set.source_version.as_deref(), Some("2025-01-01T00:00:00Z"));
    }

    #[test]
    fn foundation_model_usage_types_name_items() {
        let sonnet = "Claude Sonnet 4.6";
        for (usage_type, item) in [
            (
                "USE2-MP:USE2_InputTokenCount-Units",
                Some("Claude Sonnet 4.6"),
            ),
            (
                "USE2-MP:USE2_OutputTokenCount_Global-Units",
                Some("Claude Sonnet 4.6 · global"),
            ),
            (
                "USE2-MP:USE2_input_tokens_global_standard-Units",
                Some("Claude Sonnet 4.6 · global"),
            ),
            (
                "USE2-MP:USE2_input_tokens_long_ctx_standard-Units",
                Some("Claude Sonnet 4.6 · long context"),
            ),
            (
                "USE2-MP:USE2_cache_read_tokens_long_ctx_global_standard-Units",
                Some("Claude Sonnet 4.6 · global · long context"),
            ),
            (
                "USE2-MP:USE2_CacheWriteInputTokenCount-Units",
                Some("Claude Sonnet 4.6"),
            ),
            ("USE2-MP:USE2_CacheWrite1hInputTokenCount-Units", None),
            ("USE2-MP:USE2_cache_write_tokens_1h_standard-Units", None),
            (
                "USE2-MP:USE2_InputTokenCount_Batch-Units",
                Some("Claude Sonnet 4.6 · batch"),
            ),
            (
                "USE2-MP:USE2_OutputTokenCount_Global_Batch-Units",
                Some("Claude Sonnet 4.6 · global · batch"),
            ),
            ("USE2-MP:USE2_Reserved_1Month_InputTPM_Geo-Units", None),
            ("USE2-MP:USE2_input_tokens_flex-Units", None),
            ("no-region-separator", None),
        ] {
            assert_eq!(
                foundation_model_item(sonnet, usage_type).as_deref(),
                item,
                "{usage_type}"
            );
        }
        assert_eq!(
            foundation_model_item(" ", "USE2-MP:USE2_InputTokenCount-Units"),
            None
        );
    }

    #[test]
    fn foundation_model_offers_price_models_by_name() {
        let model =
            |name: &str| serde_json::json!({ "model": name, "feature": "On-demand Inference" });
        let products = [
            ("A", "USE1-MP:USE1_InputTokenCount-Units", "Input tokens"),
            ("B", "USE1-MP:USE1_OutputTokenCount-Units", "Output tokens"),
            (
                "C",
                "USE1-MP:USE1_CacheReadInputTokenCount-Units",
                "Prompt cache read input tokens",
            ),
            (
                "D",
                "USE1-MP:USE1_CacheWriteInputTokenCount-Units",
                "Prompt cache write input tokens",
            ),
            (
                "E",
                "USE1-MP:USE1_CacheWrite1hInputTokenCount-Units",
                "Prompt cache write input tokens",
            ),
            (
                "F",
                "USE1-MP:USE1_InputTokenCount_Global-Units",
                "Input tokens",
            ),
        ];
        let mut document = serde_json::json!({ "products": {}, "terms": { "OnDemand": {} } });
        for (sku, usage_type, inference) in products {
            let (product, term) =
                aws_product(sku, usage_type, inference, model("Claude Sonnet 4.6"));
            document["products"][sku] = product;
            document["terms"]["OnDemand"][sku] = term;
        }
        // A product without a model name prices nothing.
        let (product, term) = aws_product(
            "G",
            "USE1-MP:USE1_InputTokenCount-Units",
            "Input tokens",
            serde_json::json!({}),
        );
        document["products"]["G"] = product;
        document["terms"]["OnDemand"]["G"] = term;
        let mut collector = PriceCollector::default();
        parse_aws_offer(
            document.to_string().as_bytes(),
            AwsOffer::FoundationModels,
            &mut collector,
        )
        .unwrap();
        let mut set = collector.finish(None).unwrap().observations;
        set.sort_by(|a, b| a.item.cmp(&b.item));
        let items: Vec<&str> = set.iter().map(|o| o.item.as_str()).collect();
        assert_eq!(items, ["Claude Sonnet 4.6", "Claude Sonnet 4.6 · global"]);
        let sonnet = &set[0];
        assert_eq!(sonnet.model, None);
        assert_eq!(sonnet.region.as_deref(), Some("us-east-1"));
        for amount in [
            &sonnet.prices.input,
            &sonnet.prices.output,
            &sonnet.prices.cached_input,
            &sonnet.prices.other,
        ] {
            assert_eq!(amount.as_ref().unwrap().as_str(), "0.8");
        }
        assert_eq!(set[1].prices.output, None);

        // The Bedrock offer keeps ignoring cache writes.
        let mut collector = PriceCollector::default();
        parse_aws_offer(
            document.to_string().as_bytes(),
            AwsOffer::Bedrock,
            &mut collector,
        )
        .unwrap();
        assert!(
            collector
                .finish(None)
                .unwrap()
                .observations
                .iter()
                .all(|o| o.prices.other.is_none())
        );
    }

    #[test]
    fn foundation_model_region_index_lists_its_own_offers() {
        let path =
            "/offers/v1.0/aws/AmazonBedrockFoundationModels/20261012084008/us-east-2/index.json";
        let index = serde_json::json!({ "regions": {
            "us-east-2": { "regionCode": "us-east-2", "currentVersionUrl": path },
        }});
        let body = index.to_string();
        assert_eq!(
            parse_aws_region_index(body.as_bytes(), AwsOffer::FoundationModels)
                .unwrap()
                .offer_paths,
            [path]
        );
        assert!(parse_aws_region_index(body.as_bytes(), AwsOffer::Bedrock).is_err());
        assert_eq!(
            AwsOffer::FoundationModels.region_index(),
            "/offers/v1.0/aws/AmazonBedrockFoundationModels/current/region_index.json"
        );
    }

    #[test]
    fn aws_region_index_lists_offer_paths() {
        let index = serde_json::json!({
            "publicationDate": "2025-01-01T00:00:00Z",
            "regions": {
                "us-east-1": { "regionCode": "us-east-1",
                    "currentVersionUrl": "/offers/v1.0/aws/AmazonBedrock/20250101/us-east-1/index.json" },
                "eu-west-1": { "regionCode": "eu-west-1",
                    "currentVersionUrl": "/offers/v1.0/aws/AmazonBedrock/20250101/eu-west-1/index.json" },
            },
        });
        let parsed =
            parse_aws_region_index(index.to_string().as_bytes(), AwsOffer::Bedrock).unwrap();
        assert_eq!(
            parsed.offer_paths,
            [
                "/offers/v1.0/aws/AmazonBedrock/20250101/eu-west-1/index.json",
                "/offers/v1.0/aws/AmazonBedrock/20250101/us-east-1/index.json",
            ]
        );
        for path in [
            "https://elsewhere.example/offers/v1.0/aws/AmazonBedrock/x/index.json",
            "/offers/v1.0/aws/AmazonBedrock/../../x/index.json",
            "/offers/v1.0/aws/AmazonEC2/x/index.json",
            "/offers/v1.0/aws/AmazonBedrock/x/index.json?redirect=1",
        ] {
            let index = serde_json::json!({ "regions": {
                "r": { "regionCode": "r", "currentVersionUrl": path },
            }});
            assert!(
                parse_aws_region_index(index.to_string().as_bytes(), AwsOffer::Bedrock).is_err(),
                "{path}"
            );
        }
    }
}
