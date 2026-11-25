-- Usage export to the organization's own object storage: settled usage as
-- gzip NDJSON objects in an S3 bucket, a Google Cloud Storage bucket or an
-- Azure Blob container, written through the organization's verified cloud
-- connection. See docs/architecture-plan.md §7 and D-014.

-- One destination per organization. `connection` names the organization's
-- stored credential (an AWS role, an Entra app or a Google service account)
-- the gateway writes with; no storage keys are stored.
CREATE TABLE gateway.usage_export (
    organization_id UUID PRIMARY KEY REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The admin resource id (versions, audit, idempotency).
    id UUID NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('s3', 'gcs', 'azure_blob')),
    connection TEXT NOT NULL CHECK (length(connection) BETWEEN 1 AND 128),
    -- The bucket, or the Azure container.
    bucket TEXT NOT NULL CHECK (length(bucket) BETWEEN 3 AND 222),
    -- The Azure storage account; the S3 bucket's region.
    account TEXT CHECK (account IS NULL OR length(account) BETWEEN 3 AND 24),
    region TEXT CHECK (region IS NULL OR length(region) BETWEEN 1 AND 32),
    prefix TEXT NOT NULL DEFAULT '' CHECK (length(prefix) <= 512),
    -- Usage finalized from then on is exported.
    enabled_at TIMESTAMPTZ NOT NULL,
    verified_at TIMESTAMPTZ NOT NULL,
    created_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_success_at TIMESTAMPTZ,
    -- The first failed upload since the last success; reported as failing
    -- after an hour, once through `usage_export.failing`.
    failing_since TIMESTAMPTZ,
    failing_reported_at TIMESTAMPTZ,
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200),
    CONSTRAINT usage_export_shape_ck CHECK (
        (kind = 's3' AND region IS NOT NULL AND account IS NULL)
        OR (kind = 'gcs' AND region IS NULL AND account IS NULL)
        OR (kind = 'azure_blob' AND account IS NOT NULL AND region IS NULL)
    )
);

-- Sealed batches: each is uploaded as one object, under the name recorded
-- when it was sealed, with exactly the records sealed into it, so a retry
-- rewrites the same object.
CREATE TABLE gateway.usage_export_batch (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL
        REFERENCES gateway.usage_export (organization_id) ON DELETE CASCADE,
    object_name TEXT NOT NULL CHECK (length(object_name) BETWEEN 1 AND 1024),
    hour TIMESTAMPTZ NOT NULL,
    first_sequence BIGINT NOT NULL,
    last_sequence BIGINT NOT NULL CHECK (last_sequence >= first_sequence),
    records INTEGER NOT NULL CHECK (records > 0),
    sealed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200),
    CONSTRAINT usage_export_batch_org_id_uq UNIQUE (organization_id, id)
);

CREATE INDEX usage_export_batch_due_idx ON gateway.usage_export_batch (next_attempt_at);

-- Queued records: one per dispatched `inference.finalized` event, built when
-- the event is dispatched and unsealed until a seal assigns a batch.
CREATE TABLE gateway.usage_export_record (
    organization_id UUID NOT NULL
        REFERENCES gateway.usage_export (organization_id) ON DELETE CASCADE,
    event_id UUID NOT NULL,
    -- The outbox event's sequence: batch names and record order.
    sequence BIGINT NOT NULL,
    finalized_at TIMESTAMPTZ NOT NULL,
    -- JSON, not JSONB: the record is written out exactly as built.
    record JSON NOT NULL CHECK (json_typeof(record) = 'object'),
    batch_id UUID,
    queued_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, event_id),
    CONSTRAINT usage_export_record_batch_fk FOREIGN KEY (organization_id, batch_id)
        REFERENCES gateway.usage_export_batch (organization_id, id) ON DELETE SET NULL (batch_id)
);

CREATE INDEX usage_export_record_unsealed_idx
    ON gateway.usage_export_record (organization_id, finalized_at, sequence)
    WHERE batch_id IS NULL;
CREATE INDEX usage_export_record_batch_idx
    ON gateway.usage_export_record (batch_id, sequence) WHERE batch_id IS NOT NULL;

-- `usage_export.failing`: an export has not written for an hour.
ALTER TABLE gateway.outbox_event
    DROP CONSTRAINT outbox_event_event_type_check,
    ADD CONSTRAINT outbox_event_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved', 'usage_export.failing'
    ));

ALTER TABLE gateway.webhook_endpoint
    DROP CONSTRAINT webhook_endpoint_event_types_check,
    ADD CONSTRAINT webhook_endpoint_event_types_check CHECK (
        cardinality(event_types) > 0
        AND event_types <@ ARRAY['inference.finalized', 'reconciliation.opened',
                                 'reconciliation.updated', 'reconciliation.resolved',
                                 'usage_export.failing']::TEXT[]
    );

ALTER TABLE gateway.webhook_delivery
    DROP CONSTRAINT webhook_delivery_event_type_check,
    ADD CONSTRAINT webhook_delivery_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved', 'usage_export.failing'
    ));

-- Lists carry `config_pin` (0063) so the order these migrations land in
-- does not matter.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy', 'budget_policy',
                          'invitation', 'identity_provider', 'config_pin', 'usage_export')
    );

ALTER TABLE gateway.audit_event
    DROP CONSTRAINT audit_event_entity_type_ck,
    ADD CONSTRAINT audit_event_entity_type_ck CHECK (
        entity_type IN ('organization', 'team', 'membership', 'role_binding',
                        'service_account', 'api_key', 'usage_report',
                        'inference_trace', 'reconciliation_item', 'catalog_release',
                        'catalog_activation', 'provider_credential', 'price_override',
                        'rate_limit_policy', 'webhook_endpoint', 'webhook_delivery',
                        'catalog_sync_job', 'price_mapping', 'retention_policy',
                        'data_export', 'cloud_principal_binding', 'invitation',
                        'budget_policy', 'identity_provider', 'config_pin', 'usage_export')
    ) NOT VALID;
