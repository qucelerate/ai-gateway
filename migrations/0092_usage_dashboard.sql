-- The usage dashboard slices requests by the API key that made them.
--
-- The key is recorded on the trace when the request is admitted, like the
-- team and service account, and never changes afterwards. Requests admitted
-- before this migration have no key (`NULL`); they still count under their
-- service account.
ALTER TABLE gateway.inference_trace ADD COLUMN api_key_id UUID;

CREATE OR REPLACE FUNCTION gateway.prevent_trace_api_key_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.api_key_id IS DISTINCT FROM NEW.api_key_id THEN
        RAISE EXCEPTION 'inference trace metadata is immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER inference_trace_api_key_immutable
    BEFORE UPDATE ON gateway.inference_trace
    FOR EACH ROW EXECUTE FUNCTION gateway.prevent_trace_api_key_mutation();

-- Usage series and events filtered by key page by (time, id) within a tenant.
CREATE INDEX inference_trace_org_key_recorded_idx
    ON gateway.inference_trace (organization_id, api_key_id, recorded_at DESC, id DESC)
    WHERE api_key_id IS NOT NULL;
