-- A request every deployment of its route refused before doing any work
-- (a provider 429, or a status its adapter documents as a refusal) used
-- nothing: its reservation is released, charging nothing, rather than left
-- unknown for reconciliation (D-030, load and resilience testing). The
-- release is a finalization like a settlement, so a retry after a lost
-- commit reply replays it.
ALTER TABLE gateway.accounting_finalization
    DROP CONSTRAINT accounting_finalization_outcome_check;
ALTER TABLE gateway.accounting_finalization
    ADD CONSTRAINT accounting_finalization_outcome_check CHECK (outcome IN (
        'settled', 'unknown', 'bound-breach', 'settlement-failed', 'input-count-drift',
        'refused'
    ));
