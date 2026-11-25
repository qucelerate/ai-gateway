//! Read models for the admin catalog API: releases, provider accounts,
//! deployments with their per-operation capabilities and prices, and routes.
//!
//! Catalog rows are immutable per release, so every collection is read from
//! one release: the active release unless the caller names another. Provider
//! accounts carry only their adapter instance key; credentials are resolved by
//! the adapter from runtime configuration and never enter the catalog.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::catalog::CatalogOperation;

const MAX_LABEL_LENGTH: usize = 255;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogReleaseSummary {
    pub id: Uuid,
    pub version: String,
    /// Hex-encoded SHA-256 of the canonical manifest.
    pub digest: String,
    pub created_at: DateTime<Utc>,
    pub sealed_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub activated_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderAccountView {
    pub id: Uuid,
    pub release_id: Uuid,
    pub adapter_instance_key: String,
    pub family: String,
    pub deployment_count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CapabilityView {
    pub id: Uuid,
    pub max_input_bytes: i64,
    pub max_output_tokens: i64,
    pub max_tokens_per_byte: i32,
    pub tokenizer: String,
    pub reservation_ttl_seconds: i64,
    pub chat_input_count_mode: String,
    /// Reasoning tokens reserved beyond the request's output cap (zero:
    /// reasoning falls within it).
    pub reasoning_allowance_tokens: i32,
    /// Whether the deployment takes function tools (D-032).
    pub tools: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PriceView {
    pub id: Uuid,
    pub currency: String,
    pub input_minor_per_token: i64,
    pub output_minor_per_token: i64,
    pub cached_input_minor_per_token: i64,
    pub reasoning_minor_per_token: i64,
    pub other_minor_per_token: i64,
    pub source: String,
    pub source_version: String,
    pub confidence: String,
}

/// One operation a deployment is catalogued for. Capability and price are
/// stored separately, so either may be absent; a route requires both.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploymentOperationView {
    pub operation: String,
    pub capability: Option<CapabilityView>,
    pub price: Option<PriceView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploymentView {
    pub id: Uuid,
    pub release_id: Uuid,
    pub provider_account_id: Uuid,
    pub adapter_instance_key: String,
    pub family: String,
    pub name: String,
    pub model: String,
    pub location: Option<String>,
    pub operations: Vec<DeploymentOperationView>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RouteView {
    pub id: Uuid,
    pub release_id: Uuid,
    pub alias: String,
    pub operation: String,
    pub deployment_id: Uuid,
    pub deployment_name: String,
    pub model: String,
    pub family: String,
    pub capability_id: Uuid,
    pub price_id: Uuid,
    /// This deployment's share of its priority's traffic (D-023).
    pub weight: u32,
    /// Lower is preferred.
    pub priority: u32,
}

/// A page of one release's rows. `release_id` is `None` only when the caller
/// did not name a release and the organization has no active release yet.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogPage<T> {
    pub release_id: Option<Uuid>,
    pub items: Vec<T>,
    pub next_page_token: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeploymentQuery {
    pub provider_account_id: Option<Uuid>,
    pub model: Option<String>,
}

impl DeploymentQuery {
    pub fn new(
        provider_account_id: Option<Uuid>,
        model: Option<String>,
    ) -> Result<Self, CatalogQueryError> {
        Ok(Self {
            provider_account_id,
            model: label(model)?,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RouteQuery {
    pub alias: Option<String>,
    pub operation: Option<CatalogOperation>,
}

impl RouteQuery {
    pub fn new(
        alias: Option<String>,
        operation: Option<String>,
    ) -> Result<Self, CatalogQueryError> {
        Ok(Self {
            alias: label(alias)?,
            operation: operation
                .map(|value| {
                    CatalogOperation::from_str(&value)
                        .map_err(|_| CatalogQueryError::InvalidOperation)
                })
                .transpose()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CatalogQueryError {
    #[error("text filters must be 1 to 255 characters")]
    InvalidLabel,
    #[error("operation must be chat, chat_stream, or embeddings")]
    InvalidOperation,
}

fn label(value: Option<String>) -> Result<Option<String>, CatalogQueryError> {
    match value {
        None => Ok(None),
        Some(value) if !value.is_empty() && value.len() <= MAX_LABEL_LENGTH => Ok(Some(value)),
        Some(_) => Err(CatalogQueryError::InvalidLabel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_validate_labels_and_operations() {
        assert_eq!(
            DeploymentQuery::new(None, Some(String::new())),
            Err(CatalogQueryError::InvalidLabel)
        );
        assert_eq!(
            RouteQuery::new(None, Some("images".into())),
            Err(CatalogQueryError::InvalidOperation)
        );
        assert_eq!(
            RouteQuery::new(Some("chat".into()), Some("chat_stream".into()))
                .unwrap()
                .operation,
            Some(CatalogOperation::ChatStream)
        );
    }
}
