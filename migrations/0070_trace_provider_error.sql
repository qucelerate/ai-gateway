-- Why a provider request failed, kept on its trace so an administrator can
-- tell a provider's refusal from a response the gateway could not read. Only
-- the gateway's category, the provider's HTTP status and its short error
-- type are kept; never a message or body, which may echo the request.

ALTER TABLE gateway.inference_trace
    ADD COLUMN provider_error_category TEXT
        CHECK (provider_error_category IN ('authentication', 'authorization', 'invalid_request',
                                           'unsupported_feature', 'rate_limited', 'unavailable',
                                           'timeout', 'cancelled', 'provider', 'protocol',
                                           'internal')),
    ADD COLUMN provider_error_status SMALLINT
        CHECK (provider_error_status BETWEEN 100 AND 599),
    ADD COLUMN provider_error_code TEXT
        CHECK (provider_error_code ~ '^[A-Za-z0-9._:/-]{1,64}$'),
    ADD CONSTRAINT inference_trace_provider_error_ck
        CHECK (provider_error_category IS NOT NULL
               OR (provider_error_status IS NULL AND provider_error_code IS NULL));
