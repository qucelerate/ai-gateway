-- One record per model across providers (Phase 13, D-033): the
-- organization's own master records, and its links from a deployment's
-- provider family and model identifier to a master.
--
-- A master is a model card's id (`anthropic/claude-sonnet-4-6`), which needs
-- no row, or one of the organization's own records (`org/<slug>`) for models
-- no card covers. A link is per organization, provider family and model
-- identifier, exactly as deployments name them, and not per release, so it
-- holds as releases come and go. `master` NULL links to none: it detaches a
-- deployment a card would match.
--
-- Links are catalog metadata: nothing here is read by routing, pricing or
-- configuration versions.
--
-- Deletes are soft: `deleted_at` hides a record or clears a link, and every
-- state each has held stays in its `_revision` table, with who wrote it and
-- when.

CREATE TABLE gateway.model_master (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization and slug: the admin version resource's id.
    id UUID NOT NULL UNIQUE,
    -- The record is `org/<slug>`.
    slug TEXT NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    modality TEXT NOT NULL CHECK (modality IN ('chat', 'embeddings')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    deleted_at TIMESTAMPTZ,
    deleted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    PRIMARY KEY (organization_id, slug)
);

CREATE TABLE gateway.model_master_revision (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    master_id UUID NOT NULL REFERENCES gateway.model_master (id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    modality TEXT NOT NULL,
    -- True records a delete.
    deleted BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);
CREATE INDEX model_master_revision_master_idx
    ON gateway.model_master_revision (master_id, created_at DESC);

CREATE TABLE gateway.model_master_link (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization, family and model: the admin version
    -- resource's id.
    id UUID NOT NULL UNIQUE,
    family TEXT NOT NULL CHECK (octet_length(family) BETWEEN 1 AND 64),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    -- A card's id or `org/<slug>`; NULL links to none.
    master TEXT CHECK (master IS NULL OR (octet_length(master) <= 255
                                          AND master ~ '^[a-z0-9._-]+/[a-z0-9._-]+$')),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    deleted_at TIMESTAMPTZ,
    deleted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    PRIMARY KEY (organization_id, family, model)
);
CREATE INDEX model_master_link_master_idx
    ON gateway.model_master_link (organization_id, master) WHERE deleted_at IS NULL;

CREATE TABLE gateway.model_master_link_revision (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    link_id UUID NOT NULL REFERENCES gateway.model_master_link (id) ON DELETE CASCADE,
    master TEXT,
    -- True records a clear (the link removed, so cards decide again).
    cleared BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);
CREATE INDEX model_master_link_revision_link_idx
    ON gateway.model_master_link_revision (link_id, created_at DESC);

-- New admin resource and audit target types. The lists are extended in
-- place rather than restated, so a type another migration added is kept.
DO $$
DECLARE
    added CONSTANT TEXT := ', ''model_master''::text, ''model_master_link''::text]';
    def TEXT;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT def FROM pg_constraint
    WHERE conname = 'audit_event_entity_type_ck'
      AND conrelid = 'gateway.audit_event'::regclass;
    def := regexp_replace(def, '\s*NOT VALID\s*$', '');
    IF def !~ '\]\)\)\)$' THEN
        RAISE EXCEPTION 'unexpected audit_event_entity_type_ck: %', def;
    END IF;
    def := regexp_replace(def, '\]\)\)\)$', added || ')))');
    EXECUTE 'ALTER TABLE gateway.audit_event DROP CONSTRAINT audit_event_entity_type_ck, '
        || 'ADD CONSTRAINT audit_event_entity_type_ck ' || def || ' NOT VALID';

    SELECT pg_get_constraintdef(oid) INTO STRICT def FROM pg_constraint
    WHERE conname = 'admin_resource_version_type_ck'
      AND conrelid = 'gateway.admin_resource_version'::regclass;
    def := regexp_replace(def, '\s*NOT VALID\s*$', '');
    IF def !~ '\]\)\)\)$' THEN
        RAISE EXCEPTION 'unexpected admin_resource_version_type_ck: %', def;
    END IF;
    def := regexp_replace(def, '\]\)\)\)$', added || ')))');
    EXECUTE 'ALTER TABLE gateway.admin_resource_version '
        || 'DROP CONSTRAINT admin_resource_version_type_ck, '
        || 'ADD CONSTRAINT admin_resource_version_type_ck ' || def;
END
$$;
