-- Organization price overrides. An override replaces the catalog price of one
-- deployment (by name) and operation for a validity window, independent of
-- releases, so it survives catalog upgrades. Rows are immutable once written:
-- the only change allowed is a one-time revocation. A reservation snapshots
-- the override it was priced with, so revoking or expiring an override never
-- changes historical accounting.
CREATE TABLE gateway.catalog_price_override (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 255),
    operation TEXT NOT NULL CHECK (operation IN ('chat', 'chat_stream', 'embeddings')),
    input_minor_per_token BIGINT NOT NULL CHECK (input_minor_per_token >= 0),
    output_minor_per_token BIGINT NOT NULL CHECK (output_minor_per_token >= 0),
    cached_input_minor_per_token BIGINT NOT NULL CHECK (cached_input_minor_per_token >= 0),
    reasoning_minor_per_token BIGINT NOT NULL CHECK (reasoning_minor_per_token >= 0),
    other_minor_per_token BIGINT NOT NULL CHECK (other_minor_per_token >= 0),
    currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    confidence TEXT NOT NULL CHECK (confidence IN ('known', 'conservative')),
    -- The price version a reservation records.
    version TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(btrim(reason)) BETWEEN 1 AND 1024),
    effective_from TIMESTAMPTZ NOT NULL,
    effective_until TIMESTAMPTZ,
    created_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    revoked_at TIMESTAMPTZ,
    revoked_by UUID,
    revoke_reason TEXT CHECK (length(btrim(revoke_reason)) BETWEEN 1 AND 1024),
    CONSTRAINT catalog_price_override_version_ck CHECK (version = 'override:' || id::text),
    CONSTRAINT catalog_price_override_window_ck
        CHECK (effective_until IS NULL OR effective_until > effective_from),
    CONSTRAINT catalog_price_override_revocation_ck
        CHECK ((revoked_at IS NULL) = (revoke_reason IS NULL)
               AND (revoked_at IS NOT NULL OR revoked_by IS NULL)),
    CONSTRAINT catalog_price_override_org_id_uq UNIQUE (organization_id, id),
    CONSTRAINT catalog_price_override_org_fk FOREIGN KEY (organization_id)
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    CONSTRAINT catalog_price_override_created_by_fk FOREIGN KEY (created_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    CONSTRAINT catalog_price_override_revoked_by_fk FOREIGN KEY (revoked_by)
        REFERENCES gateway.user_account (id) ON DELETE SET NULL
);

CREATE INDEX catalog_price_override_lookup_idx
    ON gateway.catalog_price_override (organization_id, deployment, operation, effective_from DESC);
CREATE INDEX catalog_price_override_created_idx
    ON gateway.catalog_price_override (organization_id, created_at DESC, id DESC);

-- Prices, window and reason never change. Revocation happens at most once;
-- actor references may only be cleared when the user is deleted.
CREATE FUNCTION gateway.guard_catalog_price_override_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (OLD.id, OLD.organization_id, OLD.deployment, OLD.operation,
        OLD.input_minor_per_token, OLD.output_minor_per_token,
        OLD.cached_input_minor_per_token, OLD.reasoning_minor_per_token,
        OLD.other_minor_per_token, OLD.currency, OLD.confidence, OLD.version,
        OLD.reason, OLD.effective_from, OLD.effective_until, OLD.created_at)
       IS DISTINCT FROM
       (NEW.id, NEW.organization_id, NEW.deployment, NEW.operation,
        NEW.input_minor_per_token, NEW.output_minor_per_token,
        NEW.cached_input_minor_per_token, NEW.reasoning_minor_per_token,
        NEW.other_minor_per_token, NEW.currency, NEW.confidence, NEW.version,
        NEW.reason, NEW.effective_from, NEW.effective_until, NEW.created_at)
       OR (NEW.created_by IS DISTINCT FROM OLD.created_by AND NEW.created_by IS NOT NULL)
    THEN
        RAISE EXCEPTION 'catalog price override is immutable';
    END IF;
    IF OLD.revoked_at IS NOT NULL
       AND (NEW.revoked_at IS DISTINCT FROM OLD.revoked_at
            OR NEW.revoke_reason IS DISTINCT FROM OLD.revoke_reason
            OR (NEW.revoked_by IS DISTINCT FROM OLD.revoked_by AND NEW.revoked_by IS NOT NULL))
    THEN
        RAISE EXCEPTION 'catalog price override is already revoked';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER catalog_price_override_immutable
BEFORE UPDATE ON gateway.catalog_price_override
FOR EACH ROW EXECUTE FUNCTION gateway.guard_catalog_price_override_update();

-- Reservations and traces record the override they were priced with.
ALTER TABLE gateway.accounting_reservation
    ADD COLUMN catalog_price_override_id UUID,
    ADD CONSTRAINT accounting_reservation_price_override_ck
        CHECK (catalog_price_override_id IS NULL OR catalog_release_id IS NOT NULL),
    ADD CONSTRAINT accounting_reservation_price_override_fk
        FOREIGN KEY (organization_id, catalog_price_override_id)
        REFERENCES gateway.catalog_price_override (organization_id, id);
ALTER TABLE gateway.inference_trace
    ADD COLUMN catalog_price_override_id UUID,
    ADD CONSTRAINT inference_trace_price_override_ck
        CHECK (catalog_price_override_id IS NULL OR catalog_release_id IS NOT NULL),
    ADD CONSTRAINT inference_trace_price_override_fk
        FOREIGN KEY (organization_id, catalog_price_override_id)
        REFERENCES gateway.catalog_price_override (organization_id, id);
CREATE INDEX accounting_reservation_price_override_idx
    ON gateway.accounting_reservation (organization_id, catalog_price_override_id)
    WHERE catalog_price_override_id IS NOT NULL;
CREATE INDEX inference_trace_price_override_idx
    ON gateway.inference_trace (organization_id, catalog_price_override_id)
    WHERE catalog_price_override_id IS NOT NULL;

CREATE OR REPLACE FUNCTION gateway.prevent_catalog_provenance_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.catalog_release_id IS DISTINCT FROM NEW.catalog_release_id
       OR OLD.catalog_route_id IS DISTINCT FROM NEW.catalog_route_id
       OR OLD.catalog_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR OLD.catalog_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR OLD.catalog_digest IS DISTINCT FROM NEW.catalog_digest
       OR OLD.catalog_operation IS DISTINCT FROM NEW.catalog_operation
       OR OLD.catalog_family IS DISTINCT FROM NEW.catalog_family
       OR OLD.catalog_price_source IS DISTINCT FROM NEW.catalog_price_source
       OR OLD.catalog_chat_input_count_mode IS DISTINCT FROM NEW.catalog_chat_input_count_mode
       OR OLD.catalog_price_override_id IS DISTINCT FROM NEW.catalog_price_override_id
    THEN
        RAISE EXCEPTION 'catalog provenance immutable';
    END IF;
    RETURN NEW;
END;
$$;

-- The price an override snapshot must match: the override row, for the same
-- deployment name and operation as the route, obeying the family's
-- unreported-usage rule (as `ProviderFamily::prices_unreported_usage` and the
-- catalog price guard do). Validity windows and revocation are checked when
-- the route is resolved, not here: this also runs on every later update of
-- the row, after the override may have expired.
CREATE FUNCTION gateway.catalog_price_override_for(
    organization UUID, override_id UUID, deployment_name TEXT, operation_name TEXT,
    family_name TEXT
) RETURNS gateway.catalog_price_override
LANGUAGE plpgsql STABLE AS $$
DECLARE
    found gateway.catalog_price_override;
BEGIN
    SELECT * INTO found FROM gateway.catalog_price_override o
     WHERE o.organization_id = organization AND o.id = override_id
       AND o.deployment = deployment_name AND o.operation = operation_name;
    IF found.id IS NULL
       OR (family_name = 'anthropic'
           AND (found.cached_input_minor_per_token <> 0
                OR found.reasoning_minor_per_token <> 0
                OR found.other_minor_per_token <> 0))
       OR (family_name = 'bedrock' AND found.reasoning_minor_per_token <> 0)
    THEN
        RETURN NULL;
    END IF;
    RETURN found;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.validate_accounting_reservation_catalog_provenance()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    expected_digest BYTEA;
    expected_deployment_id UUID;
    expected_price_id UUID;
    expected_deployment TEXT;
    expected_model TEXT;
    expected_operation TEXT;
    expected_family TEXT;
    expected_source TEXT;
    expected_source_version TEXT;
    expected_confidence TEXT;
    expected_currency CHAR(3);
    expected_input BIGINT;
    expected_output BIGINT;
    expected_cached_input BIGINT;
    expected_reasoning BIGINT;
    expected_other BIGINT;
    override_row gateway.catalog_price_override;
BEGIN
    IF NEW.catalog_release_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT cr.digest, r.deployment_id, r.price_id, d.name, d.model,
           r.operation, p.family, pr.source, pr.source_version,
           pr.confidence, pr.currency, pr.input_minor_per_token,
           pr.output_minor_per_token, pr.cached_input_minor_per_token,
           pr.reasoning_minor_per_token, pr.other_minor_per_token
      INTO expected_digest, expected_deployment_id, expected_price_id,
           expected_deployment, expected_model, expected_operation,
           expected_family, expected_source, expected_source_version,
           expected_confidence, expected_currency, expected_input,
           expected_output, expected_cached_input, expected_reasoning,
           expected_other
      FROM gateway.catalog_route r
      JOIN gateway.catalog_release cr
        ON cr.organization_id = r.organization_id
       AND cr.id = r.release_id
      JOIN gateway.catalog_deployment d
        ON d.organization_id = r.organization_id
       AND d.release_id = r.release_id
       AND d.id = r.deployment_id
      JOIN gateway.catalog_provider_account p
        ON p.organization_id = d.organization_id
       AND p.release_id = d.release_id
       AND p.id = d.provider_account_id
       AND p.adapter_instance_key = d.adapter_instance_key
      JOIN gateway.catalog_price pr
        ON pr.organization_id = r.organization_id
       AND pr.release_id = r.release_id
       AND pr.id = r.price_id
      WHERE r.organization_id = NEW.organization_id
        AND r.release_id = NEW.catalog_release_id
        AND r.id = NEW.catalog_route_id;

    IF NEW.catalog_price_override_id IS NOT NULL THEN
        override_row := gateway.catalog_price_override_for(
            NEW.organization_id, NEW.catalog_price_override_id, expected_deployment,
            expected_operation, expected_family);
        IF override_row.id IS NULL THEN
            RAISE EXCEPTION 'catalog reservation provenance does not match catalog rows';
        END IF;
        expected_source := 'override';
        expected_source_version := override_row.version;
        expected_confidence := override_row.confidence;
        expected_currency := override_row.currency;
        expected_input := override_row.input_minor_per_token;
        expected_output := override_row.output_minor_per_token;
        expected_cached_input := override_row.cached_input_minor_per_token;
        expected_reasoning := override_row.reasoning_minor_per_token;
        expected_other := override_row.other_minor_per_token;
    END IF;

    IF expected_digest IS NULL
       OR expected_digest IS DISTINCT FROM NEW.catalog_digest
       OR expected_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR expected_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR expected_deployment IS DISTINCT FROM NEW.deployment
       OR expected_model IS DISTINCT FROM NEW.model
       OR expected_operation IS DISTINCT FROM NEW.catalog_operation
       OR expected_family IS DISTINCT FROM NEW.catalog_family
       OR expected_source IS DISTINCT FROM NEW.catalog_price_source
       OR expected_source_version IS DISTINCT FROM NEW.price_version
       OR expected_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR expected_currency IS DISTINCT FROM NEW.currency
       OR expected_input IS DISTINCT FROM NEW.input_price_minor
       OR expected_output IS DISTINCT FROM NEW.output_price_minor
       OR expected_cached_input IS DISTINCT FROM NEW.cached_input_price_minor
       OR expected_reasoning IS DISTINCT FROM NEW.reasoning_price_minor
       OR expected_other IS DISTINCT FROM NEW.other_price_minor
    THEN
        RAISE EXCEPTION 'catalog reservation provenance does not match catalog rows';
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.validate_inference_trace_catalog_provenance()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    expected_digest BYTEA;
    expected_deployment_id UUID;
    expected_price_id UUID;
    expected_deployment TEXT;
    expected_model TEXT;
    expected_operation TEXT;
    expected_family TEXT;
    expected_source TEXT;
    expected_source_version TEXT;
    expected_confidence TEXT;
    expected_currency CHAR(3);
    override_row gateway.catalog_price_override;
BEGIN
    IF NEW.catalog_release_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT cr.digest, r.deployment_id, r.price_id, d.name, d.model, r.operation,
           p.family, pr.source, pr.source_version, pr.confidence, pr.currency
      INTO expected_digest, expected_deployment_id, expected_price_id,
           expected_deployment, expected_model, expected_operation,
           expected_family, expected_source, expected_source_version,
           expected_confidence, expected_currency
      FROM gateway.catalog_route r
      JOIN gateway.catalog_release cr
        ON cr.organization_id = r.organization_id
       AND cr.id = r.release_id
      JOIN gateway.catalog_deployment d
        ON d.organization_id = r.organization_id
       AND d.release_id = r.release_id
       AND d.id = r.deployment_id
      JOIN gateway.catalog_provider_account p
        ON p.organization_id = d.organization_id
       AND p.release_id = d.release_id
       AND p.id = d.provider_account_id
       AND p.adapter_instance_key = d.adapter_instance_key
      JOIN gateway.catalog_price pr
        ON pr.organization_id = r.organization_id
       AND pr.release_id = r.release_id
       AND pr.id = r.price_id
      WHERE r.organization_id = NEW.organization_id
        AND r.release_id = NEW.catalog_release_id
        AND r.id = NEW.catalog_route_id;

    IF NEW.catalog_price_override_id IS NOT NULL THEN
        override_row := gateway.catalog_price_override_for(
            NEW.organization_id, NEW.catalog_price_override_id, expected_deployment,
            expected_operation, expected_family);
        IF override_row.id IS NULL THEN
            RAISE EXCEPTION 'catalog trace provenance does not match catalog rows';
        END IF;
        expected_source := 'override';
        expected_source_version := override_row.version;
        expected_confidence := override_row.confidence;
        expected_currency := override_row.currency;
    END IF;

    IF expected_digest IS NULL
       OR expected_digest IS DISTINCT FROM NEW.catalog_digest
       OR expected_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR expected_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR expected_model IS DISTINCT FROM NEW.model
       OR expected_operation IS DISTINCT FROM NEW.catalog_operation
       OR expected_family IS DISTINCT FROM NEW.catalog_family
       OR expected_source IS DISTINCT FROM NEW.catalog_price_source
       OR expected_source_version IS DISTINCT FROM NEW.price_version
       OR expected_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR expected_currency IS DISTINCT FROM NEW.currency
    THEN
        RAISE EXCEPTION 'catalog trace provenance does not match catalog rows';
    END IF;
    RETURN NEW;
END;
$$;

-- Override writes are versioned and audited admin resources.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override')
    ) NOT VALID;
