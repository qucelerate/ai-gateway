-- Request-rate and concurrency limits. A policy caps the organization, one
-- team, or one service account; every policy that covers a request applies.
-- Limits are enforced when a reservation is made, against the reservations
-- themselves: the rate is the reservations created in the last 60 seconds,
-- and concurrency is the reservations still reserved or claimed and not yet
-- expired. The id is derived from the scope, so set and delete address the
-- same versioned resource across re-creation.
CREATE TABLE gateway.rate_limit_policy (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('organization', 'team', 'service-account')),
    team_id UUID,
    service_account_id UUID,
    requests_per_minute INTEGER CHECK (requests_per_minute BETWEEN 1 AND 1000000),
    max_concurrent INTEGER CHECK (max_concurrent BETWEEN 1 AND 1000000),
    created_by UUID,
    updated_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT rate_limit_policy_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL)
        OR (scope_kind = 'service-account' AND team_id IS NULL AND service_account_id IS NOT NULL)
    ),
    CONSTRAINT rate_limit_policy_limit_ck
        CHECK (requests_per_minute IS NOT NULL OR max_concurrent IS NOT NULL),
    CONSTRAINT rate_limit_policy_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT rate_limit_policy_team_fk FOREIGN KEY (organization_id, team_id)
        REFERENCES gateway.team (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT rate_limit_policy_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT rate_limit_policy_created_by_fk FOREIGN KEY (created_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT rate_limit_policy_updated_by_fk FOREIGN KEY (updated_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX rate_limit_policy_scope_uq ON gateway.rate_limit_policy (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid)
);

-- Counting a scope's recent and in-flight reservations.
CREATE INDEX accounting_reservation_org_created_idx
    ON gateway.accounting_reservation (organization_id, created_at);
CREATE INDEX accounting_reservation_team_created_idx
    ON gateway.accounting_reservation (organization_id, team_id, created_at)
    WHERE team_id IS NOT NULL;
CREATE INDEX accounting_reservation_service_account_created_idx
    ON gateway.accounting_reservation (organization_id, service_account_id, created_at)
    WHERE service_account_id IS NOT NULL;
CREATE INDEX accounting_reservation_in_flight_idx
    ON gateway.accounting_reservation (organization_id, expires_at)
    WHERE state IN ('reserved', 'claimed');
CREATE INDEX accounting_reservation_team_in_flight_idx
    ON gateway.accounting_reservation (organization_id, team_id, expires_at)
    WHERE state IN ('reserved', 'claimed') AND team_id IS NOT NULL;
CREATE INDEX accounting_reservation_service_account_in_flight_idx
    ON gateway.accounting_reservation (organization_id, service_account_id, expires_at)
    WHERE state IN ('reserved', 'claimed') AND service_account_id IS NOT NULL;

-- Policy writes are versioned and audited admin resources.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy')
    ) NOT VALID;
