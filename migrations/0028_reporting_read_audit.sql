-- Reporting reads (usage aggregates, metadata-only traces, reconciliation
-- items) are audited. Widen the entity vocabulary; the constraint stays
-- NOT VALID like the original so historical rows are not rescanned.
ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item')
    ) NOT VALID;

-- Usage reports read the latest usage row per reservation inside a time
-- window; trace and reconciliation lists page by (time, id) within a tenant.
CREATE INDEX accounting_usage_org_recorded_idx
    ON gateway.accounting_usage (organization_id, recorded_at, reservation_id);
CREATE INDEX accounting_reconciliation_org_created_idx
    ON gateway.accounting_reconciliation (organization_id, created_at, id);
