-- no-transaction
-- An API key's admissions in the last minute, which the recount of its
-- rate-limit policy reads (0155). Built concurrently, as 0146, so
-- reservations keep being written while it builds on a large table; it must
-- stay a single statement. If the build fails it leaves an invalid index and
-- the next run stops on "already exists": drop the index and migrate again.
CREATE INDEX CONCURRENTLY accounting_reservation_api_key_created_idx
    ON gateway.accounting_reservation (organization_id, api_key_id, created_at)
    WHERE api_key_id IS NOT NULL
