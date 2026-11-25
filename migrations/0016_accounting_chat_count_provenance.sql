-- Catalog-backed chat accounting records the input-counting mode. The marker
-- makes old binaries, which could use an estimate, fail closed at the
-- database boundary once catalog provenance is present.
ALTER TABLE gateway.accounting_reservation
    ADD COLUMN catalog_chat_input_count_mode TEXT;

ALTER TABLE gateway.inference_trace
    ADD COLUMN catalog_chat_input_count_mode TEXT;
