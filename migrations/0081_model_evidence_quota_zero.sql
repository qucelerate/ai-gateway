-- Agent Platform partner-model evidence (D-018): discovery reads the
-- project's per-model request quota for partner models (Anthropic, Mistral,
-- xAI on Model Garden). A model whose quota is zero records
-- `requires_action` with reason
--   quota_zero  every call is refused as exhausted until the tenant has the
--               quota raised.
ALTER TABLE gateway.catalog_model_evidence
    DROP CONSTRAINT catalog_model_evidence_reason_check,
    ADD CONSTRAINT catalog_model_evidence_reason_check
        CHECK (reason IN ('access_denied', 'not_found', 'permission_missing',
                          'agreement_required', 'not_entitled', 'quota_zero'));
