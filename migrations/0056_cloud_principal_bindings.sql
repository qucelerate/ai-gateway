-- Cloud principals that trust the gateway's own identity: an Entra
-- directory its app signs in to, a Google service account it impersonates.
-- Anyone who names one could have the gateway act as it, so a credential
-- that does is used only once an operator has bound the principal to the
-- credential's organization (`gateway cloud-principal allow`). AWS roles
-- need no binding: the gateway sets the external ID from the organization.
CREATE TABLE gateway.cloud_principal_binding (
    -- The audit target id.
    id UUID NOT NULL UNIQUE,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('azure_tenant', 'google_service_account')),
    principal TEXT NOT NULL CHECK (length(principal) BETWEEN 1 AND 320),
    -- SHA-256 of the kind and principal, as credential rows store it.
    principal_digest BYTEA NOT NULL CHECK (octet_length(principal_digest) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, kind, principal)
);

CREATE UNIQUE INDEX cloud_principal_binding_digest_idx
    ON gateway.cloud_principal_binding (organization_id, principal_digest);

-- The principal a credential acts as, by digest, so the inference path
-- checks its binding with the credential lookup. NULL for credentials that
-- act as no principal. `principal_recorded` is false until the digest has
-- been worked out: only decrypting a credential tells whether it names a
-- principal, so the API records those of existing rows at startup. Families
-- whose credentials never name one need no decrypting.
ALTER TABLE gateway.provider_credential
    ADD COLUMN principal_digest BYTEA CHECK (octet_length(principal_digest) = 32),
    ADD COLUMN principal_recorded BOOLEAN NOT NULL DEFAULT false;

UPDATE gateway.provider_credential SET principal_recorded = true
WHERE family NOT IN ('azure-openai', 'azure-ai-foundry', 'gemini');

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping', 'retention_policy',
                        'data_export', 'cloud_principal_binding')
    ) NOT VALID;
