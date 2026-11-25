-- What a listing says of each discovered model. Discovery still never
-- changes a catalog release: these columns only tell operators what the
-- provider reported, and routing reads capabilities from the catalog alone.
--
-- `operations`: the operations the model supports, recorded only where the
-- listing states them or the model's family makes them certain; NULL when
-- unknown, never empty.
-- `availability`: `listed` when the account's own listing reports the model,
-- `unknown` when the listing is a catalog wider than where the account calls
-- (Google's publisher models on Agent Platform list every location's models).

ALTER TABLE gateway.catalog_discovered_model
    ADD COLUMN operations TEXT[]
        CONSTRAINT catalog_discovered_model_operations_ck CHECK (
            operations IS NULL
            OR (cardinality(operations) BETWEEN 1 AND 3
                AND operations <@ ARRAY['chat', 'chat_stream', 'embeddings']::text[])
        ),
    ADD COLUMN availability TEXT NOT NULL DEFAULT 'listed'
        CONSTRAINT catalog_discovered_model_availability_ck
            CHECK (availability IN ('listed', 'unknown'));
