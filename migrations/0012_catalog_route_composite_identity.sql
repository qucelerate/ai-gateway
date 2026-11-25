-- Bind route references to the complete organization/release/deployment/
-- operation tuple, including the referenced capability and price IDs.
ALTER TABLE gateway.catalog_capability
    ADD CONSTRAINT catalog_capability_route_identity_uq
        UNIQUE (organization_id, release_id, deployment_id, operation, id);

ALTER TABLE gateway.catalog_price
    ADD CONSTRAINT catalog_price_route_identity_uq
        UNIQUE (organization_id, release_id, deployment_id, operation, id);

ALTER TABLE gateway.catalog_route
    ADD CONSTRAINT catalog_route_capability_composite_fk
        FOREIGN KEY (organization_id, release_id, deployment_id, operation, capability_id)
        REFERENCES gateway.catalog_capability
            (organization_id, release_id, deployment_id, operation, id),
    ADD CONSTRAINT catalog_route_price_composite_fk
        FOREIGN KEY (organization_id, release_id, deployment_id, operation, price_id)
        REFERENCES gateway.catalog_price
            (organization_id, release_id, deployment_id, operation, id);
