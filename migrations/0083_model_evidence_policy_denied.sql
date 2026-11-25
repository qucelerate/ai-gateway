-- Agent Platform policy evidence (D-018): discovery reads the project's
-- effective `vertexai.allowedModels` organization policy where the tenant
-- lets the gateway read it. A model the policy forbids records
-- `requires_action` with reason
--   policy_denied  every call is refused until the tenant's policy
--                  administrator allows the model.
ALTER TABLE gateway.catalog_model_evidence
    DROP CONSTRAINT catalog_model_evidence_reason_check,
    ADD CONSTRAINT catalog_model_evidence_reason_check
        CHECK (reason IN ('access_denied', 'not_found', 'permission_missing',
                          'agreement_required', 'not_entitled', 'quota_zero',
                          'policy_denied'));
