-- Spending budgets for an organization, one team, or one service account.
-- A policy is the administrator's setting; enforcement stays with the quota
-- buckets that reservations already charge. A monthly policy has one bucket
-- per calendar month (UTC), created by the first reservation that needs it;
-- a fixed policy has one bucket for its window. Every bucket covering a
-- request is charged, and a request no bucket covers is refused. The id is
-- derived from the scope, so set and delete address the same versioned
-- resource across re-creation.
CREATE TABLE gateway.budget_policy (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('organization', 'team', 'service-account')),
    team_id UUID,
    service_account_id UUID,
    currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    -- Money units: 10^-6 of the currency's smallest unit ($100 is 10^10).
    limit_minor BIGINT NOT NULL CHECK (limit_minor >= 0),
    period TEXT NOT NULL CHECK (period IN ('monthly', 'fixed')),
    starts_at TIMESTAMPTZ,
    ends_at TIMESTAMPTZ,
    created_by UUID,
    updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT budget_policy_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL)
        OR (scope_kind = 'service-account' AND team_id IS NULL AND service_account_id IS NOT NULL)
    ),
    CONSTRAINT budget_policy_period_ck CHECK (
        (period = 'monthly' AND starts_at IS NULL AND ends_at IS NULL)
        OR (period = 'fixed' AND starts_at IS NOT NULL AND ends_at > starts_at)
    ),
    CONSTRAINT budget_policy_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT budget_policy_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT budget_policy_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT budget_policy_created_by_fk FOREIGN KEY (created_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT budget_policy_updated_by_fk FOREIGN KEY (updated_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX budget_policy_scope_uq ON gateway.budget_policy (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid)
);

-- A bucket whose policy was removed or replaced stops covering requests;
-- its counters stay for in-flight settlements and reporting.
ALTER TABLE gateway.quota_bucket ADD COLUMN retired_at TIMESTAMPTZ;

ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy')
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
                        'budget_policy')
    ) NOT VALID;
