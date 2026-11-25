-- Shared admin-access foundation. Memberships remain the identity and login
-- relationship; role bindings are a separate authorization projection.

ALTER TABLE gateway.api_key
    ADD COLUMN IF NOT EXISTS successor_of UUID,
    ADD CONSTRAINT api_key_organization_id_uq UNIQUE (organization_id, id);

ALTER TABLE gateway.api_key
    ADD CONSTRAINT api_key_successor_fk
        FOREIGN KEY (organization_id, successor_of)
        REFERENCES gateway.api_key (organization_id, id),
    ADD CONSTRAINT api_key_successor_not_self_ck
        CHECK (successor_of IS NULL OR successor_of <> id);

CREATE INDEX api_key_successor_idx
    ON gateway.api_key (organization_id, successor_of)
    WHERE successor_of IS NOT NULL;

CREATE TABLE gateway.admin_role_binding (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    user_id UUID NOT NULL,
    team_id UUID,
    role TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT admin_role_binding_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT admin_role_binding_user_fk FOREIGN KEY (organization_id, user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id) ON DELETE CASCADE,
    -- A team-scoped binding must reference a real membership of that user in
    -- that team; a bare team row is never sufficient to hold a role.
    CONSTRAINT admin_role_binding_team_fk
        FOREIGN KEY (organization_id, team_id, user_id)
        REFERENCES gateway.team_membership (organization_id, team_id, user_id) ON DELETE CASCADE,
    CONSTRAINT admin_role_binding_role_ck CHECK (
        role IN ('organization_owner', 'organization_admin', 'team_admin',
                 'operator', 'auditor', 'read_only_member')
    ),
    -- Role scope validity mirrors the Rust role matrix: an org-wide binding is
    -- never team_admin, and a team binding is always team_admin.
    CONSTRAINT admin_role_binding_scope_ck CHECK (
        (team_id IS NULL AND role <> 'team_admin')
        OR (team_id IS NOT NULL AND role = 'team_admin')
    )
);
-- Duplicate org-wide bindings must be impossible even though team_id is NULL.
-- Partial unique indexes make the NULL semantics explicit and portable.
CREATE UNIQUE INDEX admin_role_binding_org_wide_natural_uq
    ON gateway.admin_role_binding (organization_id, user_id, role)
    WHERE team_id IS NULL;
CREATE UNIQUE INDEX admin_role_binding_team_natural_uq
    ON gateway.admin_role_binding (organization_id, user_id, team_id, role)
    WHERE team_id IS NOT NULL;
CREATE INDEX admin_role_binding_lookup_idx
    ON gateway.admin_role_binding (organization_id, user_id, team_id)
    WHERE enabled;

-- Compatibility backfill. Existing membership rows still control OIDC/session
-- eligibility; these rows only provide the new authorization vocabulary.
-- Generic members are deliberately NOT granted an org-wide read binding here:
-- a membership row alone is not an administrative grant.
INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
SELECT md5(organization_id::text || ':' || user_id::text)::uuid, organization_id, user_id,
       CASE role
           WHEN 'owner' THEN 'organization_owner'
           WHEN 'admin' THEN 'organization_admin'
           ELSE 'auditor'
       END,
       enabled
FROM gateway.organization_membership
WHERE role IN ('owner', 'admin', 'auditor')
ON CONFLICT DO NOTHING;

INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
SELECT md5(organization_id::text || ':' || team_id::text || ':' || user_id::text)::uuid, organization_id, user_id, team_id,
       'team_admin', enabled
FROM gateway.team_membership
WHERE role IN ('owner', 'admin')
ON CONFLICT DO NOTHING;

CREATE TABLE gateway.admin_resource_version (
    organization_id UUID NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id UUID NOT NULL,
    version BIGINT NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, resource_type, resource_id),
    CONSTRAINT admin_resource_version_organization_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT admin_resource_version_positive_ck CHECK (version > 0),
    CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key')
    )
);

INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
SELECT id, 'organization', id FROM gateway.organization
UNION ALL SELECT organization_id, 'team', id FROM gateway.team
UNION ALL SELECT organization_id, 'membership', user_id FROM gateway.organization_membership
UNION ALL SELECT organization_id, 'membership', md5(team_id::text || ':' || user_id::text)::uuid FROM gateway.team_membership
UNION ALL SELECT organization_id, 'role_binding', id FROM gateway.admin_role_binding
UNION ALL SELECT organization_id, 'service_account', id FROM gateway.service_account
UNION ALL SELECT organization_id, 'api_key', id FROM gateway.api_key
ON CONFLICT DO NOTHING;

ALTER TABLE gateway.audit_event
    ADD COLUMN IF NOT EXISTS actor_type TEXT NOT NULL DEFAULT 'user',
    ADD COLUMN IF NOT EXISTS actor_service_account_id UUID,
    ADD COLUMN IF NOT EXISTS team_id UUID,
    ADD COLUMN IF NOT EXISTS outcome TEXT NOT NULL DEFAULT 'succeeded',
    ADD COLUMN IF NOT EXISTS request_id UUID,
    ADD COLUMN IF NOT EXISTS before_metadata JSONB,
    ADD COLUMN IF NOT EXISTS after_metadata JSONB;

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT IF EXISTS audit_event_action_ck,
    DROP CONSTRAINT IF EXISTS audit_event_metadata_shape_ck,
    ADD CONSTRAINT audit_event_action_ck CHECK (
        action ~ '^[a-z][a-z0-9._:-]{0,63}$'
    ) NOT VALID,
    ADD CONSTRAINT audit_event_metadata_shape_ck CHECK (
        jsonb_typeof(metadata) = 'object'
        AND metadata <> '{}'::JSONB
        AND (metadata - ARRAY[
            'api_key_id', 'scope_count', 'public_prefix', 'target_id',
            'resource_type', 'status', 'enabled', 'role', 'version'
        ]) = '{}'::JSONB
        AND NOT (metadata ?| ARRAY[
            'bearer', 'secret', 'digest', 'pepper', 'plaintext_key',
            'idempotency_key', 'request_body', 'authorization'
        ]::TEXT[])
    ) NOT VALID,
    ADD CONSTRAINT audit_event_actor_type_ck CHECK (
        actor_type IN ('user', 'service_account', 'system')
    ),
    ADD CONSTRAINT audit_event_outcome_ck CHECK (
        outcome IN ('succeeded', 'failed', 'denied')
    ),
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key')
    ) NOT VALID,
    ADD CONSTRAINT audit_event_actor_shape_ck CHECK (
        (actor_type = 'user' AND actor_user_id IS NOT NULL AND actor_service_account_id IS NULL)
        OR (actor_type = 'service_account' AND actor_user_id IS NULL AND actor_service_account_id IS NOT NULL)
        OR (actor_type = 'system' AND actor_user_id IS NULL AND actor_service_account_id IS NULL)
    ) NOT VALID,
    ADD CONSTRAINT audit_event_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id),
    ADD CONSTRAINT audit_event_service_actor_fk FOREIGN KEY (organization_id, actor_service_account_id)
        REFERENCES gateway.service_account (organization_id, id),
    ADD CONSTRAINT audit_event_before_metadata_ck CHECK (
        before_metadata IS NULL OR (
            jsonb_typeof(before_metadata) = 'object'
            AND (before_metadata - ARRAY[
                'status', 'enabled', 'role', 'scopes', 'expires_at',
                'public_prefix', 'version'
            ]) = '{}'::JSONB
            AND NOT (before_metadata ?| ARRAY[
                'bearer', 'secret', 'digest', 'pepper', 'plaintext_key',
                'idempotency_key'
            ]::TEXT[])
        )
    ),
    ADD CONSTRAINT audit_event_after_metadata_ck CHECK (
        after_metadata IS NULL OR (
            jsonb_typeof(after_metadata) = 'object'
            AND (after_metadata - ARRAY[
                'status', 'enabled', 'role', 'scopes', 'expires_at',
                'public_prefix', 'version'
            ]) = '{}'::JSONB
            AND NOT (after_metadata ?| ARRAY[
                'bearer', 'secret', 'digest', 'pepper', 'plaintext_key',
                'idempotency_key'
            ]::TEXT[])
        )
    ),
    ADD CONSTRAINT audit_event_request_id_ck CHECK (request_id IS NULL OR request_id <> '00000000-0000-0000-0000-000000000000');

CREATE INDEX audit_event_scope_idx
    ON gateway.audit_event (organization_id, team_id, occurred_at DESC);

CREATE OR REPLACE FUNCTION gateway.prevent_admin_audit_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'administrative audit events are immutable';
END;
$$;

DROP TRIGGER IF EXISTS audit_event_immutable ON gateway.audit_event;
CREATE TRIGGER audit_event_immutable
    BEFORE UPDATE OR DELETE ON gateway.audit_event
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_admin_audit_mutation();

CREATE TABLE gateway.admin_idempotency_record (
    organization_id UUID NOT NULL,
    actor_user_id UUID NOT NULL,
    operation TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_fingerprint BYTEA NOT NULL,
    status TEXT NOT NULL DEFAULT 'in_progress',
    claim_id UUID NOT NULL,
    resource_id UUID,
    response_metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ,
    claim_expires_at TIMESTAMPTZ NOT NULL DEFAULT (CURRENT_TIMESTAMP + INTERVAL '60 seconds'),
    PRIMARY KEY (organization_id, actor_user_id, operation, idempotency_key),
    CONSTRAINT admin_idempotency_org_fk FOREIGN KEY (organization_id, actor_user_id)
        REFERENCES gateway.organization_membership (organization_id, user_id),
    CONSTRAINT admin_idempotency_fingerprint_ck CHECK (octet_length(request_fingerprint) = 32),
    CONSTRAINT admin_idempotency_status_ck CHECK (status IN ('in_progress', 'succeeded', 'failed')),
    CONSTRAINT admin_idempotency_key_ck CHECK (length(idempotency_key) BETWEEN 1 AND 255),
    CONSTRAINT admin_idempotency_operation_ck CHECK (operation ~ '^[a-z][a-z0-9._:-]{0,95}$'),
    CONSTRAINT admin_idempotency_response_ck CHECK (
        response_metadata IS NULL OR (
            jsonb_typeof(response_metadata) = 'object'
            AND (response_metadata - ARRAY[
                'resource_id', 'version', 'secret_unavailable_on_replay'
            ]) = '{}'::JSONB
            AND jsonb_typeof(response_metadata->'resource_id') = 'string'
            AND (NOT response_metadata ? 'version' OR jsonb_typeof(response_metadata->'version') = 'number')
            AND (NOT response_metadata ? 'secret_unavailable_on_replay' OR jsonb_typeof(response_metadata->'secret_unavailable_on_replay') = 'boolean')
        )
    )
);

CREATE INDEX admin_idempotency_claim_idx
    ON gateway.admin_idempotency_record (claim_id);
