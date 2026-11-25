//! Usage export to an organization's own object storage (D-014).
//!
//! A destination is a bucket (or Azure container) and a prefix, written
//! through one of the organization's stored cloud credentials. Records are
//! metadata only, sealed into batches of one hour of `finalized_at` and
//! uploaded as gzip NDJSON under names fixed when the batch was sealed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::identity::OrganizationId;

/// The `schema` field of every record.
pub const RECORD_SCHEMA: &str = "usage-export/v1";
/// The object a destination is verified with.
pub const VERIFY_OBJECT: &str = "_gateway-verify.json";
/// Most records in one object.
pub const MAX_BATCH_RECORDS: i64 = 10_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    S3,
    Gcs,
    AzureBlob,
}

impl StorageKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::AzureBlob => "azure_blob",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "s3" => Some(Self::S3),
            "gcs" => Some(Self::Gcs),
            "azure_blob" => Some(Self::AzureBlob),
            _ => None,
        }
    }
}

/// Where an organization's usage goes, as an admin sets it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationInput {
    pub kind: StorageKind,
    /// The adapter instance key of the stored credential to write with.
    pub connection: String,
    /// The bucket, or the Azure container.
    pub bucket: String,
    /// The Azure storage account (Azure only).
    #[serde(default)]
    pub account: Option<String>,
    /// The bucket's region (S3 only).
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DestinationError {
    #[error("connection must name a stored credential")]
    Connection,
    #[error("bucket is not a valid name for the storage kind")]
    Bucket,
    #[error(
        "account is required for azure_blob, and only for it: 3 to 24 lowercase letters and digits"
    )]
    Account,
    #[error("region is required for s3, and only for it")]
    Region,
    #[error(
        "prefix may hold letters, digits and - _ . = / only, up to 512 characters, \
         without empty, '.' or '..' segments or a leading or trailing '/'"
    )]
    Prefix,
}

impl DestinationInput {
    /// The input with surrounding whitespace removed, if it is valid.
    pub fn validate(mut self) -> Result<Self, DestinationError> {
        self.connection = self.connection.trim().to_owned();
        self.bucket = self.bucket.trim().to_owned();
        self.prefix = self.prefix.trim().to_owned();
        self.account = self.account.map(|value| value.trim().to_owned());
        self.region = self.region.map(|value| value.trim().to_owned());
        if self.connection.is_empty()
            || self.connection.len() > 128
            || self.connection.chars().any(char::is_control)
        {
            return Err(DestinationError::Connection);
        }
        let bucket_ok = match self.kind {
            StorageKind::S3 => s3_bucket(&self.bucket),
            StorageKind::Gcs => gcs_bucket(&self.bucket),
            StorageKind::AzureBlob => azure_container(&self.bucket),
        };
        if !bucket_ok {
            return Err(DestinationError::Bucket);
        }
        match (self.kind, self.account.as_deref()) {
            (StorageKind::AzureBlob, Some(account)) if azure_account(account) => {}
            (StorageKind::S3 | StorageKind::Gcs, None) => {}
            _ => return Err(DestinationError::Account),
        }
        match (self.kind, self.region.as_deref()) {
            (StorageKind::S3, Some(region)) if aws_region(region) => {}
            (StorageKind::Gcs | StorageKind::AzureBlob, None) => {}
            _ => return Err(DestinationError::Region),
        }
        if !prefix(&self.prefix) {
            return Err(DestinationError::Prefix);
        }
        Ok(self)
    }

    /// The full name of `name` under the prefix.
    pub fn object(&self, name: &str) -> String {
        object_name(&self.prefix, name)
    }
}

/// `name` under `prefix`.
pub fn object_name(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// How long uploads must have failed before an export is reported failing.
pub const FAILING_AFTER: chrono::Duration = chrono::Duration::hours(1);

/// `<organization=…>/date=YYYY-MM-DD/hour=HH/<first>-<last>.ndjson.gz`,
/// relative to the destination's prefix.
pub fn batch_name(
    organization: OrganizationId,
    hour: DateTime<Utc>,
    first_sequence: i64,
    last_sequence: i64,
) -> String {
    format!(
        "organization={}/date={}/hour={}/{first_sequence}-{last_sequence}.ndjson.gz",
        organization.0,
        hour.format("%Y-%m-%d"),
        hour.format("%H"),
    )
}

/// An organization's export as the admin API shows it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageExportView {
    pub id: Uuid,
    pub kind: StorageKind,
    pub connection: String,
    pub bucket: String,
    pub account: Option<String>,
    pub region: Option<String>,
    pub prefix: String,
    pub enabled_at: DateTime<Utc>,
    pub verified_at: DateTime<Utc>,
    pub last_success_at: Option<DateTime<Utc>>,
    /// Uploads have failed for an hour.
    pub failing: bool,
    pub failing_since: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    /// Records waiting to be written, sealed or not.
    pub queued_records: i64,
    pub oldest_queued_at: Option<DateTime<Utc>>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The `usage_export.failing` event's payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageExportFailing {
    pub export_id: Uuid,
    pub kind: StorageKind,
    pub failing_since: DateTime<Utc>,
    /// A short summary, never a credential or a provider response body.
    pub last_error: Option<String>,
    pub queued_records: i64,
}

/// One line of an exported object: a finalized inference's attribution,
/// usage and cost, as of when its `inference.finalized` event was
/// dispatched. Metadata only; never prompts, completions or provider
/// responses. A trace finalized as `unknown` and settled later appears
/// again; the record with the latest `finalized_at` per `trace_id` holds.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageRecord {
    /// Always [`RECORD_SCHEMA`].
    pub schema: String,
    /// The outbox event; unique per record.
    pub event_id: Uuid,
    pub trace_id: Uuid,
    pub reservation_id: Uuid,
    pub organization_id: Uuid,
    pub team_id: Option<Uuid>,
    pub service_account_id: Option<Uuid>,
    /// The app's own id of the end user the request was for (D-028);
    /// `null` when it named none.
    pub end_user: Option<String>,
    /// The catalog operation (`chat`, `chat_stream`, `embeddings`...);
    /// `inference` for a request that was not routed by the catalog.
    pub operation: String,
    pub deployment: Option<String>,
    pub model: String,
    pub provider_family: Option<String>,
    pub location: Option<String>,
    /// `settled`, `unknown`, `bound-breach`, `expired`, `input-count-drift`,
    /// `operator-resolved`, `failed-over` (an attempt released when its
    /// request failed over to another deployment, D-023), or `refused`
    /// (released at zero: every deployment tried refused it before any
    /// work).
    pub outcome: String,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub other_tokens: Option<i64>,
    /// `exact`, `estimated` or `unknown`; absent when no usage was recorded.
    pub usage_confidence: Option<String>,
    /// `complete`, `partial` or `unavailable`.
    pub usage_completeness: Option<String>,
    pub pricing_confidence: Option<String>,
    pub amount_minor: Option<i64>,
    pub currency: String,
    /// When the request was admitted (its reservation was made).
    pub recorded_at: DateTime<Utc>,
    pub finalized_at: DateTime<Utc>,
    /// The batch the request was a line of (`batch_<uuid>`, D-021); `null`
    /// for a request made on its own.
    #[serde(default)]
    pub batch_id: Option<String>,
    /// The line's `custom_id` in its batch.
    #[serde(default)]
    pub custom_id: Option<String>,
    /// The master record (one model across providers, D-033) its
    /// deployment was linked to when it was served; `null` for none.
    #[serde(default)]
    pub model_master: Option<String>,
}

/// Why an object could not be written. Each is a fixed phrase: none
/// carries a credential, a URL or a storage service's response.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WriteError {
    #[error("connection not found")]
    ConnectionNotFound,
    #[error("connection cannot write to this kind of storage")]
    ConnectionKind,
    #[error("connection's cloud principal is not approved")]
    PrincipalNotApproved,
    #[error("connection could not be opened")]
    ConnectionUnreadable,
    #[error("the gateway has no identity in this cloud")]
    NoIdentity,
    #[error("credentials could not be obtained")]
    Credentials,
    #[error("storage denied the write")]
    Denied,
    #[error("bucket or container not found")]
    NotFound,
    #[error("bucket is in another region")]
    WrongRegion,
    #[error("storage returned an unexpected status")]
    Status,
    #[error("request timed out")]
    Timeout,
    #[error("connection to storage failed")]
    Connect,
    #[error("request failed")]
    Request,
}

/// Writes objects to an organization's storage through its connection.
#[async_trait::async_trait]
pub trait ObjectWriter: Send + Sync {
    /// Writes `body` as `name` (the full object name, prefix included),
    /// replacing any object of that name.
    async fn write(
        &self,
        organization: OrganizationId,
        destination: &DestinationInput,
        name: &str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> Result<(), WriteError>;
}

fn lower_alnum(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

/// S3 bucket naming rules, without the legacy forms.
fn s3_bucket(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| lower_alnum(*b) || matches!(b, b'.' | b'-'))
        && lower_alnum(bytes[0])
        && lower_alnum(bytes[bytes.len() - 1])
        && !value.contains("..")
        && !value.contains(".-")
        && !value.contains("-.")
        && !value
            .split('.')
            .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
        && !value.starts_with("xn--")
}

/// Cloud Storage bucket names: up to 63 characters, or 222 in dotted
/// components of at most 63.
fn gcs_bucket(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=222).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| lower_alnum(*b) || matches!(b, b'.' | b'-' | b'_'))
        && lower_alnum(bytes[0])
        && lower_alnum(bytes[bytes.len() - 1])
        && (bytes.len() <= 63 || value.split('.').all(|part| part.len() <= 63))
        && !value.starts_with("goog")
        && !value.contains("google")
}

fn azure_container(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=63).contains(&bytes.len())
        && bytes.iter().all(|b| lower_alnum(*b) || *b == b'-')
        && lower_alnum(bytes[0])
        && lower_alnum(bytes[bytes.len() - 1])
        && !value.contains("--")
}

fn azure_account(value: &str) -> bool {
    (3..=24).contains(&value.len()) && value.bytes().all(lower_alnum)
}

/// `us-east-1`, `eu-central-2`, `us-gov-west-1`, `cn-north-1`, ...
pub(crate) fn aws_region(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    (3..=4).contains(&parts.len())
        && parts[..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase()))
        && !parts[parts.len() - 1].is_empty()
        && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_digit())
}

/// Characters that need no escaping in any of the three services' paths, so
/// a name is signed and sent exactly as written.
fn prefix(value: &str) -> bool {
    value.is_empty()
        || (value.len() <= 512
            && value.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'=' | b'/')
            })
            && value
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "." && segment != ".."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: StorageKind, bucket: &str) -> DestinationInput {
        DestinationInput {
            kind,
            connection: "bedrock-main".into(),
            bucket: bucket.into(),
            account: (kind == StorageKind::AzureBlob).then(|| "usagestore".into()),
            region: (kind == StorageKind::S3).then(|| "eu-west-1".into()),
            prefix: "warehouse/gateway".into(),
        }
    }

    #[test]
    fn destinations_are_validated_per_kind() {
        assert!(input(StorageKind::S3, "acme-usage").validate().is_ok());
        assert!(
            input(StorageKind::Gcs, "acme_usage.example.com")
                .validate()
                .is_ok()
        );
        assert!(
            input(StorageKind::AzureBlob, "usage-export")
                .validate()
                .is_ok()
        );
        for (kind, bucket) in [
            (StorageKind::S3, "Acme"),
            (StorageKind::S3, "a..b"),
            (StorageKind::S3, "192.168.1.1"),
            (StorageKind::S3, "ab"),
            (StorageKind::Gcs, "google-usage"),
            (StorageKind::Gcs, "-usage"),
            (StorageKind::AzureBlob, "usage--export"),
            (StorageKind::AzureBlob, "usage_export"),
        ] {
            assert_eq!(
                input(kind, bucket).validate(),
                Err(DestinationError::Bucket),
                "{kind:?} {bucket}"
            );
        }
        let mut s3 = input(StorageKind::S3, "acme-usage");
        s3.region = None;
        assert_eq!(s3.validate(), Err(DestinationError::Region));
        let mut gcs = input(StorageKind::Gcs, "acme-usage");
        gcs.account = Some("usagestore".into());
        assert_eq!(gcs.validate(), Err(DestinationError::Account));
        let mut blob = input(StorageKind::AzureBlob, "usage");
        blob.account = Some("Usage-Store".into());
        assert_eq!(blob.validate(), Err(DestinationError::Account));
        let mut blank = input(StorageKind::Gcs, "acme-usage");
        blank.connection = "  ".into();
        assert_eq!(blank.validate(), Err(DestinationError::Connection));
    }

    #[test]
    fn prefixes_need_no_escaping() {
        for good in [
            "",
            "a",
            "warehouse/gateway",
            "raw/source=gateway",
            "v1.2/x_y",
        ] {
            let mut value = input(StorageKind::Gcs, "acme-usage");
            value.prefix = good.into();
            assert!(value.validate().is_ok(), "{good}");
        }
        for bad in [
            "/a", "a/", "a//b", "a/../b", "./a", "a b", "a%2Fb", "a?b", "ä",
        ] {
            let mut value = input(StorageKind::Gcs, "acme-usage");
            value.prefix = bad.into();
            assert_eq!(value.validate(), Err(DestinationError::Prefix), "{bad}");
        }
    }

    #[test]
    fn batch_names_are_partitioned_by_hour() {
        let org = OrganizationId(Uuid::nil());
        let hour = "2026-10-27T09:00:00Z".parse().unwrap();
        let name = batch_name(org, hour, 41, 97);
        assert_eq!(
            name,
            "organization=00000000-0000-0000-0000-000000000000/date=2026-10-27/hour=09/41-97.ndjson.gz"
        );
        let destination = input(StorageKind::Gcs, "acme-usage").validate().unwrap();
        assert_eq!(
            destination.object(&name),
            format!("warehouse/gateway/{name}")
        );
        let mut bare = destination.clone();
        bare.prefix.clear();
        assert_eq!(bare.object(VERIFY_OBJECT), VERIFY_OBJECT);
    }
}
