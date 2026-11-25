-- Phase 13 milestone 3 (D-033): each reservation (one per request
-- attempt, so per deployment served) carries the master record its
-- deployment was linked to when it was admitted, so usage can be filtered
-- and grouped by model across providers. Links stay out of configuration
-- versions and routing: this is resolved inside the database, by a
-- trigger on the reservation's insert, from the organization's links and
-- the card files' exact matches. No round trip is added to a request, and
-- the data plane's role needs no access to either table (the function runs
-- as the application role, as the provenance checks do, 0062).

-- The card files' exact matches (`matches` of each card), as the API
-- process loaded them (shipped files plus GATEWAY_MODEL_CARDS_DIR). The API
-- replaces the set at start-up; nothing else writes it.
CREATE TABLE gateway.model_card_match (
    family TEXT NOT NULL CHECK (octet_length(family) BETWEEN 1 AND 64),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    card_id TEXT NOT NULL CHECK (card_id ~ '^[a-z0-9._-]+/[a-z0-9._-]+$'),
    PRIMARY KEY (family, model)
);
CREATE INDEX model_card_match_card_idx ON gateway.model_card_match (card_id);

-- `null`: no master (none matched, linked to none, no catalog provenance,
-- or admitted before this migration).
ALTER TABLE gateway.accounting_reservation ADD COLUMN model_master TEXT;

CREATE FUNCTION gateway.stamp_accounting_reservation_model_master()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    -- Prefixed apart from every column the queries below name.
    v_model TEXT;
    v_family TEXT;
    v_account TEXT;
    v_linked BOOLEAN := false;
    v_master TEXT;
BEGIN
    -- Recorded once, when the attempt is admitted; never rewritten.
    IF TG_OP = 'UPDATE' THEN
        NEW.model_master := OLD.model_master;
        RETURN NEW;
    END IF;
    NEW.model_master := NULL;
    IF NEW.catalog_release_id IS NULL OR NEW.catalog_deployment_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT d.model, p.family, d.adapter_instance_key
      INTO v_model, v_family, v_account
      FROM gateway.catalog_deployment d
      JOIN gateway.catalog_provider_account p
        ON p.organization_id = d.organization_id
       AND p.release_id = d.release_id
       AND p.id = d.provider_account_id
     WHERE d.organization_id = NEW.organization_id
       AND d.release_id = NEW.catalog_release_id
       AND d.id = NEW.catalog_deployment_id;
    IF v_model IS NULL THEN
        RETURN NEW;
    END IF;
    -- 1. The organization's link (possibly to none).
    SELECT true, l.master INTO v_linked, v_master
      FROM gateway.model_master_link l
     WHERE l.organization_id = NEW.organization_id
       AND l.family = v_family
       AND l.model = v_model
       AND l.deleted_at IS NULL;
    IF v_linked THEN
        -- A link to a card no longer on file has no master, as the
        -- Models view lists it (organization records linked to cannot be
        -- deleted).
        IF v_master IS NOT NULL AND v_master NOT LIKE 'org/%'
           AND NOT EXISTS (SELECT 1 FROM gateway.model_card_match c
                            WHERE c.card_id = v_master) THEN
            v_master := NULL;
        END IF;
        NEW.model_master := v_master;
        RETURN NEW;
    END IF;
    -- 2. The card that lists the family and model, or, for a deployment
    -- named apart from its model (Azure), the model its listing names.
    SELECT c.card_id INTO NEW.model_master
      FROM gateway.model_card_match c
     WHERE c.family = v_family AND c.model = v_model;
    IF NEW.model_master IS NULL THEN
        SELECT c.card_id INTO NEW.model_master
          FROM gateway.catalog_discovered_model m
          JOIN gateway.model_card_match c
            ON c.family = m.family AND c.model = m.deployment_model
         WHERE m.organization_id = NEW.organization_id
           AND m.adapter_instance_key = v_account
           AND m.family = v_family
           AND m.model = v_model
           AND m.missing_since IS NULL
         LIMIT 1;
    END IF;
    RETURN NEW;
END;
$$;
REVOKE ALL ON FUNCTION gateway.stamp_accounting_reservation_model_master() FROM PUBLIC;

CREATE TRIGGER accounting_reservation_model_master
BEFORE INSERT ON gateway.accounting_reservation
FOR EACH ROW EXECUTE FUNCTION gateway.stamp_accounting_reservation_model_master();
-- Only an update naming the column fires this, so settlement pays nothing.
CREATE TRIGGER accounting_reservation_model_master_kept
BEFORE UPDATE OF model_master ON gateway.accounting_reservation
FOR EACH ROW EXECUTE FUNCTION gateway.stamp_accounting_reservation_model_master();

-- Exports remember the master they were limited to.
ALTER TABLE gateway.data_export ADD COLUMN model_master TEXT;
