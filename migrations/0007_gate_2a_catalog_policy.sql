-- Gate 2A: capability policy is part of the immutable catalog graph.
ALTER TABLE gateway.catalog_capability
    ADD COLUMN max_input_bytes BIGINT NOT NULL DEFAULT 131072 CHECK (max_input_bytes > 0),
    ADD COLUMN max_output_tokens BIGINT NOT NULL DEFAULT 4096 CHECK (max_output_tokens > 0),
    ADD COLUMN max_tokens_per_byte INTEGER NOT NULL DEFAULT 1 CHECK (max_tokens_per_byte > 0),
    ADD COLUMN tokenizer TEXT NOT NULL DEFAULT 'configured'
        CHECK (length(tokenizer) BETWEEN 1 AND 128);

ALTER TABLE gateway.catalog_capability
    ALTER COLUMN max_input_bytes DROP DEFAULT,
    ALTER COLUMN max_output_tokens DROP DEFAULT,
    ALTER COLUMN max_tokens_per_byte DROP DEFAULT,
    ALTER COLUMN tokenizer DROP DEFAULT;
