-- Model discovery. The worker asks each provider account in an organization's
-- active release which models it can call, every six hours or when an
-- operator asks, and records what it saw. Discovery never changes a catalog
-- release: it only reports drift between the provider and the catalog.

CREATE TABLE gateway.catalog_sync_job (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic', 'gemini', 'bedrock',
                                           'azure-openai', 'azure-ai-foundry',
                                           'openai-compatible')),
    trigger TEXT NOT NULL CHECK (trigger IN ('scheduled', 'manual')),
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'unsupported')),
    -- Each claim counts an attempt; a worker that stops mid-run leaves its
    -- lease to expire, and the job is claimed again until attempts run out.
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 3),
    lease_expires_at TIMESTAMPTZ,
    -- A fixed code, never provider text, which may echo request details.
    error_code TEXT CHECK (error_code IS NULL OR error_code ~ '^[a-z_]{1,64}$'),
    models_listed INTEGER CHECK (models_listed IS NULL OR models_listed >= 0),
    models_added INTEGER CHECK (models_added IS NULL OR models_added >= 0),
    models_missing INTEGER CHECK (models_missing IS NULL OR models_missing >= 0),
    requested_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    CONSTRAINT catalog_sync_job_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_sync_job_lease_ck CHECK ((status = 'running') = (lease_expires_at IS NOT NULL)),
    CONSTRAINT catalog_sync_job_finished_ck
        CHECK ((status IN ('succeeded', 'failed', 'unsupported')) = (finished_at IS NOT NULL)),
    CONSTRAINT catalog_sync_job_counts_ck
        CHECK ((status = 'succeeded') = (models_listed IS NOT NULL))
);

-- At most one open job per provider account, so a manual request cannot
-- stack behind a scheduled one and concurrent schedulers insert once.
CREATE UNIQUE INDEX catalog_sync_job_open_uq
    ON gateway.catalog_sync_job (organization_id, adapter_instance_key)
    WHERE status IN ('queued', 'running');
CREATE INDEX catalog_sync_job_account_idx
    ON gateway.catalog_sync_job (organization_id, adapter_instance_key, created_at DESC);
CREATE INDEX catalog_sync_job_org_idx
    ON gateway.catalog_sync_job (organization_id, created_at DESC, id DESC);
CREATE INDEX catalog_sync_job_claim_idx ON gateway.catalog_sync_job (created_at)
    WHERE status IN ('queued', 'running');
CREATE INDEX catalog_sync_job_finished_idx ON gateway.catalog_sync_job (finished_at)
    WHERE finished_at IS NOT NULL;

-- Models are keyed by provider account rather than release, so their
-- history survives activating a new release that keeps the account.
CREATE TABLE gateway.catalog_discovered_model (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic', 'gemini', 'bedrock',
                                           'azure-openai', 'azure-ai-foundry',
                                           'openai-compatible')),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    display_name TEXT CHECK (display_name IS NULL OR octet_length(display_name) BETWEEN 1 AND 255),
    first_seen_at TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL,
    -- Set by the first successful listing that no longer reports the model,
    -- cleared when a listing reports it again.
    missing_since TIMESTAMPTZ,
    PRIMARY KEY (organization_id, adapter_instance_key, family, model),
    CONSTRAINT catalog_discovered_model_seen_ck CHECK (first_seen_at <= last_seen_at),
    CONSTRAINT catalog_discovered_model_missing_ck
        CHECK (missing_since IS NULL OR missing_since > last_seen_at)
);

CREATE INDEX catalog_discovered_model_missing_idx
    ON gateway.catalog_discovered_model (missing_since) WHERE missing_since IS NOT NULL;

-- A manual sync request is an audited admin write on the job it creates.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job')
    ) NOT VALID;
