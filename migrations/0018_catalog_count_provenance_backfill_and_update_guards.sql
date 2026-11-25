-- Accounting rows created before count-mode provenance was introduced are
-- historical rows and cannot be re-proven to have used an exact count.
-- Backfill them with a persistence-only marker without changing any catalog
-- graph row or release digest.
ALTER TABLE gateway.accounting_reconciliation
    DROP CONSTRAINT accounting_reconciliation_reason_check,
    ADD CONSTRAINT accounting_reconciliation_reason_check CHECK (
        reason IN (
            'provider-unknown',
            'expired-after-claim',
            'bound-breach',
            'settlement-failed',
            'input-count-drift'
        )
    );

UPDATE gateway.accounting_reservation ar
   SET catalog_chat_input_count_mode = 'historical_unverified'
  FROM gateway.catalog_route r
  JOIN gateway.catalog_capability c
    ON c.organization_id = r.organization_id
   AND c.release_id = r.release_id
   AND c.id = r.capability_id
   AND c.deployment_id = r.deployment_id
   AND c.operation = r.operation
 WHERE ar.catalog_release_id IS NOT NULL
   AND ar.catalog_chat_input_count_mode IS NULL
   AND r.organization_id = ar.organization_id
   AND r.release_id = ar.catalog_release_id
   AND r.id = ar.catalog_route_id;

UPDATE gateway.inference_trace t
   SET catalog_chat_input_count_mode = 'historical_unverified'
  FROM gateway.catalog_route r
  JOIN gateway.catalog_capability c
    ON c.organization_id = r.organization_id
   AND c.release_id = r.release_id
   AND c.id = r.capability_id
   AND c.deployment_id = r.deployment_id
   AND c.operation = r.operation
 WHERE t.catalog_release_id IS NOT NULL
   AND t.catalog_chat_input_count_mode IS NULL
   AND r.organization_id = t.organization_id
   AND r.release_id = t.catalog_release_id
   AND r.id = t.catalog_route_id;

ALTER TABLE gateway.accounting_reservation
    DROP CONSTRAINT accounting_reservation_catalog_provenance_ck,
    ADD CONSTRAINT accounting_reservation_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_route_id IS NULL
         AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL
         AND catalog_digest IS NULL AND catalog_operation IS NULL
         AND catalog_family IS NULL AND catalog_price_source IS NULL
         AND catalog_chat_input_count_mode IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL
         AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL
         AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32
         AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL
         AND catalog_price_source IS NOT NULL
         AND catalog_chat_input_count_mode IS NOT NULL)
    );

ALTER TABLE gateway.inference_trace
    DROP CONSTRAINT inference_trace_catalog_provenance_ck,
    ADD CONSTRAINT inference_trace_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_route_id IS NULL
         AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL
         AND catalog_digest IS NULL AND catalog_operation IS NULL
         AND catalog_family IS NULL AND catalog_price_source IS NULL
         AND catalog_chat_input_count_mode IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL
         AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL
         AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32
         AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL
         AND catalog_price_source IS NOT NULL
         AND catalog_chat_input_count_mode IS NOT NULL)
    );

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
    THEN
        RAISE EXCEPTION 'catalog provenance immutable';
    END IF;
    RETURN NEW;
END;
$$;

-- Live catalog modes remain limited to the capability's declared values.
-- historical_unverified is only valid on rows produced by the backfill and
-- may survive an ordinary lifecycle update when it is left unchanged. The
-- provenance immutability trigger below prevents upgrading or changing it.
CREATE OR REPLACE FUNCTION gateway.validate_catalog_chat_count_mode()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    expected_mode TEXT;
BEGIN
    IF NEW.catalog_release_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT c.chat_input_count_mode
    INTO expected_mode
    FROM gateway.catalog_route r
    JOIN gateway.catalog_capability c
      ON c.organization_id = r.organization_id
     AND c.release_id = r.release_id
     AND c.id = r.capability_id
     AND c.deployment_id = r.deployment_id
     AND c.operation = r.operation
    WHERE r.organization_id = NEW.organization_id
      AND r.release_id = NEW.catalog_release_id
      AND r.id = NEW.catalog_route_id;

    -- Even historical rows must continue to resolve against a real,
    -- matching catalog graph. Only the provenance marker is exempted from
    -- capability equality, and only when it was already historical.
    IF expected_mode IS NULL THEN
        RAISE EXCEPTION 'catalog chat input count mode does not match capability';
    END IF;
    IF NEW.catalog_chat_input_count_mode = 'historical_unverified' THEN
        IF TG_OP = 'UPDATE'
           AND OLD.catalog_chat_input_count_mode = 'historical_unverified'
        THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'historical catalog chat input count mode is persistence-only';
    END IF;
    IF expected_mode IS DISTINCT FROM NEW.catalog_chat_input_count_mode THEN
        RAISE EXCEPTION 'catalog chat input count mode does not match capability';
    END IF;
    RETURN NEW;
END;
$$;

-- Re-check the immutable route projection on updates too. Normal accounting
-- state updates retain the same projection and therefore continue to pass.
CREATE TRIGGER accounting_reservation_catalog_provenance_revalidate
BEFORE UPDATE ON gateway.accounting_reservation
FOR EACH ROW EXECUTE FUNCTION gateway.validate_accounting_reservation_catalog_provenance();

CREATE TRIGGER inference_trace_catalog_provenance_revalidate
BEFORE UPDATE ON gateway.inference_trace
FOR EACH ROW EXECUTE FUNCTION gateway.validate_inference_trace_catalog_provenance();

CREATE TRIGGER accounting_reservation_catalog_count_mode_revalidate
BEFORE UPDATE ON gateway.accounting_reservation
FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_chat_count_mode();

CREATE TRIGGER inference_trace_catalog_count_mode_revalidate
BEFORE UPDATE ON gateway.inference_trace
FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_chat_count_mode();
