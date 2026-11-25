-- Chat accounting must never silently fall back to a byte/token estimate.
-- Existing releases are conservatively marked unsupported; this changes no
-- release graph row or digest and does not unseal anything.
ALTER TABLE gateway.catalog_capability
    ADD COLUMN chat_input_count_mode TEXT NOT NULL DEFAULT 'unsupported'
        CHECK (chat_input_count_mode IN ('provider_api_exact', 'unsupported'));
ALTER TABLE gateway.catalog_capability
    ALTER COLUMN chat_input_count_mode DROP DEFAULT;
