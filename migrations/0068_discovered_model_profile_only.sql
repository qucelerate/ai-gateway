-- A discovered model's availability gains `profile_only`: the account's
-- listing reports it, but only an inference profile can call it (a Bedrock
-- foundation model whose inference types lack ON_DEMAND, such as recent
-- Anthropic models). Automatic offers never offer its own identifier, which
-- Bedrock refuses, while its profiles are offered and priced by its name.

ALTER TABLE gateway.catalog_discovered_model
    DROP CONSTRAINT catalog_discovered_model_availability_ck,
    ADD CONSTRAINT catalog_discovered_model_availability_ck
        CHECK (availability IN ('listed', 'unknown', 'profile_only'));
