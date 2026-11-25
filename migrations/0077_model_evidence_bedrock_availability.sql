-- Bedrock availability evidence (D-018, Phase 6 milestone 3): discovery asks
-- Bedrock's GetFoundationModelAvailability about each listed model. Besides
-- `permission_missing` (the identity's IAM authorization), it records
-- `requires_action` with reason
--   agreement_required  the account has not accepted the model's AWS
--                       Marketplace agreement, so a call is refused;
--   not_entitled        AWS restricts the model for the account.
ALTER TABLE gateway.catalog_model_evidence
    DROP CONSTRAINT catalog_model_evidence_reason_check,
    ADD CONSTRAINT catalog_model_evidence_reason_check
        CHECK (reason IN ('access_denied', 'not_found', 'permission_missing',
                          'agreement_required', 'not_entitled'));
