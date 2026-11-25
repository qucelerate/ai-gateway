-- Failover across a route's deployments (D-023, Phase 9 milestone 3). A
-- request whose provider call fails before the provider can have started
-- any work (the connection never opened, an open circuit, a 429) is tried on
-- another deployment of its route. Each attempt is its own reservation and
-- trace: the attempt that failed is released, holding nothing and charging
-- nothing, and the next is reserved in the same transaction from what it
-- held, so the request never holds more than it was first admitted for.
-- `failover_of` names the attempt a reservation (and its trace) replaced;
-- `attempt` counts from 1. It is a name, not a foreign key: retention
-- removes final reservations by age, and either attempt may go first.

ALTER TABLE gateway.accounting_reservation
    ADD COLUMN attempt SMALLINT NOT NULL DEFAULT 1
        CONSTRAINT accounting_reservation_attempt_ck CHECK (attempt BETWEEN 1 AND 8),
    ADD COLUMN failover_of UUID,
    ADD CONSTRAINT accounting_reservation_failover_ck
        CHECK ((attempt = 1) = (failover_of IS NULL));

CREATE UNIQUE INDEX accounting_reservation_failover_uq
    ON gateway.accounting_reservation (organization_id, failover_of)
    WHERE failover_of IS NOT NULL;

ALTER TABLE gateway.inference_trace
    ADD COLUMN attempt SMALLINT NOT NULL DEFAULT 1
        CONSTRAINT inference_trace_attempt_ck CHECK (attempt BETWEEN 1 AND 8),
    ADD COLUMN failover_of UUID;

CREATE INDEX inference_trace_failover_idx
    ON gateway.inference_trace (organization_id, failover_of)
    WHERE failover_of IS NOT NULL;

CREATE OR REPLACE FUNCTION gateway.prevent_price_snapshot_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.organization_id IS DISTINCT FROM NEW.organization_id
       OR OLD.team_id IS DISTINCT FROM NEW.team_id
       OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id
       OR OLD.idempotency_key IS DISTINCT FROM NEW.idempotency_key
       OR OLD.fingerprint IS DISTINCT FROM NEW.fingerprint
       OR OLD.reserved_minor IS DISTINCT FROM NEW.reserved_minor
       OR OLD.expires_at IS DISTINCT FROM NEW.expires_at
       OR OLD.output_token_cap IS DISTINCT FROM NEW.output_token_cap
       OR OLD.input_price_minor IS DISTINCT FROM NEW.input_price_minor
       OR OLD.output_price_minor IS DISTINCT FROM NEW.output_price_minor
       OR OLD.cached_input_price_minor IS DISTINCT FROM NEW.cached_input_price_minor
       OR OLD.reasoning_price_minor IS DISTINCT FROM NEW.reasoning_price_minor
       OR OLD.other_price_minor IS DISTINCT FROM NEW.other_price_minor
       OR OLD.price_version IS DISTINCT FROM NEW.price_version
       OR OLD.pricing_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR OLD.currency IS DISTINCT FROM NEW.currency
       OR OLD.deployment IS DISTINCT FROM NEW.deployment
       OR OLD.model IS DISTINCT FROM NEW.model
       OR OLD.trace_id IS DISTINCT FROM NEW.trace_id
       OR OLD.catalog_release_id IS DISTINCT FROM NEW.catalog_release_id
       OR OLD.catalog_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR OLD.catalog_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR OLD.catalog_digest IS DISTINCT FROM NEW.catalog_digest
       OR OLD.failover_of IS DISTINCT FROM NEW.failover_of
       OR OLD.attempt IS DISTINCT FROM NEW.attempt
    THEN RAISE EXCEPTION 'accounting reservation fields are immutable'; END IF;
    IF OLD.settlement_fingerprint IS NOT NULL
       AND OLD.settlement_fingerprint IS DISTINCT FROM NEW.settlement_fingerprint
    THEN RAISE EXCEPTION 'settlement fingerprint is immutable'; END IF;
    IF NOT ((OLD.state='reserved' AND NEW.state IN ('reserved','claimed','expired'))
        -- A claimed attempt the provider refused before any work is
        -- released when the request fails over to another deployment.
        OR (OLD.state='claimed' AND NEW.state IN ('claimed','unknown','settled','released'))
        OR (OLD.state='unknown' AND NEW.state IN ('unknown','settled'))
        OR (OLD.state IN ('settled','expired','released') AND NEW.state=OLD.state))
    THEN RAISE EXCEPTION 'invalid accounting reservation transition'; END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.prevent_trace_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.organization_id IS DISTINCT FROM NEW.organization_id
       OR OLD.team_id IS DISTINCT FROM NEW.team_id
       OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id
       OR OLD.reservation_id IS DISTINCT FROM NEW.reservation_id
       OR OLD.failover_of IS DISTINCT FROM NEW.failover_of
       OR OLD.attempt IS DISTINCT FROM NEW.attempt
       OR OLD.correlation_id IS DISTINCT FROM NEW.correlation_id
       OR OLD.operation IS DISTINCT FROM NEW.operation
       OR OLD.model IS DISTINCT FROM NEW.model
       OR OLD.input_bound IS DISTINCT FROM NEW.input_bound
       OR OLD.output_cap IS DISTINCT FROM NEW.output_cap
       OR OLD.price_version IS DISTINCT FROM NEW.price_version
       OR OLD.pricing_confidence IS DISTINCT FROM NEW.pricing_confidence
       OR OLD.currency IS DISTINCT FROM NEW.currency
       OR OLD.metadata IS DISTINCT FROM NEW.metadata
       OR OLD.catalog_release_id IS DISTINCT FROM NEW.catalog_release_id
       OR OLD.catalog_deployment_id IS DISTINCT FROM NEW.catalog_deployment_id
       OR OLD.catalog_price_id IS DISTINCT FROM NEW.catalog_price_id
       OR OLD.catalog_digest IS DISTINCT FROM NEW.catalog_digest
    THEN RAISE EXCEPTION 'inference trace metadata is immutable'; END IF;
    RETURN NEW;
END;
$$;
