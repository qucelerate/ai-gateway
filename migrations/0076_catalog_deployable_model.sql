-- Deployable models (D-018, Phase 6 milestone 2): what the provider's catalog
-- lets an account deploy, per format, model and version (Azure's account
-- model list). Each successful listing replaces an account's rows; a listing
-- that could not read the catalog leaves them. Deploying is the tenant's
-- action: the gateway never creates deployments, and routing and offers do
-- not read this table.
CREATE TABLE gateway.catalog_deployable_model (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    adapter_instance_key TEXT NOT NULL CHECK (length(adapter_instance_key) BETWEEN 1 AND 128),
    family TEXT NOT NULL CHECK (octet_length(family) BETWEEN 1 AND 64),
    format TEXT NOT NULL CHECK (octet_length(format) BETWEEN 1 AND 128),
    model TEXT NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 255),
    version TEXT NOT NULL CHECK (octet_length(version) BETWEEN 1 AND 128),
    lifecycle_status TEXT CHECK (octet_length(lifecycle_status) BETWEEN 1 AND 128),
    default_version BOOLEAN NOT NULL,
    skus TEXT[] NOT NULL CHECK (cardinality(skus) <= 32),
    max_capacity INTEGER CHECK (max_capacity >= 0),
    retires_at TIMESTAMPTZ,
    operations TEXT[] CHECK (
        operations IS NULL
        OR (cardinality(operations) BETWEEN 1 AND 3
            AND operations <@ ARRAY['chat', 'chat_stream', 'embeddings']::text[])
    ),
    listed_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (organization_id, adapter_instance_key, family, format, model, version)
);
