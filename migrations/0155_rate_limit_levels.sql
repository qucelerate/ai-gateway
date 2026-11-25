-- Rate limits at every level budgets have, in the same way.
--
-- * A policy may cover one API key (`api-key`), as a budget may: a request
--   of that key is checked against the key's policy as well as its service
--   account's, its team's and the organization's. Keys are revoked, never
--   deleted, so a revoked key keeps its policy.
-- * A policy may set `tokens_per_minute`, as an end user's plan may (0115):
--   a request counts its token bound (input bound, output cap and reasoning
--   allowance, `token_bound`) from its admission while it runs, and what its
--   usage reported once it settles or is released (nothing, if released
--   without usage). Admission lowers a request's output cap to the tokens
--   the window has left, and refuses one whose input alone is more than the
--   limit.
-- * An end user's plan may set `max_concurrent`, the user's requests in
--   flight (0121's meaning: reserved or claimed).
--
-- Counting stays on the counter rows of 0121, kept by the same triggers in
-- the reservation's own transaction:
--
-- * A reservation now names its API key (`api_key_id`, written at insert
--   and never changed), so the triggers count it on the key's policy; a
--   batch does too.
-- * Tokens: a second ring like the admissions' (`token_slots`, indexed by
--   the epoch second mod 61, `token_last_second` the newest written), kept
--   only on a policy that limits tokens. Admission adds the request's bound
--   at the second it is counted in (`tokens_counted_at`, or `created_at`
--   when NULL); a request leaving for `settled` or `released` adds the
--   difference between what it reported and its bound to a ring on its
--   release shard (0121), at the same second, so settlements never wait on
--   the row admission writes. The window's tokens are the counter's ring
--   plus every release shard's ring, second by second. A failover attempt
--   (D-023) is the request it continues: it is counted at that request's
--   second (`tokens_counted_at`) and holds no more tokens than the attempt
--   it replaces, which leaves at the same moment, so a failover never needs
--   room of its own.
--
-- Rate-limit leases (0145) stay exact by not covering the new limits: a
-- lease is never granted for a scope a tokens-per-minute policy covers, is
-- not used once one does (by older processes either), and a request of a
-- key with a policy of its own is never admitted under a lease (it is admitted on the policies' rows, which
-- lock and count the key's policy with the others).
--
-- Lock order is unchanged: the organization's row (key share), then the
-- policies covering the request in id order (an API key's policy among
-- them), then the counter rows in the same order. Release shard rows are
-- written without a policy's lock, as before.

-- This step (6 of 7) locks only the small policy table: the columns the
-- functions below read were added by 0150 to 0154, one table at a time, and
-- the key's foreign key comes in 0156 (see 0150 for why). Replacing a
-- function locks no table.
SET LOCAL lock_timeout = '3s';

ALTER TABLE gateway.rate_limit_policy
    ADD COLUMN api_key_id UUID,
    ADD COLUMN tokens_per_minute INTEGER CHECK (tokens_per_minute BETWEEN 1 AND 1000000000),
    DROP CONSTRAINT rate_limit_policy_scope_kind_check,
    ADD CONSTRAINT rate_limit_policy_scope_kind_check
        CHECK (scope_kind IN ('organization', 'team', 'service-account', 'api-key')),
    DROP CONSTRAINT rate_limit_policy_scope_ck,
    ADD CONSTRAINT rate_limit_policy_scope_ck CHECK (
        (scope_kind = 'organization' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'team' AND team_id IS NOT NULL AND service_account_id IS NULL
            AND api_key_id IS NULL)
        OR (scope_kind = 'service-account' AND team_id IS NULL
            AND service_account_id IS NOT NULL AND api_key_id IS NULL)
        OR (scope_kind = 'api-key' AND team_id IS NULL AND service_account_id IS NULL
            AND api_key_id IS NOT NULL)
    ),
    DROP CONSTRAINT rate_limit_policy_limit_ck,
    ADD CONSTRAINT rate_limit_policy_limit_ck CHECK (
        requests_per_minute IS NOT NULL OR max_concurrent IS NOT NULL
        OR tokens_per_minute IS NOT NULL);

-- One policy per scope. The table holds a few rows per organization.
DROP INDEX gateway.rate_limit_policy_scope_uq;
CREATE UNIQUE INDEX rate_limit_policy_scope_uq ON gateway.rate_limit_policy (
    organization_id,
    COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(api_key_id, '00000000-0000-0000-0000-000000000000'::uuid)
);

-- rate_limit_slots_add (0121) for a token ring: `n` tokens (any sign) added
-- at second `at`, the seconds between the newest written and `at` cleared
-- first. Nothing is written for a second older than the ring, nor for 0.
CREATE FUNCTION gateway.rate_limit_token_slots_add(
    current_slots BIGINT[], newest BIGINT, at BIGINT, n BIGINT,
    OUT slots BIGINT[], OUT last_second BIGINT)
LANGUAGE plpgsql IMMUTABLE SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    second BIGINT;
BEGIN
    slots := current_slots;
    last_second := newest;
    IF n = 0 OR at <= newest - 61 THEN
        RETURN;
    END IF;
    IF at > last_second THEN
        IF at - last_second >= 61 THEN
            slots := array_fill(0::BIGINT, ARRAY[61]);
        ELSE
            FOR second IN last_second + 1 .. at LOOP
                slots[(second % 61) + 1] := 0;
            END LOOP;
        END IF;
        last_second := at;
    END IF;
    slots[(at % 61) + 1] := slots[(at % 61) + 1] + n;
END;
$$;

-- What a token ring holds for `second`, while it still holds it.
CREATE FUNCTION gateway.rate_limit_ring_at(slots BIGINT[], newest BIGINT, second BIGINT)
RETURNS BIGINT
LANGUAGE sql IMMUTABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT CASE WHEN second > newest OR second <= newest - 61 THEN 0
                ELSE COALESCE(slots[(second % 61) + 1], 0) END
$$;

-- Every second a token ring holds tokens for, with them: slot k holds the
-- second s in (newest - 61, newest] with s mod 61 = k. Set-returning and
-- without settings of its own, so a query using it is planned as one join
-- (it is inlined) rather than a lookup per second.
CREATE FUNCTION gateway.rate_limit_ring_seconds(slots BIGINT[], newest BIGINT)
RETURNS TABLE (second BIGINT, tokens BIGINT)
LANGUAGE sql IMMUTABLE AS $$
    SELECT newest - (((newest - (u.i - 1)) % 61) + 61) % 61, u.n
    FROM pg_catalog.unnest(slots) WITH ORDINALITY AS u(n, i)
    WHERE u.n <> 0
$$;

-- The tokens a reservation counts in a tokens-per-minute window, as an end
-- user's window counts them (persistence::end_users): its bound until it is
-- settled or released, then what its latest usage reported (a released one
-- that reported nothing used nothing; a settled one keeps its bound).
CREATE FUNCTION gateway.rate_limit_reservation_tokens(
    organization UUID, reservation UUID, state TEXT, bound BIGINT)
RETURNS BIGINT
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT CASE
        WHEN state IN ('settled', 'released') THEN COALESCE(
            (SELECT CASE WHEN a.input_tokens IS NOT NULL OR a.output_tokens IS NOT NULL
                         THEN COALESCE(a.input_tokens, 0) + COALESCE(a.output_tokens, 0)
                              + COALESCE(a.reasoning_tokens, 0) + COALESCE(a.other_tokens, 0)
                    END
             FROM gateway.accounting_usage a
             WHERE a.organization_id = rate_limit_reservation_tokens.organization
               AND a.reservation_id = rate_limit_reservation_tokens.reservation
             ORDER BY a.recorded_at DESC, a.id DESC
             LIMIT 1),
            CASE WHEN state = 'released' THEN 0 ELSE COALESCE(bound, 0) END)
        ELSE COALESCE(bound, 0)
    END::BIGINT
$$;

-- Counts on every policy covering the scope, in policy id order:
-- `admitted` admissions at `at` and the `flight` they start on the counter
-- row (except on the policies in `skip`, a lease's, which counted them), or
-- any other change of flight on release row `release_shard`; and on each
-- policy that limits tokens, `tokens` at `tokens_at`, on the counter row
-- with an admission and on the release row otherwise.
CREATE FUNCTION gateway.count_rate_limit_policies(
    organization UUID, team UUID, account UUID, api_key UUID, admitted INTEGER,
    at TIMESTAMPTZ, flight INTEGER, release_shard SMALLINT, skip UUID[], tokens BIGINT,
    tokens_at TIMESTAMPTZ)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    p RECORD;
    second BIGINT := floor(extract(epoch FROM at))::BIGINT;
    token_second BIGINT := floor(extract(epoch FROM tokens_at))::BIGINT;
    n BIGINT;
BEGIN
    FOR p IN
        SELECT rp.id, rp.tokens_per_minute FROM gateway.rate_limit_policy rp
        WHERE rp.organization_id = organization
          AND (rp.scope_kind = 'organization'
               OR (rp.scope_kind = 'team' AND rp.team_id = team)
               OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account)
               OR (rp.scope_kind = 'api-key' AND rp.api_key_id = api_key))
        ORDER BY rp.id
    LOOP
        n := CASE WHEN p.tokens_per_minute IS NOT NULL THEN COALESCE(tokens, 0) ELSE 0 END;
        IF admitted > 0 THEN
            IF NOT (p.id = ANY (skip)) THEN
                UPDATE gateway.rate_limit_counter c
                SET (slots, last_second) = (
                        SELECT a.slots, a.last_second
                        FROM gateway.rate_limit_slots_add(c.slots, c.last_second, second, admitted) a),
                    in_flight = c.in_flight + flight,
                    (token_slots, token_last_second) = (
                        SELECT t.slots, t.last_second
                        FROM gateway.rate_limit_token_slots_add(
                            c.token_slots, c.token_last_second, token_second, n) t)
                WHERE c.policy_id = p.id;
            ELSIF n <> 0 THEN
                UPDATE gateway.rate_limit_counter c
                SET (token_slots, token_last_second) = (
                        SELECT t.slots, t.last_second
                        FROM gateway.rate_limit_token_slots_add(
                            c.token_slots, c.token_last_second, token_second, n) t)
                WHERE c.policy_id = p.id;
            END IF;
        ELSIF flight <> 0 OR n <> 0 THEN
            INSERT INTO gateway.rate_limit_release AS r
                (policy_id, shard, released, token_slots, token_last_second)
            SELECT p.id, release_shard, -flight, t.slots, t.last_second
            FROM gateway.rate_limit_token_slots_add(
                array_fill(0::BIGINT, ARRAY[61]), 0, token_second, n) t
            ON CONFLICT (policy_id, shard) DO UPDATE
            SET released = r.released - flight,
                (token_slots, token_last_second) = (
                    SELECT t.slots, t.last_second
                    FROM gateway.rate_limit_token_slots_add(
                        r.token_slots, r.token_last_second, token_second, n) t);
        END IF;
    END LOOP;
END;
$$;

-- Whether a policy limiting tokens covers the scope.
CREATE FUNCTION gateway.rate_limit_tokens_covered(
    organization UUID, team UUID, account UUID, api_key UUID)
RETURNS BOOLEAN
LANGUAGE sql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    SELECT EXISTS (
        SELECT 1 FROM gateway.rate_limit_policy rp
        WHERE rp.organization_id = organization AND rp.tokens_per_minute IS NOT NULL
          AND (rp.scope_kind = 'organization'
               OR (rp.scope_kind = 'team' AND rp.team_id = team)
               OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account)
               OR (rp.scope_kind = 'api-key' AND rp.api_key_id = api_key)))
$$;

-- The same for a failover attempt (D-023), which is not admitted again:
-- with the organization's row key-share locked first, as admission locks
-- it, so no such policy is set or removed before the caller's transaction
-- ends. A function, so that a data plane's role can take the lock.
CREATE FUNCTION gateway.lock_rate_limit_tokens_covered(
    organization UUID, team UUID, account UUID, api_key UUID)
RETURNS BOOLEAN
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    RETURN gateway.rate_limit_tokens_covered(organization, team, account, api_key);
END;
$$;

-- As 0145, with the reservation's API key and tokens: its bound counted
-- when it is admitted, and the difference to what it reported when it is
-- settled or released (looked up only where a policy limits tokens). A
-- reservation deleted (by retention, long after its window) changes no
-- tokens.
CREATE OR REPLACE FUNCTION gateway.count_rate_limited_reservation()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    was BOOLEAN;
    now_in BOOLEAN;
    skip UUID[] := '{}';
    change BIGINT := 0;
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
            PERFORM gateway.count_rate_limit_policies(
                NEW.organization_id, NEW.team_id, NEW.service_account_id, NEW.api_key_id, 1,
                NEW.created_at, CASE WHEN NEW.state IN ('reserved', 'claimed') THEN 1 ELSE 0 END,
                0::SMALLINT, skip, COALESCE(NEW.token_bound, 0),
                COALESCE(NEW.tokens_counted_at, NEW.created_at));
        END IF;
        RETURN NULL;
    ELSIF TG_OP = 'UPDATE' THEN
        IF NEW.batch_id IS NOT NULL THEN
            RETURN NULL;
        END IF;
        was := OLD.state IN ('reserved', 'claimed');
        now_in := NEW.state IN ('reserved', 'claimed');
        IF OLD.state NOT IN ('settled', 'released') AND NEW.state IN ('settled', 'released')
           AND gateway.rate_limit_tokens_covered(
               NEW.organization_id, NEW.team_id, NEW.service_account_id, NEW.api_key_id) THEN
            change := gateway.rate_limit_reservation_tokens(
                NEW.organization_id, NEW.id, NEW.state, NEW.token_bound)
                - COALESCE(NEW.token_bound, 0);
        END IF;
        IF was <> now_in OR change <> 0 THEN
            PERFORM gateway.count_rate_limit_policies(
                NEW.organization_id, NEW.team_id, NEW.service_account_id, NEW.api_key_id, 0,
                NEW.created_at, CASE WHEN was = now_in THEN 0 WHEN now_in THEN 1 ELSE -1 END,
                (get_byte(uuid_send(NEW.id), 15) % 16)::SMALLINT, '{}', change,
                COALESCE(NEW.tokens_counted_at, NEW.created_at));
        END IF;
        RETURN NULL;
    ELSE
        IF OLD.batch_id IS NULL AND OLD.state IN ('reserved', 'claimed') THEN
            PERFORM gateway.count_rate_limit_policies(
                OLD.organization_id, OLD.team_id, OLD.service_account_id, OLD.api_key_id, 0,
                OLD.created_at, -1, (get_byte(uuid_send(OLD.id), 15) % 16)::SMALLINT, '{}', 0,
                OLD.created_at);
        END IF;
        RETURN NULL;
    END IF;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.count_rate_limited_batch()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    PERFORM gateway.count_rate_limit_policies(
        NEW.organization_id, NEW.team_id, NEW.service_account_id, NEW.api_key_id, 1,
        NEW.created_at, 0, 0::SMALLINT, '{}', 0, NEW.created_at);
    RETURN NULL;
END;
$$;

-- The recount of 0145, for an API key's policy too, and of the tokens of a
-- policy that limits them: each reservation in the window counts what it
-- counts now (rate_limit_reservation_tokens) at its second, less what the
-- release rows already hold for that second, read in the same statement.
-- So the window comes out exact, as in flight does (0123): a settlement
-- committed before that statement is in the count and in the release rows,
-- which cancel; one committed after it was counted at its bound and then
-- adds its difference. A settlement already under way when a policy starts
-- limiting tokens may not add its difference, and is counted at its bound
-- until it leaves the window: more, never less. Leases never cover tokens;
-- the reservations they admitted are counted by themselves.
CREATE OR REPLACE FUNCTION gateway.seed_rate_limit_counter(policy UUID)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    org UUID;
    kind TEXT;
    team UUID;
    account UUID;
    api_key UUID;
    tpm INTEGER;
    top BIGINT := floor(extract(epoch FROM clock_timestamp()))::BIGINT;
    counted INTEGER[];
    flight BIGINT;
    tokens BIGINT[] := array_fill(0::BIGINT, ARRAY[61]);
BEGIN
    SELECT p.organization_id, p.scope_kind, p.team_id, p.service_account_id, p.api_key_id,
           p.tokens_per_minute
    INTO org, kind, team, account, api_key, tpm
    FROM gateway.rate_limit_policy p WHERE p.id = policy;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    -- The leases' locks and then the counter's, as in 0145.
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
               OR (kind = 'service-account' AND r.service_account_id = account)
               OR (kind = 'api-key' AND r.api_key_id = api_key))
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
               OR (kind = 'service-account' AND b.service_account_id = account)
               OR (kind = 'api-key' AND b.api_key_id = api_key))
          AND b.created_at >= to_timestamp(top - 60)
        UNION ALL
        SELECT a.counted_second, a.admissions FROM active a
    )
    SELECT array_agg(
               (SELECT COALESCE(sum(a.n), 0) FROM admitted a WHERE a.second = g.second)::INTEGER
               ORDER BY g.second % 61)
    INTO counted
    FROM generate_series(top - 60, top) AS g(second);
    WITH active AS (
        SELECT l.* FROM gateway.rate_limit_lease l
        WHERE l.organization_id = org AND l.returned_at IS NULL AND policy = ANY (l.policy_ids)
    )
    SELECT (SELECT count(*)
            FROM gateway.accounting_reservation r
            WHERE r.organization_id = org AND r.batch_id IS NULL
              AND r.state IN ('reserved', 'claimed')
              AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
                   OR (kind = 'service-account' AND r.service_account_id = account)
                   OR (kind = 'api-key' AND r.api_key_id = api_key))
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
    IF tpm IS NOT NULL THEN
        -- One statement, one snapshot: the reservations' tokens and what
        -- the release rows hold.
        -- Set-based: the window's reservations once, each settled or
        -- released one joined to its latest usage (by the usage index), as
        -- rate_limit_reservation_tokens counts it; the release rows' rings
        -- once; both summed per second.
        WITH counted_tokens AS (
            SELECT floor(extract(epoch FROM COALESCE(r.tokens_counted_at, r.created_at)))::BIGINT
                       AS second,
                   CASE WHEN r.state IN ('settled', 'released') THEN COALESCE(
                            u.tokens,
                            CASE WHEN r.state = 'released' THEN 0
                                 ELSE COALESCE(r.token_bound, 0) END)
                        ELSE COALESCE(r.token_bound, 0) END AS n
            FROM gateway.accounting_reservation r
            LEFT JOIN LATERAL (
                SELECT CASE WHEN a.input_tokens IS NOT NULL OR a.output_tokens IS NOT NULL
                            THEN COALESCE(a.input_tokens, 0) + COALESCE(a.output_tokens, 0)
                                 + COALESCE(a.reasoning_tokens, 0) + COALESCE(a.other_tokens, 0)
                       END AS tokens
                FROM gateway.accounting_usage a
                WHERE a.organization_id = r.organization_id AND a.reservation_id = r.id
                ORDER BY a.recorded_at DESC, a.id DESC
                LIMIT 1) u ON r.state IN ('settled', 'released')
            WHERE r.organization_id = org AND r.batch_id IS NULL
              AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
                   OR (kind = 'service-account' AND r.service_account_id = account)
                   OR (kind = 'api-key' AND r.api_key_id = api_key))
              AND r.created_at >= to_timestamp(top - 60)
        ), per_second AS (
            SELECT t.second, sum(t.n) AS n FROM counted_tokens t GROUP BY t.second
        ), released AS (
            SELECT t.second, sum(t.tokens) AS n
            FROM gateway.rate_limit_release x,
                 gateway.rate_limit_ring_seconds(x.token_slots, x.token_last_second) t
            WHERE x.policy_id = policy
            GROUP BY t.second
        )
        SELECT array_agg((COALESCE(c.n, 0) - COALESCE(x.n, 0))::BIGINT ORDER BY g.second % 61)
        INTO tokens
        FROM generate_series(top - 60, top) AS g(second)
        LEFT JOIN per_second c ON c.second = g.second
        LEFT JOIN released x ON x.second = g.second;
    END IF;
    INSERT INTO gateway.rate_limit_counter
        (policy_id, in_flight, last_second, slots, token_last_second, token_slots)
    VALUES (policy, flight, top, counted, top, tokens)
    ON CONFLICT (policy_id) DO UPDATE
    SET in_flight = EXCLUDED.in_flight, last_second = EXCLUDED.last_second,
        slots = EXCLUDED.slots, token_last_second = EXCLUDED.token_last_second,
        token_slots = EXCLUDED.token_slots;
END;
$$;

-- Admission (0121) for a request of an API key: the organization (key
-- share), then every policy covering the request in id order, the key's
-- among them, for update; each policy's limits and counters, seeding a
-- counter a policy lacks. `token_window` is, for a policy that limits
-- tokens, the window's tokens second by second, oldest first, for the 61
-- seconds up to `now_epoch`: the counter's ring and the release rows'
-- (uncommitted releases are not seen, which only counts more).
CREATE FUNCTION gateway.lock_rate_limit_counters(
    organization UUID, team UUID, account UUID, api_key UUID)
RETURNS TABLE (scope_kind TEXT, requests_per_minute INTEGER, max_concurrent INTEGER,
               tokens_per_minute INTEGER, in_flight BIGINT, last_second BIGINT,
               slots INTEGER[], token_window BIGINT[], now_epoch DOUBLE PRECISION)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    p RECORD;
    at TIMESTAMPTZ;
    top BIGINT;
BEGIN
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    FOR p IN
        SELECT rp.id, rp.scope_kind, rp.requests_per_minute, rp.max_concurrent,
               rp.tokens_per_minute
        FROM gateway.rate_limit_policy rp
        WHERE rp.organization_id = organization
          AND (rp.scope_kind = 'organization'
               OR (rp.scope_kind = 'team' AND rp.team_id = team)
               OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account)
               OR (rp.scope_kind = 'api-key' AND rp.api_key_id = api_key))
        ORDER BY rp.id
        FOR UPDATE OF rp
    LOOP
        IF NOT EXISTS (SELECT 1 FROM gateway.rate_limit_counter c WHERE c.policy_id = p.id) THEN
            PERFORM gateway.seed_rate_limit_counter(p.id);
        END IF;
        at := clock_timestamp();
        top := floor(extract(epoch FROM at))::BIGINT;
        RETURN QUERY
        SELECT p.scope_kind, p.requests_per_minute, p.max_concurrent, p.tokens_per_minute,
               GREATEST(0, c.in_flight - COALESCE(
                   (SELECT sum(r.released) FROM gateway.rate_limit_release r
                    WHERE r.policy_id = p.id), 0))::BIGINT,
               c.last_second, c.slots,
               -- One pass over the counter's ring and the release rows'
               -- (16 at most), summed per second.
               CASE WHEN p.tokens_per_minute IS NULL THEN NULL ELSE ARRAY(
                   SELECT GREATEST(0, COALESCE(sum(w.tokens), 0))::BIGINT
                   FROM generate_series(top - 60, top) AS g(second)
                   LEFT JOIN (
                       SELECT t.second, t.tokens
                       FROM gateway.rate_limit_ring_seconds(c.token_slots, c.token_last_second) t
                       UNION ALL
                       SELECT t.second, t.tokens
                       FROM gateway.rate_limit_release x,
                            gateway.rate_limit_ring_seconds(x.token_slots, x.token_last_second) t
                       WHERE x.policy_id = p.id) w ON w.second = g.second
                   GROUP BY g.second
                   ORDER BY g.second) END,
               extract(epoch FROM at)::DOUBLE PRECISION
        FROM gateway.rate_limit_counter c WHERE c.policy_id = p.id;
    END LOOP;
END;
$$;

-- Granting a lease (0145), never for a scope a policy limiting tokens
-- covers: no row, as when no policy covers it. The rest is unchanged.
CREATE OR REPLACE FUNCTION gateway.grant_rate_limit_lease(
    lease UUID, holder UUID, organization UUID, team UUID, account UUID, admissions INTEGER,
    room_second BIGINT, min_life DOUBLE PRECISION)
RETURNS TABLE (granted BOOLEAN, seconds_left DOUBLE PRECISION)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    ids UUID[];
    rpms INTEGER[];
    concs INTEGER[];
    tokens_limited BOOLEAN;
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
           array_agg(COALESCE(x.max_concurrent, -1) ORDER BY x.id),
           bool_or(x.tokens_per_minute IS NOT NULL)
    INTO ids, rpms, concs, tokens_limited
    FROM (SELECT rp.id, rp.requests_per_minute, rp.max_concurrent, rp.tokens_per_minute
          FROM gateway.rate_limit_policy rp
          WHERE rp.organization_id = organization
            AND (rp.scope_kind = 'organization'
                 OR (rp.scope_kind = 'team' AND rp.team_id = team)
                 OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account))
          ORDER BY rp.id
          FOR UPDATE OF rp) x;
    IF ids IS NULL OR tokens_limited THEN
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

-- Admitting under a lease (0145), for this and older processes. A lease
-- covers neither tokens per minute nor an API key's own policy, so it is
-- not used for a request either applies to; the request is then admitted
-- on the policies' rows. `keyed`: the key has a policy of its own, and the
-- lease stays usable for other keys. With `unknown_key` (a process from
-- before 0155, which does not say which key) any key of the account with a
-- policy of its own refuses the lease. Otherwise the time of admission if
-- the lease is still usable, as 0145's checks it, or NULL. The organization
-- is locked first, as every admission locks it: a policy write holds it for
-- update, so none of these answers can change before this transaction ends.
CREATE FUNCTION gateway.rate_limit_lease_admission(
    lease UUID, ticket INTEGER, organization UUID, team UUID, account UUID, api_key UUID,
    unknown_key BOOLEAN)
RETURNS TABLE (admitted_at TIMESTAMPTZ, keyed BOOLEAN)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    at TIMESTAMPTZ;
    l RECORD;
    ids UUID[];
    rpms INTEGER[];
    concs INTEGER[];
    tokens_limited BOOLEAN;
BEGIN
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    IF api_key IS NOT NULL AND EXISTS (
        SELECT 1 FROM gateway.rate_limit_policy rp
        WHERE rp.organization_id = organization AND rp.scope_kind = 'api-key'
          AND rp.api_key_id = api_key) THEN
        RETURN QUERY SELECT NULL::TIMESTAMPTZ, true;
        RETURN;
    END IF;
    IF unknown_key AND EXISTS (
        SELECT 1 FROM gateway.rate_limit_policy rp
        JOIN gateway.api_key k ON k.organization_id = rp.organization_id AND k.id = rp.api_key_id
        WHERE rp.organization_id = organization AND rp.scope_kind = 'api-key'
          AND k.service_account_id = account) THEN
        RETURN QUERY SELECT NULL::TIMESTAMPTZ, false;
        RETURN;
    END IF;
    -- 0145's checks, unchanged, with tokens per minute read alongside.
    PERFORM pg_advisory_xact_lock_shared(1918989413, hashtext(lease::text));
    at := clock_timestamp();
    SELECT x.policy_ids, x.requests_per_minute, x.max_concurrent INTO l
    FROM gateway.rate_limit_lease x
    WHERE x.id = lease AND x.returned_at IS NULL AND x.organization_id = organization
      AND x.team_id IS NOT DISTINCT FROM team
      AND x.service_account_id IS NOT DISTINCT FROM account
      AND ticket >= 0 AND ticket < x.admissions
      AND at >= x.created_at AND at < x.valid_until;
    IF NOT FOUND THEN
        RETURN QUERY SELECT NULL::TIMESTAMPTZ, false;
        RETURN;
    END IF;
    SELECT array_agg(rp.id ORDER BY rp.id),
           array_agg(COALESCE(rp.requests_per_minute, -1) ORDER BY rp.id),
           array_agg(COALESCE(rp.max_concurrent, -1) ORDER BY rp.id),
           bool_or(rp.tokens_per_minute IS NOT NULL)
    INTO ids, rpms, concs, tokens_limited
    FROM gateway.rate_limit_policy rp
    WHERE rp.organization_id = organization
      AND (rp.scope_kind = 'organization'
           OR (rp.scope_kind = 'team' AND rp.team_id = team)
           OR (rp.scope_kind = 'service-account' AND rp.service_account_id = account));
    IF tokens_limited OR ids IS DISTINCT FROM l.policy_ids
       OR rpms IS DISTINCT FROM l.requests_per_minute
       OR concs IS DISTINCT FROM l.max_concurrent THEN
        RETURN QUERY SELECT NULL::TIMESTAMPTZ, false;
        RETURN;
    END IF;
    RETURN QUERY SELECT at, false;
END;
$$;

-- What a process from before 0155 calls: NULL (admit on the policies' rows)
-- also when tokens per minute or a key's own policy may apply.
CREATE OR REPLACE FUNCTION gateway.use_rate_limit_lease(
    lease UUID, ticket INTEGER, organization UUID, team UUID, account UUID)
RETURNS TIMESTAMPTZ
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    SELECT a.admitted_at
    FROM gateway.rate_limit_lease_admission(lease, ticket, organization, team, account, NULL, true) a
$$;

CREATE FUNCTION gateway.use_rate_limit_lease(
    lease UUID, ticket INTEGER, organization UUID, team UUID, account UUID, api_key UUID)
RETURNS TABLE (admitted_at TIMESTAMPTZ, keyed BOOLEAN)
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
    SELECT a.admitted_at, a.keyed
    FROM gateway.rate_limit_lease_admission(
        lease, ticket, organization, team, account, api_key, false) a
$$;

-- The policies covering an emulated batch's lines, the key's own among
-- them, locked as 0062's three-argument form locks the others: the
-- organization first, then the policies in id order, as admission and a
-- policy write take them. The old form stays for processes from before.
CREATE FUNCTION gateway.lock_rate_limit_policies(
    organization UUID, team UUID, account UUID, api_key UUID)
RETURNS TABLE (scope_kind TEXT, requests_per_minute INTEGER, max_concurrent INTEGER)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    PERFORM 1 FROM gateway.organization o WHERE o.id = organization FOR KEY SHARE;
    RETURN QUERY
    SELECT p.scope_kind, p.requests_per_minute, p.max_concurrent
    FROM gateway.rate_limit_policy p
    WHERE p.organization_id = organization
      AND (p.scope_kind = 'organization'
           OR (p.scope_kind = 'team' AND p.team_id = team)
           OR (p.scope_kind = 'service-account' AND p.service_account_id = account)
           OR (p.scope_kind = 'api-key' AND p.api_key_id = api_key))
    ORDER BY p.id
    FOR UPDATE OF p;
END;
$$;

REVOKE ALL ON FUNCTION gateway.lock_rate_limit_policies(UUID, UUID, UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.count_rate_limit_policies(
    UUID, UUID, UUID, UUID, INTEGER, TIMESTAMPTZ, INTEGER, SMALLINT, UUID[], BIGINT,
    TIMESTAMPTZ) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.rate_limit_tokens_covered(UUID, UUID, UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.lock_rate_limit_tokens_covered(UUID, UUID, UUID, UUID)
    FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.lock_rate_limit_counters(UUID, UUID, UUID, UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.use_rate_limit_lease(UUID, INTEGER, UUID, UUID, UUID, UUID)
    FROM PUBLIC;
REVOKE ALL ON FUNCTION gateway.rate_limit_lease_admission(
    UUID, INTEGER, UUID, UUID, UUID, UUID, BOOLEAN) FROM PUBLIC;
