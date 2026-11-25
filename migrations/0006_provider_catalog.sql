-- Phase 2A: immutable, organization-scoped provider catalog releases.
CREATE TABLE gateway.catalog_release (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    version TEXT NOT NULL CHECK (length(version) BETWEEN 1 AND 128),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT catalog_release_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_release_digest_uq UNIQUE (organization_id, digest),
    CONSTRAINT catalog_release_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization(id) ON DELETE CASCADE
);

CREATE TABLE gateway.catalog_provider_account (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    release_id UUID NOT NULL,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic')),
    CONSTRAINT catalog_provider_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_provider_release_id_uq UNIQUE (organization_id, release_id, id),
    CONSTRAINT catalog_provider_key_uq UNIQUE (organization_id, release_id, adapter_instance_key),
    CONSTRAINT catalog_provider_identity_uq UNIQUE (organization_id, release_id, id, adapter_instance_key),
    CONSTRAINT catalog_provider_release_fk FOREIGN KEY (organization_id, release_id)
        REFERENCES gateway.catalog_release(organization_id, id) ON DELETE CASCADE
);

CREATE TABLE gateway.catalog_deployment (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    release_id UUID NOT NULL,
    provider_account_id UUID NOT NULL,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 255),
    location TEXT CHECK (location IS NULL OR length(location) BETWEEN 1 AND 255),
    CONSTRAINT catalog_deployment_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_deployment_release_id_uq UNIQUE (organization_id, release_id, id),
    CONSTRAINT catalog_deployment_name_uq UNIQUE (organization_id, release_id, name),
    CONSTRAINT catalog_deployment_release_fk FOREIGN KEY (organization_id, release_id)
        REFERENCES gateway.catalog_release(organization_id, id) ON DELETE CASCADE,
    CONSTRAINT catalog_deployment_provider_identity_fk FOREIGN KEY
        (organization_id, release_id, provider_account_id, adapter_instance_key)
        REFERENCES gateway.catalog_provider_account(organization_id, release_id, id, adapter_instance_key)
        ON DELETE CASCADE
);

CREATE INDEX catalog_deployment_provider_identity_idx
    ON gateway.catalog_deployment
    (organization_id, release_id, provider_account_id, adapter_instance_key);

CREATE TABLE gateway.catalog_capability (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    release_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('chat', 'chat_stream', 'embeddings')),
    CONSTRAINT catalog_capability_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_capability_release_id_uq UNIQUE (organization_id, release_id, id),
    CONSTRAINT catalog_capability_identity_uq UNIQUE
        (organization_id, release_id, deployment_id, operation),
    CONSTRAINT catalog_capability_deployment_fk FOREIGN KEY
        (organization_id, release_id, deployment_id)
        REFERENCES gateway.catalog_deployment(organization_id, release_id, id)
        ON DELETE CASCADE
);

CREATE TABLE gateway.catalog_price (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    release_id UUID NOT NULL,
    deployment_id UUID NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('chat', 'chat_stream', 'embeddings')),
    input_minor_per_token BIGINT NOT NULL CHECK (input_minor_per_token >= 0),
    output_minor_per_token BIGINT NOT NULL CHECK (output_minor_per_token >= 0),
    cached_input_minor_per_token BIGINT NOT NULL CHECK (cached_input_minor_per_token >= 0),
    reasoning_minor_per_token BIGINT NOT NULL CHECK (reasoning_minor_per_token >= 0),
    other_minor_per_token BIGINT NOT NULL CHECK (other_minor_per_token >= 0),
    currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    source TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 128),
    source_version TEXT NOT NULL CHECK (length(source_version) BETWEEN 1 AND 128),
    confidence TEXT NOT NULL CHECK (confidence IN ('known', 'conservative')),
    CONSTRAINT catalog_price_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_price_release_id_uq UNIQUE (organization_id, release_id, id),
    CONSTRAINT catalog_price_release_deployment_id_uq UNIQUE
        (organization_id, release_id, deployment_id, id),
    CONSTRAINT catalog_price_identity_uq UNIQUE (organization_id, release_id, deployment_id, operation),
    CONSTRAINT catalog_price_deployment_fk FOREIGN KEY
        (organization_id, release_id, deployment_id)
        REFERENCES gateway.catalog_deployment(organization_id, release_id, id)
        ON DELETE CASCADE
);

CREATE TABLE gateway.catalog_route (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    release_id UUID NOT NULL,
    alias TEXT NOT NULL CHECK (length(alias) BETWEEN 1 AND 128),
    operation TEXT NOT NULL CHECK (operation IN ('chat', 'chat_stream', 'embeddings')),
    deployment_id UUID NOT NULL,
    capability_id UUID NOT NULL,
    price_id UUID NOT NULL,
    CONSTRAINT catalog_route_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_route_alias_uq UNIQUE (organization_id, release_id, alias, operation),
    CONSTRAINT catalog_route_release_fk FOREIGN KEY (organization_id, release_id)
        REFERENCES gateway.catalog_release(organization_id, id) ON DELETE CASCADE,
    CONSTRAINT catalog_route_deployment_fk FOREIGN KEY
        (organization_id, release_id, deployment_id)
        REFERENCES gateway.catalog_deployment(organization_id, release_id, id)
        ON DELETE CASCADE,
    CONSTRAINT catalog_route_capability_fk FOREIGN KEY
        (organization_id, release_id, deployment_id, operation)
        REFERENCES gateway.catalog_capability(organization_id, release_id, deployment_id, operation)
        ON DELETE CASCADE,
    CONSTRAINT catalog_route_price_fk FOREIGN KEY
        (organization_id, release_id, deployment_id, operation)
        REFERENCES gateway.catalog_price(organization_id, release_id, deployment_id, operation)
        ON DELETE CASCADE,
    CONSTRAINT catalog_route_capability_id_fk FOREIGN KEY (organization_id, capability_id)
        REFERENCES gateway.catalog_capability(organization_id, id) ON DELETE CASCADE,
    CONSTRAINT catalog_route_price_id_fk FOREIGN KEY (organization_id, price_id)
        REFERENCES gateway.catalog_price(organization_id, id) ON DELETE CASCADE
);

CREATE TABLE gateway.catalog_active_release (
    organization_id UUID PRIMARY KEY,
    release_id UUID NOT NULL,
    activated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT catalog_active_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization(id) ON DELETE CASCADE,
    CONSTRAINT catalog_active_release_fk FOREIGN KEY (organization_id, release_id)
        REFERENCES gateway.catalog_release(organization_id, id) ON DELETE RESTRICT
);

CREATE OR REPLACE FUNCTION gateway.prevent_catalog_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'active catalog release rows are immutable';
END;
$$;

CREATE OR REPLACE FUNCTION gateway.prevent_anthropic_embeddings()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE family_name TEXT;
BEGIN
    SELECT p.family INTO family_name
      FROM gateway.catalog_deployment d
      JOIN gateway.catalog_provider_account p
        ON p.organization_id=d.organization_id AND p.release_id=d.release_id
       AND p.id=d.provider_account_id
       AND p.adapter_instance_key=d.adapter_instance_key
     WHERE d.organization_id=NEW.organization_id
       AND d.release_id=NEW.release_id AND d.id=NEW.deployment_id;
    IF family_name = 'anthropic' AND NEW.operation = 'embeddings' THEN
        RAISE EXCEPTION 'anthropic embeddings capability is unsupported';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.prevent_catalog_active_insert()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.catalog_active_release
               WHERE organization_id=NEW.organization_id AND release_id=NEW.release_id) THEN
        RAISE EXCEPTION 'active catalog release cannot be extended';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER catalog_release_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_release
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_provider_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_provider_account
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_provider_active_insert BEFORE INSERT ON gateway.catalog_provider_account
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_active_insert();
CREATE TRIGGER catalog_deployment_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_deployment
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_deployment_active_insert BEFORE INSERT ON gateway.catalog_deployment
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_active_insert();
CREATE TRIGGER catalog_capability_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_capability
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_capability_active_insert BEFORE INSERT ON gateway.catalog_capability
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_active_insert();
CREATE TRIGGER catalog_capability_family_check BEFORE INSERT ON gateway.catalog_capability
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_anthropic_embeddings();
CREATE TRIGGER catalog_price_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_price
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_price_active_insert BEFORE INSERT ON gateway.catalog_price
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_active_insert();
CREATE TRIGGER catalog_price_family_check BEFORE INSERT ON gateway.catalog_price
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_anthropic_embeddings();
CREATE TRIGGER catalog_route_immutable BEFORE UPDATE OR DELETE ON gateway.catalog_route
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_mutation();
CREATE TRIGGER catalog_route_active_insert BEFORE INSERT ON gateway.catalog_route
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_active_insert();

ALTER TABLE gateway.accounting_reservation
    ADD COLUMN catalog_release_id UUID,
    ADD COLUMN catalog_deployment_id UUID,
    ADD COLUMN catalog_price_id UUID,
    ADD COLUMN catalog_digest BYTEA,
    ADD CONSTRAINT accounting_reservation_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32)
    ),
    ADD CONSTRAINT accounting_reservation_catalog_release_fk FOREIGN KEY
        (organization_id, catalog_release_id) REFERENCES gateway.catalog_release(organization_id, id),
    ADD CONSTRAINT accounting_reservation_catalog_deployment_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_deployment_id)
        REFERENCES gateway.catalog_deployment(organization_id, release_id, id),
    ADD CONSTRAINT accounting_reservation_catalog_price_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_deployment_id, catalog_price_id)
        REFERENCES gateway.catalog_price(organization_id, release_id, deployment_id, id);

ALTER TABLE gateway.inference_trace
    ADD COLUMN catalog_release_id UUID,
    ADD COLUMN catalog_deployment_id UUID,
    ADD COLUMN catalog_price_id UUID,
    ADD COLUMN catalog_digest BYTEA,
    ADD CONSTRAINT inference_trace_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32)
    ),
    ADD CONSTRAINT inference_trace_catalog_release_fk FOREIGN KEY
        (organization_id, catalog_release_id) REFERENCES gateway.catalog_release(organization_id, id),
    ADD CONSTRAINT inference_trace_catalog_deployment_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_deployment_id)
        REFERENCES gateway.catalog_deployment(organization_id, release_id, id),
    ADD CONSTRAINT inference_trace_catalog_price_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_deployment_id, catalog_price_id)
        REFERENCES gateway.catalog_price(organization_id, release_id, deployment_id, id);

-- Extend the Phase 1D immutable guards to cover catalog provenance. Existing
-- legacy rows remain all-null and continue to pass these checks.
CREATE OR REPLACE FUNCTION gateway.prevent_price_snapshot_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.organization_id IS DISTINCT FROM NEW.organization_id
       OR OLD.team_id IS DISTINCT FROM NEW.team_id
       OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id
       OR OLD.idempotency_key IS DISTINCT FROM NEW.idempotency_key
       OR OLD.fingerprint IS DISTINCT FROM NEW.fingerprint
       OR OLD.reserved_minor IS DISTINCT FROM NEW.reserved_minor
       OR OLD.expires_at IS DISTINCT FROM NEW.expires_at
       OR OLD.output_token_cap IS DISTINCT FROM NEW.output_token_cap
       OR OLD.input_price_minor IS DISTINCT FROM NEW.input_price_minor
       OR OLD.output_price_minor IS DISTINCT FROM NEW.output_price_minor
       OR OLD.cached_input_price_minor IS DISTINCT FROM NEW.cached_input_price_minor
       OR OLD.reasoning_price_minor IS DISTINCT FROM NEW.reasoning_price_minor
       OR OLD.other_price_minor IS DISTINCT FROM NEW.other_price_minor
       OR OLD.price_version IS DISTINCT FROM NEW.price_version
       OR OLD.pricing_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR OLD.currency IS DISTINCT FROM NEW.currency
       OR OLD.deployment IS DISTINCT FROM NEW.deployment
       OR OLD.model IS DISTINCT FROM NEW.model
       OR OLD.trace_id IS DISTINCT FROM NEW.trace_id
       OR OLD.catalog_release_id IS DISTINCT FROM NEW.catalog_release_id
       OR OLD.catalog_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR OLD.catalog_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR OLD.catalog_digest IS DISTINCT FROM NEW.catalog_digest
    THEN RAISE EXCEPTION 'accounting reservation fields are immutable'; END IF;
    IF OLD.settlement_fingerprint IS NOT NULL
       AND OLD.settlement_fingerprint IS DISTINCT FROM NEW.settlement_fingerprint
    THEN RAISE EXCEPTION 'settlement fingerprint is immutable'; END IF;
    IF NOT ((OLD.state='reserved' AND NEW.state IN ('reserved','claimed','expired'))
        OR (OLD.state='claimed' AND NEW.state IN ('claimed','unknown','settled'))
        OR (OLD.state='unknown' AND NEW.state IN ('unknown','settled'))
        OR (OLD.state IN ('settled','expired','released') AND NEW.state=OLD.state))
    THEN RAISE EXCEPTION 'invalid accounting reservation transition'; END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.prevent_trace_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.organization_id IS DISTINCT FROM NEW.organization_id
       OR OLD.team_id IS DISTINCT FROM NEW.team_id
       OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id
       OR OLD.reservation_id IS DISTINCT FROM NEW.reservation_id
       OR OLD.correlation_id IS DISTINCT FROM NEW.correlation_id
       OR OLD.operation IS DISTINCT FROM NEW.operation
       OR OLD.model IS DISTINCT FROM NEW.model
       OR OLD.input_bound IS DISTINCT FROM NEW.input_bound
       OR OLD.output_cap IS DISTINCT FROM NEW.output_cap
       OR OLD.price_version IS DISTINCT FROM NEW.price_version
       OR OLD.pricing_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR OLD.currency IS DISTINCT FROM NEW.currency
       OR OLD.metadata IS DISTINCT FROM NEW.metadata
       OR OLD.catalog_release_id IS DISTINCT FROM NEW.catalog_release_id
       OR OLD.catalog_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR OLD.catalog_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR OLD.catalog_digest IS DISTINCT FROM NEW.catalog_digest
    THEN RAISE EXCEPTION 'inference trace metadata is immutable'; END IF;
    RETURN NEW;
END;
$$;
