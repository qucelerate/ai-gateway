-- Phase 7 (D-021, docs/batch-design.md): batches of chat and embedding
-- requests through one OpenAI-shaped API, routed per line by alias, reserved
-- in full at submission and settled per line.

-- Batch operations: a deployment's batch capability, price and routes are
-- their own (a batch price is a price like any other, D-004).
ALTER TABLE gateway.catalog_capability
    DROP CONSTRAINT catalog_capability_operation_check,
    ADD CONSTRAINT catalog_capability_operation_check CHECK (
        operation IN ('chat', 'chat_stream', 'embeddings', 'chat_batch', 'embeddings_batch'));
ALTER TABLE gateway.catalog_price
    DROP CONSTRAINT catalog_price_operation_check,
    ADD CONSTRAINT catalog_price_operation_check CHECK (
        operation IN ('chat', 'chat_stream', 'embeddings', 'chat_batch', 'embeddings_batch'));
ALTER TABLE gateway.catalog_route
    DROP CONSTRAINT catalog_route_operation_check,
    ADD CONSTRAINT catalog_route_operation_check CHECK (
        operation IN ('chat', 'chat_stream', 'embeddings', 'chat_batch', 'embeddings_batch'));
ALTER TABLE gateway.catalog_price_override
    DROP CONSTRAINT catalog_price_override_operation_check,
    ADD CONSTRAINT catalog_price_override_operation_check CHECK (
        operation IN ('chat', 'chat_stream', 'embeddings', 'chat_batch', 'embeddings_batch'));

-- Families without embeddings have no embedding batches either.
CREATE OR REPLACE FUNCTION gateway.prevent_anthropic_embeddings()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE family_name TEXT;
BEGIN
    SELECT p.family INTO family_name
      FROM gateway.catalog_deployment d
      JOIN gateway.catalog_provider_account p
        ON p.organization_id=d.organization_id AND p.release_id=d.release_id
       AND p.id=d.provider_account_id
       AND p.adapter_instance_key=d.adapter_instance_key
     WHERE d.organization_id=NEW.organization_id
       AND d.release_id=NEW.release_id AND d.id=NEW.deployment_id;
    IF family_name IN ('anthropic', 'bedrock')
       AND NEW.operation IN ('embeddings', 'embeddings_batch') THEN
        RAISE EXCEPTION '% embeddings capability is unsupported', family_name;
    END IF;
    IF TG_TABLE_NAME = 'catalog_price' THEN
        IF family_name = 'anthropic'
           AND (NEW.cached_input_minor_per_token <> 0
                OR NEW.reasoning_minor_per_token <> 0
                OR NEW.other_minor_per_token <> 0)
        THEN
            RAISE EXCEPTION 'anthropic unsupported usage prices must be zero';
        ELSIF family_name = 'bedrock' AND NEW.reasoning_minor_per_token <> 0 THEN
            RAISE EXCEPTION 'bedrock unsupported usage prices must be zero';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- Files a service account uploads (batch input) or the gateway writes
-- (batch output). The content lives in the file store, under `object_key`;
-- it holds request and response content, so the organization's retention
-- policy purges it (`content_purged_at`). Records are never removed.
CREATE TABLE gateway.batch_file (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    service_account_id UUID NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('batch', 'batch_output')),
    filename TEXT NOT NULL CHECK (length(filename) BETWEEN 1 AND 255),
    bytes BIGINT NOT NULL CHECK (bytes >= 0),
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    object_key TEXT NOT NULL CHECK (length(object_key) BETWEEN 1 AND 1024),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    deleted_at TIMESTAMPTZ,
    content_purged_at TIMESTAMPTZ,
    CONSTRAINT batch_file_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT batch_file_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE
);
CREATE INDEX batch_file_owner_idx
    ON gateway.batch_file (organization_id, service_account_id, created_at DESC, id DESC);
CREATE INDEX batch_file_purge_idx
    ON gateway.batch_file (organization_id, created_at) WHERE content_purged_at IS NULL;

-- A batch: one client request, validated, routed and reserved in full in
-- the transaction that creates it, so nothing is partly submitted.
CREATE TABLE gateway.batch (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    team_id UUID,
    service_account_id UUID NOT NULL,
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 255),
    fingerprint BYTEA NOT NULL CHECK (octet_length(fingerprint) = 32),
    endpoint TEXT NOT NULL CHECK (endpoint IN ('/v1/chat/completions', '/v1/embeddings')),
    operation TEXT NOT NULL CHECK (operation IN ('chat_batch', 'embeddings_batch')),
    input_file_id UUID NOT NULL,
    output_file_id UUID,
    error_file_id UUID,
    completion_window TEXT NOT NULL CHECK (completion_window = '24h'),
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    status TEXT NOT NULL CHECK (status IN ('validating', 'in_progress', 'finalizing',
                                           'completed', 'failed', 'expired',
                                           'cancelling', 'cancelled')),
    total_lines INTEGER NOT NULL CHECK (total_lines >= 0),
    completed_lines INTEGER NOT NULL DEFAULT 0 CHECK (completed_lines >= 0),
    failed_lines INTEGER NOT NULL DEFAULT 0 CHECK (failed_lines >= 0),
    -- Why the batch failed as a whole, as OpenAI's `errors.data`.
    errors JSONB CHECK (errors IS NULL OR jsonb_typeof(errors) = 'array'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- The end of the completion window.
    expires_at TIMESTAMPTZ NOT NULL,
    in_progress_at TIMESTAMPTZ,
    finalizing_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    failed_at TIMESTAMPTZ,
    expired_at TIMESTAMPTZ,
    cancelling_at TIMESTAMPTZ,
    cancelled_at TIMESTAMPTZ,
    -- Who finalizes it next, so one worker writes the output file.
    lease_token UUID,
    lease_expires_at TIMESTAMPTZ,
    CONSTRAINT batch_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT batch_idempotency_uq UNIQUE (organization_id, service_account_id, idempotency_key),
    CONSTRAINT batch_counts_ck CHECK (completed_lines + failed_lines <= total_lines),
    CONSTRAINT batch_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT batch_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT batch_input_file_fk FOREIGN KEY (organization_id, input_file_id)
        REFERENCES gateway.batch_file (organization_id, id),
    CONSTRAINT batch_output_file_fk FOREIGN KEY (organization_id, output_file_id)
        REFERENCES gateway.batch_file (organization_id, id),
    CONSTRAINT batch_error_file_fk FOREIGN KEY (organization_id, error_file_id)
        REFERENCES gateway.batch_file (organization_id, id)
);
CREATE INDEX batch_owner_idx
    ON gateway.batch (organization_id, service_account_id, created_at DESC, id DESC);
CREATE INDEX batch_open_idx ON gateway.batch (status)
    WHERE status IN ('in_progress', 'finalizing', 'cancelling');

-- One provider job: the lines of a batch that one deployment serves. Its
-- record is written before any provider call, and `ordinal` tags the
-- provider job (OpenAI's `metadata`), so a worker that crashed after
-- submitting finds the job again rather than submitting it twice.
CREATE TABLE gateway.batch_job (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    batch_id UUID NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 255),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 255),
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (length(family) BETWEEN 1 AND 64),
    mode TEXT NOT NULL CHECK (mode IN ('native', 'emulated')),
    -- `pending`: not submitted. `submitted`: the provider has it.
    -- `finished`: its results are collected and every line settled.
    state TEXT NOT NULL CHECK (state IN ('pending', 'submitted', 'finished')),
    -- The provider's own status and ids, as it reported them last.
    provider_status TEXT CHECK (provider_status IS NULL OR length(provider_status) <= 64),
    provider_input_file_id TEXT CHECK (length(provider_input_file_id) <= 255),
    provider_batch_id TEXT CHECK (length(provider_batch_id) <= 255),
    provider_output_file_id TEXT CHECK (length(provider_output_file_id) <= 255),
    provider_error_file_id TEXT CHECK (length(provider_error_file_id) <= 255),
    cancel_requested BOOLEAN NOT NULL DEFAULT false,
    -- The job's normalized result lines, in the file store, once collected.
    result_object_key TEXT CHECK (length(result_object_key) BETWEEN 1 AND 1024),
    next_run_at TIMESTAMPTZ NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    lease_token UUID,
    lease_expires_at TIMESTAMPTZ,
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) <= 64),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    submitted_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    CONSTRAINT batch_job_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT batch_job_ordinal_uq UNIQUE (batch_id, ordinal),
    CONSTRAINT batch_job_batch_fk FOREIGN KEY (organization_id, batch_id)
        REFERENCES gateway.batch (organization_id, id) ON DELETE CASCADE
);
CREATE INDEX batch_job_due_idx ON gateway.batch_job (next_run_at)
    WHERE state IN ('pending', 'submitted');
CREATE INDEX batch_job_batch_idx ON gateway.batch_job (organization_id, batch_id, ordinal);

-- Each line of a batch, with the reservation it is charged against. No
-- content: the input and output live in files.
CREATE TABLE gateway.batch_line (
    organization_id UUID NOT NULL,
    batch_id UUID NOT NULL,
    line_number INTEGER NOT NULL CHECK (line_number >= 1),
    custom_id TEXT NOT NULL CHECK (length(custom_id) BETWEEN 1 AND 512),
    job_id UUID NOT NULL,
    reservation_id UUID NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'succeeded', 'failed', 'expired',
                                         'cancelled', 'unknown')),
    status_code INTEGER CHECK (status_code IS NULL OR status_code BETWEEN 100 AND 599),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) <= 64),
    finished_at TIMESTAMPTZ,
    PRIMARY KEY (batch_id, line_number),
    CONSTRAINT batch_line_custom_id_uq UNIQUE (batch_id, custom_id),
    CONSTRAINT batch_line_reservation_uq UNIQUE (reservation_id),
    CONSTRAINT batch_line_batch_fk FOREIGN KEY (organization_id, batch_id)
        REFERENCES gateway.batch (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT batch_line_job_fk FOREIGN KEY (organization_id, job_id)
        REFERENCES gateway.batch_job (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT batch_line_reservation_fk FOREIGN KEY (organization_id, reservation_id)
        REFERENCES gateway.accounting_reservation (organization_id, id) ON DELETE RESTRICT
);
CREATE INDEX batch_line_job_idx ON gateway.batch_line (job_id, state);

-- A line's reservation names its batch: budgets report reserved batch spend
-- apart, and rate limits (requests per minute, concurrency) count the batch
-- once, at submission, rather than each of its lines for a day.
ALTER TABLE gateway.accounting_reservation ADD COLUMN batch_id UUID;
ALTER TABLE gateway.accounting_reservation
    ADD CONSTRAINT accounting_reservation_batch_fk FOREIGN KEY (organization_id, batch_id)
        REFERENCES gateway.batch (organization_id, id) ON DELETE RESTRICT;
CREATE INDEX accounting_reservation_batch_idx
    ON gateway.accounting_reservation (organization_id, batch_id) WHERE batch_id IS NOT NULL;

CREATE OR REPLACE FUNCTION gateway.prevent_reservation_batch_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.batch_id IS DISTINCT FROM NEW.batch_id THEN
        RAISE EXCEPTION 'a reservation''s batch is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER accounting_reservation_batch_immutable
    BEFORE UPDATE ON gateway.accounting_reservation
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_reservation_batch_change();
