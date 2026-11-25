//! Webhook delivery: POSTs claimed deliveries, signed per Standard Webhooks.
//!
//! Endpoint URLs are validated when written, but DNS can change afterwards,
//! so every attempt goes through [`egress::pinned_client`]. Recorded errors
//! are fixed phrases that never include the URL, the response body, or the
//! secret.

use std::time::Duration;

use chrono::Utc;
use futures::future::join_all;
use url::Url;

pub use super::egress::DeliveryPolicy;
use super::egress::{self, EgressError};
use crate::domain::{
    credentials::CredentialCipher,
    identity::OrganizationId,
    webhook::{DeliveryEnvelope, sign},
};
use crate::persistence::{
    outbox::backoff,
    webhooks::{ClaimedDelivery, WebhookStore},
};

/// Attempts in a run before a delivery is parked; with the outbox backoff
/// this spans about fifteen hours. A redelivery starts a new run.
pub const MAX_DELIVERY_ATTEMPTS: i32 = 24;

const BATCH_SIZE: i64 = 16;
/// How long a claimed delivery stays invisible to other workers. Longer than
/// DNS plus request timeouts, so an attempt normally finishes inside it.
const LEASE: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("qucelerate-gateway-webhooks/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeliveryReport {
    pub claimed: usize,
    pub delivered: usize,
    pub failed: usize,
    pub parked: usize,
}

enum Outcome {
    Delivered(u16),
    Failed {
        status_code: Option<u16>,
        error: &'static str,
    },
}

#[derive(Clone)]
pub struct WebhookDeliverer {
    store: WebhookStore,
    cipher: Option<CredentialCipher>,
    policy: DeliveryPolicy,
}

impl WebhookDeliverer {
    pub fn new(
        store: WebhookStore,
        cipher: Option<CredentialCipher>,
        policy: DeliveryPolicy,
    ) -> Self {
        Self {
            store,
            cipher,
            policy,
        }
    }

    /// Claims one batch of due deliveries and attempts them concurrently.
    /// Without a credential key no secret can be opened, so nothing is
    /// claimed.
    pub async fn deliver_due(&self) -> Result<DeliveryReport, sqlx::Error> {
        let Some(cipher) = self.cipher.as_ref() else {
            return Ok(DeliveryReport::default());
        };
        let now = Utc::now();
        let lease_until = now + chrono::Duration::from_std(LEASE).unwrap_or_default();
        let claimed = self.store.claim_due(now, lease_until, BATCH_SIZE).await?;
        let outcomes = join_all(
            claimed
                .iter()
                .map(|delivery| self.attempt(cipher, delivery)),
        )
        .await;
        let mut report = DeliveryReport {
            claimed: claimed.len(),
            ..DeliveryReport::default()
        };
        for (delivery, outcome) in claimed.iter().zip(outcomes) {
            match outcome {
                Outcome::Delivered(status_code) => {
                    self.store
                        .record_delivered(delivery, Utc::now(), status_code)
                        .await?;
                    report.delivered += 1;
                }
                Outcome::Failed { status_code, error } => {
                    let retry_at = (delivery.run_attempt < MAX_DELIVERY_ATTEMPTS).then(|| {
                        Utc::now()
                            + chrono::Duration::from_std(backoff(delivery.run_attempt))
                                .unwrap_or(chrono::Duration::hours(1))
                    });
                    self.store
                        .record_failed(delivery, status_code, error, retry_at)
                        .await?;
                    if retry_at.is_some() {
                        report.failed += 1;
                    } else {
                        report.parked += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    async fn attempt(&self, cipher: &CredentialCipher, delivery: &ClaimedDelivery) -> Outcome {
        let failed = |error| Outcome::Failed {
            status_code: None,
            error,
        };
        let Ok(key) = cipher.open_webhook_secret(
            OrganizationId(delivery.organization_id),
            delivery.endpoint_id,
            &delivery.secret_key_id,
            &delivery.secret_ciphertext,
        ) else {
            return failed("signing secret unavailable");
        };
        let Ok(url) = Url::parse(&delivery.url) else {
            return failed("endpoint url is invalid");
        };
        let client = match self.client_for(&url).await {
            Ok(client) => client,
            Err(error) => return failed(error),
        };
        let envelope = DeliveryEnvelope {
            id: delivery.event_id,
            event_type: &delivery.event_type,
            organization_id: OrganizationId(delivery.organization_id),
            occurred_at: delivery.occurred_at,
            data: &delivery.payload,
        };
        let Ok(body) = serde_json::to_vec(&envelope) else {
            return failed("event could not be encoded");
        };
        let message_id = delivery.event_id.to_string();
        let timestamp = Utc::now().timestamp();
        let signature = sign(&key, &message_id, timestamp, &body);
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("webhook-id", message_id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(body)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                Outcome::Delivered(response.status().as_u16())
            }
            Ok(response) => Outcome::Failed {
                status_code: Some(response.status().as_u16()),
                error: "endpoint returned a non-success status",
            },
            Err(error) if error.is_timeout() => failed("request timed out"),
            Err(error) if error.is_connect() => failed("connection failed"),
            Err(_) => failed("request failed"),
        }
    }

    /// A client that can only reach `url`'s host at addresses resolved and
    /// checked when it is built.
    async fn client_for(&self, url: &Url) -> Result<reqwest::Client, &'static str> {
        let builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT);
        egress::pinned_client(url, self.policy, builder)
            .await
            .map_err(|error| match error {
                EgressError::InvalidUrl => "endpoint url is invalid",
                EgressError::DestinationNotAllowed => "destination address not allowed",
                EgressError::DnsTimedOut => "dns lookup timed out",
                EgressError::DnsFailed => "dns lookup failed",
                EgressError::ClientUnavailable => "request failed",
            })
    }
}
