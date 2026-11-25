-- Budget leases (D-030, GATEWAY_BUDGET_LEASES). A gateway process moves part
-- of a budget's remaining allowance into a lease in one transaction and then
-- reserves requests against it in memory, without locking the budget's row
-- for each one. Each request still gets its reservation and allocations and
-- is still finalized exactly once; only the budget row's updates are batched,
-- into the lease's return.
--
-- * A lease's amount is reserved on its bucket while the lease is active:
--   `reserved_minor` includes `leased_minor`, so consumed plus reserved still
--   never passes the limit and every reader of `reserved_minor` stays right.
-- * An allocation made under a lease names it (`lease_id`) and, while the
--   lease is active, moves nothing on the bucket: its hold and its charge
--   are inside the lease's amount.
-- * Returning a lease (when it goes idle, is used up and drained, its process
--   stops, or it expires and the worker returns it) moves what its
--   allocations still hold into `reserved_minor`, what they were charged into
--   `consumed_minor`, and gives the rest back. After that its allocations
--   finalize against the bucket as any other.
--
-- Every transaction that writes or finalizes an allocation under a lease
-- takes the lease's advisory lock shared (keyed by the lease id, see
-- persistence::leases) and checks it is still active; a return takes it
-- exclusively. So a return sees
-- every allocation made under the lease and no allocation moves while it
-- folds them. The advisory lock is taken before any bucket row, on every
-- path, so the two orders cannot deadlock.
ALTER TABLE gateway.quota_bucket
    ADD COLUMN leased_minor BIGINT NOT NULL DEFAULT 0 CHECK (leased_minor >= 0),
    ADD CONSTRAINT quota_bucket_leased_within_reserved_ck CHECK (leased_minor <= reserved_minor);

CREATE TABLE gateway.budget_lease (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    quota_bucket_id UUID NOT NULL,
    -- The process holding it: a fresh id each time a gateway starts.
    holder UUID NOT NULL,
    amount_minor BIGINT NOT NULL CHECK (amount_minor > 0),
    -- The bucket's limit when the lease was taken: a lease is not used once
    -- the budget's limit changes.
    limit_minor BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- After this the worker returns it, whether or not its holder is alive.
    expires_at TIMESTAMPTZ NOT NULL,
    returned_at TIMESTAMPTZ,
    return_reason TEXT CHECK (return_reason IN ('idle', 'drained', 'shutdown', 'expired', 'limit', 'operator')),
    -- What its allocations held and had been charged when it was returned.
    returned_held_minor BIGINT,
    returned_settled_minor BIGINT,
    CONSTRAINT budget_lease_bucket_fk FOREIGN KEY (organization_id, quota_bucket_id)
        REFERENCES gateway.quota_bucket (organization_id, id) ON DELETE RESTRICT,
    CONSTRAINT budget_lease_returned_ck CHECK (
        (returned_at IS NULL AND return_reason IS NULL AND returned_held_minor IS NULL
            AND returned_settled_minor IS NULL)
        OR (returned_at IS NOT NULL AND return_reason IS NOT NULL
            AND returned_held_minor IS NOT NULL AND returned_settled_minor IS NOT NULL))
);
CREATE INDEX budget_lease_active_expiry_idx ON gateway.budget_lease (expires_at)
    WHERE returned_at IS NULL;
CREATE INDEX budget_lease_active_bucket_idx ON gateway.budget_lease (quota_bucket_id)
    WHERE returned_at IS NULL;

-- No foreign key: one would take a key-share lock on the lease's row with
-- every reservation, hundreds a second on one row. The lease is checked
-- under its advisory lock instead.
ALTER TABLE gateway.accounting_allocation ADD COLUMN lease_id UUID;
CREATE INDEX accounting_allocation_lease_idx ON gateway.accounting_allocation (lease_id)
    WHERE lease_id IS NOT NULL;

-- The lease an allocation was made under never changes.
CREATE OR REPLACE FUNCTION gateway.prevent_allocation_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.reservation_id<>NEW.reservation_id OR OLD.quota_bucket_id<>NEW.quota_bucket_id OR OLD.organization_id<>NEW.organization_id OR OLD.reserved_minor<>NEW.reserved_minor OR OLD.lease_id IS DISTINCT FROM NEW.lease_id THEN RAISE EXCEPTION 'accounting allocation fields are immutable'; END IF;
  IF OLD.state='held' AND NEW.state='settled' AND (NEW.settled_minor IS NULL OR NEW.usage_id IS NULL OR NEW.released_at IS NOT NULL) THEN RAISE EXCEPTION 'invalid settled allocation'; END IF;
  IF OLD.state='held' AND NEW.state='released' AND (NEW.settled_minor IS NOT NULL OR NEW.usage_id IS NOT NULL OR NEW.released_at IS NULL) THEN RAISE EXCEPTION 'invalid released allocation'; END IF;
  IF OLD.state='held' AND NEW.state='held' AND (NEW.settled_minor IS NOT NULL OR NEW.usage_id IS NOT NULL OR NEW.released_at IS NOT NULL) THEN RAISE EXCEPTION 'invalid held allocation'; END IF;
  IF OLD.state IN ('settled','released') AND (NEW.state<>OLD.state OR OLD.settled_minor IS DISTINCT FROM NEW.settled_minor OR OLD.usage_id IS DISTINCT FROM NEW.usage_id OR OLD.released_at IS DISTINCT FROM NEW.released_at) THEN RAISE EXCEPTION 'allocation state is immutable'; END IF;
  IF OLD.state NOT IN ('held','settled','released') OR NEW.state NOT IN ('held','settled','released') THEN RAISE EXCEPTION 'invalid allocation transition'; END IF;
  RETURN NEW;
END;
$$;
