-- Model Garden partner models served through Agent Platform
-- (docs/custom-deployments-design.md): a call the provider refuses because
-- the project has not enabled the model in Model Garden (its terms are not
-- accepted) records `requires_action` with reason
--   enable_in_model_garden  every call is refused until a project owner
--                           enables the model on its Model Garden card.
-- The list is extended in place rather than restated, so a reason another
-- migration added is kept.
DO $$
DECLARE
    def TEXT;
BEGIN
    SELECT pg_get_constraintdef(oid) INTO STRICT def FROM pg_constraint
    WHERE conname = 'catalog_model_evidence_reason_check'
      AND conrelid = 'gateway.catalog_model_evidence'::regclass;
    def := regexp_replace(def, '\s*NOT VALID\s*$', '');
    IF def !~ '\]\)\)\)$' THEN
        RAISE EXCEPTION 'unexpected catalog_model_evidence_reason_check: %', def;
    END IF;
    def := regexp_replace(def, '\]\)\)\)$', ', ''enable_in_model_garden''::text])))');
    EXECUTE 'ALTER TABLE gateway.catalog_model_evidence '
        || 'DROP CONSTRAINT catalog_model_evidence_reason_check, '
        || 'ADD CONSTRAINT catalog_model_evidence_reason_check ' || def;
END
$$;
