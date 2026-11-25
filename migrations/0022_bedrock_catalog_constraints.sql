-- Bedrock Converse exposes no embeddings operation, and this adapter does
-- not map a reasoning-token usage dimension for Bedrock. Extend the existing
-- family-scoped guard (rather than adding a second, since both checks fire
-- from the same triggers on the same rows).
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
    IF family_name = 'anthropic' THEN
        IF TG_TABLE_NAME = 'catalog_price'
           AND (NEW.cached_input_minor_per_token <> 0
                OR NEW.reasoning_minor_per_token <> 0
                OR NEW.other_minor_per_token <> 0)
        THEN
            RAISE EXCEPTION 'anthropic unsupported usage prices must be zero';
        END IF;
    ELSIF family_name = 'bedrock' THEN
        IF TG_TABLE_NAME = 'catalog_price' AND NEW.reasoning_minor_per_token <> 0 THEN
            RAISE EXCEPTION 'bedrock unsupported usage prices must be zero';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;
