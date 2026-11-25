-- An automatic offer may defer a listed model whose entitlement the listing
-- could not check (D-035): on a family where a first call can subscribe the
-- account (Bedrock's Marketplace models), a model with no evidence the
-- account may call it is not offered until a listing checks it. A model
-- already offered stays offered.
ALTER TABLE gateway.catalog_auto_offered_model
    DROP CONSTRAINT catalog_auto_offered_model_status_check,
    ADD CONSTRAINT catalog_auto_offered_model_status_check
        CHECK (status IN ('offered', 'needs_price', 'unsupported', 'withdrawn',
                          'catalog_full', 'refused', 'requires_action', 'unchecked'));
