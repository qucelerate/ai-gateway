-- Budgets for one API key. Apps that share a service account but need limits
-- of their own each get a key and a budget on it. A key's budget works as the
-- others do: it is one more quota bucket a request of that key must fit,
-- alongside its service account's, its team's and the organization's. Keys
-- are revoked, never deleted, so a revoked key's budget and what it spent stay
-- readable.

ALTER TABLE gateway.budget_policy ADD COLUMN api_key_id UUID;
ALTER TABLE gateway.quota_bucket ADD COLUMN api_key_id UUID;

-- The scope_kind column checks were declared inline, so their names are the
-- ones PostgreSQL generated.
ALTER TABLE gateway.budget_policy
    DROP CONSTRAINT budget_policy_scope_kind_check,
    ADD CONSTRAINT budget_policy_scope_kind_check
        CHECK (scope_kind IN ('organization', 'team', 'service-account', 'api-key')),
    DROP CONSTRAINT budget_policy_scope_ck,
    ADD CONSTRAINT budget_policy_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'service-account' AND team_id IS NULL
            AND service_account_id IS NOT NULL AND api_key_id IS NULL)
        OR (scope_kind = 'api-key' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NOT NULL)
    ),
    ADD CONSTRAINT budget_policy_api_key_fk FOREIGN KEY (organization_id, api_key_id)
        REFERENCES gateway.api_key (organization_id, id) ON DELETE CASCADE;

DROP INDEX gateway.budget_policy_scope_uq;
CREATE UNIQUE INDEX budget_policy_scope_uq ON gateway.budget_policy (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(api_key_id, '00000000-0000-0000-0000-000000000000'::uuid)
);

ALTER TABLE gateway.quota_bucket
    DROP CONSTRAINT quota_bucket_scope_kind_check,
    ADD CONSTRAINT quota_bucket_scope_kind_check
        CHECK (scope_kind IN ('organization', 'team', 'service-account', 'api-key')),
    DROP CONSTRAINT quota_bucket_scope_ck,
    ADD CONSTRAINT quota_bucket_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'service-account' AND service_account_id IS NOT NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'api-key' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NOT NULL)
    ),
    ADD CONSTRAINT quota_bucket_api_key_fk FOREIGN KEY (organization_id, api_key_id)
        REFERENCES gateway.api_key (organization_id, id) ON DELETE CASCADE;

-- One bucket per scope, window and currency, now told apart by key too.
DROP INDEX gateway.quota_bucket_scope_uq;
CREATE UNIQUE INDEX quota_bucket_scope_uq ON gateway.quota_bucket (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(api_key_id, '00000000-0000-0000-0000-000000000000'::uuid),
    period_start, period_end, currency
);

-- A reservation looks up the key's buckets by key.
CREATE INDEX quota_bucket_api_key_idx ON gateway.quota_bucket (organization_id, api_key_id)
    WHERE api_key_id IS NOT NULL;
