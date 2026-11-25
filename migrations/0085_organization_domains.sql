-- Email domains an organization has proven it controls, so the sign-in page
-- can route `ada@example.com` to the organization that owns `example.com`
-- (and on to its own identity provider). An owner adds a domain and
-- publishes a TXT record `gateway-domain-verification=<token>` at
-- `_gateway-verification.<domain>`; the first organization to verify a
-- domain owns it. Removing a domain only marks it deleted.
CREATE TABLE gateway.organization_domain (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- Lowercase, without a trailing dot.
    domain TEXT NOT NULL CHECK (length(domain) BETWEEN 3 AND 253 AND domain = lower(domain)),
    -- Published in DNS, so not a secret.
    verification_token TEXT NOT NULL CHECK (length(verification_token) BETWEEN 16 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    verified_at TIMESTAMPTZ,
    last_checked_at TIMESTAMPTZ,
    deleted_at TIMESTAMPTZ,
    deleted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

-- An organization lists a domain once.
CREATE UNIQUE INDEX organization_domain_organization_uq
    ON gateway.organization_domain (organization_id, domain) WHERE deleted_at IS NULL;
-- A verified domain has one owner.
CREATE UNIQUE INDEX organization_domain_verified_uq
    ON gateway.organization_domain (domain) WHERE verified_at IS NOT NULL AND deleted_at IS NULL;

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
                        'price_source_fetch', 'trace_policy', 'organization_domain')
    ) NOT VALID;
