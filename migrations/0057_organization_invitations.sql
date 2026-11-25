-- Invitations: a single-use link that admits whoever signs in with it to an
-- organization in a given role, so nobody has to learn a person's OIDC
-- subject in advance. The person is still keyed on (issuer, subject), taken
-- from their sign-in; the link only proves they were invited. Only a
-- SHA-256 digest of the token is stored.
CREATE TABLE gateway.organization_invitation (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member', 'auditor', 'read-only')),
    -- The OIDC issuer the accepting sign-in must come from.
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 2048),
    -- NULL: created by the operator, outside the admin API.
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ,
    accepted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    revoked_at TIMESTAMPTZ,
    CONSTRAINT organization_invitation_one_end_ck
        CHECK (accepted_at IS NULL OR revoked_at IS NULL),
    CONSTRAINT organization_invitation_expiry_ck CHECK (expires_at > created_at)
);

CREATE INDEX organization_invitation_org_idx
    ON gateway.organization_invitation (organization_id, created_at DESC);

-- The invitation a sign-in was started from, accepted when it completes.
ALTER TABLE gateway.oidc_login_transaction
    ADD COLUMN invitation_id UUID
        REFERENCES gateway.organization_invitation (id) ON DELETE CASCADE;

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping', 'retention_policy',
                        'data_export', 'cloud_principal_binding', 'invitation')
    ) NOT VALID;
