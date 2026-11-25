-- Plans of an app's end users (D-028) get rate limits per user and
-- allowance periods other than the calendar month.
--
-- A plan's `period` is `monthly` (the UTC calendar month, as before),
-- `weekly` (the ISO week from Monday 00:00 UTC), `daily` (the UTC day) or
-- `days` (windows of `period_days` days counted from Monday 1970-01-05
-- 00:00 UTC). Each window's allowance is its own quota bucket, keyed as
-- before by its start and end.
ALTER TABLE gateway.end_user_plan
    ADD COLUMN period TEXT NOT NULL DEFAULT 'monthly'
        CHECK (period IN ('monthly', 'weekly', 'daily', 'days')),
    ADD COLUMN period_days INTEGER CHECK (period_days BETWEEN 1 AND 366),
    ADD COLUMN requests_per_minute INTEGER CHECK (requests_per_minute BETWEEN 1 AND 1000000),
    ADD COLUMN tokens_per_minute INTEGER CHECK (tokens_per_minute BETWEEN 1 AND 1000000000),
    ADD CONSTRAINT end_user_plan_period_days_ck
        CHECK ((period = 'days') = (period_days IS NOT NULL));

-- The window of a plan's period containing `at`, the same as
-- `domain::end_user::PlanPeriod::window`. Fixed-day windows add seconds, so
-- the session's time zone never moves them.
CREATE FUNCTION gateway.end_user_period_start(period TEXT, period_days INTEGER, at TIMESTAMPTZ)
RETURNS TIMESTAMPTZ
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE period
        WHEN 'daily' THEN date_trunc('day', at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
        WHEN 'weekly' THEN date_trunc('week', at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
        WHEN 'days' THEN TIMESTAMPTZ '1970-01-05 00:00:00+00' + make_interval(secs =>
            floor(extract(epoch FROM at - TIMESTAMPTZ '1970-01-05 00:00:00+00')
                  / (86400.0 * period_days)) * 86400.0 * period_days)
        ELSE date_trunc('month', at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
    END
$$;

CREATE FUNCTION gateway.end_user_period_end(period TEXT, period_days INTEGER, at TIMESTAMPTZ)
RETURNS TIMESTAMPTZ
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE period
        WHEN 'daily' THEN gateway.end_user_period_start(period, period_days, at)
            + make_interval(secs => 86400)
        WHEN 'weekly' THEN gateway.end_user_period_start(period, period_days, at)
            + make_interval(secs => 7 * 86400)
        WHEN 'days' THEN gateway.end_user_period_start(period, period_days, at)
            + make_interval(secs => 86400.0 * period_days)
        ELSE (date_trunc('month', at AT TIME ZONE 'UTC') + INTERVAL '1 month') AT TIME ZONE 'UTC'
    END
$$;

-- The tokens a request holds against its end user's `tokens_per_minute`
-- while in flight: its input bound, output cap and reasoning allowance.
-- NULL on reservations made before this column (and so never counted).
ALTER TABLE gateway.accounting_reservation
    ADD COLUMN token_bound BIGINT CHECK (token_bound IS NULL OR token_bound >= 0);

-- An end user's admissions in the last minute.
CREATE INDEX accounting_reservation_end_user_created_idx
    ON gateway.accounting_reservation (end_user_id, created_at)
    WHERE end_user_id IS NOT NULL;
