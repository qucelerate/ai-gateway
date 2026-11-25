-- Phase 0B identity foundation. The gateway schema is provisioned outside
-- this repository and is owned by gateway_app.

CREATE TABLE gateway.organization (
    id UUID PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE gateway.user_account (
    id UUID PRIMARY KEY,
    oidc_issuer TEXT,
    oidc_subject TEXT,
    email TEXT,
    display_name TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT user_account_oidc_pair_ck CHECK (
        (oidc_issuer IS NULL AND oidc_subject IS NULL)
        OR (oidc_issuer IS NOT NULL AND oidc_subject IS NOT NULL)
    )
);

CREATE UNIQUE INDEX user_account_oidc_pair_uq
    ON gateway.user_account (oidc_issuer, oidc_subject)
    WHERE oidc_issuer IS NOT NULL AND oidc_subject IS NOT NULL;

CREATE TABLE gateway.team (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT team_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT team_organization_id_uq UNIQUE (organization_id, id),
    CONSTRAINT team_name_uq UNIQUE (organization_id, name)
);

CREATE TABLE gateway.organization_membership (
    organization_id UUID NOT NULL,
    user_id UUID NOT NULL,
    role TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, user_id),
    CONSTRAINT organization_membership_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT organization_membership_user_fk FOREIGN KEY (user_id)
        REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    CONSTRAINT organization_membership_role_ck CHECK (
        role IN ('owner', 'admin', 'member', 'auditor', 'read-only')
    )
);

CREATE TABLE gateway.team_membership (
    organization_id UUID NOT NULL,
    team_id UUID NOT NULL,
    user_id UUID NOT NULL,
    role TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, team_id, user_id),
    CONSTRAINT team_membership_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT team_membership_organization_user_fk FOREIGN KEY (organization_id, user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id) ON DELETE CASCADE,
    CONSTRAINT team_membership_role_ck CHECK (
        role IN ('owner', 'admin', 'member', 'auditor', 'read-only')
    )
);

CREATE TABLE gateway.service_account (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    team_id UUID,
    owner_user_id UUID NOT NULL,
    name TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT service_account_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT service_account_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id) ON DELETE RESTRICT,
    CONSTRAINT service_account_owner_organization_fk
        FOREIGN KEY (organization_id, owner_user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id)
        ON DELETE RESTRICT,
    CONSTRAINT service_account_owner_team_fk
        FOREIGN KEY (organization_id, team_id, owner_user_id)
        REFERENCES gateway.team_membership (organization_id, team_id, user_id)
        ON DELETE RESTRICT,
    CONSTRAINT service_account_organization_id_uq UNIQUE (organization_id, id)
);

CREATE TABLE gateway.api_key (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    service_account_id UUID NOT NULL,
    public_prefix VARCHAR(23) NOT NULL UNIQUE,
    digest BYTEA NOT NULL UNIQUE,
    scopes TEXT[] NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    disabled_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT api_key_service_account_fk
        FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT api_key_digest_length_ck CHECK (octet_length(digest) = 32),
    CONSTRAINT api_key_scopes_nonempty_ck CHECK (cardinality(scopes) > 0),
    CONSTRAINT api_key_scopes_vocabulary_ck CHECK (
        scopes <@ ARRAY['inference', 'embeddings', 'catalog-read', 'admin-read']::TEXT[]
    ),
    CONSTRAINT api_key_status_ck CHECK (status IN ('active', 'revoked', 'disabled')),
    CONSTRAINT api_key_lifecycle_timestamps_ck CHECK (
        (status = 'active' AND revoked_at IS NULL AND disabled_at IS NULL)
        OR (status = 'revoked' AND revoked_at IS NOT NULL AND disabled_at IS NULL)
        OR (status = 'disabled' AND revoked_at IS NULL AND disabled_at IS NOT NULL)
    )
);

CREATE INDEX api_key_service_account_idx
    ON gateway.api_key (organization_id, service_account_id);

CREATE TABLE gateway.audit_event (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    actor_user_id UUID,
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id UUID NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT audit_event_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT audit_event_actor_membership_fk
        FOREIGN KEY (organization_id, actor_user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id),
    CONSTRAINT audit_event_metadata_ck CHECK (
        NOT (metadata ?| ARRAY[
            'bearer', 'secret', 'digest', 'pepper', 'plaintext_key', 'prompt'
        ]::TEXT[])
    )
);

CREATE INDEX audit_event_organization_occurred_idx
    ON gateway.audit_event (organization_id, occurred_at DESC);
