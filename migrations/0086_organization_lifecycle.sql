-- Deployment admins manage organizations as a whole: create, suspend,
-- delete and restore them, without seeing inside them.
--
-- Suspended: every way in is refused (API keys, agent tokens, sessions,
-- invitations, the organization's identity provider); reversible.
-- Deleted: only from suspended; hidden everywhere, but kept, and
-- restorable. Its secrets (provider credentials, webhook signing secrets,
-- identity-provider client secrets) stay stored, sealed and unused, so a
-- restore brings it back whole. Everything inside it is refused by its
-- state alone; nothing inside is switched off.
ALTER TABLE gateway.organization
    ADD COLUMN suspended_at TIMESTAMPTZ,
    ADD COLUMN suspended_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD COLUMN deleted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    -- An owner asked the deployment's admins to delete it.
    ADD COLUMN deletion_requested_at TIMESTAMPTZ,
    ADD COLUMN deletion_requested_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    ADD CONSTRAINT organization_deleted_suspended_ck
        CHECK (deleted_at IS NULL OR suspended_at IS NOT NULL);

-- A deleted organization's slug is free for a new one; a restore that
-- finds it taken asks for another.
ALTER TABLE gateway.organization DROP CONSTRAINT organization_slug_key;
CREATE UNIQUE INDEX organization_slug_live_uq
    ON gateway.organization (slug) WHERE deleted_at IS NULL;

-- Who did what to organizations as a whole. Append-only.
CREATE TABLE gateway.deployment_audit_event (
    id UUID PRIMARY KEY,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- NULL: the operator, through the CLI.
    actor_user_id UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    action TEXT NOT NULL CHECK (action ~ '^[a-z][a-z0-9._:-]{0,63}$'),
    organization_id UUID REFERENCES gateway.organization (id) ON DELETE SET NULL,
    target_user_id UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    -- Names at the time (slug, name), never secrets.
    detail JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (jsonb_typeof(detail) = 'object')
);

CREATE INDEX deployment_audit_event_time_idx
    ON gateway.deployment_audit_event (occurred_at DESC, id DESC);

DROP TRIGGER IF EXISTS deployment_audit_event_immutable ON gateway.deployment_audit_event;
CREATE TRIGGER deployment_audit_event_immutable
    BEFORE UPDATE OR DELETE ON gateway.deployment_audit_event
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_admin_audit_mutation();
