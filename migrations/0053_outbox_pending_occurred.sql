-- The worker samples the age of the oldest undispatched outbox event every
-- minute. `occurred_at` is the emitting transaction's start, which need not
-- follow `sequence`, so the oldest is found by time; this index finds it in
-- one lookup however large the backlog grows.
CREATE INDEX outbox_event_pending_occurred_idx ON gateway.outbox_event (occurred_at)
    WHERE dispatched_at IS NULL;
