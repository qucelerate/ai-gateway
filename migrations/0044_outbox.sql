-- Transactional outbox. Accounting writes an event in the same transaction as
-- the change it describes; the worker hands pending events to consumers and
-- retries failures with backoff. Payloads are fixed-shape metadata only: no
-- prompts, completions, credentials or free-form user text.
CREATE TABLE gateway.outbox_event (
    id UUID PRIMARY KEY,
    sequence BIGINT GENERATED ALWAYS AS IDENTITY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    event_type TEXT NOT NULL CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.resolved'
    )),
    payload JSONB NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    dispatched_at TIMESTAMPTZ,
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) <= 200),
    CONSTRAINT outbox_event_sequence_uq UNIQUE (sequence)
);

CREATE INDEX outbox_event_pending_idx ON gateway.outbox_event (next_attempt_at, sequence)
    WHERE dispatched_at IS NULL;
CREATE INDEX outbox_event_dispatched_idx ON gateway.outbox_event (dispatched_at)
    WHERE dispatched_at IS NOT NULL;

-- Only the dispatch bookkeeping may change, and a dispatched event stays
-- dispatched.
CREATE OR REPLACE FUNCTION gateway.prevent_outbox_event_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.id IS DISTINCT FROM NEW.id
       OR OLD.sequence IS DISTINCT FROM NEW.sequence
       OR OLD.organization_id IS DISTINCT FROM NEW.organization_id
       OR OLD.event_type IS DISTINCT FROM NEW.event_type
       OR OLD.payload IS DISTINCT FROM NEW.payload
       OR OLD.occurred_at IS DISTINCT FROM NEW.occurred_at
    THEN RAISE EXCEPTION 'outbox events are immutable'; END IF;
    IF OLD.dispatched_at IS NOT NULL THEN
        RAISE EXCEPTION 'dispatched outbox events are final';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER outbox_event_immutable BEFORE UPDATE ON gateway.outbox_event
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_outbox_event_mutation();
