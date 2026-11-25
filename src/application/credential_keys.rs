//! The keyring of stored secrets, backed by AWS KMS when configured.
//!
//! Without `GATEWAY_CREDENTIAL_KMS_KEY_ID`, secrets are sealed under
//! `GATEWAY_CREDENTIAL_ENCRYPTION_KEY` as before. With it, they are sealed
//! under a data key that KMS generated (envelope encryption): only the
//! KMS-wrapped data key is stored (`credential_data_key`), and every process
//! unwraps it through KMS at startup, so the database alone never yields a
//! secret. The first process that starts with a KMS key for which no data
//! key exists creates one, under an advisory lock; a changed KMS key gets a
//! new data key the same way.
//!
//! The newest live data key is the active one for every process, whichever
//! KMS key that process is configured with: the configuration only decides
//! which data key a process creates. So during a rolling change of KMS key
//! all processes agree on what to seal under (each needs decrypt access to
//! both KMS keys until the change is done). Each process reloads the data
//! keys every minute, so all of them learn a new active key within
//! [`REFRESH_INTERVAL`]. The worker waits [`RESEAL_GRACE`] after a data key
//! appears, then re-seals every secret not under it, and retires older data
//! keys no secret uses; it reloads first, so it never acts on a stale
//! keyring. The environment key, when still set, stays in the keyring to
//! open what it sealed; once nothing is sealed under it, it can be removed.
//!
//! KMS failures at startup stop the process: a gateway that cannot open its
//! secrets, or seal under the active key, fails closed. A failed refresh
//! keeps the keyring it had.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use tokio::sync::Mutex;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    aws::{KmsClient, KmsError},
    config::CredentialKmsConfig,
    domain::{
        catalog::ProviderFamily,
        credentials::{CredentialBinding, CredentialCipher, CredentialError},
        identity::OrganizationId,
    },
    metrics::{CREDENTIAL_SECRETS_PENDING, CREDENTIAL_SECRETS_RESEALED, global as metrics},
    persistence::credential_keys::{CredentialKeyStore, DataKeyRow, SealedRow, SecretKind},
};

/// How often every process reloads the data keys.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// How long a new data key waits before the worker re-seals under it, so
/// every process has loaded it first.
pub const RESEAL_GRACE: Duration = Duration::from_secs(5 * 60);
const RESEAL_BATCH: i64 = 100;

#[derive(Debug, thiserror::Error)]
pub enum CredentialKeyError {
    #[error("KMS: {0}")]
    Kms(#[from] KmsError),
    #[error("credential key store: {0}")]
    Store(#[from] sqlx::Error),
    #[error("data key is invalid")]
    Key(#[from] CredentialError),
    #[error("the active data key could not be unwrapped")]
    ActiveUnavailable,
    #[error("re-seal could not be recorded: {0}")]
    Audit(String),
}

/// The KMS encryption context of a data key: it can only be unwrapped as
/// the row it was created for.
fn context(id: Uuid) -> [(&'static str, String); 2] {
    [
        ("gateway", "credential-data-key".to_owned()),
        ("data_key_id", id.to_string()),
    ]
}

/// The keyring holder for one process.
#[derive(Clone)]
pub struct CredentialKeys {
    store: CredentialKeyStore,
    kms: KmsClient,
    /// The cipher every component holds; `install` updates them all.
    cipher: CredentialCipher,
    env: Option<CredentialCipher>,
    /// Unwrapped data keys by row, so a refresh only calls KMS for new ones.
    unwrapped: Arc<Mutex<HashMap<Uuid, Zeroizing<Vec<u8>>>>>,
    /// When the active data key was created.
    active_since: Arc<std::sync::Mutex<chrono::DateTime<Utc>>>,
}

impl CredentialKeys {
    /// Loads the keyring, creating the data key for the configured KMS key
    /// if there is none.
    pub async fn start(
        store: CredentialKeyStore,
        config: &CredentialKmsConfig,
        env: Option<CredentialCipher>,
    ) -> Result<Self, CredentialKeyError> {
        let kms = KmsClient::new(
            config.endpoint.clone(),
            config.region.clone(),
            config.credentials.clone(),
        )?;
        let rows = {
            let mut tx = store.pool().begin().await?;
            CredentialKeyStore::lock(&mut tx).await?;
            let mut rows = CredentialKeyStore::data_keys_on(&mut tx).await?;
            if !rows.iter().any(|row| row.kms_key_id == config.key_id) {
                let id = Uuid::new_v4();
                let context = context(id);
                let generated = kms
                    .generate_data_key(&config.key_id, &pairs(&context))
                    .await?;
                let key_id = CredentialCipher::new(&generated.plaintext)?.key_id();
                rows.push(
                    CredentialKeyStore::insert(
                        &mut tx,
                        id,
                        &key_id,
                        &config.key_id,
                        &generated.wrapped,
                    )
                    .await?,
                );
                tracing::info!(key_id, "credential data key created");
            }
            tx.commit().await?;
            rows
        };
        // A placeholder until the first install below replaces it.
        let cipher = CredentialCipher::new(&[0_u8; 32])?;
        let keys = Self {
            store,
            kms,
            cipher,
            env,
            unwrapped: Arc::default(),
            active_since: Arc::new(std::sync::Mutex::new(Utc::now())),
        };
        keys.install(rows).await?;
        Ok(keys)
    }

    /// The cipher to hand to every component.
    pub fn cipher(&self) -> CredentialCipher {
        self.cipher.clone()
    }

    /// Reloads the data keys; keeps the current keyring if that fails.
    pub async fn refresh(&self) -> Result<(), CredentialKeyError> {
        let rows = self.store.data_keys().await?;
        self.install(rows).await
    }

    /// Refreshes every [`REFRESH_INTERVAL`] for the life of the process.
    pub fn spawn_refresh(&self) {
        let keys = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(error) = keys.refresh().await {
                    tracing::warn!(error = %error, "credential key refresh failed");
                }
            }
        });
    }

    /// Unwraps `rows` (new ones only) and installs them. The newest data key
    /// is active; it must unwrap. Another that does not is skipped, and the
    /// secrets under it cannot be opened.
    async fn install(&self, rows: Vec<DataKeyRow>) -> Result<(), CredentialKeyError> {
        let active = rows.last().ok_or(CredentialKeyError::ActiveUnavailable)?;
        let mut unwrapped = self.unwrapped.lock().await;
        unwrapped.retain(|id, _| rows.iter().any(|row| row.id == *id));
        for row in &rows {
            if unwrapped.contains_key(&row.id) {
                continue;
            }
            let context = context(row.id);
            match self
                .kms
                .decrypt(&row.kms_key_id, &row.wrapped_key, &pairs(&context))
                .await
            {
                Ok(plaintext) => {
                    unwrapped.insert(row.id, plaintext);
                }
                Err(error) if row.id == active.id => return Err(error.into()),
                Err(error) => {
                    tracing::warn!(key_id = %row.key_id, error = %error, "credential data key could not be unwrapped");
                }
            }
        }
        let active_key = unwrapped
            .get(&active.id)
            .ok_or(CredentialKeyError::ActiveUnavailable)?;
        let previous: Vec<&[u8]> = rows
            .iter()
            .filter(|row| row.id != active.id)
            .filter_map(|row| unwrapped.get(&row.id).map(|key| key.as_slice()))
            .collect();
        self.cipher
            .install(active_key, &previous, self.env.as_ref())?;
        *self
            .active_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = active.created_at;
        Ok(())
    }

    /// Re-seals up to a bounded number of secrets not under the active key,
    /// once it is old enough that every process has loaded it, then retires
    /// data keys nothing uses. Returns how many secrets were re-sealed.
    pub async fn reseal(&self) -> Result<u64, CredentialKeyError> {
        // Never act on a keyring older than the data keys in the database.
        self.refresh().await?;
        let active = self.cipher.key_id();
        let since = *self
            .active_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let age = (Utc::now() - since).to_std().unwrap_or_default();
        let pending = self.store.pending(&active).await?;
        metrics().set(&CREDENTIAL_SECRETS_PENDING, &[], pending);
        if age < RESEAL_GRACE {
            return Ok(0);
        }
        // Only what this keyring can open; anything else would fail again.
        let stale: Vec<String> = self
            .cipher
            .key_ids()
            .into_iter()
            .filter(|key_id| *key_id != active)
            .collect();
        let mut resealed = 0;
        // The request id of every audit event this run writes.
        let run_id = Uuid::new_v4();
        if !stale.is_empty() {
            for kind in SecretKind::ALL {
                // Rows that fail are skipped for the rest of this run.
                let mut failed: Vec<Uuid> = Vec::new();
                loop {
                    let mut tx = self.store.pool().begin().await?;
                    let rows =
                        CredentialKeyStore::stale(&mut tx, kind, &stale, &failed, RESEAL_BATCH)
                            .await?;
                    for row in &rows {
                        match self.reseal_row(kind, row) {
                            Ok((key_id, ciphertext)) => {
                                if CredentialKeyStore::reseal(
                                    &mut tx,
                                    kind,
                                    row,
                                    &key_id,
                                    &ciphertext,
                                    run_id,
                                )
                                .await
                                .map_err(|error| CredentialKeyError::Audit(error.to_string()))?
                                {
                                    resealed += 1;
                                    metrics().inc(&CREDENTIAL_SECRETS_RESEALED, &[kind.as_str()]);
                                }
                            }
                            Err(error) => {
                                failed.push(row.id);
                                tracing::warn!(kind = kind.as_str(), id = %row.id, error = %error, "secret could not be re-sealed");
                            }
                        }
                    }
                    tx.commit().await?;
                    if rows.len() < RESEAL_BATCH as usize {
                        break;
                    }
                }
            }
        }
        let retired = self.store.retire_unused(&active).await?;
        if retired > 0 {
            tracing::info!(retired, "unused credential data keys retired");
            self.refresh().await?;
        }
        metrics().set(
            &CREDENTIAL_SECRETS_PENDING,
            &[],
            self.store.pending(&active).await?,
        );
        Ok(resealed)
    }

    /// The row's secret, opened with its key and sealed under the active one.
    fn reseal_row(
        &self,
        kind: SecretKind,
        row: &SealedRow,
    ) -> Result<(String, Vec<u8>), CredentialError> {
        let organization_id = OrganizationId(row.organization_id);
        let sealed = match kind {
            SecretKind::ProviderCredential => {
                let family: ProviderFamily = row
                    .family
                    .parse()
                    .map_err(|_| CredentialError::Invalid("unknown family"))?;
                let binding = CredentialBinding {
                    organization_id,
                    adapter_instance_key: &row.adapter_instance_key,
                    family,
                };
                let secret = self.cipher.open(binding, &row.key_id, &row.ciphertext)?;
                self.cipher.seal(binding, &secret)?
            }
            SecretKind::WebhookSecret => {
                let secret = self.cipher.open_webhook_secret(
                    organization_id,
                    row.id,
                    &row.key_id,
                    &row.ciphertext,
                )?;
                self.cipher
                    .seal_webhook_secret(organization_id, row.id, &secret)?
            }
            SecretKind::IdentityProviderSecret => {
                let secret = self.cipher.open_identity_provider_secret(
                    organization_id,
                    row.id,
                    &row.key_id,
                    &row.ciphertext,
                )?;
                self.cipher
                    .seal_identity_provider_secret(organization_id, row.id, &secret)?
            }
        };
        Ok((sealed.key_id, sealed.ciphertext))
    }
}

fn pairs<'a>(context: &'a [(&'static str, String); 2]) -> [(&'static str, &'a str); 2] {
    [(context[0].0, &context[0].1), (context[1].0, &context[1].1)]
}
