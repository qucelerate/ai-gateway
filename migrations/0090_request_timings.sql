-- How each request went, on the always-kept request record, for model cards
-- (Phase 8, docs/model-card-design.md): how long the provider took, how long
-- a stream took to its first event, and why the first choice finished. No
-- content (D-017): a finish reason is the provider's short code, and anything
-- else is recorded as `other`. Requests before this migration have none, and
-- cards say from when these are known.
ALTER TABLE gateway.inference_trace
    -- From sending the request to the provider to its last event or failure.
    ADD COLUMN provider_latency_ms INTEGER CHECK (provider_latency_ms >= 0),
    -- Streams only: from sending the request to the first content event.
    ADD COLUMN first_event_ms INTEGER CHECK (first_event_ms >= 0),
    ADD COLUMN finish_reason TEXT CHECK (finish_reason ~ '^[a-z0-9_.:-]{1,64}$');

-- A model's requests over a window, for its card.
CREATE INDEX inference_trace_model_idx
    ON gateway.inference_trace (organization_id, model, recorded_at DESC);
