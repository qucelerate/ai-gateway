-- Bedrock serves embeddings through InvokeModel (Titan and Cohere Embed), so
-- only Anthropic still has no embeddings. Neither family's adapter runs
-- embedding batches. Price rules are unchanged from 0089.
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
    IF (family_name = 'anthropic' AND NEW.operation IN ('embeddings', 'embeddings_batch'))
       OR (family_name = 'bedrock' AND NEW.operation = 'embeddings_batch') THEN
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
