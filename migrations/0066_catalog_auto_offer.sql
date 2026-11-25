-- Automatic model offers. An organization opts a provider account in; after
-- each successful discovery listing the worker offers the account's listed
-- chat and embedding models that a price source prices, each as a
-- deployment aliased by its model identifier, and keeps their prices in step
-- with the source. Every change is a new release, created and activated by
-- the system and audited. Models without a price are recorded for an admin
-- to price (with a price mapping), never guessed at.

-- One opt-in per provider account.
CREATE TABLE gateway.catalog_auto_offer (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    -- The admin resource id (versions, audit, idempotency).
    id UUID NOT NULL UNIQUE,
    -- The location offered deployments declare, which also picks regional
    -- prices; NULL takes the account's existing deployments' location.
    location TEXT CHECK (location IS NULL OR length(location) BETWEEN 1 AND 255),
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- The latest offer run: when, the release active after it (a run whose
    -- release differs from the active one knows an admin has changed the
    -- catalog since), or why it changed nothing, as a fixed code.
    applied_at TIMESTAMPTZ,
    applied_release_id UUID,
    error_code TEXT CHECK (error_code IS NULL OR error_code ~ '^[a-z_]{1,64}$'),
    PRIMARY KEY (organization_id, adapter_instance_key)
);

-- What the latest run decided for each listed model. `withdrawn` marks an
-- offered deployment an admin's release dropped: it is not offered again
-- until the opt-in is set again. `catalog_full`: the release had no room.
CREATE TABLE gateway.catalog_auto_offered_model (
    organization_id UUID NOT NULL,
    adapter_instance_key TEXT NOT NULL,
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 255),
    status TEXT NOT NULL
        CHECK (status IN ('offered', 'needs_price', 'unsupported', 'withdrawn',
                          'catalog_full')),
    -- The price source and item of an offered model's price.
    price_source TEXT CHECK (price_source IS NULL OR length(price_source) BETWEEN 1 AND 128),
    price_item TEXT CHECK (price_item IS NULL OR octet_length(price_item) BETWEEN 1 AND 255),
    updated_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (organization_id, adapter_instance_key, model),
    CONSTRAINT catalog_auto_offered_model_offer_fk FOREIGN KEY (organization_id, adapter_instance_key)
        REFERENCES gateway.catalog_auto_offer (organization_id, adapter_instance_key)
        ON DELETE CASCADE,
    CONSTRAINT catalog_auto_offered_model_price_ck
        CHECK ((status = 'offered') = (price_source IS NOT NULL AND price_item IS NOT NULL))
);

ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy',
                          'invitation', 'identity_provider', 'config_pin', 'usage_export',
                          'catalog_auto_offer')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping', 'retention_policy',
                        'data_export', 'cloud_principal_binding', 'invitation',
                        'budget_policy', 'identity_provider', 'config_pin', 'usage_export',
                        'catalog_auto_offer')
    ) NOT VALID;
