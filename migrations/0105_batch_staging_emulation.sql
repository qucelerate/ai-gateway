-- Phase 7 milestones 3 to 5 (D-021, docs/batch-design.md): Bedrock and
-- Gemini batch jobs staged through the tenant's own bucket, and emulated
-- batches for families without a native batch API.

-- An organization's batch settings. Emulated batches run each line as an
-- ordinary request at the on-demand price, so they are off until an
-- administrator allows them. No row: not allowed.
CREATE TABLE gateway.organization_batch_settings (
    organization_id UUID PRIMARY KEY REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization: the admin version resource's id.
    id UUID NOT NULL UNIQUE,
    emulated_allowed BOOLEAN NOT NULL DEFAULT false,
    -- Lines of one emulated job in flight at once, within the
    -- organization's rate limits.
    emulated_concurrency INTEGER NOT NULL DEFAULT 4 CHECK (emulated_concurrency BETWEEN 1 AND 32),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

-- Where a provider connection's batch jobs stage their input and output:
-- a bucket in the tenant's own cloud, written and read with the
-- connection's own credentials (D-014), so no storage keys are stored.
-- Bedrock (`s3`) also names the service role its batch job assumes; Gemini
-- on Agent Platform (`gcs`) needs none, as its service agent reads and
-- writes the bucket. Rows are never removed: a deleted one is hidden.
CREATE TABLE gateway.batch_staging (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    id UUID NOT NULL UNIQUE,
    -- The provider account (`adapter_instance_key`) whose jobs stage here.
    connection TEXT NOT NULL CHECK (length(connection) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (kind IN ('s3', 'gcs')),
    bucket TEXT NOT NULL CHECK (bucket ~ '^[a-z0-9][a-z0-9._-]{1,220}[a-z0-9]$'),
    -- Object names start with it; empty, or ending in `/`.
    prefix TEXT NOT NULL DEFAULT '' CHECK (
        length(prefix) <= 256 AND (prefix = '' OR (prefix ~ '^[A-Za-z0-9!_.*''()/-]+/$'
                                                   AND prefix !~ '^/' AND prefix !~ '//'))),
    -- The bucket's region, for S3 request signing.
    region TEXT CHECK (region IS NULL OR region ~ '^[a-z]{2}(-[a-z0-9]+)+-[0-9]+$'),
    role_arn TEXT CHECK (role_arn IS NULL OR (length(role_arn) <= 600 AND
                         role_arn ~ '^arn:aws(-[a-z-]+)?:iam::[0-9]{12}:role/[A-Za-z0-9+=,.@_/-]+$')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (organization_id, connection),
    CONSTRAINT batch_staging_kind_ck CHECK (
        (kind = 's3' AND role_arn IS NOT NULL AND region IS NOT NULL)
        OR (kind = 'gcs' AND role_arn IS NULL AND region IS NULL))
);

-- An emulated job's line is marked sent before its request goes out. A
-- worker that dies with the line in flight cannot know whether the provider
-- ran it, so the line is never sent again: it settles from its saved
-- result, or goes to reconciliation. The mark also counts the line against
-- the organization's rate limits.
ALTER TABLE gateway.batch_line ADD COLUMN sent_at TIMESTAMPTZ;
CREATE INDEX batch_line_sent_idx ON gateway.batch_line (organization_id, sent_at)
    WHERE sent_at IS NOT NULL;

-- Admin writes to the settings and staging are versioned and audited as
-- `batch_settings` and `batch_staging`; an admin's cancel of a batch as
-- `batch`.
-- The lists are extended in place rather than restated, so a type another
-- migration added (in whatever order they apply) is kept.
DO $$
DECLARE
    added CONSTANT TEXT := ', ''batch_settings''::text, ''batch_staging''::text, '
        || '''batch''::text]';
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
