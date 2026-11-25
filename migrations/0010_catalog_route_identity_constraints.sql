-- Gate 2A: a route's capability and price must describe the exact same
-- deployment and operation as the route itself.
ALTER TABLE gateway.catalog_route
    ADD CONSTRAINT catalog_route_capability_identity_fk
        FOREIGN KEY (organization_id, release_id, deployment_id, operation)
        REFERENCES gateway.catalog_capability
            (organization_id, release_id, deployment_id, operation),
    ADD CONSTRAINT catalog_route_price_identity_fk
        FOREIGN KEY (organization_id, release_id, deployment_id, operation)
        REFERENCES gateway.catalog_price
            (organization_id, release_id, deployment_id, operation);

CREATE OR REPLACE FUNCTION gateway.validate_catalog_provenance()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    expected_digest BYTEA;
    expected_operation TEXT;
    expected_family TEXT;
    expected_source TEXT;
    expected_deployment UUID;
    expected_price UUID;
BEGIN
    IF NEW.catalog_release_id IS NULL THEN
        RETURN NEW;
    END IF;

    SELECT cr.digest, r.operation, p.family, pr.source, r.deployment_id, r.price_id
      INTO expected_digest, expected_operation, expected_family, expected_source,
           expected_deployment, expected_price
      FROM gateway.catalog_route r
      JOIN gateway.catalog_release cr
        ON cr.organization_id = r.organization_id AND cr.id = r.release_id
      JOIN gateway.catalog_deployment d
        ON d.organization_id = r.organization_id AND d.release_id = r.release_id
       AND d.id = r.deployment_id
      JOIN gateway.catalog_provider_account p
        ON p.organization_id = d.organization_id AND p.release_id = d.release_id
       AND p.id = d.provider_account_id
       AND p.adapter_instance_key = d.adapter_instance_key
      JOIN gateway.catalog_price pr
        ON pr.organization_id = r.organization_id AND pr.release_id = r.release_id
       AND pr.id = r.price_id
     WHERE r.organization_id = NEW.organization_id
       AND r.release_id = NEW.catalog_release_id
       AND r.id = NEW.catalog_route_id;

    IF expected_digest IS NULL
       OR expected_digest IS DISTINCT FROM NEW.catalog_digest
       OR expected_operation IS DISTINCT FROM NEW.catalog_operation
       OR expected_family IS DISTINCT FROM NEW.catalog_family
       OR expected_source IS DISTINCT FROM NEW.catalog_price_source
       OR expected_deployment IS DISTINCT FROM NEW.catalog_deployment_id
       OR expected_price IS DISTINCT FROM NEW.catalog_price_id
    THEN
        RAISE EXCEPTION 'catalog provenance does not match immutable route';
    END IF;
    RETURN NEW;
END;
$$;
