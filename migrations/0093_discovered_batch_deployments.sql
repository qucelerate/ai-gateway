-- Azure OpenAI's batch-type deployments (`GlobalBatch`, `DataZoneBatch`)
-- take batch jobs only (D-021, Phase 7 milestone 2), so discovery records
-- them with batch operations: `chat_batch` for a chat model,
-- `embeddings_batch` for an embedding one.
ALTER TABLE gateway.catalog_discovered_model
    DROP CONSTRAINT catalog_discovered_model_operations_ck,
    ADD CONSTRAINT catalog_discovered_model_operations_ck CHECK (
        operations IS NULL
        OR (cardinality(operations) BETWEEN 1 AND 5
            AND operations <@ ARRAY['chat', 'chat_stream', 'embeddings', 'chat_batch',
                                    'embeddings_batch']::text[])
    );

-- A price source publishes a model's batch price beside its on-demand one,
-- as the batch item (`<item> · batch`): one model may have one of each.
DROP INDEX gateway.price_observation_model_uq;
CREATE UNIQUE INDEX price_observation_model_uq
    ON gateway.price_observation (snapshot_id, model, region, currency,
                                  (right(item, 8) = ' · batch')) NULLS NOT DISTINCT
    WHERE model IS NOT NULL;
