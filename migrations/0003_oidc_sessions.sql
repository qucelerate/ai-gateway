-- Phase 1A OIDC one-time transactions, opaque server-side sessions, and security events.
CREATE TABLE gateway.oidc_login_transaction (
    id UUID PRIMARY KEY,
    state_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(state_digest) = 32),
    nonce_digest BYTEA NOT NULL CHECK (octet_length(nonce_digest) = 32),
    verifier_ciphertext BYTEA NOT NULL,
    browser_binding_digest BYTEA NOT NULL CHECK (octet_length(browser_binding_digest) = 32),
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX oidc_login_transaction_expiry_idx
    ON gateway.oidc_login_transaction (expires_at);

CREATE TABLE gateway.session (
    id UUID PRIMARY KEY,
    token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    user_id UUID NOT NULL REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    browser_binding_digest BYTEA NOT NULL CHECK (octet_length(browser_binding_digest) = 32),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX session_user_active_idx
    ON gateway.session (user_id, expires_at)
    WHERE revoked_at IS NULL;

CREATE TABLE gateway.security_event (
    id UUID PRIMARY KEY,
    user_id UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    event_type TEXT NOT NULL,
    correlation_id UUID NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT security_event_type_ck CHECK (
        event_type IN ('login.succeeded', 'login.replay_or_invalid_state', 'logout.requested')
    ),
    CONSTRAINT security_event_metadata_ck CHECK (
        jsonb_typeof(metadata) = 'object'
        AND metadata ? 'method'
        AND (metadata - 'method') = '{}'::JSONB
        AND metadata->>'method' IN ('oidc', 'browser')
    )
);

CREATE INDEX security_event_occurred_idx
ON gateway.security_event (occurred_at DESC);

-- Close the previously-created administrative audit surface as well.  Phase
-- 1A only emits these scalar, identifier-only events; arbitrary action names
-- and JSON are not an acceptable security boundary.
--
-- These checks are deliberately NOT VALID: audit_event already exists from
-- 0002, and rows written by that migration may contain arbitrary action and
-- metadata values.  NOT VALID leaves those historical rows untouched while
-- enforcing both checks for every new or updated row.  After an operator has
-- remediated the legacy rows, validate them explicitly with:
--
--   ALTER TABLE gateway.audit_event
--     VALIDATE CONSTRAINT audit_event_action_ck;
--   ALTER TABLE gateway.audit_event
--     VALIDATE CONSTRAINT audit_event_metadata_shape_ck;
ALTER TABLE gateway.audit_event
    ADD CONSTRAINT audit_event_action_ck CHECK (
        action IN ('api_key.created', 'api_key.revoked', 'api_key.disabled')
    ) NOT VALID,
    ADD CONSTRAINT audit_event_metadata_shape_ck CHECK (
        jsonb_typeof(metadata) = 'object'
        AND metadata ? 'api_key_id'
        AND (metadata - ARRAY['api_key_id', 'scope_count', 'public_prefix']) = '{}'::JSONB
        AND jsonb_typeof(metadata->'api_key_id') = 'string'
        AND (metadata->>'api_key_id') ~ '^[0-9a-fA-F-]{36}$'
        AND (
            NOT metadata ? 'scope_count'
            OR (metadata->>'scope_count') ~ '^[0-9]+$'
        )
        AND (
            NOT metadata ? 'public_prefix'
            OR (
                jsonb_typeof(metadata->'public_prefix') = 'string'
                AND (metadata->>'public_prefix') ~ '^gw_[0-9a-fA-F]{20}$'
            )
        )
    ) NOT VALID;
