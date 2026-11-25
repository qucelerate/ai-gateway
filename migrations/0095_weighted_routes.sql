-- Routing across deployments (D-023, Phase 9): one alias may route an
-- operation to several deployments, each with a weight (its share of its
-- priority's traffic) and a priority (lower is preferred; a request goes to
-- the lowest priority with a healthy deployment). A route of one deployment
-- is weight 1, priority 1, as every route stored before this was.

ALTER TABLE gateway.catalog_route
    ADD COLUMN weight INTEGER NOT NULL DEFAULT 1
        CONSTRAINT catalog_route_weight_ck CHECK (weight BETWEEN 1 AND 1000),
    ADD COLUMN priority INTEGER NOT NULL DEFAULT 1
        CONSTRAINT catalog_route_priority_ck CHECK (priority BETWEEN 1 AND 10),
    DROP CONSTRAINT catalog_route_alias_uq,
    ADD CONSTRAINT catalog_route_alias_uq
        UNIQUE (organization_id, release_id, alias, operation, deployment_id);

-- Configuration versions carry each route's weight and priority. The keys
-- are present only when not 1, so documents of single-deployment routes keep
-- their digest and no organization gets a new version from this change.
CREATE OR REPLACE FUNCTION gateway.publish_config_version(organization UUID)
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
            SELECT jsonb_agg((jsonb_build_object(
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
                ) || CASE WHEN c.reasoning_allowance_tokens > 0
                          THEN jsonb_build_object('reasoning_allowance_tokens',
                                                  c.reasoning_allowance_tokens)
                          ELSE '{}'::JSONB END
            ) || CASE WHEN r.weight <> 1 THEN jsonb_build_object('weight', r.weight)
                      ELSE '{}'::JSONB END
              || CASE WHEN r.priority <> 1 THEN jsonb_build_object('priority', r.priority)
                      ELSE '{}'::JSONB END
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
