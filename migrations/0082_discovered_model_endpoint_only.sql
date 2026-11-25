-- A discovered model's availability gains `endpoint_only`: one of the
-- account's own Agent Platform endpoints, a model it deployed to dedicated
-- machines (a Model Garden deployment, a tuned model). The Gemini adapter
-- does not call endpoints, so automatic offers never offer one and it is
-- never probed; it is listed for what it serves and on what machines.

ALTER TABLE gateway.catalog_discovered_model
    DROP CONSTRAINT catalog_discovered_model_availability_ck,
    ADD CONSTRAINT catalog_discovered_model_availability_ck
        CHECK (availability IN ('listed', 'unknown', 'profile_only', 'endpoint_only'));
