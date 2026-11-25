-- D-030 milestone 5: queue every change to what a request's ClickHouse row
-- holds, not only those an `inference.finalized` event follows.
--
-- A request's row is read from its reservation, its trace, its latest usage
-- row and its reconciliation item's reason. While the reservation is open
-- (and not a batch line) reads take the request from PostgreSQL anyway, and
-- the change that closes it is queued below; so a change needs queueing
-- only once the reservation is closed (late settlement, recovery, an
-- operator's resolution, which may add usage to a request whose trace
-- retention already deleted) or for a batch line. The outbox's queueing of
-- `inference.finalized` stays: an entry for the same request is written
-- once with the others its read covers.
--
-- Like 0148's, every trigger does nothing but one lookup while the sink is
-- off, and queues with an append-only insert, so it never waits on or
-- conflicts with the worker.

-- A change to a trace, usage row or reconciliation item: queue its request
-- if the reservation is closed or a batch line.
CREATE FUNCTION gateway.analytics_sink_enqueue_dependent()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.analytics_sink) THEN
        INSERT INTO gateway.analytics_sink_queue
            (organization_id, reservation_id, reservation_created_at)
        SELECT r.organization_id, r.id, r.created_at
        FROM gateway.accounting_reservation r
        WHERE r.organization_id = NEW.organization_id AND r.id = NEW.reservation_id
          AND (r.state NOT IN ('reserved', 'claimed') OR r.batch_id IS NOT NULL);
    END IF;
    RETURN NULL;
END;
$$;

-- A reservation closing, or changing once closed (or as a batch line), in
-- a column the row reads. Checked in the WHEN clause, so admission's own
-- updates (reserved to claimed) call nothing.
CREATE TRIGGER accounting_reservation_analytics_changed
    AFTER UPDATE ON gateway.accounting_reservation
    FOR EACH ROW
    WHEN ((OLD.state NOT IN ('reserved', 'claimed') OR NEW.state NOT IN ('reserved', 'claimed')
           OR NEW.batch_id IS NOT NULL)
          AND (OLD.state, OLD.team_id, OLD.service_account_id, OLD.model, OLD.deployment,
               OLD.catalog_family, OLD.batch_id, OLD.model_master, OLD.catalog_release_id,
               OLD.catalog_deployment_id, OLD.created_at)
              IS DISTINCT FROM
              (NEW.state, NEW.team_id, NEW.service_account_id, NEW.model, NEW.deployment,
               NEW.catalog_family, NEW.batch_id, NEW.model_master, NEW.catalog_release_id,
               NEW.catalog_deployment_id, NEW.created_at))
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_inserted();

-- A trace admitted pending belongs to an open reservation: nothing to do.
CREATE TRIGGER inference_trace_analytics_inserted
    AFTER INSERT ON gateway.inference_trace
    FOR EACH ROW WHEN (NEW.status <> 'pending')
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_dependent();

CREATE TRIGGER inference_trace_analytics_changed
    AFTER UPDATE ON gateway.inference_trace
    FOR EACH ROW WHEN (OLD.* IS DISTINCT FROM NEW.*)
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_dependent();

-- Usage rows are append only (0004).
CREATE TRIGGER accounting_usage_analytics_inserted
    AFTER INSERT ON gateway.accounting_usage
    FOR EACH ROW
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_dependent();

-- The item's reason decides a request's status and duration.
CREATE TRIGGER accounting_reconciliation_analytics_changed
    AFTER INSERT OR UPDATE OF reason ON gateway.accounting_reconciliation
    FOR EACH ROW
    EXECUTE FUNCTION gateway.analytics_sink_enqueue_dependent();
