-- D-030 milestone 5: the optional ClickHouse analytics sink. PostgreSQL
-- stays the system of record; ClickHouse holds one row per request
-- (reservation) for the usage dashboard, the usage report and trace search.
--
-- `analytics_sink` exists only while the sink is configured: a worker with
-- `GATEWAY_CLICKHOUSE_URL` creates it, and a worker without the setting
-- removes it (with the queue) once no configured worker has been seen for
-- an hour. Nothing below writes anything while it is absent, so a
-- deployment without ClickHouse writes nothing extra. (The two delete
-- triggers' transition tables still collect deleted rows, about 0.4 us a
-- row, before one EXISTS check; see docs/operations.md.)
CREATE TABLE gateway.analytics_sink (
    id BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    -- `starting`: the ClickHouse tables are being emptied for a fresh copy;
    -- `backfilling`: existing history is being copied; `live`: copied, and
    -- reads may use ClickHouse.
    phase TEXT NOT NULL DEFAULT 'starting'
        CHECK (phase IN ('starting', 'backfilling', 'live')),
    -- The ClickHouse store's own id (its `gateway_meta` row), so a store
    -- that was emptied or replaced is noticed and copied again.
    instance_id UUID,
    enabled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Last seen by a worker with the sink configured.
    heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- The backfill walks reservations in id order; the last id copied.
    backfill_cursor UUID,
    backfill_rows BIGINT NOT NULL DEFAULT 0 CHECK (backfill_rows >= 0),
    backfill_started_at TIMESTAMPTZ,
    backfill_completed_at TIMESTAMPTZ,
    -- The worker running `starting` or `backfilling`, until its lease ends.
    lease_owner UUID,
    lease_until TIMESTAMPTZ,
    last_success_at TIMESTAMPTZ,
    failing_since TIMESTAMPTZ,
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200),
    CONSTRAINT analytics_sink_live_ck CHECK (
        phase <> 'live' OR (instance_id IS NOT NULL AND backfill_completed_at IS NOT NULL))
);

-- Requests whose row in ClickHouse is (or may be) out of date. A request is
-- queued when its `inference.finalized` event is dispatched, when a batch
-- line is admitted (it stays pending for hours) and when retention deletes
-- its trace or reservation. Append only: queueing inserts a new entry and
-- never updates or locks an existing one, so it cannot conflict with the
-- worker or with another dispatcher. The worker removes the entries its
-- read covered (those visible in the snapshot it read the request from), so
-- a request queued several times is written once.
CREATE TABLE gateway.analytics_sink_queue (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    organization_id UUID NOT NULL,
    reservation_id UUID NOT NULL,
    -- The row's ClickHouse sort key, kept so a deleted request can still
    -- be written as deleted.
    reservation_created_at TIMESTAMPTZ NOT NULL,
    queued_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200)
);
CREATE INDEX analytics_sink_queue_due_idx
    ON gateway.analytics_sink_queue (next_attempt_at, id);
CREATE INDEX analytics_sink_queue_request_idx
    ON gateway.analytics_sink_queue (organization_id, reservation_id);

-- Queues reservations while the sink exists. Runs as the application role,
-- so the data plane's role (which admits batch lines) needs no access.
CREATE FUNCTION gateway.analytics_sink_enqueue_inserted()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.analytics_sink) THEN
        INSERT INTO gateway.analytics_sink_queue
            (organization_id, reservation_id, reservation_created_at)
        VALUES (NEW.organization_id, NEW.id, NEW.created_at);
    END IF;
    RETURN NULL;
END;
$$;

CREATE FUNCTION gateway.analytics_sink_enqueue_deleted_reservations()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.analytics_sink) THEN
        INSERT INTO gateway.analytics_sink_queue
            (organization_id, reservation_id, reservation_created_at)
        SELECT organization_id, id, created_at FROM gone;
    END IF;
    RETURN NULL;
END;
$$;

-- A trace deleted on its own (trace retention) leaves its reservation, so
-- the request's usage stays in ClickHouse without its trace.
CREATE FUNCTION gateway.analytics_sink_enqueue_deleted_traces()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.analytics_sink) THEN
        INSERT INTO gateway.analytics_sink_queue
            (organization_id, reservation_id, reservation_created_at)
        SELECT r.organization_id, r.id, r.created_at
        FROM gone g
        JOIN gateway.accounting_reservation r
          ON r.organization_id = g.organization_id AND r.id = g.reservation_id;
    END IF;
    RETURN NULL;
END;
$$;

-- Batch lines only: an interactive request is read from PostgreSQL while
-- it is in flight (its reservation is open), and queued when it finalizes.
-- The WHEN clause is checked without calling the function, so a request's
-- own admission pays nothing.
CREATE TRIGGER accounting_reservation_analytics_batch_line
    AFTER INSERT ON gateway.accounting_reservation
    FOR EACH ROW WHEN (NEW.batch_id IS NOT NULL)
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_inserted();

CREATE TRIGGER accounting_reservation_analytics_deleted
    AFTER DELETE ON gateway.accounting_reservation
    REFERENCING OLD TABLE AS gone
    FOR EACH STATEMENT EXECUTE FUNCTION gateway.analytics_sink_enqueue_deleted_reservations();

CREATE TRIGGER inference_trace_analytics_deleted
    AFTER DELETE ON gateway.inference_trace
    REFERENCING OLD TABLE AS gone
    FOR EACH STATEMENT EXECUTE FUNCTION gateway.analytics_sink_enqueue_deleted_traces();
