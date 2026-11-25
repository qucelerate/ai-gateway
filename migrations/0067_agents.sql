-- Agents: non-human administrators for automation (CI, coding agents, the
-- `gwctl` CLI). An agent is a user account without an OIDC identity, a
-- member of exactly one organization, so the role bindings, permission
-- checks and audit trail of people apply to it unchanged. It signs in with
-- an agent token (`Authorization: Bearer gwa_…`) instead of a session. Only
-- a SHA-256 digest of each token is stored.

ALTER TABLE gateway.user_account
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'person',
    ADD CONSTRAINT user_account_kind_ck CHECK (kind IN ('person', 'agent')),
    -- An agent never signs in through OIDC.
    ADD CONSTRAINT user_account_agent_oidc_ck CHECK (kind = 'person' OR oidc_issuer IS NULL);

CREATE TABLE gateway.agent (
    -- The agent's user account; also the audit target id.
    user_id UUID PRIMARY KEY REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (
        octet_length(name) BETWEEN 1 AND 100 AND name = btrim(name) AND name !~ '[[:cntrl:]]'
    ),
    description TEXT CHECK (
        description IS NULL OR (octet_length(description) BETWEEN 1 AND 500
                                AND description !~ '[[:cntrl:]]')
    ),
    -- NULL when an operator created it (`gateway agent create`).
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    disabled_at TIMESTAMPTZ,
    CONSTRAINT agent_name_uq UNIQUE (organization_id, name),
    CONSTRAINT agent_membership_fk FOREIGN KEY (organization_id, user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id) ON DELETE CASCADE
);

CREATE TABLE gateway.agent_token (
    -- Also the audit target id.
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    agent_user_id UUID NOT NULL REFERENCES gateway.agent (user_id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (
        octet_length(name) BETWEEN 1 AND 100 AND name = btrim(name) AND name !~ '[[:cntrl:]]'
    ),
    token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    -- `gwa_` and the token's first eight characters, to tell tokens apart.
    public_prefix TEXT NOT NULL CHECK (public_prefix ~ '^gwa_[A-Za-z0-9_-]{8}$'),
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    CONSTRAINT agent_token_expiry_ck CHECK (expires_at IS NULL OR expires_at > created_at)
);

CREATE INDEX agent_token_agent_idx ON gateway.agent_token (agent_user_id, created_at DESC);

ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy',
                          'invitation', 'identity_provider', 'config_pin', 'usage_export',
                          'catalog_auto_offer', 'agent', 'agent_token')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping', 'retention_policy',
                        'data_export', 'cloud_principal_binding', 'invitation',
                        'budget_policy', 'identity_provider', 'config_pin', 'usage_export',
                        'catalog_auto_offer', 'agent', 'agent_token')
    ) NOT VALID;
