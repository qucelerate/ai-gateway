#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
//! Integration tests for envelope encryption of stored secrets with AWS KMS,
//! against a local fake of the two KMS calls: data keys are created once
//! and shared by every process, secrets sealed under the environment key are
//! re-sealed automatically after the grace period, a changed KMS key rotates
//! the data key, and a KMS refusal stops startup.
//!
//! Run with `GATEWAY_TEST_DATABASE_URL` set; tests are skipped locally when it
//! is absent (matching the rest of the integration suite).

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::Engine;
use gateway::{
    ApiKeyPepper, CredentialKmsConfig, IdentityStore, OrganizationId,
    application::credential_keys::CredentialKeys,
    aws::AwsCredentials,
    domain::{
        catalog::ProviderFamily,
        credentials::{CredentialBinding, CredentialCipher, ProviderSecret, ProviderSecretInput},
    },
    persistence::credential_keys::CredentialKeyStore,
};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;
use zeroize::Zeroizing;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn test_database_url() -> Option<String> {
    match std::env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => Some(value),
        Err(_)
            if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() =>
        {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => None,
    }
}

/// A fake KMS. A "wrapped" key is JSON naming its KMS key and encryption
/// context with the plaintext; Decrypt refuses any mismatch, as KMS would.
#[derive(Clone, Default)]
struct FakeKms {
    calls: Arc<Mutex<Vec<String>>>,
    /// KMS keys that refuse every call.
    refused: Arc<Mutex<Vec<String>>>,
}

/// Re-signs the request as received and requires the same signature, so
/// a target, header or body changed after signing is caught. (The signing
/// algorithm itself is checked against an independent vector in `aws`.)
fn assert_signed(headers: &HeaderMap, body: &[u8]) {
    let header = |name: &str| headers[name].to_str().unwrap().to_owned();
    let url = url::Url::parse(&format!("http://{}/", header("host"))).unwrap();
    let (content_type, target) = (header("content-type"), header("x-amz-target"));
    let expected = gateway::aws::sign(
        &credentials(),
        "us-east-1",
        "kms",
        &gateway::aws::SignedRequest {
            method: "POST",
            url: &url,
            headers: &[("content-type", &content_type), ("x-amz-target", &target)],
            body,
        },
        &header("x-amz-date"),
    )
    .unwrap();
    assert_eq!(
        header("authorization"),
        expected[0].1.to_str().unwrap(),
        "the signature covers the request as sent"
    );
}

async fn kms(
    State(fake): State<FakeKms>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    assert_eq!(headers["content-type"], "application/x-amz-json-1.1");
    assert_signed(&headers, &body);
    let body: Value = serde_json::from_slice(&body).unwrap();
    let target = headers["x-amz-target"].to_str().unwrap().to_owned();
    fake.calls.lock().unwrap().push(target.clone());
    let key = body["KeyId"].as_str().unwrap().to_owned();
    if fake.refused.lock().unwrap().contains(&key) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"__type": "AccessDeniedException"})),
        );
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    match target.as_str() {
        "TrentService.GenerateDataKey" => {
            assert_eq!(body["KeySpec"], "AES_256");
            let plaintext: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
            let wrapped = json!({
                "key": key, "context": body["EncryptionContext"], "plaintext": b64.encode(&plaintext),
            })
            .to_string();
            (
                StatusCode::OK,
                Json(json!({
                    "KeyId": key,
                    "Plaintext": b64.encode(&plaintext),
                    "CiphertextBlob": b64.encode(wrapped),
                })),
            )
        }
        "TrentService.Decrypt" => {
            let wrapped: Value = serde_json::from_slice(
                &b64.decode(body["CiphertextBlob"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap();
            if wrapped["key"] != key || wrapped["context"] != body["EncryptionContext"] {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"__type": "InvalidCiphertextException"})),
                );
            }
            (
                StatusCode::OK,
                Json(json!({"KeyId": key, "Plaintext": wrapped["plaintext"]})),
            )
        }
        other => panic!("unexpected KMS call {other}"),
    }
}

impl FakeKms {
    async fn start() -> (Self, url::Url) {
        let fake = Self::default();
        let app = Router::new().route("/", post(kms)).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            fake,
            url::Url::parse(&format!("http://{address}/")).unwrap(),
        )
    }

    fn count(&self, target: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.ends_with(target))
            .count()
    }
}

fn credentials() -> AwsCredentials {
    AwsCredentials {
        access_key_id: "AKIDTEST".to_owned(),
        secret_access_key: Zeroizing::new("secret".to_owned()),
        session_token: None,
    }
}

fn config(endpoint: &url::Url, key_id: &str) -> CredentialKmsConfig {
    CredentialKmsConfig {
        key_id: key_id.to_owned(),
        region: "us-east-1".to_owned(),
        endpoint: endpoint.clone(),
        credentials: credentials(),
    }
}

struct Harness {
    pool: PgPool,
    store: CredentialKeyStore,
    org: OrganizationId,
    /// Holds secrets that no key opens.
    other_org: OrganizationId,
}

impl Harness {
    /// Data keys are gateway-wide, so every test starts without any.
    async fn new(url: &str) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await
            .unwrap_or_else(|error| panic!("GATEWAY_TEST_DATABASE_URL is unreachable: {error}"));
        gateway::persistence::migrations::run(&pool)
            .await
            .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
        sqlx::query("DELETE FROM gateway.credential_data_key")
            .execute(&pool)
            .await
            .unwrap();
        let identity = IdentityStore::new(
            pool.clone(),
            ApiKeyPepper::from_bytes(b"kms-integration-pepper").unwrap(),
        );
        let suffix = Uuid::new_v4().simple().to_string();
        let org = identity
            .create_organization(&format!("kms-{suffix}"), "KMS")
            .await
            .unwrap()
            .id;
        let other_org = identity
            .create_organization(&format!("kms-other-{suffix}"), "KMS other")
            .await
            .unwrap()
            .id;
        Self {
            store: CredentialKeyStore::new(pool.clone()),
            pool,
            org,
            other_org,
        }
    }

    fn binding(&self) -> CredentialBinding<'static> {
        CredentialBinding {
            organization_id: self.org,
            adapter_instance_key: "openai-main",
            family: ProviderFamily::OpenAi,
        }
    }

    /// A provider credential and a webhook secret sealed under `cipher`.
    async fn seal(&self, cipher: &CredentialCipher) -> Uuid {
        let secret = ProviderSecret::for_family(
            ProviderFamily::OpenAi,
            ProviderSecretInput {
                api_key: Some(Zeroizing::new("sk-test-123".to_owned())),
                ..ProviderSecretInput::default()
            },
        )
        .unwrap();
        let sealed = cipher.seal(self.binding(), &secret).unwrap();
        sqlx::query(
            "INSERT INTO gateway.provider_credential
                 (id, organization_id, adapter_instance_key, family, key_id, ciphertext)
             VALUES ($1, $2, 'openai-main', 'openai', $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(self.org.0)
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .execute(&self.pool)
        .await
        .unwrap();
        let endpoint = Uuid::new_v4();
        let sealed = cipher
            .seal_webhook_secret(self.org, endpoint, b"whsec-test")
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.webhook_endpoint
                 (id, organization_id, url, event_types, secret_key_id, secret_ciphertext)
             VALUES ($1, $2, 'https://hooks.example.test/', ARRAY['inference.finalized'], $3, $4)",
        )
        .bind(endpoint)
        .bind(self.org.0)
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .execute(&self.pool)
        .await
        .unwrap();
        endpoint
    }

    /// An identity-provider configuration whose client secret is sealed
    /// under `cipher`.
    async fn seal_identity_provider(&self, cipher: &CredentialCipher) -> Uuid {
        let configuration = Uuid::new_v4();
        let sealed = cipher
            .seal_identity_provider_secret(self.org, configuration, "idp-client-secret")
            .unwrap();
        sqlx::query(
            "INSERT INTO gateway.identity_provider_configuration
                 (id, organization_id, version, issuer, client_id, secret_key_id, secret_ciphertext)
             VALUES ($1, $2, 1, 'https://idp.example.test', 'client', $3, $4)",
        )
        .bind(configuration)
        .bind(self.org.0)
        .bind(&sealed.key_id)
        .bind(&sealed.ciphertext)
        .execute(&self.pool)
        .await
        .unwrap();
        configuration
    }

    /// The identity-provider secret opens with `cipher`, unchanged, and the
    /// key id it is sealed under.
    async fn assert_identity_provider_open(
        &self,
        cipher: &CredentialCipher,
        configuration: Uuid,
    ) -> String {
        let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
            "SELECT secret_key_id, secret_ciphertext FROM gateway.identity_provider_configuration
             WHERE id = $1",
        )
        .bind(configuration)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let secret = cipher
            .open_identity_provider_secret(self.org, configuration, &key_id, &ciphertext)
            .unwrap();
        assert_eq!(secret.as_str(), "idp-client-secret");
        key_id
    }

    /// The key ids this organization's secrets are sealed under.
    async fn sealed_under(&self) -> (String, String) {
        sqlx::query_as(
            "SELECT c.key_id, w.secret_key_id FROM gateway.provider_credential c
             JOIN gateway.webhook_endpoint w ON w.organization_id = c.organization_id
             WHERE c.organization_id = $1",
        )
        .bind(self.org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Both secrets open with `cipher` and are unchanged.
    async fn assert_open(&self, cipher: &CredentialCipher, endpoint: Uuid) {
        let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
            "SELECT key_id, ciphertext FROM gateway.provider_credential WHERE organization_id = $1",
        )
        .bind(self.org.0)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let secret = cipher.open(self.binding(), &key_id, &ciphertext).unwrap();
        assert!(
            matches!(secret, ProviderSecret::ApiKey { ref api_key } if api_key.as_str() == "sk-test-123")
        );
        let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
            "SELECT secret_key_id, secret_ciphertext FROM gateway.webhook_endpoint WHERE id = $1",
        )
        .bind(endpoint)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        let secret = cipher
            .open_webhook_secret(self.org, endpoint, &key_id, &ciphertext)
            .unwrap();
        assert_eq!(secret.as_slice(), b"whsec-test");
    }

    async fn data_keys(&self) -> Vec<(String, String, bool)> {
        sqlx::query_as(
            "SELECT key_id, kms_key_id, retired_at IS NOT NULL FROM gateway.credential_data_key
             ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    /// Ages every data key past the re-seal grace period.
    async fn age_data_keys(&self) {
        sqlx::query(
            "UPDATE gateway.credential_data_key SET created_at = created_at - interval '10 minutes'",
        )
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn secrets_move_to_a_kms_data_key_automatically() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let (fake, endpoint) = FakeKms::start().await;
    let env = CredentialCipher::new(&[7_u8; 32]).unwrap();
    let webhook = harness.seal(&env).await;

    // The first process creates the data key; the environment key still
    // opens what it sealed.
    let keys = CredentialKeys::start(
        harness.store.clone(),
        &config(&endpoint, "alias/gateway"),
        Some(env.clone()),
    )
    .await
    .unwrap();
    let cipher = keys.cipher();
    assert_ne!(cipher.key_id(), env.key_id());
    assert_eq!(fake.count("GenerateDataKey"), 1);
    let data_keys = harness.data_keys().await;
    assert_eq!(data_keys.len(), 1);
    assert_eq!(data_keys[0].0, cipher.key_id());
    harness.assert_open(&cipher, webhook).await;

    // Concurrent processes share it.
    let shared = config(&endpoint, "alias/gateway");
    let (a, b) = tokio::join!(
        CredentialKeys::start(harness.store.clone(), &shared, None),
        CredentialKeys::start(harness.store.clone(), &shared, None),
    );
    assert_eq!(a.unwrap().cipher().key_id(), cipher.key_id());
    assert_eq!(b.unwrap().cipher().key_id(), cipher.key_id());
    assert_eq!(fake.count("GenerateDataKey"), 1);

    // A secret that cannot be opened is skipped, not retried forever.
    sqlx::query(
        "INSERT INTO gateway.webhook_endpoint
             (id, organization_id, url, event_types, secret_key_id, secret_ciphertext)
         SELECT gen_random_uuid(), $1, 'https://hooks.example.test/broken',
                ARRAY['inference.finalized'], $2, decode(repeat('00', 40), 'hex')
         FROM generate_series(1, 101)",
    )
    .bind(harness.other_org.0)
    .bind(env.key_id())
    .execute(&harness.pool)
    .await
    .unwrap();

    // Nothing is re-sealed until every process has had time to load it.
    assert_eq!(keys.reseal().await.unwrap(), 0);
    assert_eq!(harness.sealed_under().await, (env.key_id(), env.key_id()));
    harness.age_data_keys().await;
    keys.refresh().await.unwrap();
    assert!(keys.reseal().await.unwrap() >= 2);
    assert_eq!(
        harness.sealed_under().await,
        (cipher.key_id(), cipher.key_id())
    );
    harness.assert_open(&cipher, webhook).await;
    // Each re-seal is in the audit log, by the system, naming the secret.
    let resealed: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT action, actor_type, entity_id FROM gateway.audit_event
         WHERE organization_id = $1 AND action LIKE '%resealed' ORDER BY action",
    )
    .bind(harness.org.0)
    .fetch_all(&harness.pool)
    .await
    .unwrap();
    assert_eq!(resealed.len(), 2, "{resealed:?}");
    assert_eq!(resealed[0].0, "provider_credential.resealed");
    assert_eq!(
        (resealed[1].0.as_str(), resealed[1].2),
        ("webhook_endpoint.secret_resealed", webhook)
    );
    assert!(resealed.iter().all(|(_, actor, _)| actor == "system"));
    // A process with the KMS keyring but without the environment key opens
    // everything now; the environment key alone no longer does.
    let kms_only = CredentialKeys::start(
        harness.store.clone(),
        &config(&endpoint, "alias/gateway"),
        None,
    )
    .await
    .unwrap();
    harness.assert_open(&kms_only.cipher(), webhook).await;
    let (key_id, ciphertext): (String, Vec<u8>) = sqlx::query_as(
        "SELECT key_id, ciphertext FROM gateway.provider_credential WHERE organization_id = $1",
    )
    .bind(harness.org.0)
    .fetch_one(&harness.pool)
    .await
    .unwrap();
    assert!(env.open(harness.binding(), &key_id, &ciphertext).is_err());
}

#[tokio::test]
async fn a_new_kms_key_rotates_the_data_key_and_retires_the_old() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let (fake, endpoint) = FakeKms::start().await;
    let first = CredentialKeys::start(harness.store.clone(), &config(&endpoint, "key-1"), None)
        .await
        .unwrap();
    let webhook = harness.seal(&first.cipher()).await;
    let identity_provider = harness.seal_identity_provider(&first.cipher()).await;
    let old_key = first.cipher().key_id();

    // A process configured with another KMS key creates a new data key and
    // keeps the old one to open what it sealed.
    let second = CredentialKeys::start(harness.store.clone(), &config(&endpoint, "key-2"), None)
        .await
        .unwrap();
    let new_key = second.cipher().key_id();
    assert_ne!(new_key, old_key);
    assert_eq!(fake.count("GenerateDataKey"), 2);
    harness.assert_open(&second.cipher(), webhook).await;
    // The newest data key is active everywhere, also in a process still
    // configured with the old KMS key, so they never re-seal back and forth.
    first.refresh().await.unwrap();
    assert_eq!(first.cipher().key_id(), new_key);

    harness.age_data_keys().await;
    // The old-config worker re-seals forward and retires the old key, the
    // identity provider's client secret included.
    assert!(first.reseal().await.unwrap() >= 3);
    assert_eq!(
        harness.sealed_under().await,
        (new_key.clone(), new_key.clone())
    );
    let data_keys = harness.data_keys().await;
    assert_eq!(
        data_keys,
        [
            (old_key.clone(), "key-1".to_owned(), true),
            (new_key.clone(), "key-2".to_owned(), false)
        ]
    );
    // The retired key leaves every keyring at its next reload, and the
    // newest key is never retired.
    second.refresh().await.unwrap();
    assert_eq!(second.cipher().key_ids(), std::slice::from_ref(&new_key));
    harness.assert_open(&second.cipher(), webhook).await;
    assert_eq!(
        harness
            .assert_identity_provider_open(&second.cipher(), identity_provider)
            .await,
        new_key
    );
    assert_eq!(second.reseal().await.unwrap(), 0);
    assert_eq!(
        harness.data_keys().await[1],
        (new_key, "key-2".to_owned(), false)
    );
}

#[tokio::test]
async fn startup_fails_closed_when_kms_refuses() {
    let _guard = database_guard();
    let Some(url) = test_database_url() else {
        return;
    };
    let harness = Harness::new(&url).await;
    let (fake, endpoint) = FakeKms::start().await;
    fake.refused.lock().unwrap().push("key-denied".to_owned());
    assert!(
        CredentialKeys::start(
            harness.store.clone(),
            &config(&endpoint, "key-denied"),
            None
        )
        .await
        .is_err()
    );
    assert!(harness.data_keys().await.is_empty());

    // A data key whose KMS key is later refused cannot become active.
    CredentialKeys::start(harness.store.clone(), &config(&endpoint, "key-ok"), None)
        .await
        .unwrap();
    fake.refused.lock().unwrap().push("key-ok".to_owned());
    assert!(
        CredentialKeys::start(harness.store.clone(), &config(&endpoint, "key-ok"), None)
            .await
            .is_err()
    );
}
