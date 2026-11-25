-- Rate-limit leases (D-030, GATEWAY_RATE_LIMIT_LEASES). Admission locks
-- the scope's policy rows for update and counts each new reservation on the
-- policy's counter row (0121), so one organization's admissions pass one at
-- a time through that row. With leases on, a gateway process takes a *lease*
-- of N admissions for a scope (organization, team, service account) in one
-- transaction, under the same locks as an admission, and then admits up to N
-- requests in memory, without locking or writing the policy's rows:
--
-- * Taking the lease counts all N on every policy covering the scope, as N
--   admissions in the current second (`counted_second`) and N requests in
--   flight, only if each limit has room for them in the window admission
--   reads during that second. The lease admits only until that second ends
--   (`valid_until`), so each request it admits falls in the second it was
--   counted in, as if admitted on the row then. No second ahead of the clock
--   is written: the ring holds 61 seconds, and writing second S+1 would clear
--   S-60, still in the window during S.
-- * A reservation admitted under a lease names it (`rate_lease_id`), the
--   ticket it used (`rate_lease_ticket`, 0 to N-1, one reservation per ticket
--   by a unique index, 0146) and when it was admitted (`rate_admitted_at`;
--   `created_at` stays the insert's time, which end-user limits count by).
--   None of them ever changes. The counter trigger skips the reservation for
--   the lease's policies only when all of these hold up; anything else is
--   counted, so a mistake can only count more. Leaving flight it is released
--   on the shard rows as any other.
-- * Returning the lease (once its second has passed, on shutdown, or by the
--   worker for a crashed process) counts the reservations made under it and
--   gives back the rest: N less those, from the counted second (while the
--   ring still holds it) and from in flight. The worker's return is what
--   frees a crashed process's in-flight count; its admissions leave the
--   window on their own.
--
-- Every transaction that admits under a lease takes the lease's advisory
-- lock shared and checks it is still usable (the trigger takes it too); a
-- return takes it exclusively, so it sees every reservation made under the
-- lease and no new one follows. A return then updates counter rows without
-- their policy's lock: it only lowers what they count, as a release does.
CREATE TABLE gateway.rate_limit_lease (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    team_id UUID,
    service_account_id UUID,
    -- The process holding it: a fresh id each time a gateway starts.
    holder UUID NOT NULL,
    -- The policies it counts on, in id order, and their limits when it was
    -- taken (-1 for none): it is not used once any of them changes.
    policy_ids UUID[] NOT NULL,
    requests_per_minute INTEGER[] NOT NULL,
    max_concurrent INTEGER[] NOT NULL,
    admissions INTEGER NOT NULL CHECK (admissions > 0),
    counted_second BIGINT NOT NULL,
    valid_until TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    returned_at TIMESTAMPTZ,
    return_reason TEXT CHECK (return_reason IN ('ended', 'shutdown', 'expired')),
    -- Reservations made under it, counted when it was returned.
    returned_admitted INTEGER,
    CONSTRAINT rate_limit_lease_policies_ck CHECK (
        cardinality(policy_ids) > 0
        AND cardinality(requests_per_minute) = cardinality(policy_ids)
        AND cardinality(max_concurrent) = cardinality(policy_ids)),
    -- It admits within the one second it is counted in.
    CONSTRAINT rate_limit_lease_second_ck CHECK (
        valid_until = to_timestamp(counted_second + 1)
        AND created_at >= to_timestamp(counted_second)
        AND created_at < valid_until),
    CONSTRAINT rate_limit_lease_returned_ck CHECK (
        (returned_at IS NULL AND return_reason IS NULL AND returned_admitted IS NULL)
        OR (returned_at IS NOT NULL AND return_reason IS NOT NULL
            AND returned_admitted IS NOT NULL))
);
CREATE INDEX rate_limit_lease_active_idx ON gateway.rate_limit_lease (organization_id)
    WHERE returned_at IS NULL;
CREATE INDEX rate_limit_lease_returned_idx ON gateway.rate_limit_lease (returned_at)
    WHERE returned_at IS NOT NULL;

-- No foreign key: one would take a key-share lock on the lease's row for
-- every reservation, hundreds a second on one row. The lease is checked
-- under its advisory lock instead. Nullable columns without a default and a
-- NOT VALID check change only the catalog (existing rows have none of them);
-- the unique index on (rate_lease_id, rate_lease_ticket), which caps a lease
-- at N in the database, is built concurrently by 0146.
ALTER TABLE gateway.accounting_reservation
    ADD COLUMN rate_lease_id UUID,
    ADD COLUMN rate_lease_ticket INTEGER,
    ADD COLUMN rate_admitted_at TIMESTAMPTZ,
    ADD CONSTRAINT accounting_reservation_rate_lease_ck CHECK (
        (rate_lease_id IS NULL AND rate_lease_ticket IS NULL AND rate_admitted_at IS NULL)
        OR (rate_lease_id IS NOT NULL AND rate_lease_ticket >= 0
            AND rate_admitted_at IS NOT NULL)) NOT VALID;

CREATE FUNCTION gateway.prevent_reservation_rate_lease_change()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.rate_lease_id IS DISTINCT FROM NEW.rate_lease_id
       OR OLD.rate_lease_ticket IS DISTINCT FROM NEW.rate_lease_ticket
       OR OLD.rate_admitted_at IS DISTINCT FROM NEW.rate_admitted_at THEN
        RAISE EXCEPTION 'a reservation''s rate-limit lease is immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER accounting_reservation_rate_lease_immutable
    BEFORE UPDATE OF rate_lease_id, rate_lease_ticket, rate_admitted_at
    ON gateway.accounting_reservation
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_reservation_rate_lease_change();

-- count_rate_limits (0121), leaving out the policies in `skip`.
CREATE FUNCTION gateway.count_rate_limits_except(
    organization UUID, team UUID, account UUID, admitted INTEGER, at TIMESTAMPTZ,
    flight INTEGER, release_shard SMALLINT, skip UUID[])
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    policy UUID;
    second BIGINT := floor(extract(epoch FROM at))::BIGINT;
BEGIN
    FOR policy IN
        SELECT p.id FROM gateway.rate_limit_policy p
        WHERE p.organization_id = organization
          AND (p.scope_kind = 'organization'
               OR (p.scope_kind = 'team' AND p.team_id = team)
               OR (p.scope_kind = 'service-account' AND p.service_account_id = account))
          AND NOT (p.id = ANY (skip))
        ORDER BY p.id
    LOOP
        IF admitted > 0 THEN
            UPDATE gateway.rate_limit_counter c
            SET (slots, last_second) = (
                    SELECT a.slots, a.last_second
                    FROM gateway.rate_limit_slots_add(c.slots, c.last_second, second, admitted) a),
                in_flight = c.in_flight + flight
            WHERE c.policy_id = policy;
        ELSIF flight <> 0 THEN
            INSERT INTO gateway.rate_limit_release AS r (policy_id, shard, released)
            VALUES (policy, release_shard, -flight)
            ON CONFLICT (policy_id, shard) DO UPDATE SET released = r.released - flight;
        END IF;
    END LOOP;
END;
$$;

-- Whether a lease counts a reservation (and the counter trigger skipped it):
-- under the lease, for its scope, on a ticket it has, admitted within its
-- second. The return, the recount and the load harness all use this test.
CREATE FUNCTION gateway.rate_lease_counts(
    lease_organization UUID, lease_team UUID, lease_account UUID, admissions INTEGER,
    lease_created TIMESTAMPTZ, valid_until TIMESTAMPTZ,
    organization UUID, team UUID, account UUID, batch UUID, ticket INTEGER,
    admitted_at TIMESTAMPTZ)
RETURNS BOOLEAN
LANGUAGE sql IMMUTABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT batch IS NULL AND organization = lease_organization
       AND team IS NOT DISTINCT FROM lease_team
       AND account IS NOT DISTINCT FROM lease_account
       AND ticket >= 0 AND ticket < admissions
       AND admitted_at >= lease_created AND admitted_at < valid_until
$$;

-- As 0121, except that a reservation a lease counts is not counted on the
-- lease's policies: the lease counted it when it was taken. The lease must
-- still be active, and the admission stamped within this transaction
-- (between its start and now), so a reservation cannot be backdated into an
-- old lease. The lease's lock, shared, orders the insert against a return.
CREATE OR REPLACE FUNCTION gateway.count_rate_limited_reservation()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    was BOOLEAN;
    now_in BOOLEAN;
    skip UUID[] := '{}';
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.batch_id IS NULL THEN
            IF NEW.rate_lease_id IS NOT NULL
               AND NEW.rate_admitted_at BETWEEN now() AND clock_timestamp() THEN
                PERFORM pg_advisory_xact_lock_shared(1918989413, hashtext(NEW.rate_lease_id::text));
                SELECT l.policy_ids INTO skip
                FROM gateway.rate_limit_lease l
                WHERE l.id = NEW.rate_lease_id AND l.returned_at IS NULL
                  AND gateway.rate_lease_counts(
                      l.organization_id, l.team_id, l.service_account_id, l.admissions,
                      l.created_at, l.valid_until, NEW.organization_id, NEW.team_id,
                      NEW.service_account_id, NEW.batch_id, NEW.rate_lease_ticket,
                      NEW.rate_admitted_at);
                skip := COALESCE(skip, '{}');
            END IF;
            PERFORM gateway.count_rate_limits_except(
                NEW.organization_id, NEW.team_id, NEW.service_account_id, 1, NEW.created_at,
                CASE WHEN NEW.state IN ('reserved', 'claimed') THEN 1 ELSE 0 END, 0::SMALLINT,
                skip);
        END IF;
        RETURN NULL;
    ELSIF TG_OP = 'UPDATE' THEN
        IF NEW.batch_id IS NOT NULL THEN
            RETURN NULL;
        END IF;
        was := OLD.state IN ('reserved', 'claimed');
        now_in := NEW.state IN ('reserved', 'claimed');
        IF was <> now_in THEN
            PERFORM gateway.count_rate_limits(
                NEW.organization_id, NEW.team_id, NEW.service_account_id, 0, NEW.created_at,
                CASE WHEN now_in THEN 1 ELSE -1 END, (get_byte(uuid_send(NEW.id), 15) % 16)::SMALLINT);
        END IF;
        RETURN NULL;
    ELSE
        IF OLD.batch_id IS NULL AND OLD.state IN ('reserved', 'claimed') THEN
            PERFORM gateway.count_rate_limits(
                OLD.organization_id, OLD.team_id, OLD.service_account_id, 0, OLD.created_at, -1,
                (get_byte(uuid_send(OLD.id), 15) % 16)::SMALLINT);
        END IF;
        RETURN NULL;
    END IF;
END;
$$;

-- The recount of 0123, with active leases: a reservation an active lease
-- covering the policy counts is counted as the lease counts it (its N at the
-- counted second; in flight, N less the lease's reservations that have left
-- flight) rather than by itself. A reservation the trigger counted although
-- it names a lease (stamped outside its transaction) is counted twice here,
-- never less. Ordered against lease returns by the leases' locks (below).
CREATE OR REPLACE FUNCTION gateway.seed_rate_limit_counter(policy UUID)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    org UUID;
    kind TEXT;
    team UUID;
    account UUID;
    top BIGINT := floor(extract(epoch FROM clock_timestamp()))::BIGINT;
    counted INTEGER[];
    flight BIGINT;
BEGIN
    SELECT p.organization_id, p.scope_kind, p.team_id, p.service_account_id
    INTO org, kind, team, account
    FROM gateway.rate_limit_policy p WHERE p.id = policy;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    -- Each active lease naming the policy is locked shared, in id order,
    -- before the counter row, as a return locks a lease and then the
    -- counter. So a return either committed before the reads below (and
    -- they see the lease returned) or waits for this transaction and then
    -- corrects the counter it writes. Locking the counter row alone is not
    -- enough: a policy deleted and created again has no counter row to lock,
    -- and a return committing between the reads and the insert would
    -- update nothing while the recount kept the lease's unused admissions.
    -- No new lease can name the policy meanwhile: taking one locks the
    -- policy's row, which every caller of this holds.
    PERFORM pg_advisory_xact_lock_shared(1918989413, hashtext(l.id::text))
    FROM (SELECT l.id FROM gateway.rate_limit_lease l
          WHERE l.organization_id = org AND l.returned_at IS NULL
            AND policy = ANY (l.policy_ids)
          ORDER BY l.id) l;
    PERFORM 1 FROM gateway.rate_limit_counter c WHERE c.policy_id = policy FOR UPDATE;
    WITH active AS (
        SELECT l.* FROM gateway.rate_limit_lease l
        WHERE l.organization_id = org AND l.returned_at IS NULL AND policy = ANY (l.policy_ids)
    ), admitted AS (
        SELECT floor(extract(epoch FROM r.created_at))::BIGINT AS second, 1 AS n
        FROM gateway.accounting_reservation r
        WHERE r.organization_id = org AND r.batch_id IS NULL
          AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
               OR (kind = 'service-account' AND r.service_account_id = account))
          AND r.created_at >= to_timestamp(top - 60)
          AND NOT EXISTS (
              SELECT 1 FROM active a
              WHERE a.id = r.rate_lease_id
                AND gateway.rate_lease_counts(
                    a.organization_id, a.team_id, a.service_account_id, a.admissions,
                    a.created_at, a.valid_until, r.organization_id, r.team_id,
                    r.service_account_id, r.batch_id, r.rate_lease_ticket, r.rate_admitted_at))
        UNION ALL
        SELECT floor(extract(epoch FROM b.created_at))::BIGINT, 1
        FROM gateway.batch b
        WHERE b.organization_id = org
          AND (kind = 'organization' OR (kind = 'team' AND b.team_id = team)
               OR (kind = 'service-account' AND b.service_account_id = account))
          AND b.created_at >= to_timestamp(top - 60)
        UNION ALL
        SELECT a.counted_second, a.admissions FROM active a
    )
    SELECT array_agg(
               (SELECT COALESCE(sum(a.n), 0) FROM admitted a WHERE a.second = g.second)::INTEGER
               ORDER BY g.second % 61)
    INTO counted
    FROM generate_series(top - 60, top) AS g(second);
    -- One statement, one snapshot: the reservations in flight, the active
    -- leases' share and the releases already written.
    WITH active AS (
        SELECT l.* FROM gateway.rate_limit_lease l
        WHERE l.organization_id = org AND l.returned_at IS NULL AND policy = ANY (l.policy_ids)
    )
    SELECT (SELECT count(*)
            FROM gateway.accounting_reservation r
            WHERE r.organization_id = org AND r.batch_id IS NULL
              AND r.state IN ('reserved', 'claimed')
              AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
                   OR (kind = 'service-account' AND r.service_account_id = account))
              AND NOT EXISTS (
                  SELECT 1 FROM active a
                  WHERE a.id = r.rate_lease_id
                    AND gateway.rate_lease_counts(
                        a.organization_id, a.team_id, a.service_account_id, a.admissions,
                        a.created_at, a.valid_until, r.organization_id, r.team_id,
                        r.service_account_id, r.batch_id, r.rate_lease_ticket,
                        r.rate_admitted_at)))
         + (SELECT COALESCE(sum(a.admissions - (
                SELECT count(*) FROM gateway.accounting_reservation r
                WHERE r.rate_lease_id = a.id AND r.state NOT IN ('reserved', 'claimed')
                  AND gateway.rate_lease_counts(
                      a.organization_id, a.team_id, a.service_account_id, a.admissions,
                      a.created_at, a.valid_until, r.organization_id, r.team_id,
                      r.service_account_id, r.batch_id, r.rate_lease_ticket,
                      r.rate_admitted_at))), 0)
            FROM active a)
         + (SELECT COALESCE(sum(x.released), 0)
            FROM gateway.rate_limit_release x WHERE x.policy_id = policy)
    INTO flight;
    INSERT INTO gateway.rate_limit_counter (policy_id, in_flight, last_second, slots)
    VALUES (policy, flight, top, counted)
    ON CONFLICT (policy_id) DO UPDATE
    SET in_flight = EXCLUDED.in_flight, last_second = EXCLUDED.last_second,
        slots = EXCLUDED.slots;
END;
$$;

-- Taking a lease, step one: admission's locks and counters
-- (lock_rate_limit_counters), and how many other processes hold an active
-- lease in the organization, which the lease's share is divided by.
CREATE FUNCTION gateway.lock_rate_limit_lease_counters(
    organization UUID, team UUID, account UUID, holder UUID)
RETURNS TABLE (requests_per_minute INTEGER, max_concurrent INTEGER, in_flight BIGINT,
               last_second BIGINT, slots INTEGER[], now_epoch DOUBLE PRECISION, holders BIGINT)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    RETURN QUERY
    SELECT c.requests_per_minute, c.max_concurrent, c.in_flight, c.last_second, c.slots,
           c.now_epoch,
           (SELECT count(DISTINCT l.holder) FROM gateway.rate_limit_lease l
            WHERE l.organization_id = organization AND l.returned_at IS NULL
              AND l.valid_until > clock_timestamp() AND l.holder <> lock_rate_limit_lease_counters.holder)
    FROM gateway.lock_rate_limit_counters(organization, team, account) c;
END;
$$;

-- Step two, in the same transaction, the policies still locked: the lease of
-- `admissions`, counted in the current second on each policy covering the
-- scope. Granted only if that second is `room_second`, the one the room was
-- read in (step one), and at least `min_life` seconds of it are left. Returns
-- no row when no policy covers the scope; otherwise whether it was granted
-- and the seconds left in the current second.
CREATE FUNCTION gateway.grant_rate_limit_lease(
    lease UUID, holder UUID, organization UUID, team UUID, account UUID, admissions INTEGER,
    room_second BIGINT, min_life DOUBLE PRECISION)
RETURNS TABLE (granted BOOLEAN, seconds_left DOUBLE PRECISION)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    ids UUID[];
    rpms INTEGER[];
    concs INTEGER[];
    policy UUID;
    at TIMESTAMPTZ;
    second BIGINT;
    left_in_second DOUBLE PRECISION;
BEGIN
    IF admissions IS NULL OR admissions <= 0 THEN
        RETURN;
    END IF;
    -- The locks admission takes, already held by this transaction.
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    SELECT array_agg(x.id ORDER BY x.id),
           array_agg(COALESCE(x.requests_per_minute, -1) ORDER BY x.id),
           array_agg(COALESCE(x.max_concurrent, -1) ORDER BY x.id)
    INTO ids, rpms, concs
    FROM (SELECT rp.id, rp.requests_per_minute, rp.max_concurrent
          FROM gateway.rate_limit_policy rp
          WHERE rp.organization_id = organization
            AND (rp.scope_kind = 'organization'
                 OR (rp.scope_kind = 'team' AND rp.team_id = team)
                 OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account))
          ORDER BY rp.id
          FOR UPDATE OF rp) x;
    IF ids IS NULL THEN
        RETURN;
    END IF;
    FOREACH policy IN ARRAY ids LOOP
        IF NOT EXISTS (SELECT 1 FROM gateway.rate_limit_counter c WHERE c.policy_id = policy) THEN
            PERFORM gateway.seed_rate_limit_counter(policy);
        END IF;
    END LOOP;
    at := clock_timestamp();
    second := floor(extract(epoch FROM at))::BIGINT;
    left_in_second := (second + 1)::DOUBLE PRECISION - extract(epoch FROM at)::DOUBLE PRECISION;
    -- The room was read for the window admission reads during `room_second`;
    -- in another second it would be a different window.
    IF second <> room_second THEN
        RETURN QUERY SELECT false, 0::DOUBLE PRECISION;
        RETURN;
    END IF;
    IF left_in_second < min_life THEN
        RETURN QUERY SELECT false, left_in_second;
        RETURN;
    END IF;
    INSERT INTO gateway.rate_limit_lease
        (id, organization_id, team_id, service_account_id, holder, policy_ids,
         requests_per_minute, max_concurrent, admissions, counted_second, valid_until,
         created_at)
    VALUES (lease, organization, team, account, holder, ids, rpms, concs, admissions, second,
            to_timestamp(second + 1), at);
    FOREACH policy IN ARRAY ids LOOP
        UPDATE gateway.rate_limit_counter c
        SET (slots, last_second) = (
                SELECT a.slots, a.last_second
                FROM gateway.rate_limit_slots_add(c.slots, c.last_second, second, admissions) a),
            in_flight = c.in_flight + admissions
        WHERE c.policy_id = policy;
    END LOOP;
    RETURN QUERY SELECT true, left_in_second;
END;
$$;

-- Admitting under a lease, in the reservation's transaction: the
-- organization's lock and the lease's advisory lock shared (until the
-- transaction ends), and the time of admission if the lease is still usable:
-- not returned, not past its end, for this scope, `ticket` one of its N, and
-- the policies covering the scope exactly those it was taken under, with the
-- same limits. NULL otherwise.
CREATE FUNCTION gateway.use_rate_limit_lease(
    lease UUID, ticket INTEGER, organization UUID, team UUID, account UUID)
RETURNS TIMESTAMPTZ
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    at TIMESTAMPTZ;
    l RECORD;
    ids UUID[];
    rpms INTEGER[];
    concs INTEGER[];
BEGIN
    -- The organization first, as admission locks it (the insert's foreign
    -- key would too, later): a policy write holds it for update, so its
    -- recount and this admission are ordered whichever trigger runs first.
    -- Before the lease's lock, which a return holds while it waits on a
    -- counter row a policy write may hold.
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    PERFORM pg_advisory_xact_lock_shared(1918989413, hashtext(lease::text));
    at := clock_timestamp();
    SELECT x.policy_ids, x.requests_per_minute, x.max_concurrent INTO l
    FROM gateway.rate_limit_lease x
    WHERE x.id = lease AND x.returned_at IS NULL AND x.organization_id = organization
      AND x.team_id IS NOT DISTINCT FROM team AND x.service_account_id IS NOT DISTINCT FROM account
      AND ticket >= 0 AND ticket < x.admissions
      AND at >= x.created_at AND at < x.valid_until;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    SELECT array_agg(rp.id ORDER BY rp.id),
           array_agg(COALESCE(rp.requests_per_minute, -1) ORDER BY rp.id),
           array_agg(COALESCE(rp.max_concurrent, -1) ORDER BY rp.id)
    INTO ids, rpms, concs
    FROM gateway.rate_limit_policy rp
    WHERE rp.organization_id = organization
      AND (rp.scope_kind = 'organization'
           OR (rp.scope_kind = 'team' AND rp.team_id = team)
           OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account));
    IF ids IS DISTINCT FROM l.policy_ids OR rpms IS DISTINCT FROM l.requests_per_minute
       OR concs IS DISTINCT FROM l.max_concurrent THEN
        RETURN NULL;
    END IF;
    RETURN at;
END;
$$;

-- Returns a lease, by its holder or the worker: waits for every transaction
-- admitting under it, counts the reservations it admitted (at most N, one
-- per ticket), and gives back the rest of its N on each policy's counter.
-- Returns that count, or NULL if it was already returned.
CREATE FUNCTION gateway.return_rate_limit_lease(lease UUID, reason TEXT)
RETURNS INTEGER
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    l RECORD;
    used INTEGER;
    change INTEGER;
    slot INTEGER;
    policy UUID;
BEGIN
    PERFORM pg_advisory_xact_lock(1918989413, hashtext(lease::text));
    SELECT x.* INTO l FROM gateway.rate_limit_lease x
    WHERE x.id = lease AND x.returned_at IS NULL FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    SELECT count(*) INTO used
    FROM gateway.accounting_reservation r
    WHERE r.rate_lease_id = lease
      AND gateway.rate_lease_counts(
          l.organization_id, l.team_id, l.service_account_id, l.admissions, l.created_at,
          l.valid_until, r.organization_id, r.team_id, r.service_account_id, r.batch_id,
          r.rate_lease_ticket, r.rate_admitted_at);
    change := used - l.admissions;
    slot := (l.counted_second % 61)::INTEGER + 1;
    IF change <> 0 THEN
        FOREACH policy IN ARRAY l.policy_ids LOOP
            UPDATE gateway.rate_limit_counter c
            SET in_flight = c.in_flight + change,
                slots[slot] = CASE
                    WHEN l.counted_second <= c.last_second AND l.counted_second > c.last_second - 61
                    THEN GREATEST(0, c.slots[slot] + change)
                    ELSE c.slots[slot] END
            WHERE c.policy_id = policy;
        END LOOP;
    END IF;
    UPDATE gateway.rate_limit_lease x
    SET returned_at = clock_timestamp(), return_reason = reason, returned_admitted = used
    WHERE x.id = lease;
    RETURN used;
END;
$$;

REVOKE ALL ON FUNCTION gateway.count_rate_limits_except(
    UUID, UUID, UUID, INTEGER, TIMESTAMPTZ, INTEGER, SMALLINT, UUID[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.lock_rate_limit_lease_counters(UUID, UUID, UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.grant_rate_limit_lease(
    UUID, UUID, UUID, UUID, UUID, INTEGER, BIGINT, DOUBLE PRECISION) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.use_rate_limit_lease(UUID, INTEGER, UUID, UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.return_rate_limit_lease(UUID, TEXT) FROM PUBLIC;
