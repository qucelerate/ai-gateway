-- Discovery's probes (D-018, Phase 6 milestone 3): when the gateway last
-- tried to call each listed model itself. Probing claims a model by setting
-- `attempted_at`, so two workers listing one account at once never probe
-- the same model twice (each probe is billed to the tenant), and a model
-- whose probe showed nothing yields to the others before it is tried again.
-- The result, when there is one, is evidence (`catalog_model_evidence`,
-- source `probe`). Removed with the discovered model.
CREATE TABLE gateway.catalog_model_probe (
    organization_id UUID NOT NULL,
    adapter_instance_key TEXT NOT NULL,
    family TEXT NOT NULL,
    model TEXT NOT NULL,
    attempted_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (organization_id, adapter_instance_key, family, model),
    FOREIGN KEY (organization_id, adapter_instance_key, family, model)
        REFERENCES gateway.catalog_discovered_model
            (organization_id, adapter_instance_key, family, model)
        ON DELETE CASCADE
);
