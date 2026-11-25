-- An automatic offer may decide a listed model requires action (D-018): a
-- read-only check during discovery found something the tenant must do before
-- the account can call it, such as accept the model's agreement. It is not
-- offered until a check or a call shows the account may call it.
ALTER TABLE gateway.catalog_auto_offered_model
    DROP CONSTRAINT catalog_auto_offered_model_status_check,
    ADD CONSTRAINT catalog_auto_offered_model_status_check
        CHECK (status IN ('offered', 'needs_price', 'unsupported', 'withdrawn',
                          'catalog_full', 'refused', 'requires_action'));
