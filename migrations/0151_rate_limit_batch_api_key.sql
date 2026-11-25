-- Rate limits at every level (0155), step 2 of 7: a batch names its API key.
--
-- 0150 to 0156 each take a strong lock on one busy table only. Admission
-- locks the rate-limit policies and then writes a reservation; a settlement
-- updates its reservation and its trigger then reads the policies. One
-- transaction altering both tables (as a first version of this change did)
-- waits for them in the opposite order and can deadlock with a request.
-- Each fails fast rather than queueing traffic behind a lock it waits for,
-- rolls back whole, and is tried again (see operations.md, Rate limits).
SET LOCAL lock_timeout = '3s';

ALTER TABLE gateway.batch ADD COLUMN api_key_id UUID;
