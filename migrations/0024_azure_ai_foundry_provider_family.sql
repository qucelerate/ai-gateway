-- Widen the provider family allow-list to admit Azure AI Foundry catalog
-- accounts. Azure AI Foundry is a separate provider family from Azure
-- OpenAI: distinct inference contract, deployment routing, and error
-- envelope (see docs/architecture-plan.md Decision Log D-006). It supports
-- embeddings and the full usage-field set (cached + reasoning tokens), so
-- like Azure OpenAI it needs no accompanying change to
-- gateway.prevent_anthropic_embeddings().
ALTER TABLE gateway.catalog_provider_account
    DROP CONSTRAINT IF EXISTS catalog_provider_account_family_check;
ALTER TABLE gateway.catalog_provider_account
    ADD CONSTRAINT catalog_provider_account_family_check
    CHECK (family = ANY (ARRAY['openai'::text, 'anthropic'::text, 'gemini'::text, 'bedrock'::text, 'azure-openai'::text, 'azure-ai-foundry'::text]));
