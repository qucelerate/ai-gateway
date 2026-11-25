-- Local accounts: people who sign in with an email address and a password
-- the gateway keeps, so a new installation can be tried before it is
-- registered with any identity provider. A local account is still a user
-- keyed on (issuer, subject): the issuer is 'urn:gateway:local' and the
-- subject the user's own id. Only an Argon2id PHC string of the password is
-- stored.
CREATE TABLE gateway.local_credential (
    user_id UUID PRIMARY KEY REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    -- Trimmed and lowercased; the key people sign in with.
    email TEXT NOT NULL CHECK (length(email) BETWEEN 3 AND 254 AND email = lower(email)),
    password_hash TEXT NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    password_changed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Failed sign-ins in a row, and until when the account refuses more.
    failed_attempts INTEGER NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    locked_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX local_credential_email_uq ON gateway.local_credential (email);

-- Single-use links that set a password: the first-run setup link (no user
-- yet) and password-reset links an owner or the operator makes. Only a
-- SHA-256 digest of the token is stored.
CREATE TABLE gateway.password_link (
    id UUID PRIMARY KEY,
    purpose TEXT NOT NULL CHECK (purpose IN ('setup', 'reset')),
    token_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(token_digest) = 32),
    user_id UUID REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    -- NULL: made by the operator or at startup.
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ NOT NULL,
    used_at TIMESTAMPTZ,
    CONSTRAINT password_link_user_ck CHECK ((purpose = 'setup') = (user_id IS NULL)),
    CONSTRAINT password_link_expiry_ck CHECK (expires_at > created_at)
);

CREATE INDEX password_link_user_idx ON gateway.password_link (user_id) WHERE used_at IS NULL;

-- Administrators of the deployment as a whole, above its organizations:
-- they manage organizations without seeing inside them. The account made by
-- first-run setup is one; the operator grants others with `gateway admin
-- grant`.
CREATE TABLE gateway.deployment_admin (
    user_id UUID PRIMARY KEY REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- NULL: granted by the operator or by first-run setup.
    granted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

-- An owner can require the organization's own identity provider: its
-- members' password sessions then cannot act in it. Only meaningful while
-- that provider is enabled.
ALTER TABLE gateway.organization
    ADD COLUMN require_sso BOOLEAN NOT NULL DEFAULT FALSE;

-- How a session was signed in. Password sessions end when local accounts
-- are turned off, and cannot act in an organization that requires SSO.
ALTER TABLE gateway.session
    ADD COLUMN method TEXT NOT NULL DEFAULT 'oidc' CHECK (method IN ('oidc', 'password')),
    ADD CONSTRAINT session_password_unscoped_ck
        CHECK (method = 'oidc' OR organization_id IS NULL);

ALTER TABLE gateway.security_event
    DROP CONSTRAINT IF EXISTS security_event_type_ck,
    ADD CONSTRAINT security_event_type_ck CHECK (
        event_type IN (
            'login.succeeded',
            'login.replay_or_invalid_state',
            'login.backend_unavailable',
            'login.backend_rejected',
            'login.claims_rejected',
            'login.unprovisioned',
            'login.disabled',
            'login.identity_unavailable',
            'logout.requested',
            'login.password_rejected',
            'login.locked',
            'password.changed',
            'password.reset',
            'password.reset_link_created',
            'setup.completed'
        )
    ),
    DROP CONSTRAINT IF EXISTS security_event_metadata_ck,
    ADD CONSTRAINT security_event_metadata_ck CHECK (
        jsonb_typeof(metadata) = 'object'
        AND metadata ? 'method'
        AND (metadata - 'method') = '{}'::JSONB
        AND metadata->>'method' IN ('oidc', 'browser', 'password')
    );
