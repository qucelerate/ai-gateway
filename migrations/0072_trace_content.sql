-- What a traced request kept (D-017): one row per traced request, beside its
-- request record. The content itself is an encrypted object in the trace
-- store, named here with the sealed key it opens with; the row holds timing
-- and sizes only.
--
-- state:
--   writing      the object is being written: its name and key are recorded
--                first, so an object never exists without a row naming it;
--   stored       the object holds the request and response;
--   off          the organization traces without content; timing only;
--   unavailable  content was on but no trace store or keyring is configured;
--   failed       the object could not be written;
--   expired      the content passed its retention period and was deleted.
--
-- The worker expires content before the request record it belongs to, and
-- the retention purge leaves a record whose content is still stored (or
-- being written), so an object is never left without its row. A row left
-- `writing` (the gateway stopped, or the outcome could not be recorded) is
-- cleaned up by the worker: its object deleted and the row marked failed.

CREATE TABLE gateway.inference_trace_content (
    trace_id UUID PRIMARY KEY
        REFERENCES gateway.inference_trace (id) ON DELETE CASCADE,
    organization_id UUID NOT NULL
        REFERENCES gateway.organization (id) ON DELETE CASCADE,
    state TEXT NOT NULL
        CHECK (state IN ('writing', 'stored', 'off', 'unavailable', 'failed', 'expired')),
    object_name TEXT CHECK (length(object_name) BETWEEN 1 AND 600 AND object_name ~ '^[A-Za-z0-9._/-]+$'),
    key_id TEXT,
    sealed_key BYTEA,
    request_bytes BIGINT CHECK (request_bytes >= 0),
    response_bytes BIGINT CHECK (response_bytes >= 0),
    request_truncated BOOLEAN NOT NULL DEFAULT false,
    response_truncated BOOLEAN NOT NULL DEFAULT false,
    -- When the gateway took the request, called the provider, saw the
    -- first streamed event, and finished.
    started_at TIMESTAMPTZ NOT NULL,
    provider_started_at TIMESTAMPTZ,
    first_event_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expired_at TIMESTAMPTZ,
    CONSTRAINT inference_trace_content_stored_ck CHECK (
        (state IN ('writing', 'stored'))
        = (object_name IS NOT NULL AND key_id IS NOT NULL AND sealed_key IS NOT NULL)
    ),
    CONSTRAINT inference_trace_content_expired_ck CHECK (
        (state = 'expired') = (expired_at IS NOT NULL)
    )
);

CREATE INDEX inference_trace_content_stored_idx
    ON gateway.inference_trace_content (organization_id, recorded_at)
    WHERE state IN ('writing', 'stored');
CREATE INDEX inference_trace_content_key_idx
    ON gateway.inference_trace_content (key_id)
    WHERE state IN ('writing', 'stored');
