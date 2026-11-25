-- The gateway schema is provisioned and owned by gateway_app outside this
-- repository. Phase 0 creates only a durable foundation marker.
CREATE TABLE IF NOT EXISTS gateway.schema_version (
    version BIGINT PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO gateway.schema_version (version)
VALUES (1)
ON CONFLICT (version) DO NOTHING;
