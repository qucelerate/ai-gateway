-- Rate limits at every level (0155), step 5 of 7: an end user's plan may
-- limit their requests in flight.
--
-- 0150 to 0156 each take a strong lock on one busy table only. Admission
-- locks the rate-limit policies and then writes a reservation; a settlement
-- updates its reservation and its trigger then reads the policies. One
-- transaction altering both tables (as a first version of this change did)
-- waits for them in the opposite order and can deadlock with a request.
-- Each fails fast rather than queueing traffic behind a lock it waits for,
-- rolls back whole, and is tried again (see operations.md, Rate limits).
SET LOCAL lock_timeout = '3s';

ALTER TABLE gateway.end_user_plan
    ADD COLUMN max_concurrent INTEGER CHECK (max_concurrent BETWEEN 1 AND 1000000);
