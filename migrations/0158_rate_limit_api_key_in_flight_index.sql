-- no-transaction
-- An API key's requests in flight, which the recount of its rate-limit
-- policy reads (0155). Built concurrently and a single statement, as 0157.
CREATE INDEX CONCURRENTLY accounting_reservation_api_key_in_flight_idx
    ON gateway.accounting_reservation (organization_id, api_key_id, expires_at)
    WHERE state IN ('reserved', 'claimed') AND api_key_id IS NOT NULL
