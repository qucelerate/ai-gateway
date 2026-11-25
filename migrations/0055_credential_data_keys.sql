-- Envelope encryption for stored secrets. With AWS KMS configured, provider
-- credentials and webhook signing secrets are sealed under a data key that
-- KMS generated; only the KMS-wrapped data key is stored here. Every process
-- unwraps the data keys through KMS at startup and refreshes them each
-- minute. A new data key is created when the configured KMS key changes;
-- the worker then re-seals every secret under it and retires data keys no
-- secret uses any more.
CREATE TABLE gateway.credential_data_key (
    id UUID PRIMARY KEY,
    -- The public id of the derived key, as stored with each secret.
    key_id TEXT NOT NULL UNIQUE CHECK (key_id ~ '^[0-9a-f]{16}$'),
    -- The KMS key (id, ARN or alias) that wrapped it.
    kms_key_id TEXT NOT NULL CHECK (length(kms_key_id) BETWEEN 1 AND 2048),
    wrapped_key BYTEA NOT NULL CHECK (octet_length(wrapped_key) BETWEEN 1 AND 6144),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    retired_at TIMESTAMPTZ
);

CREATE INDEX credential_data_key_live_idx ON gateway.credential_data_key (created_at)
    WHERE retired_at IS NULL;

-- Re-sealing finds secrets by key.
CREATE INDEX provider_credential_key_idx ON gateway.provider_credential (key_id);
CREATE INDEX webhook_endpoint_secret_key_idx ON gateway.webhook_endpoint (secret_key_id);
