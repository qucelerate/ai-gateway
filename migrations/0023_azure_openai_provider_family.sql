-- Widen the provider family allow-list to admit Azure OpenAI catalog
-- accounts. Azure OpenAI supports embeddings and the full usage-field set
-- (cached + reasoning tokens), so unlike Anthropic/Bedrock it needs no
-- accompanying change to gateway.prevent_anthropic_embeddings().
ALTER TABLE gateway.catalog_provider_account
    DROP CONSTRAINT IF EXISTS catalog_provider_account_family_check;
ALTER TABLE gateway.catalog_provider_account
    ADD CONSTRAINT catalog_provider_account_family_check
    CHECK (family = ANY (ARRAY['openai'::text, 'anthropic'::text, 'gemini'::text, 'bedrock'::text, 'azure-openai'::text]));
