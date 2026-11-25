-- A listed deployment's details (D-018, Phase 6 milestone 2): where a
-- listing is a list of the account's own deployments (Azure), what each
-- serves and on what terms. NULL for listings of models. Shown to operators
-- only; routing and offers never read them.
--
-- `deployment_format`: the model's format, which says which API serves it
-- (`OpenAI`, `Anthropic`, `xAI`, ...). `deployment_model` and
-- `deployment_version`: the model the deployment serves. `deployment_sku`:
-- the deployment type (`GlobalStandard`, `ProvisionedManaged`, ...).
-- `deployment_capacity`: the capacity bought, in the SKU's unit.
ALTER TABLE gateway.catalog_discovered_model
    ADD COLUMN deployment_format TEXT
        CONSTRAINT catalog_discovered_model_deployment_format_ck
            CHECK (octet_length(deployment_format) BETWEEN 1 AND 128),
    ADD COLUMN deployment_model TEXT
        CONSTRAINT catalog_discovered_model_deployment_model_ck
            CHECK (octet_length(deployment_model) BETWEEN 1 AND 128),
    ADD COLUMN deployment_version TEXT
        CONSTRAINT catalog_discovered_model_deployment_version_ck
            CHECK (octet_length(deployment_version) BETWEEN 1 AND 128),
    ADD COLUMN deployment_sku TEXT
        CONSTRAINT catalog_discovered_model_deployment_sku_ck
            CHECK (octet_length(deployment_sku) BETWEEN 1 AND 128),
    ADD COLUMN deployment_capacity INTEGER
        CONSTRAINT catalog_discovered_model_deployment_capacity_ck
            CHECK (deployment_capacity >= 0);
