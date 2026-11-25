-- no-transaction
-- One reservation per rate-limit lease ticket (0145): the unique index that
-- caps a lease at its N admissions in the database. Built concurrently, so
-- reservations keep being written while it builds on a large table; sqlx
-- runs this file outside a transaction (the first line), and it must stay a
-- single statement, since PostgreSQL runs several in one query string as one
-- transaction. If the build fails it leaves an invalid index and the next run
-- stops on "already exists": drop the index and migrate again
-- (docs/operations.md, rate-limit leases).
CREATE UNIQUE INDEX CONCURRENTLY accounting_reservation_rate_lease_ticket_idx
    ON gateway.accounting_reservation (rate_lease_id, rate_lease_ticket)
    WHERE rate_lease_id IS NOT NULL
