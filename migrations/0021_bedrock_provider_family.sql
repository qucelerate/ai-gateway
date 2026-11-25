-- Widen the provider family allow-list to admit Bedrock catalog accounts.
ALTER TABLE gateway.catalog_provider_account
    DROP CONSTRAINT IF EXISTS catalog_provider_account_family_check;
ALTER TABLE gateway.catalog_provider_account
    ADD CONSTRAINT catalog_provider_account_family_check
    CHECK (family = ANY (ARRAY['openai'::text, 'anthropic'::text, 'gemini'::text, 'bedrock'::text]));
