-- Per-organization identity providers: an organization may sign its people
-- in through its own OpenID Connect provider instead of the process-wide one.
--
-- Every saved configuration is a new, immutable row with its own version, so
-- a sign-in in flight can be bound to exactly the configuration it started
-- with. The organization's row points at the active configuration (the one
-- sign-in uses while enabled) and at a pending one (saved, not yet proven by
-- a test sign-in). The client secret is stored only sealed with the
-- credential-encryption key.
CREATE TABLE gateway.identity_provider_configuration (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    version BIGINT NOT NULL CHECK (version > 0),
    -- https only; the API also refuses IP literals and private names. A
    -- loopback http issuer is admitted only for the test suites' provider,
    -- which the API accepts only under the tests' outbound policy.
    issuer TEXT NOT NULL CHECK (
        length(issuer) BETWEEN 9 AND 2048
        AND (issuer LIKE 'https://%' OR issuer LIKE 'http://127.0.0.1:%')
    ),
    client_id TEXT NOT NULL CHECK (length(client_id) BETWEEN 1 AND 255),
    secret_key_id TEXT NOT NULL CHECK (length(secret_key_id) BETWEEN 1 AND 64),
    secret_ciphertext BYTEA NOT NULL CHECK (octet_length(secret_ciphertext) > 12),
    extra_scopes TEXT[] NOT NULL DEFAULT '{}'::TEXT[]
        CHECK (cardinality(extra_scopes) <= 10),
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- The test sign-in that activated it.
    tested_at TIMESTAMPTZ,
    tested_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT identity_provider_configuration_version_uq UNIQUE (organization_id, version),
    CONSTRAINT identity_provider_configuration_org_id_uq UNIQUE (organization_id, id)
);

CREATE TABLE gateway.organization_identity_provider (
    organization_id UUID PRIMARY KEY REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The admin resource id (versions, audit, idempotency).
    id UUID NOT NULL UNIQUE,
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    active_configuration_id UUID,
    pending_configuration_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    disabled_at TIMESTAMPTZ,
    CONSTRAINT organization_identity_provider_active_fk
        FOREIGN KEY (organization_id, active_configuration_id)
        REFERENCES gateway.identity_provider_configuration (organization_id, id),
    CONSTRAINT organization_identity_provider_pending_fk
        FOREIGN KEY (organization_id, pending_configuration_id)
        REFERENCES gateway.identity_provider_configuration (organization_id, id),
    CONSTRAINT organization_identity_provider_enabled_ck
        CHECK (NOT enabled OR active_configuration_id IS NOT NULL),
    CONSTRAINT organization_identity_provider_distinct_ck
        CHECK (active_configuration_id IS DISTINCT FROM pending_configuration_id
               OR active_configuration_id IS NULL)
);

-- Organizations sign in by slug; the active configuration is read by it.
CREATE INDEX identity_provider_configuration_org_idx
    ON gateway.identity_provider_configuration (organization_id, version DESC);

-- A sign-in through an organization's provider records the organization and
-- the configuration it started with; the callback validates against exactly
-- that configuration. A test sign-in also records the owner who started it
-- and the resource version they saved, which its activation must still match.
ALTER TABLE gateway.oidc_login_transaction
    ADD COLUMN organization_id UUID,
    ADD COLUMN identity_provider_configuration_id UUID,
    ADD COLUMN purpose TEXT NOT NULL DEFAULT 'sign_in',
    ADD COLUMN started_by UUID REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    ADD COLUMN expected_version BIGINT,
    ADD CONSTRAINT oidc_login_transaction_configuration_fk
        FOREIGN KEY (organization_id, identity_provider_configuration_id)
        REFERENCES gateway.identity_provider_configuration (organization_id, id)
        ON DELETE CASCADE,
    ADD CONSTRAINT oidc_login_transaction_purpose_ck
        CHECK (purpose IN ('sign_in', 'provider_test')),
    ADD CONSTRAINT oidc_login_transaction_provider_shape_ck CHECK (
        (identity_provider_configuration_id IS NULL
            AND organization_id IS NULL
            AND purpose = 'sign_in'
            AND started_by IS NULL
            AND expected_version IS NULL)
        OR (identity_provider_configuration_id IS NOT NULL
            AND organization_id IS NOT NULL
            AND ((purpose = 'sign_in' AND started_by IS NULL AND expected_version IS NULL)
                 OR (purpose = 'provider_test' AND started_by IS NOT NULL
                     AND expected_version IS NOT NULL AND expected_version > 0
                     AND invitation_id IS NULL)))
    );

-- A session from an organization's provider acts only in that organization,
-- and only while the configuration it came from is the active one. NULL for
-- the process-wide provider, whose sessions are unchanged.
ALTER TABLE gateway.session
    ADD COLUMN organization_id UUID,
    ADD COLUMN identity_provider_configuration_id UUID,
    ADD CONSTRAINT session_configuration_fk
        FOREIGN KEY (organization_id, identity_provider_configuration_id)
        REFERENCES gateway.identity_provider_configuration (organization_id, id)
        ON DELETE CASCADE,
    ADD CONSTRAINT session_scope_shape_ck CHECK (
        (organization_id IS NULL) = (identity_provider_configuration_id IS NULL)
    );

ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy',
                          'invitation', 'identity_provider')
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
                        'budget_policy', 'identity_provider')
    ) NOT VALID;
