-- An organization's tracing policy (D-017). Tracing is off until an owner or
-- admin turns it on, for the organization or for chosen teams and service
-- accounts. Content (what was sent and what came back) is stored by default
-- once tracing is on and kept `content_days`, 30 unless set. The request
-- record (`inference_trace`) never carries content and is kept under the
-- retention policy whatever this says.
--
-- The selection is kept as id lists: a team or service account deleted
-- later simply never matches. Writes check that each listed id exists.

CREATE TABLE gateway.organization_trace_policy (
    organization_id UUID PRIMARY KEY
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The versioned admin resource id; stable per organization.
    id UUID NOT NULL UNIQUE,
    mode TEXT NOT NULL CHECK (mode IN ('off', 'organization', 'selected')),
    team_ids UUID[] NOT NULL DEFAULT '{}',
    service_account_ids UUID[] NOT NULL DEFAULT '{}',
    content BOOLEAN NOT NULL DEFAULT true,
    content_days INTEGER NOT NULL DEFAULT 30 CHECK (content_days BETWEEN 1 AND 3650),
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT organization_trace_policy_selection_ck CHECK (
        (mode = 'selected')
        = (cardinality(team_ids) + cardinality(service_account_ids) > 0)
    ),
    CONSTRAINT organization_trace_policy_size_ck CHECK (
        cardinality(team_ids) <= 200 AND cardinality(service_account_ids) <= 200
    )
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
                          'catalog_auto_offer', 'agent', 'agent_token',
                          'price_source_fetch', 'trace_policy')
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
                        'catalog_auto_offer', 'agent', 'agent_token',
                        'price_source_fetch', 'trace_policy')
    ) NOT VALID;
