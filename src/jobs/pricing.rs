//! Price sync: fetches each configured price source daily and records what
//! it publishes: operator price sheets, the Azure Retail Prices API for Azure
//! OpenAI, and the AWS Price List for Bedrock.
//!
//! Every tick registers the configured sources, retires those no worker has
//! configured for an hour, then fetches those that are due, one claim at a
//! time. A failed fetch is retried a few times with a
//! growing delay, then waits for the next daily fetch; it is recorded with a
//! fixed error code, and the URL and response body never reach the source
//! row or the log. Nothing here changes a catalog price.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use url::Url;

use super::egress::{self, DeliveryPolicy, EgressError};
use crate::{
    domain::{
        catalog::ProviderFamily,
        notifications::{NewNotification, NotificationKind},
        pricing::{
            ERROR_CONNECTION_FAILED, ERROR_DESTINATION_NOT_ALLOWED, ERROR_DNS_FAILED,
            ERROR_HTTP_STATUS, ERROR_INVALID_PRICE_LIST, ERROR_INVALID_PRICE_SHEET,
            ERROR_INVALID_URL, ERROR_REQUEST_FAILED, ERROR_RESPONSE_TOO_LARGE, ERROR_TIMED_OUT,
            ObservationSet, parse_price_sheet,
        },
        public_prices::{
            AwsOffer, PriceCollector, parse_aws_offer, parse_aws_region_index, parse_azure_page,
        },
    },
    persistence::{
        notifications::NotificationChange,
        price_sync::{ClaimedSource, FetchOutcome, PriceSyncStore},
    },
};

/// How often each source is fetched.
pub const FETCH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Delay before the first retry of a failed fetch; later retries wait
/// multiples of it.
const RETRY_DELAY: Duration = Duration::from_secs(15 * 60);
/// How long a claim lasts; a fetch is abandoned well before it expires.
const LEASE: Duration = Duration::from_secs(5 * 60);
/// How long a source no worker has configured stays active, so workers with
/// different configurations during a rolling change keep each other's.
const RETIRE_AFTER: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest price document read.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("qucelerate-gateway-pricing/", env!("CARGO_PKG_VERSION"));

pub const KIND_PRICE_SHEET: &str = "price-sheet";
pub const KIND_AZURE_RETAIL: &str = "azure-retail";
pub const KIND_AWS_PRICE_LIST: &str = "aws-price-list";

pub const AZURE_RETAIL_URL: &str = "https://prices.azure.com/api/retail/prices";
const AZURE_RETAIL_FILTER: &str =
    "serviceName eq 'Foundry Models' and productName eq 'Azure OpenAI'";
/// Most pages read per currency; Azure OpenAI spans about ten.
const MAX_AZURE_PAGES: usize = 50;
pub const AWS_PRICE_LIST_URL: &str = "https://pricing.us-east-1.amazonaws.com";
/// Reading an offer per region takes longer than one document, but must
/// still end inside the claim lease.
const AWS_FETCH_TIMEOUT: Duration = Duration::from_secs(4 * 60);

/// Where prices for one provider family come from.
#[async_trait]
pub trait PriceSource: Send + Sync {
    /// `{kind}:{family}`.
    fn id(&self) -> String {
        format!("{}:{}", self.kind(), self.family().as_str())
    }
    fn kind(&self) -> &'static str;
    fn family(&self) -> ProviderFamily;
    /// How long a fetch may take; less than the claim lease.
    fn timeout(&self) -> Duration {
        FETCH_TIMEOUT
    }
    /// Fetches the prices published now, or a fixed error code.
    async fn fetch(&self) -> Result<ObservationSet, &'static str>;
}

/// An operator price sheet: a JSON document at a configured URL, in the
/// format [`parse_price_sheet`] reads.
pub struct PriceSheetSource {
    family: ProviderFamily,
    url: Url,
    policy: DeliveryPolicy,
}

impl PriceSheetSource {
    pub fn new(family: ProviderFamily, url: Url, policy: DeliveryPolicy) -> Self {
        Self {
            family,
            url,
            policy,
        }
    }
}

#[async_trait]
impl PriceSource for PriceSheetSource {
    fn kind(&self) -> &'static str {
        KIND_PRICE_SHEET
    }

    fn family(&self) -> ProviderFamily {
        self.family
    }

    async fn fetch(&self) -> Result<ObservationSet, &'static str> {
        let body = get(&self.url, self.policy).await?;
        parse_price_sheet(&body).map_err(|_| ERROR_INVALID_PRICE_SHEET)
    }
}

/// The Azure Retail Prices API's Azure OpenAI meters, read once per
/// currency.
pub struct AzureRetailSource {
    base: Url,
    currencies: Vec<String>,
    policy: DeliveryPolicy,
}

impl AzureRetailSource {
    /// `base` is [`AZURE_RETAIL_URL`] outside tests; next-page links are only
    /// followed to its origin.
    pub fn new(base: Url, currencies: Vec<String>, policy: DeliveryPolicy) -> Self {
        Self {
            base,
            currencies,
            policy,
        }
    }
}

#[async_trait]
impl PriceSource for AzureRetailSource {
    fn kind(&self) -> &'static str {
        KIND_AZURE_RETAIL
    }

    fn family(&self) -> ProviderFamily {
        ProviderFamily::AzureOpenAi
    }

    async fn fetch(&self) -> Result<ObservationSet, &'static str> {
        let mut collector = PriceCollector::default();
        for currency in &self.currencies {
            let mut url = self.base.clone();
            url.query_pairs_mut()
                .append_pair("currencyCode", &format!("'{currency}'"))
                .append_pair("$filter", AZURE_RETAIL_FILTER);
            let mut pages = 0;
            loop {
                pages += 1;
                if pages > MAX_AZURE_PAGES {
                    return Err(ERROR_INVALID_PRICE_LIST);
                }
                let body = get(&url, self.policy).await?;
                let page = parse_azure_page(&body, &mut collector)
                    .map_err(|_| ERROR_INVALID_PRICE_LIST)?;
                let Some(next) = page.next_page else {
                    break;
                };
                url = Url::parse(&next)
                    .ok()
                    .filter(|next| same_origin(next, &self.base))
                    .ok_or(ERROR_INVALID_PRICE_LIST)?;
            }
        }
        collector.finish(None).map_err(|_| ERROR_INVALID_PRICE_LIST)
    }
}

fn same_origin(url: &Url, base: &Url) -> bool {
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
}

/// The AWS Price List's Bedrock offers, Amazon's models and partners'
/// foundation models: each one's region index, then each region's current
/// offer.
pub struct AwsPriceListSource {
    base: Url,
    policy: DeliveryPolicy,
}

impl AwsPriceListSource {
    /// `base` is [`AWS_PRICE_LIST_URL`] outside tests; offers are read from
    /// the paths the region index lists under it.
    pub fn new(base: Url, policy: DeliveryPolicy) -> Self {
        Self { base, policy }
    }
}

#[async_trait]
impl PriceSource for AwsPriceListSource {
    fn kind(&self) -> &'static str {
        KIND_AWS_PRICE_LIST
    }

    fn family(&self) -> ProviderFamily {
        ProviderFamily::Bedrock
    }

    fn timeout(&self) -> Duration {
        AWS_FETCH_TIMEOUT
    }

    async fn fetch(&self) -> Result<ObservationSet, &'static str> {
        let join = |path: &str| self.base.join(path).map_err(|_| ERROR_INVALID_URL);
        let mut collector = PriceCollector::default();
        let mut published = Vec::new();
        for offer in AwsOffer::ALL {
            let body = get(&join(&offer.region_index())?, self.policy).await?;
            let index =
                parse_aws_region_index(&body, offer).map_err(|_| ERROR_INVALID_PRICE_LIST)?;
            for path in &index.offer_paths {
                let body = get(&join(path)?, self.policy).await?;
                parse_aws_offer(&body, offer, &mut collector)
                    .map_err(|_| ERROR_INVALID_PRICE_LIST)?;
            }
            published.extend(index.publication_date.filter(|date| date.len() <= 60));
        }
        // Either offer's publication changes the version.
        let version = (!published.is_empty()).then(|| published.join(" + "));
        collector
            .finish(version)
            .map_err(|_| ERROR_INVALID_PRICE_LIST)
    }
}

/// GETs `url` through a pinned client and returns its body.
async fn get(url: &Url, policy: DeliveryPolicy) -> Result<Vec<u8>, &'static str> {
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT);
    let client =
        egress::pinned_client(url, policy, builder)
            .await
            .map_err(|error| match error {
                EgressError::InvalidUrl => ERROR_INVALID_URL,
                EgressError::DestinationNotAllowed => ERROR_DESTINATION_NOT_ALLOWED,
                EgressError::DnsTimedOut | EgressError::DnsFailed => ERROR_DNS_FAILED,
                EgressError::ClientUnavailable => ERROR_REQUEST_FAILED,
            })?;
    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(request_error)?;
    if !response.status().is_success() {
        return Err(ERROR_HTTP_STATUS);
    }
    egress::read_limited(response, MAX_BODY_BYTES)
        .await
        .map_err(request_error)?
        .ok_or(ERROR_RESPONSE_TOO_LARGE)
}

fn request_error(error: reqwest::Error) -> &'static str {
    if error.is_timeout() {
        ERROR_TIMED_OUT
    } else if error.is_connect() {
        ERROR_CONNECTION_FAILED
    } else {
        ERROR_REQUEST_FAILED
    }
}

/// What one pricing tick did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PricingReport {
    pub changed: u64,
    pub unchanged: u64,
    pub failed: u64,
}

pub struct PricingRunner {
    store: PriceSyncStore,
    sources: Vec<Arc<dyn PriceSource>>,
    fetch_timeout: Option<Duration>,
    /// Whether a failing source raises a notification.
    notify: bool,
}

impl PricingRunner {
    pub fn new(store: PriceSyncStore, sources: Vec<Arc<dyn PriceSource>>) -> Self {
        Self {
            store,
            sources,
            fetch_timeout: None,
            notify: false,
        }
    }

    /// Tells the organizations that use the catalog when a source fails to
    /// fetch, and resolves it once it fetches again.
    pub fn with_notifications(mut self) -> Self {
        self.notify = true;
        self
    }

    /// Abandons a fetch after `timeout` when that is sooner than the
    /// source's own limit.
    pub fn with_fetch_timeout(mut self, timeout: Duration) -> Self {
        self.fetch_timeout = Some(timeout);
        self
    }

    pub fn store(&self) -> &PriceSyncStore {
        &self.store
    }

    /// Registers the configured sources, retiring any no worker has
    /// configured for an hour, then fetches each due one at most once.
    pub async fn run_once(&self) -> Result<PricingReport, sqlx::Error> {
        let mut report = PricingReport::default();
        let registrations: Vec<_> = self
            .sources
            .iter()
            .map(|source| (source.id(), source.kind(), source.family()))
            .collect();
        let now = Utc::now();
        let retire_before = now - chrono::Duration::from_std(RETIRE_AFTER).expect("fits");
        self.store
            .register(&registrations, now, retire_before)
            .await?;
        let mut pending: Vec<String> = registrations.into_iter().map(|(id, _, _)| id).collect();
        while !pending.is_empty() {
            let now = Utc::now();
            let lease_until = now + chrono::Duration::from_std(LEASE).expect("lease fits");
            let Some(claim) = self.store.claim(&pending, now, lease_until).await? else {
                break;
            };
            pending.retain(|id| *id != claim.source);
            let Some(source) = self.sources.iter().find(|s| s.id() == claim.source) else {
                continue;
            };
            match self.fetch(source.as_ref(), &claim).await? {
                Some(FetchOutcome::Changed(_)) => report.changed += 1,
                Some(FetchOutcome::Unchanged(_)) => report.unchanged += 1,
                None => report.failed += 1,
            }
        }
        Ok(report)
    }

    /// Fetches one claimed source and records the outcome; `None` when the
    /// fetch failed or the lease was lost.
    async fn fetch(
        &self,
        source: &dyn PriceSource,
        claim: &ClaimedSource,
    ) -> Result<Option<FetchOutcome>, sqlx::Error> {
        let timeout = self
            .fetch_timeout
            .map_or(source.timeout(), |timeout| timeout.min(source.timeout()));
        let fetched = tokio::time::timeout(timeout.min(LEASE), source.fetch())
            .await
            .unwrap_or(Err(ERROR_TIMED_OUT));
        let now = Utc::now();
        let next_fetch_at =
            now + chrono::Duration::from_std(FETCH_INTERVAL).expect("interval fits");
        match fetched {
            Ok(set) => {
                // Resolves the source's failure warnings in the same
                // transaction.
                self.store
                    .record_success(claim, &set, now, next_fetch_at)
                    .await
            }
            Err(code) => {
                tracing::warn!(source = %claim.source, code, "price source fetch failed");
                let retry_delay = chrono::Duration::from_std(RETRY_DELAY).expect("delay fits");
                let notify = self
                    .notify
                    .then(|| failing_notification(&claim.source, code));
                self.store
                    .record_failure(
                        claim,
                        code,
                        now,
                        retry_delay,
                        next_fetch_at,
                        notify.as_ref(),
                    )
                    .await?;
                Ok(None)
            }
        }
    }
}

/// Raised in every organization that uses the catalog, with the failure
/// recorded (so an older failure never reopens what a newer fetch resolved),
/// best effort: it never fails the record, and the next failure tries again.
fn failing_notification(source: &str, code: &str) -> NotificationChange {
    NotificationChange::RaiseForCatalogOrganizations(
        NewNotification::new(
            NotificationKind::PriceSourceFailing,
            source,
            format!("Price source {source} is failing"),
            format!(
                "The latest fetch of {source} failed ({code}). Its last fetched prices still \
                 apply, and the gateway tries again."
            ),
        )
        .with_link("/admin/models/pricing")
        .with_data(serde_json::json!({ "source": source, "error_code": code })),
    )
}
