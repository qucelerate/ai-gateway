-- Public price sources and price mappings. The worker also fetches the Azure
-- Retail Prices API (Azure OpenAI) and the AWS Price List (Bedrock). Their
-- prices are published per product rather than per model, so they are
-- recorded unmapped; an organization maps a deployment to the item that
-- prices it. Nothing here changes a catalog price.

ALTER TABLE gateway.price_source
    DROP CONSTRAINT price_source_kind_check,
    ADD CONSTRAINT price_source_kind_check
        CHECK (kind IN ('price-sheet', 'azure-retail', 'aws-price-list'));

-- Which published item prices a deployment (by name, so a mapping survives
-- catalog upgrades), per source. `region` pins the item's region; NULL
-- prefers the deployment's own region, then a global price.
CREATE TABLE gateway.catalog_price_mapping (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 255),
    source TEXT NOT NULL REFERENCES gateway.price_source (source) ON DELETE CASCADE,
    item TEXT NOT NULL CHECK (octet_length(item) BETWEEN 1 AND 255),
    region TEXT CHECK (region IS NULL OR octet_length(region) BETWEEN 1 AND 255),
    created_by UUID,
    updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT catalog_price_mapping_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_price_mapping_deployment_uq UNIQUE (organization_id, deployment, source),
    CONSTRAINT catalog_price_mapping_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT catalog_price_mapping_created_by_fk FOREIGN KEY (created_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT catalog_price_mapping_updated_by_fk FOREIGN KEY (updated_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

CREATE INDEX catalog_price_mapping_source_idx ON gateway.catalog_price_mapping (source);

-- Setting or deleting a mapping is an audited admin write.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping')
    ) NOT VALID;
