-- Pinning an organization to an earlier configuration version through the
-- admin API, and the sweep of versions nobody can serve any more. See
-- docs/architecture-plan.md §2 (Rollback, Retention).

-- The pin is an organization-wide pointer, versioned on the organization
-- (like the active release) and audited as `config_pin`.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy',
                          'invitation', 'identity_provider', 'config_pin')
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
                        'budget_policy', 'identity_provider', 'config_pin')
    ) NOT VALID;

-- What one organization serves at feed position `upto`, through its own
-- entries only; `gateway.config_served_versions` gives every organization's.
CREATE FUNCTION gateway.config_served_version(organization UUID, upto BIGINT)
RETURNS UUID LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN p.kind = 'pin' THEN p.version_id ELSE n.version_id END
    FROM (
        SELECT f.version_id FROM gateway.config_feed f
        WHERE f.organization_id = organization AND f.kind = 'version' AND f.sequence <= upto
        ORDER BY f.sequence DESC LIMIT 1
    ) n
    LEFT JOIN (
        SELECT f.kind, f.version_id FROM gateway.config_feed f
        WHERE f.organization_id = organization AND f.kind IN ('pin', 'unpin')
          AND f.sequence <= upto
        ORDER BY f.sequence DESC LIMIT 1
    ) p ON true
$$;

-- The version an organization is pinned to now, if any.
CREATE FUNCTION gateway.config_pinned_version(organization UUID)
RETURNS UUID LANGUAGE sql STABLE AS $$
    SELECT CASE WHEN f.kind = 'pin' THEN f.version_id END
    FROM gateway.config_feed f
    WHERE f.organization_id = organization AND f.kind IN ('pin', 'unpin')
    ORDER BY f.sequence DESC LIMIT 1
$$;

-- A swept version leaves its feed entries in place, still naming it:
-- removing entries would make every process that applied one reload, and
-- the instances view still reports a lagging instance's version by its ID.
-- So entries no longer reference versions by foreign key; publishing and
-- pinning insert only versions that exist, under the feed lock the sweep
-- also takes, and the sweep never removes a version any process can still
-- be asked to serve (an organization's newest or pinned one).
ALTER TABLE gateway.config_feed DROP CONSTRAINT config_feed_version_fk;

CREATE INDEX gateway_instance_last_seen_idx ON gateway.gateway_instance (last_seen_at);
