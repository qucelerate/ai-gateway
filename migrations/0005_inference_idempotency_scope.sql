-- Gate 1D: public idempotency is scoped to the authenticated service account.
-- Keep organization-scoped internal reservations unique when service_account_id
-- is NULL while allowing two service accounts in one organization to reuse a
-- key independently.
DROP INDEX gateway.accounting_reservation_idempotency_uq;

CREATE UNIQUE INDEX accounting_reservation_idempotency_uq
ON gateway.accounting_reservation (
    organization_id,
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    idempotency_key
);

-- Embeddings reserve and settle input usage only. A NULL cap is distinct from
-- zero: it records that the operation has no output-token dimension.
ALTER TABLE gateway.accounting_reservation
    ALTER COLUMN output_token_cap DROP NOT NULL;
ALTER TABLE gateway.accounting_reservation
    DROP CONSTRAINT IF EXISTS accounting_reservation_output_token_cap_check;
ALTER TABLE gateway.accounting_reservation
    ADD CONSTRAINT accounting_reservation_output_token_cap_check
    CHECK (output_token_cap IS NULL OR output_token_cap > 0);

ALTER TABLE gateway.inference_trace
    ALTER COLUMN output_cap DROP NOT NULL;
ALTER TABLE gateway.inference_trace
    DROP CONSTRAINT IF EXISTS inference_trace_output_cap_check;
ALTER TABLE gateway.inference_trace
    ADD CONSTRAINT inference_trace_output_cap_check
    CHECK (output_cap IS NULL OR output_cap > 0);

-- Recreate immutable guards after nullable columns were introduced. Plain
-- <> evaluates to NULL for NULL/value changes and would silently allow them.
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
    THEN
        RAISE EXCEPTION 'accounting reservation fields immutable';
    END IF;
    IF OLD.settlement_fingerprint IS NOT NULL
       AND OLD.settlement_fingerprint IS DISTINCT FROM NEW.settlement_fingerprint
    THEN
        RAISE EXCEPTION 'settlement fingerprint immutable';
    END IF;
    IF NOT (
        (OLD.state = 'reserved' AND NEW.state IN ('reserved','claimed','expired'))
        OR (OLD.state = 'claimed' AND NEW.state IN ('claimed','unknown','settled'))
        OR (OLD.state = 'unknown' AND NEW.state IN ('unknown','settled'))
        OR (OLD.state IN ('settled','expired','released') AND NEW.state = OLD.state)
    ) THEN
        RAISE EXCEPTION 'invalid accounting reservation transition';
    END IF;
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
    THEN
        RAISE EXCEPTION 'inference trace metadata is immutable';
    END IF;
    RETURN NEW;
END;
$$;
