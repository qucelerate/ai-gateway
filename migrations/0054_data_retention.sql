-- Per-organization data retention. Tenant administrators set how many days
-- traces, usage records and audit events are kept; the worker deletes what
-- is older. A period left unset keeps that data indefinitely. Exports of the
-- same data are read-only and audited (`data_export`).

CREATE TABLE gateway.organization_retention_policy (
    organization_id UUID PRIMARY KEY
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- The versioned admin resource id; stable per organization.
    id UUID NOT NULL UNIQUE,
    trace_days INTEGER CHECK (trace_days IS NULL OR trace_days BETWEEN 7 AND 3650),
    -- Reservations with their usage, allocations, price snapshot,
    -- settlement record, resolved reconciliation item and trace.
    usage_days INTEGER CHECK (usage_days IS NULL OR usage_days BETWEEN 30 AND 3650),
    audit_days INTEGER CHECK (audit_days IS NULL OR audit_days BETWEEN 90 AND 3650),
    updated_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_purged_at TIMESTAMPTZ,
    purged_traces BIGINT NOT NULL DEFAULT 0 CHECK (purged_traces >= 0),
    purged_reservations BIGINT NOT NULL DEFAULT 0 CHECK (purged_reservations >= 0),
    purged_audit_events BIGINT NOT NULL DEFAULT 0 CHECK (purged_audit_events >= 0),
    -- A trace never outlives the reservation it belongs to, so expiring
    -- usage requires a trace period no longer than it.
    CONSTRAINT organization_retention_policy_trace_ck
        CHECK (usage_days IS NULL OR (trace_days IS NOT NULL AND trace_days <= usage_days))
);

-- One row per export: what was asked for and how it ended. The export's
-- audit events name this row. Kept as long as audit events.
CREATE TABLE gateway.data_export (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    actor_user_id UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL,
    dataset TEXT NOT NULL CHECK (dataset IN ('usage', 'traces', 'audit-events')),
    format TEXT NOT NULL CHECK (format IN ('ndjson', 'csv')),
    range_from TIMESTAMPTZ NOT NULL,
    range_to TIMESTAMPTZ NOT NULL CHECK (range_to > range_from),
    request_id UUID NOT NULL,
    status TEXT NOT NULL DEFAULT 'streaming'
        CHECK (status IN ('streaming', 'completed', 'failed', 'abandoned')),
    rows BIGINT NOT NULL DEFAULT 0 CHECK (rows >= 0),
    started_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TIMESTAMPTZ,
    CONSTRAINT data_export_finished_ck CHECK ((status = 'streaming') = (finished_at IS NULL))
);

CREATE INDEX data_export_org_started_idx ON gateway.data_export (organization_id, started_at DESC, id DESC);

-- Usage purge removes a reservation's trace by reservation.
CREATE INDEX inference_trace_reservation_idx ON gateway.inference_trace (reservation_id);

-- Usage records, price snapshots and audit events stay append-only, except
-- that the retention purge may delete them. It marks its own transaction with
-- `SET LOCAL gateway.retention_purge = 'on'`; this guards against accidental
-- deletes, not against the application role, which owns the schema.
CREATE OR REPLACE FUNCTION gateway.prevent_accounting_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND current_setting('gateway.retention_purge', true) = 'on' THEN
    RETURN OLD;
  END IF;
  RAISE EXCEPTION 'accounting record is append-only';
END;
$$;

CREATE OR REPLACE FUNCTION gateway.prevent_admin_audit_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' AND current_setting('gateway.retention_purge', true) = 'on' THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'administrative audit events are immutable';
END;
$$;

-- Setting the policy is an audited admin write; exports are audited reads.
ALTER TABLE gateway.admin_resource_version
    DROP CONSTRAINT admin_resource_version_type_ck,
    ADD CONSTRAINT admin_resource_version_type_ck CHECK (
        resource_type IN ('organization', 'team', 'membership', 'role_binding',
                          'service_account', 'api_key', 'catalog_release',
                          'catalog_activation', 'provider_credential',
                          'price_override', 'rate_limit_policy', 'reconciliation_item',
                          'webhook_endpoint', 'webhook_delivery', 'catalog_sync_job',
                          'price_mapping', 'retention_policy')
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
                        'data_export')
    ) NOT VALID;
