-- Configuration versions: the routing configuration every API-serving
-- process (all-in-one, api, data-plane) loads and serves from memory. A
-- version is an immutable document holding an organization's active catalog
-- release's routes and its unrevoked, unexpired price overrides with their
-- windows. It is published in the same transaction as each change to those
-- inputs, by the triggers below, so no writer can change routing without
-- publishing. Access controls (keys, credentials, budgets, limits) are never
-- part of a version; see docs/architecture-plan.md §2 and D-013.
CREATE TABLE gateway.config_version (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    -- SHA-256 of the document's text form, which instances check on load.
    digest BYTEA NOT NULL CHECK (length(digest) = 32),
    document JSONB NOT NULL CHECK (jsonb_typeof(document) = 'object'),
    release_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT config_version_org_id_uq UNIQUE (organization_id, id)
);

CREATE INDEX config_version_org_created_idx
    ON gateway.config_version (organization_id, created_at DESC);

-- One deployment-wide feed of publications and pin changes. Publishers hold a
-- transaction-level advisory lock from before their entry is numbered until
-- commit, so feed order is commit order: a reader that has applied position
-- n never misses a lower position committed later. `entry_id` lets a reader
-- notice that the entry at its position was lost (a restore or a failover
-- that lost writes) even after new entries pass that position.
CREATE TABLE gateway.config_feed (
    sequence BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    entry_id UUID NOT NULL UNIQUE,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('version', 'pin', 'unpin')),
    version_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT config_feed_version_ck CHECK ((kind = 'unpin') = (version_id IS NULL)),
    CONSTRAINT config_feed_version_fk FOREIGN KEY (organization_id, version_id)
        REFERENCES gateway.config_version (organization_id, id)
);

CREATE INDEX config_feed_org_idx ON gateway.config_feed (organization_id, sequence DESC);

CREATE FUNCTION gateway.prevent_config_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'configuration versions and feed entries are immutable';
END;
$$;

CREATE TRIGGER config_version_immutable BEFORE UPDATE ON gateway.config_version
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_config_mutation();
CREATE TRIGGER config_feed_immutable BEFORE UPDATE ON gateway.config_feed
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_config_mutation();

-- What each organization serves at feed position `upto`: the version of its
-- last pin entry when that entry is a pin, otherwise its newest version.
CREATE FUNCTION gateway.config_served_versions(upto BIGINT)
RETURNS TABLE (organization_id UUID, version_id UUID)
LANGUAGE sql STABLE AS $$
    WITH newest AS (
        SELECT DISTINCT ON (f.organization_id) f.organization_id, f.version_id
        FROM gateway.config_feed f
        WHERE f.kind = 'version' AND f.sequence <= upto
        ORDER BY f.organization_id, f.sequence DESC
    ), last_pin AS (
        SELECT DISTINCT ON (f.organization_id) f.organization_id, f.kind, f.version_id
        FROM gateway.config_feed f
        WHERE f.kind IN ('pin', 'unpin') AND f.sequence <= upto
        ORDER BY f.organization_id, f.sequence DESC
    )
    SELECT n.organization_id,
           CASE WHEN p.kind = 'pin' THEN p.version_id ELSE n.version_id END
    FROM newest n LEFT JOIN last_pin p ON p.organization_id = n.organization_id
$$;

-- Timestamps in documents are fixed-format UTC text, so a document's text
-- (and digest) never depends on the session's time zone.
CREATE FUNCTION gateway.config_timestamp(value TIMESTAMPTZ)
RETURNS TEXT LANGUAGE sql IMMUTABLE AS $$
    SELECT to_char(value AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')
$$;

-- Builds the organization's document from its current catalog state and
-- publishes it as a new version, unless it equals the newest one. Returns the
-- version that is newest afterwards, or NULL when the organization has none.
CREATE FUNCTION gateway.publish_config_version(organization UUID)
RETURNS UUID LANGUAGE plpgsql AS $$
DECLARE
    active UUID;
    doc JSONB;
    doc_digest BYTEA;
    newest_id UUID;
    newest_digest BYTEA;
    created UUID;
BEGIN
    -- Taken before the inputs are read and held until commit: concurrent
    -- publishers then build their documents one after another, each seeing
    -- the changes committed before it, and number their entries in commit
    -- order.
    PERFORM pg_advisory_xact_lock(hashtextextended('config:feed', 0));

    SELECT release_id INTO active
    FROM gateway.catalog_active_release WHERE organization_id = organization;

    doc := jsonb_build_object(
        'schema', 1,
        'release', (
            SELECT jsonb_build_object('id', r.id, 'digest', encode(r.digest, 'hex'))
            FROM gateway.catalog_release r
            WHERE r.organization_id = organization AND r.id = active
        ),
        'routes', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                'route_id', r.id,
                'alias', r.alias,
                'operation', r.operation,
                'deployment_id', d.id,
                'deployment', d.name,
                'model', d.model,
                'adapter_instance_key', d.adapter_instance_key,
                'family', p.family,
                'price', jsonb_build_object(
                    'id', pr.id,
                    'input_minor_per_token', pr.input_minor_per_token,
                    'output_minor_per_token', pr.output_minor_per_token,
                    'cached_input_minor_per_token', pr.cached_input_minor_per_token,
                    'reasoning_minor_per_token', pr.reasoning_minor_per_token,
                    'other_minor_per_token', pr.other_minor_per_token,
                    'currency', pr.currency::TEXT,
                    'source', pr.source,
                    'source_version', pr.source_version,
                    'confidence', pr.confidence
                ),
                'capability', jsonb_build_object(
                    'max_input_bytes', c.max_input_bytes,
                    'max_output_tokens', c.max_output_tokens,
                    'max_tokens_per_byte', c.max_tokens_per_byte,
                    'tokenizer', c.tokenizer,
                    'reservation_ttl_seconds', c.reservation_ttl_seconds,
                    'chat_input_count_mode', c.chat_input_count_mode
                )
            ) ORDER BY r.alias, r.operation, r.id)
            FROM gateway.catalog_route r
            JOIN gateway.catalog_deployment d
              ON d.organization_id = r.organization_id AND d.release_id = r.release_id
             AND d.id = r.deployment_id
            JOIN gateway.catalog_provider_account p
              ON p.organization_id = d.organization_id AND p.release_id = d.release_id
             AND p.id = d.provider_account_id AND p.adapter_instance_key = d.adapter_instance_key
            JOIN gateway.catalog_capability c
              ON c.organization_id = r.organization_id AND c.release_id = r.release_id
             AND c.id = r.capability_id AND c.deployment_id = r.deployment_id
             AND c.operation = r.operation
            JOIN gateway.catalog_price pr
              ON pr.organization_id = r.organization_id AND pr.release_id = r.release_id
             AND pr.id = r.price_id AND pr.deployment_id = r.deployment_id
             AND pr.operation = r.operation
            WHERE r.organization_id = organization AND r.release_id = active
        ), '[]'::JSONB),
        -- Windows are evaluated when a request is routed, so an override
        -- that starts or ends needs no new version.
        'overrides', COALESCE((
            SELECT jsonb_agg(jsonb_build_object(
                'id', o.id,
                'deployment', o.deployment,
                'operation', o.operation,
                'input_minor_per_token', o.input_minor_per_token,
                'output_minor_per_token', o.output_minor_per_token,
                'cached_input_minor_per_token', o.cached_input_minor_per_token,
                'reasoning_minor_per_token', o.reasoning_minor_per_token,
                'other_minor_per_token', o.other_minor_per_token,
                'currency', o.currency::TEXT,
                'confidence', o.confidence,
                'version', o.version,
                'effective_from', gateway.config_timestamp(o.effective_from),
                'effective_until', gateway.config_timestamp(o.effective_until),
                'created_at', gateway.config_timestamp(o.created_at)
            ) ORDER BY o.deployment, o.operation, o.effective_from, o.created_at, o.id)
            FROM gateway.catalog_price_override o
            WHERE o.organization_id = organization AND o.revoked_at IS NULL
              AND (o.effective_until IS NULL OR o.effective_until > now())
        ), '[]'::JSONB)
    );
    doc_digest := sha256(convert_to(doc::TEXT, 'UTF8'));

    SELECT v.id, v.digest INTO newest_id, newest_digest
    FROM gateway.config_feed f
    JOIN gateway.config_version v ON v.organization_id = f.organization_id AND v.id = f.version_id
    WHERE f.organization_id = organization AND f.kind = 'version'
    ORDER BY f.sequence DESC LIMIT 1;
    IF newest_digest = doc_digest THEN
        RETURN newest_id;
    END IF;

    created := gen_random_uuid();
    INSERT INTO gateway.config_version (id, organization_id, digest, document, release_id)
    VALUES (created, organization, doc_digest, doc, active);
    INSERT INTO gateway.config_feed (entry_id, organization_id, kind, version_id)
    VALUES (gen_random_uuid(), organization, 'version', created);
    RETURN created;
END;
$$;

-- A deactivated release (its active row deleted) publishes a version without
-- routes. Rows removed because their organization is being deleted publish
-- nothing: its versions and feed entries go with it.
CREATE FUNCTION gateway.publish_config_version_trigger()
RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    changed UUID := CASE WHEN TG_OP = 'DELETE' THEN OLD.organization_id
                         ELSE NEW.organization_id END;
BEGIN
    IF EXISTS (SELECT 1 FROM gateway.organization o WHERE o.id = changed) THEN
        PERFORM gateway.publish_config_version(changed);
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER catalog_active_release_publish
    AFTER INSERT OR UPDATE OR DELETE ON gateway.catalog_active_release
    FOR EACH ROW EXECUTE FUNCTION gateway.publish_config_version_trigger();
CREATE TRIGGER catalog_price_override_publish
    AFTER INSERT OR UPDATE OF revoked_at OR DELETE ON gateway.catalog_price_override
    FOR EACH ROW EXECUTE FUNCTION gateway.publish_config_version_trigger();

SELECT gateway.publish_config_version(organization_id)
FROM gateway.catalog_active_release
ORDER BY organization_id;

-- Every API-serving process reports itself here on each poll: which feed
-- entry it has applied and when it was last seen. Not tenant data.
CREATE TABLE gateway.gateway_instance (
    instance_id TEXT PRIMARY KEY CHECK (length(instance_id) BETWEEN 1 AND 253),
    profile TEXT NOT NULL CHECK (profile IN ('all-in-one', 'api', 'worker', 'data-plane')),
    region TEXT CHECK (region IS NULL OR length(region) BETWEEN 1 AND 64),
    build TEXT NOT NULL CHECK (length(build) BETWEEN 1 AND 64),
    applied_sequence BIGINT,
    applied_entry_id UUID,
    loaded_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ NOT NULL,
    last_seen_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT gateway_instance_applied_ck
        CHECK ((applied_sequence IS NULL) = (applied_entry_id IS NULL))
);

-- Rate-limit admission locks the organization (as the reservation insert's
-- foreign key will) and then the scope's policies, in the order an admin
-- write takes them. As a function owned by the application role, a data
-- plane's role can take those locks without the right to update either
-- table.
CREATE FUNCTION gateway.lock_rate_limit_policies(organization UUID, team UUID, account UUID)
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
           OR (p.scope_kind = 'service-account' AND p.service_account_id = account))
    ORDER BY p.id
    FOR UPDATE OF p;
END;
$$;

REVOKE ALL ON FUNCTION gateway.lock_rate_limit_policies(UUID, UUID, UUID) FROM PUBLIC;

-- Reservations and traces check their catalog provenance against the catalog
-- tables. Run the checks as the application role, so a data plane's role,
-- which routes from configuration versions, needs no access to the catalog.
ALTER FUNCTION gateway.validate_accounting_reservation_catalog_provenance()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION gateway.validate_inference_trace_catalog_provenance()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION gateway.validate_catalog_chat_count_mode()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
