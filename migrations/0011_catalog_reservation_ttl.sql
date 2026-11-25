-- Reservation lifetime is a catalog capability policy, not process config.
ALTER TABLE gateway.catalog_capability
    ADD COLUMN reservation_ttl_seconds BIGINT NOT NULL DEFAULT 300
        CHECK (reservation_ttl_seconds BETWEEN 1 AND 86400);

ALTER TABLE gateway.catalog_capability
    ALTER COLUMN reservation_ttl_seconds DROP DEFAULT;
