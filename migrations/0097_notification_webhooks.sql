-- Notifications, milestone 4 (Phase 10): more kinds (budgets reaching their
-- threshold, API keys and agent tokens near expiry, webhook endpoints
-- failing), and each new notification sent to webhooks as
-- `notification.created`. Kinds are free text checked by pattern, so only
-- the event type lists change.
ALTER TABLE gateway.outbox_event
    DROP CONSTRAINT outbox_event_event_type_check,
    ADD CONSTRAINT outbox_event_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved', 'usage_export.failing', 'notification.created'
    ));

ALTER TABLE gateway.webhook_endpoint
    DROP CONSTRAINT webhook_endpoint_event_types_check,
    ADD CONSTRAINT webhook_endpoint_event_types_check CHECK (
        cardinality(event_types) > 0
        AND event_types <@ ARRAY['inference.finalized', 'reconciliation.opened',
                                 'reconciliation.updated', 'reconciliation.resolved',
                                 'usage_export.failing', 'notification.created']::TEXT[]
    );

ALTER TABLE gateway.webhook_delivery
    DROP CONSTRAINT webhook_delivery_event_type_check,
    ADD CONSTRAINT webhook_delivery_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved', 'usage_export.failing', 'notification.created'
    ));

-- The condition sweep finds keys and tokens near expiry without a full scan.
CREATE INDEX api_key_expiring_idx ON gateway.api_key (expires_at)
    WHERE status = 'active' AND expires_at IS NOT NULL;
CREATE INDEX agent_token_expiring_idx ON gateway.agent_token (expires_at)
    WHERE revoked_at IS NULL AND expires_at IS NOT NULL;
