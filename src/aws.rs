//! AWS request signing (Signature Version 4) and a minimal AWS KMS client.
//!
//! The signer is shared by the Bedrock adapter and the KMS client. The KMS
//! client implements the two calls envelope encryption needs:
//! `GenerateDataKey` and `Decrypt`. Credentials are static (access key, secret,
//! optional session token); short-lived role credentials are a later phase.

use std::{fmt, time::Duration};

use base64::Engine;
use reqwest::{Url, header};
use serde_json::{Value, json};
use zeroize::Zeroizing;

/// Static AWS credentials. The secret and token are redacted from `Debug`.
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: Zeroizing<String>,
    pub session_token: Option<Zeroizing<String>>,
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &"REDACTED")
            .field("secret_access_key", &"REDACTED")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "REDACTED"),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SigningError;

/// One request to sign: the exact bytes about to be sent, and the headers
/// besides `host`, `x-amz-date` and `x-amz-security-token` that the
/// signature covers (lower-case names).
pub struct SignedRequest<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

/// The canonical URI of `path`, the path as sent: every service but S3
/// signs each segment URI-encoded again, so a model ID's `:`, sent as
/// `%3A`, is signed as `%253A`.
fn canonical_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                        char::from(b).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// SigV4 headers (`authorization`, `x-amz-date`, and the session token when
/// present) for `request`. The URL's query must already be in canonical
/// form.
pub fn sign(
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    request: &SignedRequest<'_>,
    amz_date: &str,
) -> Result<Vec<(header::HeaderName, header::HeaderValue)>, SigningError> {
    let date_stamp = amz_date.get(..8).ok_or(SigningError)?;
    let host_name = request.url.host_str().ok_or(SigningError)?;
    let host = match request.url.port() {
        Some(port) => format!("{host_name}:{port}"),
        None => host_name.to_owned(),
    };
    let mut signed: Vec<(&str, &str)> = request.headers.to_vec();
    signed.push(("host", &host));
    signed.push(("x-amz-date", amz_date));
    if let Some(token) = &credentials.session_token {
        signed.push(("x-amz-security-token", token.as_str()));
    }
    signed.sort_unstable_by_key(|(name, _)| *name);
    let canonical_headers: String = signed
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let signed_headers = signed
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method,
        canonical_path(request.url.path()),
        request.url.query().unwrap_or(""),
        sha256_hex(request.body)
    );
    let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key.as_str()).as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    let mut authorization = header::HeaderValue::from_str(&format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    ))
    .map_err(|_| SigningError)?;
    authorization.set_sensitive(true);
    let mut headers = vec![
        (header::AUTHORIZATION, authorization),
        (
            header::HeaderName::from_static("x-amz-date"),
            header::HeaderValue::from_str(amz_date).map_err(|_| SigningError)?,
        ),
    ];
    if let Some(token) = &credentials.session_token {
        let mut value = header::HeaderValue::from_str(token).map_err(|_| SigningError)?;
        value.set_sensitive(true);
        headers.push((
            header::HeaderName::from_static("x-amz-security-token"),
            value,
        ));
    }
    Ok(headers)
}

pub fn amz_date_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_encode(&Sha256::digest(data))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts every non-empty key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Why a KMS call failed. Never carries the response body, which can name
/// the key or account.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum KmsError {
    #[error("KMS request could not be signed or sent")]
    Request,
    #[error("KMS refused the request (HTTP {0})")]
    Status(u16),
    #[error("KMS response was not understood")]
    Response,
}

/// A data key fresh from `GenerateDataKey`: the plaintext to use, and the
/// ciphertext blob to store.
pub struct DataKey {
    pub plaintext: Zeroizing<Vec<u8>>,
    pub wrapped: Vec<u8>,
}

/// The two AWS KMS calls envelope encryption needs.
#[derive(Clone, Debug)]
pub struct KmsClient {
    client: reqwest::Client,
    endpoint: Url,
    region: String,
    credentials: AwsCredentials,
}

const KMS_CONTENT_TYPE: &str = "application/x-amz-json-1.1";
const KMS_TIMEOUT: Duration = Duration::from_secs(10);

impl KmsClient {
    /// `endpoint` is `https://kms.{region}.amazonaws.com/` outside tests.
    pub fn new(
        endpoint: Url,
        region: String,
        credentials: AwsCredentials,
    ) -> Result<Self, KmsError> {
        let client = reqwest::Client::builder()
            .timeout(KMS_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| KmsError::Request)?;
        Ok(Self {
            client,
            endpoint,
            region,
            credentials,
        })
    }

    pub fn default_endpoint(region: &str) -> Option<Url> {
        Url::parse(&format!("https://kms.{region}.amazonaws.com/")).ok()
    }

    /// A new 256-bit data key under `key_id` (a key id, ARN or alias),
    /// bound to `context` as KMS encryption context.
    pub async fn generate_data_key(
        &self,
        key_id: &str,
        context: &[(&str, &str)],
    ) -> Result<DataKey, KmsError> {
        let response = self
            .call(
                "GenerateDataKey",
                json!({
                    "KeyId": key_id,
                    "KeySpec": "AES_256",
                    "EncryptionContext": encryption_context(context),
                }),
            )
            .await?;
        Ok(DataKey {
            plaintext: decode_blob(&response, "Plaintext")?,
            wrapped: decode_blob(&response, "CiphertextBlob")?.to_vec(),
        })
    }

    /// Unwraps a data key. `key_id` pins the KMS key, so a blob wrapped
    /// under another key is refused rather than decrypted.
    pub async fn decrypt(
        &self,
        key_id: &str,
        wrapped: &[u8],
        context: &[(&str, &str)],
    ) -> Result<Zeroizing<Vec<u8>>, KmsError> {
        let response = self
            .call(
                "Decrypt",
                json!({
                    "KeyId": key_id,
                    "CiphertextBlob": base64::engine::general_purpose::STANDARD.encode(wrapped),
                    "EncryptionContext": encryption_context(context),
                }),
            )
            .await?;
        decode_blob(&response, "Plaintext")
    }

    async fn call(&self, action: &str, body: Value) -> Result<Value, KmsError> {
        let body = serde_json::to_vec(&body).map_err(|_| KmsError::Request)?;
        let target = format!("TrentService.{action}");
        let headers = sign(
            &self.credentials,
            &self.region,
            "kms",
            &SignedRequest {
                method: "POST",
                url: &self.endpoint,
                headers: &[
                    ("content-type", KMS_CONTENT_TYPE),
                    ("x-amz-target", &target),
                ],
                body: &body,
            },
            &amz_date_now(),
        )
        .map_err(|_| KmsError::Request)?;
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(header::CONTENT_TYPE, KMS_CONTENT_TYPE)
            .header("x-amz-target", &target)
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|_| KmsError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(KmsError::Status(status.as_u16()));
        }
        let bytes = response.bytes().await.map_err(|_| KmsError::Response)?;
        if bytes.len() > 64 * 1024 {
            return Err(KmsError::Response);
        }
        serde_json::from_slice(&bytes).map_err(|_| KmsError::Response)
    }
}

fn encryption_context(context: &[(&str, &str)]) -> Value {
    Value::Object(
        context
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Value::String((*value).to_owned())))
            .collect(),
    )
}

fn decode_blob(response: &Value, field: &str) -> Result<Zeroizing<Vec<u8>>, KmsError> {
    let encoded = response
        .get(field)
        .and_then(Value::as_str)
        .ok_or(KmsError::Response)?;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map(Zeroizing::new)
        .map_err(|_| KmsError::Response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(token: Option<&str>) -> AwsCredentials {
        AwsCredentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
            session_token: token.map(|token| Zeroizing::new(token.to_owned())),
        }
    }

    fn authorization(headers: &[(header::HeaderName, header::HeaderValue)]) -> &str {
        headers
            .iter()
            .find(|(name, _)| *name == header::AUTHORIZATION)
            .map(|(_, value)| value.to_str().unwrap())
            .unwrap()
    }

    /// The signature for a KMS `Decrypt` call, computed independently with
    /// Python's hmac and hashlib.
    #[test]
    fn kms_requests_sign_their_target_header() {
        let url = Url::parse("https://kms.us-east-1.amazonaws.com/").unwrap();
        let headers = sign(
            &credentials(None),
            "us-east-1",
            "kms",
            &SignedRequest {
                method: "POST",
                url: &url,
                headers: &[
                    ("content-type", KMS_CONTENT_TYPE),
                    ("x-amz-target", "TrentService.Decrypt"),
                ],
                body: br#"{"CiphertextBlob":"AAAA"}"#,
            },
            "20150830T123600Z",
        )
        .unwrap();
        assert_eq!(
            authorization(&headers),
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/kms/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date;x-amz-target, \
             Signature=f63eadd7cdfdb0d3495fffa458423d4ee9d7d28ea482ecb8719fda808adfe410"
        );
        assert!(headers[0].1.is_sensitive());
    }

    #[test]
    fn session_tokens_are_signed_and_redacted() {
        let url = Url::parse("https://kms.us-east-1.amazonaws.com/").unwrap();
        let credentials = credentials(Some("token-123"));
        let headers = sign(
            &credentials,
            "us-east-1",
            "kms",
            &SignedRequest {
                method: "POST",
                url: &url,
                headers: &[("content-type", KMS_CONTENT_TYPE)],
                body: b"{}",
            },
            "20150830T123600Z",
        )
        .unwrap();
        assert!(
            authorization(&headers)
                .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token,")
        );
        let token = headers
            .iter()
            .find(|(name, _)| name.as_str() == "x-amz-security-token")
            .unwrap();
        assert!(token.1.is_sensitive());
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("token-123") && !debug.contains("EXAMPLEKEY"));
    }
}
