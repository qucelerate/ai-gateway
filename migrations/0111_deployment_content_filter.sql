-- Model cards milestone 3 (Phase 8, D-022): the content filter policy each
-- listed deployment applies, as the cloud reports it (Azure's
-- `raiPolicyName`), and that policy's settings from the account's policy
-- list when it could be read. Both are replaced by each listing, like the
-- other deployment details.

ALTER TABLE gateway.catalog_discovered_model
    ADD COLUMN deployment_content_filter TEXT
        CHECK (octet_length(deployment_content_filter) BETWEEN 1 AND 128),
    ADD COLUMN deployment_content_filter_policy JSONB
        CHECK (
            jsonb_typeof(deployment_content_filter_policy) = 'object'
            AND octet_length(deployment_content_filter_policy::text) <= 65536
        );
