-- Batches (Phase 7, D-021): a data export can be limited to one batch's
-- lines (`?batch_id=`), and the export history says which batch it was.
-- Usage reports, the usage dashboard, traces and exports read a line's batch
-- from its reservation (`accounting_reservation.batch_id`, indexed by 0089)
-- and its `custom_id` from `batch_line`, found by reservation through
-- `batch_line_reservation_uq`; nothing else needs storing.

ALTER TABLE gateway.data_export ADD COLUMN batch_id UUID;
