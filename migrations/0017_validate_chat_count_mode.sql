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
    IF expected_mode IS NULL
       OR expected_mode IS DISTINCT FROM NEW.catalog_chat_input_count_mode
    THEN
        RAISE EXCEPTION 'catalog chat input count mode does not match capability';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER accounting_reservation_catalog_count_mode_validate
BEFORE INSERT ON gateway.accounting_reservation
FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_chat_count_mode();

CREATE TRIGGER inference_trace_catalog_count_mode_validate
BEFORE INSERT ON gateway.inference_trace
FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_chat_count_mode();
