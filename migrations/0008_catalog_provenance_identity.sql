-- Complete the immutable provenance copied into legacy accounting records.
ALTER TABLE gateway.catalog_route
    ADD CONSTRAINT catalog_route_release_id_uq UNIQUE (organization_id, release_id, id);

ALTER TABLE gateway.accounting_reservation
    ADD COLUMN catalog_route_id UUID,
    ADD COLUMN catalog_operation TEXT,
    ADD COLUMN catalog_family TEXT,
    ADD COLUMN catalog_price_source TEXT,
    DROP CONSTRAINT accounting_reservation_catalog_provenance_ck,
    ADD CONSTRAINT accounting_reservation_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_route_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL AND catalog_operation IS NULL AND catalog_family IS NULL AND catalog_price_source IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32 AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL AND catalog_price_source IS NOT NULL)
    ),
    ADD CONSTRAINT accounting_reservation_catalog_route_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_route_id)
        REFERENCES gateway.catalog_route(organization_id, release_id, id);

ALTER TABLE gateway.inference_trace
    ADD COLUMN catalog_route_id UUID,
    ADD COLUMN catalog_operation TEXT,
    ADD COLUMN catalog_family TEXT,
    ADD COLUMN catalog_price_source TEXT,
    DROP CONSTRAINT inference_trace_catalog_provenance_ck,
    ADD CONSTRAINT inference_trace_catalog_provenance_ck CHECK (
        (catalog_release_id IS NULL AND catalog_route_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL AND catalog_operation IS NULL AND catalog_family IS NULL AND catalog_price_source IS NULL)
        OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest) = 32 AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL AND catalog_price_source IS NOT NULL)
    ),
    ADD CONSTRAINT inference_trace_catalog_route_fk FOREIGN KEY
        (organization_id, catalog_release_id, catalog_route_id)
        REFERENCES gateway.catalog_route(organization_id, release_id, id);

CREATE OR REPLACE FUNCTION gateway.validate_catalog_provenance()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE expected_operation TEXT; expected_family TEXT; expected_source TEXT; expected_deployment UUID; expected_price UUID;
BEGIN
    IF NEW.catalog_release_id IS NULL THEN RETURN NEW; END IF;
    SELECT r.operation, p.family, pr.source, r.deployment_id, r.price_id
      INTO expected_operation, expected_family, expected_source, expected_deployment, expected_price
      FROM gateway.catalog_route r
      JOIN gateway.catalog_deployment d ON d.organization_id=r.organization_id AND d.release_id=r.release_id AND d.id=r.deployment_id
      JOIN gateway.catalog_provider_account p ON p.organization_id=d.organization_id AND p.release_id=d.release_id AND p.id=d.provider_account_id AND p.adapter_instance_key=d.adapter_instance_key
      JOIN gateway.catalog_price pr ON pr.organization_id=r.organization_id AND pr.release_id=r.release_id AND pr.id=r.price_id
     WHERE r.organization_id=NEW.organization_id AND r.release_id=NEW.catalog_release_id AND r.id=NEW.catalog_route_id;
    IF expected_operation IS NULL OR expected_operation IS DISTINCT FROM NEW.catalog_operation
       OR expected_family IS DISTINCT FROM NEW.catalog_family OR expected_source IS DISTINCT FROM NEW.catalog_price_source
       OR expected_deployment IS DISTINCT FROM NEW.catalog_deployment_id OR expected_price IS DISTINCT FROM NEW.catalog_price_id
    THEN RAISE EXCEPTION 'catalog provenance does not match immutable route'; END IF;
    RETURN NEW;
END;
$$;
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
    THEN RAISE EXCEPTION 'catalog provenance is immutable'; END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER accounting_reservation_catalog_provenance_validate
    BEFORE INSERT ON gateway.accounting_reservation FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_provenance();
CREATE TRIGGER inference_trace_catalog_provenance_validate
    BEFORE INSERT ON gateway.inference_trace FOR EACH ROW EXECUTE FUNCTION gateway.validate_catalog_provenance();
CREATE TRIGGER accounting_reservation_catalog_provenance_immutable
    BEFORE UPDATE ON gateway.accounting_reservation FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_provenance_mutation();
CREATE TRIGGER inference_trace_catalog_provenance_immutable
    BEFORE UPDATE ON gateway.inference_trace FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_provenance_mutation();
