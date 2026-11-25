-- Price sync. The worker fetches each configured price source daily and
-- records what it publishes as a versioned snapshot. Sources are gateway-wide
-- (public price lists and operator price sheets are not tenant data), so
-- these tables carry no organization. Nothing here changes a catalog price:
-- drift against a release is computed when read.

CREATE TABLE gateway.price_source (
    -- `{kind}:{family}`, e.g. `price-sheet:openai`. The fetch URL is process
    -- configuration and never stored, since it may carry a token.
    source TEXT PRIMARY KEY CHECK (source ~ '^[a-z][a-z0-9-]{0,31}:[a-z][a-z0-9-]{0,31}$'),
    kind TEXT NOT NULL CHECK (kind IN ('price-sheet')),
    family TEXT NOT NULL CHECK (family IN ('openai', 'anthropic', 'gemini', 'bedrock',
                                           'azure-openai', 'azure-ai-foundry',
                                           'openai-compatible')),
    next_fetch_at TIMESTAMPTZ NOT NULL,
    -- A claimed fetch holds a lease; its token fences a worker whose lease
    -- expired and whose source was claimed again.
    lease_token UUID,
    lease_expires_at TIMESTAMPTZ,
    -- Consecutive failed fetches since the last success, reset after the
    -- retry budget is spent so the next daily fetch starts afresh.
    failures INTEGER NOT NULL DEFAULT 0 CHECK (failures BETWEEN 0 AND 3),
    last_attempt_at TIMESTAMPTZ,
    last_success_at TIMESTAMPTZ,
    -- A fixed code, never a URL, response body or provider text.
    last_error_code TEXT CHECK (last_error_code IS NULL OR last_error_code ~ '^[a-z_]{1,64}$'),
    current_snapshot_id UUID,
    -- The latest tick of a worker that has the source configured. A source
    -- no worker has registered for a while is retired: not fetched or
    -- reported, its snapshots age out, and configuring it again makes it due
    -- at once with no current snapshot.
    registered_at TIMESTAMPTZ NOT NULL,
    retired_at TIMESTAMPTZ,
    CONSTRAINT price_source_kind_ck CHECK (source = kind || ':' || family),
    CONSTRAINT price_source_lease_ck CHECK ((lease_token IS NULL) = (lease_expires_at IS NULL)),
    CONSTRAINT price_source_retired_ck CHECK (retired_at IS NULL OR lease_token IS NULL)
);

CREATE TABLE gateway.price_snapshot (
    id UUID PRIMARY KEY,
    source TEXT NOT NULL REFERENCES gateway.price_source (source) ON DELETE CASCADE,
    source_version TEXT CHECK (source_version IS NULL OR length(source_version) BETWEEN 1 AND 128),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    observations INTEGER NOT NULL CHECK (observations >= 0),
    unmapped INTEGER NOT NULL CHECK (unmapped BETWEEN 0 AND observations),
    -- First fetched, and the latest fetch that saw the same prices (whose
    -- version label `source_version` then holds). A fetch
    -- that differs from the current snapshot starts a new one, so prices
    -- that change back are a new version rather than a rewrite of history.
    fetched_at TIMESTAMPTZ NOT NULL,
    confirmed_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT price_snapshot_seen_ck CHECK (fetched_at <= confirmed_at),
    CONSTRAINT price_snapshot_source_id_uq UNIQUE (source, id)
);

CREATE INDEX price_snapshot_source_idx ON gateway.price_snapshot (source, fetched_at DESC);
CREATE INDEX price_snapshot_confirmed_idx ON gateway.price_snapshot (confirmed_at);

ALTER TABLE gateway.price_source
    ADD CONSTRAINT price_source_current_fk FOREIGN KEY (source, current_snapshot_id)
        REFERENCES gateway.price_snapshot (source, id)
        ON DELETE SET NULL (current_snapshot_id) DEFERRABLE INITIALLY DEFERRED;

-- Published decimals exactly as the source states them, per `per_tokens`
-- tokens; NULL where the source states no price for a dimension.
CREATE TABLE gateway.price_observation (
    snapshot_id UUID NOT NULL REFERENCES gateway.price_snapshot (id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    item TEXT NOT NULL CHECK (octet_length(item) BETWEEN 1 AND 255),
    -- NULL when the source could not be mapped to a model with confidence.
    model TEXT CHECK (model IS NULL OR octet_length(model) BETWEEN 1 AND 255),
    -- NULL for a price that applies in every region.
    region TEXT CHECK (region IS NULL OR octet_length(region) BETWEEN 1 AND 255),
    currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    per_tokens INTEGER NOT NULL CHECK (per_tokens IN (1, 1000, 1000000)),
    input NUMERIC(24, 12) CHECK (input >= 0),
    output NUMERIC(24, 12) CHECK (output >= 0),
    cached_input NUMERIC(24, 12) CHECK (cached_input >= 0),
    reasoning NUMERIC(24, 12) CHECK (reasoning >= 0),
    other NUMERIC(24, 12) CHECK (other >= 0),
    PRIMARY KEY (snapshot_id, ordinal),
    CONSTRAINT price_observation_published_ck
        CHECK (num_nonnulls(input, output, cached_input, reasoning, other) > 0)
);

CREATE UNIQUE INDEX price_observation_item_uq
    ON gateway.price_observation (snapshot_id, item, region, currency) NULLS NOT DISTINCT;
CREATE UNIQUE INDEX price_observation_model_uq
    ON gateway.price_observation (snapshot_id, model, region, currency) NULLS NOT DISTINCT
    WHERE model IS NOT NULL;
