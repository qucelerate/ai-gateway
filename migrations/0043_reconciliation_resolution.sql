-- Operators resolve open reconciliation items by charging the amount they
-- determined for the request (zero charges nothing). The resolution records
-- how an item was closed: by a later provider settlement, or by an operator,
-- with the amount and who charged it.
ALTER TABLE gateway.accounting_reconciliation
    ADD COLUMN resolution TEXT,
    ADD COLUMN resolved_amount_minor BIGINT,
    ADD COLUMN resolved_by UUID;

UPDATE gateway.accounting_reconciliation
SET resolution = 'settlement'
WHERE status = 'resolved';

ALTER TABLE gateway.accounting_reconciliation
    ADD CONSTRAINT accounting_reconciliation_resolution_ck CHECK (
        (status = 'open' AND resolution IS NULL
            AND resolved_amount_minor IS NULL AND resolved_by IS NULL)
        OR (status = 'resolved' AND resolution = 'settlement'
            AND resolved_amount_minor IS NULL AND resolved_by IS NULL)
        OR (status = 'resolved' AND resolution = 'operator'
            AND resolved_amount_minor IS NOT NULL AND resolved_amount_minor >= 0)
    ),
    ADD CONSTRAINT accounting_reconciliation_resolved_by_fk FOREIGN KEY (resolved_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL;

-- Resolutions are versioned and audited admin resources.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item')
    );
