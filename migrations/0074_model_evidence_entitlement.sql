-- Control-plane evidence (D-018, Phase 6 milestone 2): discovery checks
-- whether the identity the gateway calls with holds the permission to call a
-- listed model. It records `entitled`, or `requires_action` with reason
-- `permission_missing` (on Azure, the inference role on the resource).
ALTER TABLE gateway.catalog_model_evidence
    DROP CONSTRAINT catalog_model_evidence_reason_check,
    ADD CONSTRAINT catalog_model_evidence_reason_check
        CHECK (reason IN ('access_denied', 'not_found', 'permission_missing'));
