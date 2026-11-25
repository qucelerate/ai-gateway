-- Operators can redeliver a parked webhook delivery. Each redelivery is a
-- versioned, audited admin write on the delivery itself.

-- A redelivery starts a fresh run of attempts without resetting `attempts`,
-- which keeps counting so it stays a sound fence against stale workers. The
-- run's backoff and parking count from this offset.
ALTER TABLE gateway.webhook_delivery
    ADD COLUMN attempts_before_run INTEGER NOT NULL DEFAULT 0,
    ADD CONSTRAINT webhook_delivery_run_ck
        CHECK (attempts_before_run >= 0 AND attempts_before_run <= attempts);
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery')
    ) NOT VALID;
