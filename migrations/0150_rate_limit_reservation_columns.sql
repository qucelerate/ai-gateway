-- Rate limits at every level (0155), step 1 of 7: a reservation names its
-- API key, and a failover attempt the second its tokens are counted in.
--
-- 0150 to 0156 each take a strong lock on one busy table only. Admission
-- locks the rate-limit policies and then writes a reservation; a settlement
-- updates its reservation and its trigger then reads the policies. One
-- transaction altering both tables (as a first version of this change did)
-- waits for them in the opposite order and can deadlock with a request.
-- Each fails fast rather than queueing traffic behind a lock it waits for,
-- rolls back whole, and is tried again (see operations.md, Rate limits).
SET LOCAL lock_timeout = '3s';

-- Nullable columns without a default change only the catalog, however many
-- reservations there are. No foreign key on the key: checking one would
-- lock the key's row on every reservation, and keys are never deleted. The
-- indexes a key policy's recount reads are built concurrently by 0157 and
-- 0158.
ALTER TABLE gateway.accounting_reservation
    ADD COLUMN api_key_id UUID,
    ADD COLUMN tokens_counted_at TIMESTAMPTZ;

CREATE OR REPLACE FUNCTION gateway.prevent_reservation_rate_lease_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.rate_lease_id IS DISTINCT FROM NEW.rate_lease_id
       OR OLD.rate_lease_ticket IS DISTINCT FROM NEW.rate_lease_ticket
       OR OLD.rate_admitted_at IS DISTINCT FROM NEW.rate_admitted_at
       OR OLD.api_key_id IS DISTINCT FROM NEW.api_key_id
       OR OLD.tokens_counted_at IS DISTINCT FROM NEW.tokens_counted_at THEN
        RAISE EXCEPTION 'a reservation''s rate-limit admission is immutable';
    END IF;
    RETURN NEW;
END;
$$;
DROP TRIGGER accounting_reservation_rate_lease_immutable ON gateway.accounting_reservation;
CREATE TRIGGER accounting_reservation_rate_lease_immutable
    BEFORE UPDATE OF rate_lease_id, rate_lease_ticket, rate_admitted_at, api_key_id,
        tokens_counted_at
    ON gateway.accounting_reservation
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_reservation_rate_lease_change();
