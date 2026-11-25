-- Rate-limit admission without scanning (D-030, load and resilience
-- testing). Admission used to find the limit-th newest reservation of the
-- last minute and the limit-th one in flight inside the policy's lock, so
-- one organization's serial section grew with its own traffic. Each policy
-- now has a counter row, kept by triggers in the same transaction as the
-- reservation (or batch) it counts, so admission reads a fixed-size row:
--
-- * `slots`: admissions per second over the last 61 seconds, a ring indexed
--   by the second (epoch seconds mod 61), `last_second` the newest second
--   written. Admission counts every second that overlaps the minute before
--   now, so it never counts fewer than the reservations made in that
--   minute (at most one second's more).
-- * `in_flight`: single requests (not batch lines) reserved or claimed,
--   counted until they leave those states, so one abandoned by a crash
--   holds its slot until the expiry worker closes it. Admission adds to
--   the counter row; a request leaving flight adds to one of 16
--   `rate_limit_release` rows instead (chosen by its reservation id), so
--   settlements never wait on the row admission writes or on each other
--   much. In flight is the counter less what the releases have counted;
--   a release not yet committed is not seen, which only ever counts more.
--
-- Counters are written in policy id order, the order admission locks the
-- policies, so two writers cannot deadlock.
CREATE TABLE gateway.rate_limit_counter (
    policy_id UUID PRIMARY KEY
        REFERENCES gateway.rate_limit_policy (id) ON DELETE CASCADE,
    in_flight BIGINT NOT NULL DEFAULT 0,
    last_second BIGINT NOT NULL DEFAULT 0,
    slots INTEGER[] NOT NULL DEFAULT array_fill(0, ARRAY[61]),
    CONSTRAINT rate_limit_counter_slots_ck CHECK (cardinality(slots) = 61)
);

-- No foreign key: checking one on a release's first write would take a
-- key-share lock on the policy's row, which admission holds for update while
-- it waits on budget rows that settlements hold, so the two would deadlock.
-- A policy's release rows go with its counter instead.
CREATE TABLE gateway.rate_limit_release (
    policy_id UUID NOT NULL,
    shard SMALLINT NOT NULL CHECK (shard BETWEEN 0 AND 15),
    released BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (policy_id, shard)
);

CREATE FUNCTION gateway.delete_rate_limit_releases()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    DELETE FROM gateway.rate_limit_release r WHERE r.policy_id = OLD.policy_id;
    RETURN NULL;
END;
$$;
CREATE TRIGGER rate_limit_counter_releases_delete
    AFTER DELETE ON gateway.rate_limit_counter
    FOR EACH ROW EXECUTE FUNCTION gateway.delete_rate_limit_releases();

-- `slots` with `n` admissions added at second `at`: seconds between the
-- newest written and `at` are cleared first. An admission older than the
-- ring is not kept (it is past every window).
CREATE FUNCTION gateway.rate_limit_slots_add(
    current_slots INTEGER[], newest BIGINT, at BIGINT, n INTEGER,
    OUT slots INTEGER[], OUT last_second BIGINT)
LANGUAGE plpgsql IMMUTABLE SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    second BIGINT;
BEGIN
    slots := current_slots;
    last_second := newest;
    IF at > last_second THEN
        IF at - last_second >= 61 THEN
            slots := array_fill(0, ARRAY[61]);
        ELSE
            FOR second IN last_second + 1 .. at LOOP
                slots[(second % 61) + 1] := 0;
            END LOOP;
        END IF;
        last_second := at;
    END IF;
    IF at > last_second - 61 THEN
        slots[(at % 61) + 1] := slots[(at % 61) + 1] + n;
    END IF;
END;
$$;

-- Counts `admitted` admissions at `at` and `flight` requests in flight for
-- every policy covering the scope, in policy id order: an admission (and
-- the flight it starts) on the counter row, any other change of flight on
-- release row `release_shard`.
CREATE FUNCTION gateway.count_rate_limits(
    organization UUID, team UUID, account UUID, admitted INTEGER, at TIMESTAMPTZ,
    flight INTEGER, release_shard SMALLINT)
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

-- A reservation counts as an admission when it is written (failover
-- attempts included, as the scan counted them) and is in flight while
-- reserved or claimed. Batch lines are not counted: their batch is.
CREATE FUNCTION gateway.count_rate_limited_reservation()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    was BOOLEAN;
    now_in BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.batch_id IS NULL THEN
            PERFORM gateway.count_rate_limits(
                NEW.organization_id, NEW.team_id, NEW.service_account_id, 1, NEW.created_at,
                CASE WHEN NEW.state IN ('reserved', 'claimed') THEN 1 ELSE 0 END, 0::SMALLINT);
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

CREATE FUNCTION gateway.count_rate_limited_batch()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    PERFORM gateway.count_rate_limits(
        NEW.organization_id, NEW.team_id, NEW.service_account_id, 1, NEW.created_at, 0,
        0::SMALLINT);
    RETURN NULL;
END;
$$;

-- (Re)counts a policy's counter from the reservations and batches
-- themselves. Exact only while no admission to the scope can commit
-- meanwhile: a policy write first locks its organization's row, which
-- every reservation and batch insert takes a key-share lock on. Releases
-- go on meanwhile: the release rows are cleared first, waiting for any
-- release that has written one, so each release is either seen as such by
-- the count below or kept on a release row written after the clearing.
CREATE FUNCTION gateway.seed_rate_limit_counter(policy UUID)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    org UUID;
    kind TEXT;
    team UUID;
    account UUID;
    now_second BIGINT := floor(extract(epoch FROM clock_timestamp()))::BIGINT;
    counted INTEGER[];
    flight BIGINT;
BEGIN
    SELECT p.organization_id, p.scope_kind, p.team_id, p.service_account_id
    INTO org, kind, team, account
    FROM gateway.rate_limit_policy p WHERE p.id = policy;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    DELETE FROM gateway.rate_limit_release r WHERE r.policy_id = policy;
    WITH admitted AS (
        SELECT floor(extract(epoch FROM r.created_at))::BIGINT AS second
        FROM gateway.accounting_reservation r
        WHERE r.organization_id = org AND r.batch_id IS NULL
          AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
               OR (kind = 'service-account' AND r.service_account_id = account))
          AND r.created_at >= to_timestamp(now_second - 60)
        UNION ALL
        SELECT floor(extract(epoch FROM b.created_at))::BIGINT
        FROM gateway.batch b
        WHERE b.organization_id = org
          AND (kind = 'organization' OR (kind = 'team' AND b.team_id = team)
               OR (kind = 'service-account' AND b.service_account_id = account))
          AND b.created_at >= to_timestamp(now_second - 60)
    )
    SELECT array_agg(
               (SELECT count(*) FROM admitted a WHERE a.second = g.second)::INTEGER
               ORDER BY g.second % 61)
    INTO counted
    FROM generate_series(now_second - 60, now_second) AS g(second);
    SELECT count(*) INTO flight
    FROM gateway.accounting_reservation r
    WHERE r.organization_id = org AND r.batch_id IS NULL
      AND r.state IN ('reserved', 'claimed')
      AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
           OR (kind = 'service-account' AND r.service_account_id = account));
    INSERT INTO gateway.rate_limit_counter (policy_id, in_flight, last_second, slots)
    VALUES (policy, flight, now_second, counted)
    ON CONFLICT (policy_id) DO UPDATE
    SET in_flight = EXCLUDED.in_flight, last_second = EXCLUDED.last_second,
        slots = EXCLUDED.slots;
END;
$$;

-- Admission: locks the organization (as the reservation insert's foreign
-- key will) and then the scope's policies in id order, as before, and
-- returns each policy's limits with its counter, seeding a counter a
-- policy lacks (one written by a process from before this migration).
CREATE FUNCTION gateway.lock_rate_limit_counters(organization UUID, team UUID, account UUID)
RETURNS TABLE (scope_kind TEXT, requests_per_minute INTEGER, max_concurrent INTEGER,
               in_flight BIGINT, last_second BIGINT, slots INTEGER[], now_epoch DOUBLE PRECISION)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    p RECORD;
BEGIN
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    FOR p IN
        SELECT rp.id, rp.scope_kind, rp.requests_per_minute, rp.max_concurrent
        FROM gateway.rate_limit_policy rp
        WHERE rp.organization_id = organization
          AND (rp.scope_kind = 'organization'
               OR (rp.scope_kind = 'team' AND rp.team_id = team)
               OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account))
        ORDER BY rp.id
        FOR UPDATE OF rp
    LOOP
        IF NOT EXISTS (SELECT 1 FROM gateway.rate_limit_counter c WHERE c.policy_id = p.id) THEN
            PERFORM gateway.seed_rate_limit_counter(p.id);
        END IF;
        RETURN QUERY
        SELECT p.scope_kind, p.requests_per_minute, p.max_concurrent,
               GREATEST(0, c.in_flight - COALESCE(
                   (SELECT sum(r.released) FROM gateway.rate_limit_release r
                    WHERE r.policy_id = p.id), 0))::BIGINT,
               c.last_second, c.slots, extract(epoch FROM clock_timestamp())::DOUBLE PRECISION
        FROM gateway.rate_limit_counter c WHERE c.policy_id = p.id;
    END LOOP;
END;
$$;

REVOKE ALL ON FUNCTION gateway.count_rate_limits(UUID, UUID, UUID, INTEGER, TIMESTAMPTZ, INTEGER, SMALLINT)
    FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.seed_rate_limit_counter(UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.lock_rate_limit_counters(UUID, UUID, UUID) FROM PUBLIC;

-- The triggers take the tables' share-row-exclusive lock, so no
-- reservation or batch is written between them and the seeding below.
CREATE TRIGGER accounting_reservation_rate_limit_count
    AFTER INSERT OR DELETE OR UPDATE OF state ON gateway.accounting_reservation
    FOR EACH ROW EXECUTE FUNCTION gateway.count_rate_limited_reservation();
CREATE TRIGGER batch_rate_limit_count
    AFTER INSERT ON gateway.batch
    FOR EACH ROW EXECUTE FUNCTION gateway.count_rate_limited_batch();

SELECT gateway.seed_rate_limit_counter(id) FROM gateway.rate_limit_policy;
