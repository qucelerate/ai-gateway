-- Catalog prices follow their price sources (D-024). After price sources
-- are fetched, the worker adopts every published price that differs from
-- an organization's active release, in a new release it creates and
-- activates, unless the organization turned this off. A change that looks
-- wrong (more than double, less than half, to zero, or two sources
-- disagreeing) is held for an admin instead.

-- Whether an organization's prices follow their sources. No row: they do.
CREATE TABLE gateway.catalog_price_policy (
    organization_id UUID PRIMARY KEY REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Stable per organization: the admin version resource's id.
    id UUID NOT NULL UNIQUE,
    auto_adopt BOOLEAN NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

-- A published price held for an admin rather than adopted. `changes` lists
-- each differing dimension: `{dimension, catalog, observed, proposed}`
-- (money units per token; `observed` the exact decimal, `proposed` the
-- whole units adopting it would bill). `fingerprint` identifies the
-- proposal (deployment, operation, source and proposed prices), so one an
-- admin dismissed is not held again until the source publishes another.
CREATE TABLE gateway.catalog_price_hold (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The active release the change was found against.
    release_id UUID NOT NULL,
    deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 256),
    operation TEXT NOT NULL CHECK (length(operation) BETWEEN 1 AND 64),
    model TEXT NOT NULL,
    currency TEXT NOT NULL,
    -- The source whose price is held; for disagreeing sources, the first.
    source TEXT NOT NULL,
    snapshot_id UUID NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('large_increase', 'large_decrease', 'to_zero',
                                           'sources_disagree')),
    changes JSONB NOT NULL,
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
    status TEXT NOT NULL DEFAULT 'held'
        CHECK (status IN ('held', 'adopted', 'dismissed', 'superseded')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMPTZ,
    -- Who dismissed it; adopted and superseded holds are resolved by the
    -- system when the catalog or the source moves on.
    resolved_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT catalog_price_hold_resolved_ck
        CHECK ((status = 'held') = (resolved_at IS NULL))
);

-- At most one open hold per priced operation.
CREATE UNIQUE INDEX catalog_price_hold_open_uq
    ON gateway.catalog_price_hold (organization_id, deployment, operation)
    WHERE status = 'held';
CREATE INDEX catalog_price_hold_dismissed_idx
    ON gateway.catalog_price_hold (organization_id, fingerprint)
    WHERE status = 'dismissed';

-- Each price the worker adopted: what it was, what it became, and from
-- which published price. Append-only, with the release it went into.
CREATE TABLE gateway.catalog_price_adoption (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    release_id UUID NOT NULL,
    -- The release it replaced.
    previous_release_id UUID NOT NULL,
    deployment TEXT NOT NULL,
    operation TEXT NOT NULL,
    model TEXT NOT NULL,
    currency TEXT NOT NULL,
    source TEXT NOT NULL,
    snapshot_id UUID NOT NULL,
    item TEXT,
    -- `[{dimension, before, after, observed}]`, money units per token.
    changes JSONB NOT NULL,
    adopted_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX catalog_price_adoption_org_idx
    ON gateway.catalog_price_adoption (organization_id, adopted_at DESC);

CREATE FUNCTION gateway.prevent_price_adoption_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- An organization's removal still cascades.
    IF TG_OP = 'DELETE' AND NOT EXISTS (
        SELECT 1 FROM gateway.organization WHERE id = OLD.organization_id
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'catalog price adoptions are append-only';
END;
$$;

CREATE TRIGGER catalog_price_adoption_immutable
    BEFORE UPDATE OR DELETE ON gateway.catalog_price_adoption
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_price_adoption_mutation();

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
                        'price_source_fetch', 'trace_policy', 'organization_domain',
                        'catalog_price_policy', 'catalog_price_hold')
    ) NOT VALID;

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
                          'price_source_fetch', 'trace_policy',
                          'catalog_price_policy', 'catalog_price_hold')
    );
