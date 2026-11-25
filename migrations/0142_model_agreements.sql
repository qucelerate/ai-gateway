-- Model agreements accepted through the gateway (D-035): on Bedrock, a
-- third-party model's AWS Marketplace offer, which the account must accept
-- before Bedrock serves the model.
--
-- `model_agreement` is where each model's agreement stands: one row per
-- organization, provider account and foundation model, updated as the
-- worker checks it (pending, then available, then verified or refused by a
-- probe). `model_agreement_acceptance` keeps every attempt: who accepted
-- which offer, its rate card as shown, and what the provider answered.
-- An attempt that calls the provider is written as `accepting` and
-- committed before the call, so a subscription made by a request that then
-- dies is still on record; the request (or, failing it, the worker)
-- settles it. Audit events name the attempt.

CREATE TABLE gateway.model_agreement (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL,
    family TEXT NOT NULL,
    -- The foundation model the agreement is for.
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 255),
    -- The listed identifiers (the model and its inference profiles) it was
    -- needed for when accepted: the verification probe calls one of them.
    unblocks TEXT[] NOT NULL DEFAULT '{}',
    offer_id TEXT NOT NULL CHECK (length(offer_id) BETWEEN 1 AND 64),
    status TEXT NOT NULL
        CHECK (status IN ('pending', 'available', 'verified', 'refused', 'failed')),
    reason TEXT CHECK (length(reason) <= 64),
    detail TEXT CHECK (length(detail) <= 1024),
    provider_code TEXT CHECK (length(provider_code) <= 128),
    accepted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    accepted_at TIMESTAMPTZ NOT NULL,
    checked_at TIMESTAMPTZ,
    -- When the worker checks it next; NULL once it stops.
    next_check_at TIMESTAMPTZ,
    -- When the worker gives up on a pending agreement.
    check_until TIMESTAMPTZ,
    -- Whether the administrator asked for one billed 1-token probe once
    -- the agreement is available; without it only the free availability
    -- check runs.
    probe BOOLEAN NOT NULL DEFAULT false,
    verified_at TIMESTAMPTZ,
    probed_model TEXT,
    -- The latest attempt (gateway.model_agreement_acceptance): a check
    -- stores what it found only while it is still the latest.
    attempt_id UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (organization_id, adapter_instance_key, family, model)
);
CREATE INDEX model_agreement_due_idx
    ON gateway.model_agreement (next_check_at)
    WHERE next_check_at IS NOT NULL;

CREATE TABLE gateway.model_agreement_acceptance (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL,
    family TEXT NOT NULL,
    model TEXT NOT NULL,
    offer_id TEXT,
    -- The rates shown for the offer (the account's region's where the card
    -- names it), as accepted.
    rate_card JSONB NOT NULL DEFAULT '[]'::jsonb,
    outcome TEXT NOT NULL
        CHECK (outcome IN ('accepting', 'accepted', 'already_accepted', 'failed')),
    reason TEXT,
    provider_code TEXT,
    actor_user_id UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    request_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- When `accepting` was settled; NULL for one recorded settled.
    settled_at TIMESTAMPTZ
);
CREATE INDEX model_agreement_acceptance_model_idx
    ON gateway.model_agreement_acceptance
        (organization_id, adapter_instance_key, model, created_at DESC);

-- A new admin resource and audit target type. The lists are extended in
-- place rather than restated, so a type another migration added is kept.
DO $$
DECLARE
    added CONSTANT TEXT := ', ''model_agreement''::text]';
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
