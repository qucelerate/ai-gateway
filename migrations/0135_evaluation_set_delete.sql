-- Model cards (Phase 8, D-022): deleting an organization's evaluation set.
--
-- Deletes are soft: `deleted_at` (0113) hides the set from lists, reads and
-- model cards, and `deleted_by` records who deleted it. The set's items, its
-- runs and their scores are kept. The audit event is
-- `model_evaluation_set.delete`, on the `model_evaluation_set` resource type
-- 0110 added.

ALTER TABLE gateway.model_evaluation_set
    ADD COLUMN deleted_by UUID REFERENCES gateway.user_account (id) ON DELETE SET NULL;
