-- A release is sealed once its graph has been completely ingested, before it
-- is first published. Sealing is not tied to the active pointer: sealed
-- releases remain valid rollback targets, but cannot gain graph rows.
INSERT INTO gateway.catalog_release_seal (organization_id, release_id)
SELECT organization_id, id
FROM gateway.catalog_release
ON CONFLICT (organization_id, release_id) DO NOTHING;

-- Deactivation does not create a seal. All releases, including releases being
-- activated for the first time, are sealed before the active pointer changes.
DROP TRIGGER catalog_active_release_seal_update
    ON gateway.catalog_active_release;
DROP TRIGGER catalog_active_release_seal_delete
    ON gateway.catalog_active_release;
DROP FUNCTION gateway.seal_catalog_release_on_deactivate();

CREATE OR REPLACE FUNCTION gateway.prevent_catalog_sealed_activation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM gateway.catalog_release_seal s
        WHERE s.organization_id = NEW.organization_id
          AND s.release_id = NEW.release_id
    ) THEN
        RAISE EXCEPTION 'catalog release must be sealed before activation';
    END IF;
    RETURN NEW;
END;
$$;
