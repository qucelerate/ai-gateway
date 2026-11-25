-- Model availability evidence (D-018): what the gateway has seen about whether
-- an organization's provider account can call a model, beside what the
-- account's listing says (`catalog_discovered_model.availability`).
--
-- One row per (organization, account, provider family, model identifier the
-- gateway calls), the latest observation. The family is part of the key, as
-- in `catalog_discovered_model`: a release may redeclare an account key with
-- another family, which is another account.
--
-- state:
--   verified         a call succeeded;
--   refused          a call was refused for this model: the account may not
--                    call it, or the provider does not serve it where the
--                    gateway calls it (`reason`);
--   entitled         a read-only control-plane check says the account may
--                    call it (written by later milestones);
--   requires_action  the provider offers it, but something the tenant does
--                    stands in the way (`reason`; later milestones).
-- source: where the observation came from (`traffic`: a real request;
-- `probe`, `control_plane`: later milestones).
--
-- `verified_at` is the newest success, kept when a later observation refuses
-- the model, so an operator sees it used to work. Observations from several
-- processes may arrive out of order: the newest `observed_at` sets the
-- verdict, and any success can still advance `verified_at`.
--
-- `refused_since` is when the current run of refusals began: later refusals
-- keep it, and a success clears it. Automatic offers act on a refusal once
-- another model of the account has succeeded since then (the credentials
-- work, so the refusal is the model's), and the refusal stays in force while
-- the model keeps being refused.
CREATE TABLE gateway.catalog_model_evidence (
    organization_id UUID NOT NULL
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic', 'gemini', 'bedrock',
                                           'azure-openai', 'azure-ai-foundry',
                                           'openai-compatible')),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    state TEXT NOT NULL
        CHECK (state IN ('verified', 'refused', 'entitled', 'requires_action')),
    reason TEXT CHECK (reason IN ('access_denied', 'not_found')),
    source TEXT NOT NULL CHECK (source IN ('traffic', 'probe', 'control_plane')),
    -- The deployment the observation was made through.
    deployment TEXT CHECK (deployment IS NULL OR length(deployment) BETWEEN 1 AND 255),
    provider_status SMALLINT CHECK (provider_status BETWEEN 100 AND 599),
    provider_code TEXT CHECK (provider_code IS NULL OR length(provider_code) BETWEEN 1 AND 128),
    observed_at TIMESTAMPTZ NOT NULL,
    verified_at TIMESTAMPTZ,
    refused_since TIMESTAMPTZ,
    PRIMARY KEY (organization_id, adapter_instance_key, family, model),
    CONSTRAINT catalog_model_evidence_reason_ck
        CHECK ((state IN ('refused', 'requires_action')) = (reason IS NOT NULL)),
    CONSTRAINT catalog_model_evidence_verified_ck
        CHECK ((state <> 'verified' OR verified_at = observed_at)
               AND (verified_at IS NULL OR verified_at <= observed_at)),
    CONSTRAINT catalog_model_evidence_refused_ck
        CHECK ((state = 'refused') = (refused_since IS NOT NULL)
               AND (refused_since IS NULL OR refused_since <= observed_at))
);

-- An automatic offer may decide a listed model is refused: the account's
-- evidence says it cannot call it, so it is not offered.
ALTER TABLE gateway.catalog_auto_offered_model
    DROP CONSTRAINT catalog_auto_offered_model_status_check,
    ADD CONSTRAINT catalog_auto_offered_model_status_check
        CHECK (status IN ('offered', 'needs_price', 'unsupported', 'withdrawn',
                          'catalog_full', 'refused'));
