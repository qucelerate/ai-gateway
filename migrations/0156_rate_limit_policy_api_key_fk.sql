-- Rate limits at every level (0155), step 7 of 7: an API key's policy refers
-- to its key. Locks the small policy table and, briefly, `api_key` (the
-- referenced table); no policy names a key yet, so the check reads nothing.
--
-- 0150 to 0156 each take a strong lock on one busy table only. Admission
-- locks the rate-limit policies and then writes a reservation; a settlement
-- updates its reservation and its trigger then reads the policies. One
-- transaction altering both tables (as a first version of this change did)
-- waits for them in the opposite order and can deadlock with a request.
-- Each fails fast rather than queueing traffic behind a lock it waits for,
-- rolls back whole, and is tried again (see operations.md, Rate limits).
SET LOCAL lock_timeout = '3s';

ALTER TABLE gateway.rate_limit_policy
    ADD CONSTRAINT rate_limit_policy_api_key_fk FOREIGN KEY (organization_id, api_key_id)
        REFERENCES gateway.api_key (organization_id, id) ON DELETE CASCADE;
