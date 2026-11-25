-- A policy write must never wait on a release row (0121) while it holds its
-- organization's row. A release (a request refused, settled or expired)
-- updates its policy's release shard and then, in the same transaction,
-- writes rows whose foreign keys take a key-share lock on the organization,
-- so it waits on the policy write's lock on that row. Before this, the
-- recount and the policy's deletion each deleted the policy's release rows
-- and so waited on that shard: a deadlock.
--
-- The recount now leaves the release rows alone and sets the counter to
-- what is in flight plus what the release rows already hold, read in one
-- statement, so in flight (the counter less its releases) comes out exact:
-- a release committed before that statement is in neither the count nor
-- (as an addition) the counter; one committed after it was counted in
-- flight and then adds its release. Deleting a policy deletes only the
-- release rows no one is writing; any left over name a policy that no
-- longer exists and are never read (a policy set again under the same id is
-- recounted the same way, so they cancel out).
CREATE OR REPLACE FUNCTION gateway.seed_rate_limit_counter(policy UUID)
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
    -- One statement, one snapshot: the reservations in flight and the
    -- releases already written.
    SELECT (SELECT count(*)
            FROM gateway.accounting_reservation r
            WHERE r.organization_id = org AND r.batch_id IS NULL
              AND r.state IN ('reserved', 'claimed')
              AND (kind = 'organization' OR (kind = 'team' AND r.team_id = team)
                   OR (kind = 'service-account' AND r.service_account_id = account)))
         + (SELECT COALESCE(sum(x.released), 0)
            FROM gateway.rate_limit_release x WHERE x.policy_id = policy)
    INTO flight;
    INSERT INTO gateway.rate_limit_counter (policy_id, in_flight, last_second, slots)
    VALUES (policy, flight, now_second, counted)
    ON CONFLICT (policy_id) DO UPDATE
    SET in_flight = EXCLUDED.in_flight, last_second = EXCLUDED.last_second,
        slots = EXCLUDED.slots;
END;
$$;

CREATE OR REPLACE FUNCTION gateway.delete_rate_limit_releases()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    DELETE FROM gateway.rate_limit_release r
    WHERE (r.policy_id, r.shard) IN (
        SELECT x.policy_id, x.shard FROM gateway.rate_limit_release x
        WHERE x.policy_id = OLD.policy_id
        FOR UPDATE SKIP LOCKED);
    RETURN NULL;
END;
$$;
