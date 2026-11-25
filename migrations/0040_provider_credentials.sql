-- Encrypted, org-scoped provider credentials. Credentials live outside the
-- immutable catalog release graph so rotating a secret never requires a new
-- release. Only AES-256-GCM ciphertext is stored: the plaintext secret, its
-- digest, and the encryption key never reach the database. `key_id` names the
-- configured key that sealed the row so a future key rotation can re-encrypt.
CREATE TABLE gateway.provider_credential (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic', 'gemini', 'bedrock',
                                           'azure-openai', 'azure-ai-foundry',
                                           'openai-compatible')),
    key_id TEXT NOT NULL CHECK (key_id ~ '^[0-9a-f]{16}$'),
    -- 12-byte nonce, then ciphertext and 16-byte tag.
    ciphertext BYTEA NOT NULL CHECK (octet_length(ciphertext) BETWEEN 29 AND 65536),
    created_by UUID,
    rotated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    rotated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT provider_credential_org_key_uq UNIQUE (organization_id, adapter_instance_key),
    CONSTRAINT provider_credential_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT provider_credential_created_by_fk FOREIGN KEY (created_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT provider_credential_rotated_by_fk FOREIGN KEY (rotated_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

-- Catalog writes (release create, activation) and credential writes are
-- versioned and audited admin resources. Activation is versioned per
-- organization: its resource id is the organization id.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential')
    ) NOT VALID;

-- A completed admin mutation may record a non-secret response snapshot so a
-- replay returns the original result even after the resource has changed.
ALTER TABLE gateway.admin_idempotency_record
    DROP CONSTRAINT admin_idempotency_response_ck,
    ADD CONSTRAINT admin_idempotency_response_ck CHECK (
        response_metadata IS NULL OR (
            jsonb_typeof(response_metadata) = 'object'
            AND (response_metadata - ARRAY[
                'resource_id', 'version', 'secret_unavailable_on_replay', 'snapshot'
            ]) = '{}'::JSONB
            AND jsonb_typeof(response_metadata->'resource_id') = 'string'
            AND (NOT response_metadata ? 'version' OR jsonb_typeof(response_metadata->'version') = 'number')
            AND (NOT response_metadata ? 'secret_unavailable_on_replay' OR jsonb_typeof(response_metadata->'secret_unavailable_on_replay') = 'boolean')
            AND (NOT response_metadata ? 'snapshot' OR jsonb_typeof(response_metadata->'snapshot') = 'object')
        )
    ) NOT VALID;
