-- An end user's settled charges by month, for their spend this month
-- (read from the ledger, which outlives traces).
CREATE INDEX accounting_reservation_end_user_settled_idx
    ON gateway.accounting_reservation (end_user_id, settled_at)
    WHERE end_user_id IS NOT NULL AND state = 'settled';
