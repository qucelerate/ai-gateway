-- Model cards milestone 3 (Phase 8, D-022): a card an organization imports
-- from a Hugging Face Hub model card, for a model no curated or operator
-- card covers.
--
-- One import per organization and model identifier; importing again
-- replaces it. Removing it is soft: `deleted_at` hides it, and every card an
-- import has held stays in `model_card_import_revision` with who imported it
-- and when.

CREATE TABLE gateway.model_card_import (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization and model: the admin version resource's id.
    id UUID NOT NULL UNIQUE,
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    repository TEXT NOT NULL CHECK (octet_length(repository) BETWEEN 3 AND 193),
    -- Whether the caller stated the repository is the model provider's own,
    -- so its facts are self-reported.
    provider_repository BOOLEAN NOT NULL,
    -- A `qucelerate.model-card.v1` document.
    card JSONB NOT NULL CHECK (
        jsonb_typeof(card) = 'object' AND octet_length(card::text) <= 262144
    ),
    imported_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    imported_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    deleted_at TIMESTAMPTZ,
    PRIMARY KEY (organization_id, model)
);

CREATE TABLE gateway.model_card_import_revision (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    import_id UUID NOT NULL REFERENCES gateway.model_card_import (id) ON DELETE CASCADE,
    -- NULL records a removal.
    repository TEXT,
    provider_repository BOOLEAN,
    card JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);
CREATE INDEX model_card_import_revision_import_idx
    ON gateway.model_card_import_revision (import_id, created_at DESC);
