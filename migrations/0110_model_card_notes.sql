-- Model cards (Phase 8, D-022): the organization's own note on a model, and
-- the admin resource types the later card milestones write (Hugging Face
-- imports, evaluation sets and runs).
--
-- A note belongs to one catalog model identifier, exactly as deployments
-- name it, and is kept whether or not the active release still serves the
-- model. Setting it replaces the text; clearing it stores an empty note.
-- Every text a note has held stays in `model_card_note_revision` (deletes
-- are soft), with who wrote it and when.

CREATE TABLE gateway.model_card_note (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization and model: the admin version resource's id.
    id UUID NOT NULL UNIQUE,
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    note TEXT NOT NULL CHECK (char_length(note) <= 4000),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    PRIMARY KEY (organization_id, model)
);

CREATE TABLE gateway.model_card_note_revision (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    note_id UUID NOT NULL REFERENCES gateway.model_card_note (id) ON DELETE CASCADE,
    note TEXT NOT NULL CHECK (char_length(note) <= 4000),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);
CREATE INDEX model_card_note_revision_note_idx
    ON gateway.model_card_note_revision (note_id, created_at DESC);

-- New admin resource and audit target types. The lists are extended in
-- place rather than restated, so a type another migration added is kept.
DO $$
DECLARE
    added CONSTANT TEXT := ', ''model_card_note''::text, ''model_card_import''::text, '
        || '''model_evaluation_set''::text, ''model_evaluation_run''::text]';
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
