-- The family guard fires on both catalog_capability and catalog_price
-- inserts, but guarded the price-only columns with
-- `TG_TABLE_NAME = 'catalog_price' AND NEW.<price column> ...` in a single
-- expression. PL/pgSQL resolves every NEW field in an expression before
-- evaluating it, so any Anthropic or Bedrock capability row failed with
-- "record new has no field cached_input_minor_per_token" and no catalog with
-- those families could be applied. Branch on the table first so price columns
-- are only referenced for price rows. Behaviour is otherwise unchanged.
CREATE OR REPLACE FUNCTION gateway.prevent_anthropic_embeddings()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE family_name TEXT;
BEGIN
    SELECT p.family INTO family_name
      FROM gateway.catalog_deployment d
      JOIN gateway.catalog_provider_account p
        ON p.organization_id=d.organization_id AND p.release_id=d.release_id
       AND p.id=d.provider_account_id
       AND p.adapter_instance_key=d.adapter_instance_key
     WHERE d.organization_id=NEW.organization_id
       AND d.release_id=NEW.release_id AND d.id=NEW.deployment_id;
    IF family_name IN ('anthropic', 'bedrock') AND NEW.operation = 'embeddings' THEN
        RAISE EXCEPTION '% embeddings capability is unsupported', family_name;
    END IF;
    IF TG_TABLE_NAME = 'catalog_price' THEN
        IF family_name = 'anthropic'
           AND (NEW.cached_input_minor_per_token <> 0
                OR NEW.reasoning_minor_per_token <> 0
                OR NEW.other_minor_per_token <> 0)
        THEN
            RAISE EXCEPTION 'anthropic unsupported usage prices must be zero';
        ELSIF family_name = 'bedrock' AND NEW.reasoning_minor_per_token <> 0 THEN
            RAISE EXCEPTION 'bedrock unsupported usage prices must be zero';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
