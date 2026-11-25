-- Budgets per end user of an app (D-028). An app's service account names
-- the user behind each request (`Gateway-End-User`); each user is on one of
-- the service account's plans, a monthly allowance, and may hold prepaid
-- credits. Both are quota buckets (scope `end-user`), so reservations,
-- settlement, expiry and operator resolution charge them as they charge
-- budgets; the allocations of one request across an end user's allowance
-- and credits carry a `draw_rank`, and are settled in rank order instead of
-- each being charged the full amount.

-- One per service account that has turned end users on. `currency` is fixed
-- once a plan or a grant exists.
CREATE TABLE gateway.end_user_setting (
    service_account_id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    -- The admin version resource's id; stable per service account.
    id UUID NOT NULL UNIQUE,
    requirement TEXT NOT NULL DEFAULT 'optional' CHECK (requirement IN ('optional', 'required')),
    currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    -- The plan of a user the app never assigned; NULL refuses such users.
    default_plan_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT end_user_setting_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE
);

CREATE TABLE gateway.end_user_plan (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    service_account_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,62}$'),
    -- Money units per UTC calendar month; 0 is a credits-only plan.
    allowance_minor BIGINT NOT NULL CHECK (allowance_minor >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Soft delete: a removed plan is hidden, never dropped.
    deleted_at TIMESTAMPTZ,
    CONSTRAINT end_user_plan_setting_fk FOREIGN KEY (service_account_id)
        REFERENCES gateway.end_user_setting (service_account_id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX end_user_plan_name_uq ON gateway.end_user_plan (service_account_id, name)
    WHERE deleted_at IS NULL;

ALTER TABLE gateway.end_user_setting
    ADD CONSTRAINT end_user_setting_default_plan_fk FOREIGN KEY (default_plan_id)
        REFERENCES gateway.end_user_plan (id);

-- An end user, created by their first request or the first admin write.
-- `external_id` is the app's own ID, stored as sent.
CREATE TABLE gateway.end_user (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    service_account_id UUID NOT NULL,
    external_id TEXT NOT NULL CHECK (octet_length(external_id) BETWEEN 1 AND 256),
    -- NULL: the service account's default plan.
    plan_id UUID REFERENCES gateway.end_user_plan (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT end_user_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT end_user_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX end_user_external_uq ON gateway.end_user (service_account_id, external_id);

-- Prepaid credits: an append-only ledger of grants. A grant's balance is its
-- quota bucket (`limit_minor` the amount less what was reversed).
CREATE TABLE gateway.end_user_credit_grant (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    service_account_id UUID NOT NULL,
    end_user_id UUID NOT NULL,
    -- The app's purchase reference: one grant per reference per service
    -- account, whoever sends it.
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    amount_minor BIGINT NOT NULL CHECK (amount_minor > 0),
    currency CHAR(3) NOT NULL,
    expires_at TIMESTAMPTZ,
    quota_bucket_id UUID NOT NULL UNIQUE,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    granted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT end_user_credit_grant_user_fk FOREIGN KEY (organization_id, end_user_id)
        REFERENCES gateway.end_user (organization_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX end_user_credit_grant_reference_uq
    ON gateway.end_user_credit_grant (service_account_id, reference);
CREATE INDEX end_user_credit_grant_user_idx ON gateway.end_user_credit_grant (end_user_id, granted_at);

-- A refund or chargeback: takes back up to what is unspent and unreserved
-- of one grant.
CREATE TABLE gateway.end_user_credit_reversal (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    grant_id UUID NOT NULL REFERENCES gateway.end_user_credit_grant (id) ON DELETE CASCADE,
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    requested_minor BIGINT NOT NULL CHECK (requested_minor > 0),
    reversed_minor BIGINT NOT NULL CHECK (reversed_minor >= 0 AND reversed_minor <= requested_minor),
    reason TEXT NOT NULL CHECK (length(reason) BETWEEN 1 AND 500),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX end_user_credit_reversal_reference_uq
    ON gateway.end_user_credit_reversal (grant_id, reference);

CREATE FUNCTION gateway.prevent_credit_ledger_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Only the organization's removal cascades: a service account with
    -- grants is disabled, never deleted.
    IF TG_OP = 'DELETE' AND NOT EXISTS (
        SELECT 1 FROM gateway.organization WHERE id = OLD.organization_id
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'end-user credit ledgers are append-only';
END;
$$;
CREATE TRIGGER end_user_credit_grant_immutable
    BEFORE UPDATE OR DELETE ON gateway.end_user_credit_grant
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_credit_ledger_mutation();
CREATE TRIGGER end_user_credit_reversal_immutable
    BEFORE UPDATE OR DELETE ON gateway.end_user_credit_reversal
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_credit_ledger_mutation();

-- End-user buckets: a month's allowance (no grant) or one grant's credits.
ALTER TABLE gateway.quota_bucket
    ADD COLUMN end_user_id UUID,
    ADD COLUMN credit_grant_id UUID,
    ADD CONSTRAINT quota_bucket_end_user_fk FOREIGN KEY (organization_id, end_user_id)
        REFERENCES gateway.end_user (organization_id, id) ON DELETE CASCADE,
    DROP CONSTRAINT quota_bucket_scope_kind_check,
    ADD CONSTRAINT quota_bucket_scope_kind_check
        CHECK (scope_kind IN ('organization', 'team', 'service-account', 'api-key', 'end-user')),
    DROP CONSTRAINT quota_bucket_scope_ck,
    ADD CONSTRAINT quota_bucket_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NULL AND end_user_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL
            AND api_key_id IS NULL AND end_user_id IS NULL)
        OR (scope_kind = 'service-account' AND service_account_id IS NOT NULL
            AND api_key_id IS NULL AND end_user_id IS NULL)
        OR (scope_kind = 'api-key' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NOT NULL AND end_user_id IS NULL)
        OR (scope_kind = 'end-user' AND team_id IS NULL AND service_account_id IS NOT NULL
            AND api_key_id IS NULL AND end_user_id IS NOT NULL)
    ),
    ADD CONSTRAINT quota_bucket_credit_ck CHECK (credit_grant_id IS NULL OR end_user_id IS NOT NULL);

-- Budget buckets keep their key; end-user buckets have their own.
DROP INDEX gateway.quota_bucket_scope_uq;
CREATE UNIQUE INDEX quota_bucket_scope_uq ON gateway.quota_bucket (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(api_key_id, '00000000-0000-0000-0000-000000000000'::uuid),
    period_start, period_end, currency
) WHERE end_user_id IS NULL;
CREATE UNIQUE INDEX quota_bucket_end_user_allowance_uq
    ON gateway.quota_bucket (end_user_id, period_start, period_end, currency)
    WHERE end_user_id IS NOT NULL AND credit_grant_id IS NULL;
CREATE UNIQUE INDEX quota_bucket_credit_grant_uq ON gateway.quota_bucket (credit_grant_id)
    WHERE credit_grant_id IS NOT NULL;
CREATE INDEX quota_bucket_end_user_idx ON gateway.quota_bucket (end_user_id)
    WHERE end_user_id IS NOT NULL;

-- One request's share of an end user's allowance (rank 0) and credits (rank
-- 1..n, in draw order). NULL: a budget, charged the whole amount.
ALTER TABLE gateway.accounting_allocation
    ADD COLUMN draw_rank SMALLINT CHECK (draw_rank IS NULL OR draw_rank >= 0);

ALTER TABLE gateway.accounting_reservation ADD COLUMN end_user_id UUID;
ALTER TABLE gateway.inference_trace ADD COLUMN end_user_id UUID;
CREATE INDEX inference_trace_end_user_idx ON gateway.inference_trace (end_user_id, recorded_at)
    WHERE end_user_id IS NOT NULL;

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report', 'inference_trace',
                        'reconciliation_item', 'catalog_release', 'catalog_activation',
                        'provider_credential', 'price_override', 'rate_limit_policy',
                        'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                        'price_mapping', 'retention_policy', 'data_export',
                        'cloud_principal_binding', 'invitation', 'budget_policy',
                        'identity_provider', 'config_pin', 'usage_export',
                        'catalog_auto_offer', 'agent', 'agent_token', 'price_source_fetch',
                        'trace_policy', 'organization_domain', 'catalog_price_policy',
                        'catalog_price_hold', 'end_user_setting', 'end_user_plan', 'end_user',
                        'end_user_credit_grant')
    ) NOT VALID;

ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release', 'catalog_activation',
                          'provider_credential', 'price_override', 'rate_limit_policy',
                          'reconciliation_item', 'webhook_endpoint', 'webhook_delivery',
                          'catalog_sync_job', 'price_mapping', 'retention_policy',
                          'budget_policy', 'invitation', 'identity_provider', 'config_pin',
                          'usage_export', 'catalog_auto_offer', 'agent', 'agent_token',
                          'price_source_fetch', 'trace_policy', 'catalog_price_policy',
                          'catalog_price_hold', 'end_user_setting', 'end_user_plan', 'end_user',
                          'end_user_credit_grant')
    );
