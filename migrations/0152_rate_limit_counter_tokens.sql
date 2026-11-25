-- Rate limits at every level (0155), step 3 of 7: the token ring on each
-- policy's counter row.
--
-- 0150 to 0156 each take a strong lock on one busy table only. Admission
-- locks the rate-limit policies and then writes a reservation; a settlement
-- updates its reservation and its trigger then reads the policies. One
-- transaction altering both tables (as a first version of this change did)
-- waits for them in the opposite order and can deadlock with a request.
-- Each fails fast rather than queueing traffic behind a lock it waits for,
-- rolls back whole, and is tried again (see operations.md, Rate limits).
SET LOCAL lock_timeout = '3s';

-- The token rings. Defaults that are constants: no table rewrite.
ALTER TABLE gateway.rate_limit_counter
    ADD COLUMN token_last_second BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN token_slots BIGINT[] NOT NULL DEFAULT '{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}',
    ADD CONSTRAINT rate_limit_counter_token_slots_ck CHECK (cardinality(token_slots) = 61);
