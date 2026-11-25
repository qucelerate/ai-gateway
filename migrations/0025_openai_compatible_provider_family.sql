-- Widen the provider family allow-list to admit generic OpenAI-compatible
-- catalog accounts. This family is explicitly trusted and constrained
-- (docs/architecture-plan.md Rollout item 7): compatibility with the OpenAI
-- wire format is not proof of identical semantics, so operators are
-- responsible for verifying their configured endpoint's actual behavior.
-- It supports embeddings and the standard usage-field set, so it needs no
-- accompanying change to gateway.prevent_anthropic_embeddings().
ALTER TABLE gateway.catalog_provider_account
    DROP CONSTRAINT IF EXISTS catalog_provider_account_family_check;
ALTER TABLE gateway.catalog_provider_account
    ADD CONSTRAINT catalog_provider_account_family_check
    CHECK (family = ANY (ARRAY['openai'::text, 'anthropic'::text, 'gemini'::text, 'bedrock'::text, 'azure-openai'::text, 'azure-ai-foundry'::text, 'openai-compatible'::text]));
