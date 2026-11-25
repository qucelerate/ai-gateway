-- Webhook endpoints and their deliveries. The worker fans each outbox event
-- out into one delivery per subscribed endpoint, then POSTs deliveries with a
-- signature, retrying with backoff until they succeed or are parked.

CREATE TABLE gateway.webhook_endpoint (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The API accepts only https URLs to public hosts; the column also admits
    -- http so the delivery path can be tested against a local listener.
    url TEXT NOT NULL CHECK (length(url) <= 2048 AND url ~ '^https?://'),
    description TEXT CHECK (description IS NULL OR length(description) BETWEEN 1 AND 200),
    event_types TEXT[] NOT NULL CHECK (
        cardinality(event_types) > 0
        AND event_types <@ ARRAY['inference.finalized', 'reconciliation.opened',
                                 'reconciliation.resolved']::TEXT[]
    ),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    -- Signing secret, sealed like provider credentials and bound to the
    -- organization and endpoint.
    secret_key_id TEXT NOT NULL,
    secret_ciphertext BYTEA NOT NULL,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT webhook_endpoint_org_id_uq UNIQUE (organization_id, id)
);

CREATE TABLE gateway.webhook_delivery (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    endpoint_id UUID NOT NULL,
    -- The outbox event this delivers. Outbox rows are swept independently,
    -- so the delivery keeps its own copy of the event.
    event_id UUID NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.resolved'
    )),
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    occurred_at TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'parked')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_attempt_at TIMESTAMPTZ,
    last_status_code INTEGER CHECK (last_status_code IS NULL OR last_status_code BETWEEN 100 AND 599),
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200),
    delivered_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT webhook_delivery_endpoint_fk FOREIGN KEY (organization_id, endpoint_id)
        REFERENCES gateway.webhook_endpoint (organization_id, id) ON DELETE CASCADE,
    CONSTRAINT webhook_delivery_event_uq UNIQUE (endpoint_id, event_id),
    CONSTRAINT webhook_delivery_delivered_ck CHECK ((status = 'delivered') = (delivered_at IS NOT NULL))
);

CREATE INDEX webhook_delivery_pending_idx ON gateway.webhook_delivery (next_attempt_at)
    WHERE status = 'pending';
CREATE INDEX webhook_delivery_endpoint_idx
    ON gateway.webhook_delivery (organization_id, endpoint_id, created_at DESC, id DESC);
CREATE INDEX webhook_delivery_finished_idx ON gateway.webhook_delivery (created_at)
    WHERE status <> 'pending';

-- Endpoint writes are versioned and audited admin resources.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint')
    ) NOT VALID;
