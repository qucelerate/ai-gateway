-- Model cards milestone 4 (Phase 8, D-022): an organization's own
-- evaluation sets, and the scores of runs against several models through
-- the batch API.
--
-- A set is a list of items, each with an id, the expected answer and how an
-- answer matches it. The organization runs it by submitting a batch whose
-- lines' custom_id start with the item id (`<item>:<anything>`), then
-- records a run from that completed batch: the gateway reads the batch's
-- output, matches each answer and keeps only the counts per model, never
-- the answers.

CREATE TABLE gateway.model_evaluation_set (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    description TEXT CHECK (char_length(description) <= 1000),
    -- [{"id", "expected", "match": "exact" | "contains"}], at most 1000.
    items JSONB NOT NULL CHECK (
        jsonb_typeof(items) = 'array' AND jsonb_array_length(items) BETWEEN 1 AND 1000
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    deleted_at TIMESTAMPTZ,
    CONSTRAINT model_evaluation_set_org_id_uq UNIQUE (organization_id, id)
);
CREATE INDEX model_evaluation_set_org_idx
    ON gateway.model_evaluation_set (organization_id, created_at DESC);

CREATE TABLE gateway.model_evaluation_run (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    set_id UUID NOT NULL,
    batch_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    -- Output lines whose custom_id named no item of the set.
    unmatched_lines INTEGER NOT NULL CHECK (unmatched_lines >= 0),
    CONSTRAINT model_evaluation_run_set_fk FOREIGN KEY (organization_id, set_id)
        REFERENCES gateway.model_evaluation_set (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT model_evaluation_run_batch_fk FOREIGN KEY (organization_id, batch_id)
        REFERENCES gateway.batch (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT model_evaluation_run_org_id_uq UNIQUE (organization_id, id)
);
CREATE INDEX model_evaluation_run_set_idx
    ON gateway.model_evaluation_run (organization_id, set_id, created_at DESC);

-- One row per model the batch's lines ran on.
CREATE TABLE gateway.model_evaluation_result (
    organization_id UUID NOT NULL,
    run_id UUID NOT NULL,
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    items INTEGER NOT NULL CHECK (items >= 0),
    answered INTEGER NOT NULL CHECK (answered BETWEEN 0 AND items),
    correct INTEGER NOT NULL CHECK (correct BETWEEN 0 AND answered),
    PRIMARY KEY (run_id, model),
    CONSTRAINT model_evaluation_result_run_fk FOREIGN KEY (organization_id, run_id)
        REFERENCES gateway.model_evaluation_run (organization_id, id) ON DELETE CASCADE
);
CREATE INDEX model_evaluation_result_model_idx
    ON gateway.model_evaluation_result (organization_id, model);
