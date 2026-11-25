-- The Anthropic adapter reports prompt cache reads as cached input and cache
-- writes as other tokens, so an Anthropic price may charge them. Reasoning
-- stays zero for Anthropic and Bedrock: both count thinking as output.
-- Embedding rules are unchanged from 0102.
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
    -- Price columns are read only for price rows: capability rows have none
    -- (see 0029), and PL/pgSQL does not short-circuit one condition.
    IF TG_TABLE_NAME = 'catalog_price' THEN
        IF family_name IN ('anthropic', 'bedrock') AND NEW.reasoning_minor_per_token <> 0 THEN
            RAISE EXCEPTION '% unsupported usage prices must be zero', family_name;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- As in 0041, with the same family rule as the catalog price guard above.
CREATE OR REPLACE FUNCTION gateway.catalog_price_override_for(
    organization UUID, override_id UUID, deployment_name TEXT, operation_name TEXT,
    family_name TEXT
) RETURNS gateway.catalog_price_override
LANGUAGE plpgsql STABLE AS $$
DECLARE
    found gateway.catalog_price_override;
BEGIN
    SELECT * INTO found FROM gateway.catalog_price_override o
     WHERE o.organization_id = organization AND o.id = override_id
       AND o.deployment = deployment_name AND o.operation = operation_name;
    IF found.id IS NULL
       OR (family_name IN ('anthropic', 'bedrock') AND found.reasoning_minor_per_token <> 0)
    THEN
        RETURN NULL;
    END IF;
    RETURN found;
END;
$$;
