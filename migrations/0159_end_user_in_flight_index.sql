-- no-transaction
-- An end user's requests in flight, which their plan's `max_concurrent`
-- (0155) counts at each admission with the user's row locked. Built
-- concurrently and a single statement, as 0157.
CREATE INDEX CONCURRENTLY accounting_reservation_end_user_in_flight_idx
    ON gateway.accounting_reservation (end_user_id)
    WHERE state IN ('reserved', 'claimed') AND batch_id IS NULL AND end_user_id IS NOT NULL
