-- `reconciliation.updated`: an open item opened by expiry recovery
-- (`expired-after-claim`) takes the reason of a finalization that lands
-- later, and the change is emitted like the item's other transitions.
ALTER TABLE gateway.outbox_event
    DROP CONSTRAINT outbox_event_event_type_check,
    ADD CONSTRAINT outbox_event_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved'
    ));

ALTER TABLE gateway.webhook_endpoint
    DROP CONSTRAINT webhook_endpoint_event_types_check,
    ADD CONSTRAINT webhook_endpoint_event_types_check CHECK (
        cardinality(event_types) > 0
        AND event_types <@ ARRAY['inference.finalized', 'reconciliation.opened',
                                 'reconciliation.updated', 'reconciliation.resolved']::TEXT[]
    );

ALTER TABLE gateway.webhook_delivery
    DROP CONSTRAINT webhook_delivery_event_type_check,
    ADD CONSTRAINT webhook_delivery_event_type_check CHECK (event_type IN (
        'inference.finalized', 'reconciliation.opened', 'reconciliation.updated',
        'reconciliation.resolved'
    ));
