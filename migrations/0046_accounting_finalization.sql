-- Settlement idempotency. Each finalization (a settlement, or an input-count
-- drift reconciliation) carries an id chosen before its first attempt, and
-- this row commits in the same transaction as the finalization. A retry after
-- a lost commit reply finds the row and replays the recorded outcome, so
-- usage, quota and events are never applied twice. Retries stop within 15
-- minutes; the worker sweeps rows after 7 days.
CREATE TABLE gateway.accounting_finalization (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL,
    reservation_id UUID NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN (
        'settled', 'unknown', 'bound-breach', 'settlement-failed', 'input-count-drift'
    )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT accounting_finalization_reservation_fk FOREIGN KEY (organization_id, reservation_id)
        REFERENCES gateway.accounting_reservation (organization_id, id) ON DELETE CASCADE
);

CREATE INDEX accounting_finalization_created_idx
    ON gateway.accounting_finalization (created_at);
