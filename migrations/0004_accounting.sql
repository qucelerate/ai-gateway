-- Phase 1C hard-budget accounting. Every monetary value is a checked bigint
-- in the immutable currency snapshot; no provider or request body is stored.
CREATE UNIQUE INDEX service_account_scope_uq
  ON gateway.service_account (organization_id, id, team_id);

CREATE TABLE gateway.quota_bucket (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL,
  team_id UUID,
  service_account_id UUID,
  scope_kind TEXT NOT NULL CHECK (scope_kind IN ('organization','team','service-account')),
  period_start TIMESTAMPTZ NOT NULL,
  period_end TIMESTAMPTZ NOT NULL CHECK (period_end > period_start),
  currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  limit_minor BIGINT NOT NULL CHECK (limit_minor >= 0),
  reserved_minor BIGINT NOT NULL DEFAULT 0 CHECK (reserved_minor >= 0),
  consumed_minor BIGINT NOT NULL DEFAULT 0 CHECK (consumed_minor >= 0),
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT quota_bucket_org_id_uq UNIQUE (organization_id,id),
  CONSTRAINT quota_bucket_org_fk FOREIGN KEY (organization_id) REFERENCES gateway.organization(id) ON DELETE CASCADE,
  CONSTRAINT quota_bucket_team_fk FOREIGN KEY (organization_id, team_id) REFERENCES gateway.team(organization_id, id) ON DELETE CASCADE,
  CONSTRAINT quota_bucket_service_fk FOREIGN KEY (organization_id, service_account_id, team_id) REFERENCES gateway.service_account(organization_id, id, team_id) ON DELETE CASCADE,
  CONSTRAINT quota_bucket_service_account_fk FOREIGN KEY (organization_id, service_account_id) REFERENCES gateway.service_account(organization_id, id) ON DELETE CASCADE,
  CONSTRAINT quota_bucket_scope_ck CHECK (
    (scope_kind='organization' AND team_id IS NULL AND service_account_id IS NULL) OR
    (scope_kind='team' AND team_id IS NOT NULL AND service_account_id IS NULL) OR
    (scope_kind='service-account' AND service_account_id IS NOT NULL)
  )
);
CREATE UNIQUE INDEX quota_bucket_scope_uq ON gateway.quota_bucket
  (organization_id, COALESCE(team_id, '00000000-0000-0000-0000-000000000000'::uuid),
    COALESCE(service_account_id, '00000000-0000-0000-0000-000000000000'::uuid), period_start, period_end, currency);
CREATE INDEX quota_bucket_expiry_idx ON gateway.quota_bucket(period_end);

CREATE TABLE gateway.accounting_reservation (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL,
  team_id UUID,
  service_account_id UUID,
  idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 255),
  fingerprint BYTEA NOT NULL CHECK (octet_length(fingerprint)=32),
  state TEXT NOT NULL CHECK (state IN ('reserved','claimed','settled','released','unknown','expired')),
  reserved_minor BIGINT NOT NULL CHECK (reserved_minor >= 0),
  expires_at TIMESTAMPTZ NOT NULL,
  output_token_cap BIGINT NOT NULL CHECK (output_token_cap > 0),
  input_price_minor BIGINT NOT NULL CHECK (input_price_minor >= 0),
  output_price_minor BIGINT NOT NULL CHECK (output_price_minor >= 0),
  cached_input_price_minor BIGINT NOT NULL CHECK (cached_input_price_minor >= 0),
  reasoning_price_minor BIGINT NOT NULL CHECK (reasoning_price_minor >= 0),
  other_price_minor BIGINT NOT NULL CHECK (other_price_minor >= 0),
  price_version TEXT NOT NULL CHECK (length(price_version) BETWEEN 1 AND 128),
  pricing_confidence TEXT NOT NULL CHECK (pricing_confidence IN ('known','conservative')),
  currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  deployment TEXT NOT NULL CHECK (length(deployment) BETWEEN 1 AND 255),
  model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 255),
  trace_id UUID NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  claimed_at TIMESTAMPTZ,
  settled_at TIMESTAMPTZ,
  settlement_fingerprint BYTEA CHECK (settlement_fingerprint IS NULL OR octet_length(settlement_fingerprint)=32),
  CONSTRAINT accounting_reservation_org_fk FOREIGN KEY (organization_id) REFERENCES gateway.organization(id) ON DELETE CASCADE,
  CONSTRAINT accounting_reservation_team_fk FOREIGN KEY (organization_id, team_id) REFERENCES gateway.team(organization_id, id) ON DELETE CASCADE,
  CONSTRAINT accounting_reservation_service_fk FOREIGN KEY (organization_id, service_account_id, team_id) REFERENCES gateway.service_account(organization_id, id, team_id) ON DELETE CASCADE,
  CONSTRAINT accounting_reservation_service_account_fk FOREIGN KEY (organization_id, service_account_id) REFERENCES gateway.service_account(organization_id, id) ON DELETE CASCADE,
  CONSTRAINT accounting_reservation_scope_ck CHECK (
    (team_id IS NULL AND service_account_id IS NULL) OR service_account_id IS NOT NULL
  ),
  CONSTRAINT accounting_reservation_org_id_uq UNIQUE (organization_id, id),
  CONSTRAINT accounting_reservation_currency_uq UNIQUE (organization_id, id, currency)
);
CREATE UNIQUE INDEX accounting_reservation_idempotency_uq ON gateway.accounting_reservation(organization_id,idempotency_key);
CREATE INDEX accounting_reservation_recovery_idx ON gateway.accounting_reservation(state,expires_at);

CREATE TABLE gateway.accounting_price_snapshot (
  organization_id UUID NOT NULL,
  reservation_id UUID PRIMARY KEY,
  input_price_minor BIGINT NOT NULL CHECK (input_price_minor >= 0),
  output_price_minor BIGINT NOT NULL CHECK (output_price_minor >= 0),
  cached_input_price_minor BIGINT NOT NULL CHECK (cached_input_price_minor >= 0),
  reasoning_price_minor BIGINT NOT NULL CHECK (reasoning_price_minor >= 0),
  other_price_minor BIGINT NOT NULL CHECK (other_price_minor >= 0),
  price_version TEXT NOT NULL CHECK (length(price_version) BETWEEN 1 AND 128),
  pricing_confidence TEXT NOT NULL CHECK (pricing_confidence IN ('known','conservative')),
  currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT accounting_price_snapshot_reservation_org_fk FOREIGN KEY (organization_id,reservation_id) REFERENCES gateway.accounting_reservation(organization_id,id) ON DELETE CASCADE,
  CONSTRAINT accounting_price_snapshot_currency_fk FOREIGN KEY (organization_id,reservation_id,currency) REFERENCES gateway.accounting_reservation(organization_id,id,currency) ON DELETE CASCADE
);

CREATE TABLE gateway.accounting_usage (
  id UUID PRIMARY KEY,
  reservation_id UUID NOT NULL REFERENCES gateway.accounting_reservation(id) ON DELETE RESTRICT,
  organization_id UUID NOT NULL REFERENCES gateway.organization(id) ON DELETE CASCADE,
  currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  confidence TEXT NOT NULL CHECK (confidence IN ('exact','estimated','unknown')),
  input_tokens BIGINT CHECK (input_tokens IS NULL OR input_tokens >= 0),
  output_tokens BIGINT CHECK (output_tokens IS NULL OR output_tokens >= 0),
  cached_input_tokens BIGINT CHECK (cached_input_tokens IS NULL OR cached_input_tokens >= 0),
  reasoning_tokens BIGINT CHECK (reasoning_tokens IS NULL OR reasoning_tokens >= 0),
  other_tokens BIGINT CHECK (other_tokens IS NULL OR other_tokens >= 0),
  amount_minor BIGINT CHECK (amount_minor IS NULL OR amount_minor >= 0),
  completeness TEXT NOT NULL CHECK (completeness IN ('complete','partial','unavailable')),
  finality TEXT NOT NULL CHECK (finality IN ('interim','partial','final')),
  recorded_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT accounting_usage_reservation_org_fk FOREIGN KEY (organization_id,reservation_id) REFERENCES gateway.accounting_reservation(organization_id,id) ON DELETE RESTRICT,
  CONSTRAINT accounting_usage_currency_fk FOREIGN KEY (organization_id,reservation_id,currency) REFERENCES gateway.accounting_reservation(organization_id,id,currency) ON DELETE RESTRICT,
  CONSTRAINT accounting_usage_currency_ck CHECK (currency ~ '^[A-Z]{3}$')
);
CREATE INDEX accounting_usage_reservation_idx ON gateway.accounting_usage(reservation_id,recorded_at);

CREATE TABLE gateway.accounting_allocation (
  id UUID PRIMARY KEY,
  reservation_id UUID NOT NULL REFERENCES gateway.accounting_reservation(id) ON DELETE RESTRICT,
  quota_bucket_id UUID NOT NULL REFERENCES gateway.quota_bucket(id) ON DELETE RESTRICT,
  organization_id UUID NOT NULL REFERENCES gateway.organization(id) ON DELETE CASCADE,
  reserved_minor BIGINT NOT NULL CHECK (reserved_minor >= 0),
  settled_minor BIGINT,
  state TEXT NOT NULL CHECK (state IN ('held','settled','released')),
  usage_id UUID REFERENCES gateway.accounting_usage(id) ON DELETE RESTRICT,
  released_at TIMESTAMPTZ,
  CONSTRAINT accounting_allocation_reservation_org_fk FOREIGN KEY (organization_id,reservation_id) REFERENCES gateway.accounting_reservation(organization_id,id) ON DELETE RESTRICT,
  CONSTRAINT accounting_allocation_bucket_org_fk FOREIGN KEY (organization_id,quota_bucket_id) REFERENCES gateway.quota_bucket(organization_id,id) ON DELETE RESTRICT,
  CONSTRAINT accounting_allocation_settlement_ck CHECK ((state='held' AND settled_minor IS NULL AND usage_id IS NULL) OR (state='settled' AND settled_minor IS NOT NULL AND usage_id IS NOT NULL) OR (state='released' AND settled_minor IS NULL AND usage_id IS NULL))
);
CREATE UNIQUE INDEX accounting_allocation_reservation_bucket_uq ON gateway.accounting_allocation(reservation_id,quota_bucket_id);
CREATE INDEX accounting_allocation_recovery_idx ON gateway.accounting_allocation(reservation_id,state);

-- Safe metadata only. Content-bearing provider fields never have a storage
-- column and the JSON object is a fixed-shape application value.
CREATE TABLE gateway.inference_trace (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL REFERENCES gateway.organization(id) ON DELETE CASCADE,
  team_id UUID,
  service_account_id UUID,
  reservation_id UUID NOT NULL REFERENCES gateway.accounting_reservation(id) ON DELETE RESTRICT,
  correlation_id UUID NOT NULL,
  operation TEXT NOT NULL CHECK (operation='inference'),
  status TEXT NOT NULL CHECK (status IN ('pending','finalized')),
  outcome TEXT,
  provider_request_id TEXT,
  model TEXT NOT NULL,
  input_bound BIGINT NOT NULL CHECK (input_bound >= 0),
  output_cap BIGINT NOT NULL CHECK (output_cap > 0),
  price_version TEXT NOT NULL CHECK (length(price_version) BETWEEN 1 AND 128),
  pricing_confidence TEXT NOT NULL CHECK (pricing_confidence IN ('known','conservative')),
  currency CHAR(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  amount_minor BIGINT CHECK (amount_minor IS NULL OR amount_minor >= 0),
  metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
  recorded_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  finalized_at TIMESTAMPTZ,
  CONSTRAINT inference_trace_team_fk FOREIGN KEY (organization_id,team_id) REFERENCES gateway.team(organization_id,id) ON DELETE CASCADE,
  CONSTRAINT inference_trace_service_fk FOREIGN KEY (organization_id,service_account_id,team_id) REFERENCES gateway.service_account(organization_id,id,team_id) ON DELETE CASCADE,
  CONSTRAINT inference_trace_service_account_fk FOREIGN KEY (organization_id,service_account_id) REFERENCES gateway.service_account(organization_id,id) ON DELETE CASCADE,
  CONSTRAINT inference_trace_scope_ck CHECK ((team_id IS NULL AND service_account_id IS NULL) OR service_account_id IS NOT NULL),
  CONSTRAINT inference_trace_reservation_org_fk FOREIGN KEY (organization_id,reservation_id) REFERENCES gateway.accounting_reservation(organization_id,id) ON DELETE RESTRICT,
  CONSTRAINT inference_trace_currency_fk FOREIGN KEY (organization_id,reservation_id,currency) REFERENCES gateway.accounting_reservation(organization_id,id,currency) ON DELETE RESTRICT,
  CONSTRAINT inference_trace_metadata_ck CHECK (metadata = jsonb_build_object('input_bound',to_jsonb(input_bound),'output_cap',to_jsonb(output_cap),'currency',to_jsonb(btrim(currency))))
);
CREATE INDEX inference_trace_org_idx ON gateway.inference_trace(organization_id,recorded_at DESC);

CREATE TABLE gateway.accounting_reconciliation (
  id UUID PRIMARY KEY,
  organization_id UUID NOT NULL,
  reservation_id UUID NOT NULL,
  reason TEXT NOT NULL CHECK (reason IN ('provider-unknown','expired-after-claim','bound-breach','settlement-failed')),
  status TEXT NOT NULL CHECK (status IN ('open','resolved')) DEFAULT 'open',
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  resolved_at TIMESTAMPTZ,
  CONSTRAINT accounting_reconciliation_reservation_fk FOREIGN KEY (organization_id,reservation_id) REFERENCES gateway.accounting_reservation(organization_id,id) ON DELETE RESTRICT,
  CONSTRAINT accounting_reconciliation_uq UNIQUE (reservation_id),
  CONSTRAINT accounting_reconciliation_status_ck CHECK ((status='open' AND resolved_at IS NULL) OR (status='resolved' AND resolved_at IS NOT NULL))
);
CREATE INDEX accounting_reconciliation_open_idx ON gateway.accounting_reconciliation(organization_id,created_at) WHERE status='open';

CREATE OR REPLACE FUNCTION gateway.prevent_accounting_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'accounting record is append-only'; END;
$$;
CREATE OR REPLACE FUNCTION gateway.prevent_price_snapshot_change() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.organization_id<>NEW.organization_id OR OLD.team_id IS DISTINCT FROM NEW.team_id OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id OR OLD.idempotency_key<>NEW.idempotency_key OR OLD.fingerprint<>NEW.fingerprint OR OLD.reserved_minor<>NEW.reserved_minor OR OLD.expires_at<>NEW.expires_at OR OLD.output_token_cap<>NEW.output_token_cap OR OLD.input_price_minor<>NEW.input_price_minor OR OLD.output_price_minor<>NEW.output_price_minor OR OLD.cached_input_price_minor<>NEW.cached_input_price_minor OR OLD.reasoning_price_minor<>NEW.reasoning_price_minor OR OLD.other_price_minor<>NEW.other_price_minor OR OLD.price_version<>NEW.price_version OR OLD.pricing_confidence<>NEW.pricing_confidence OR OLD.currency<>NEW.currency OR OLD.deployment<>NEW.deployment OR OLD.model<>NEW.model OR OLD.trace_id<>NEW.trace_id THEN RAISE EXCEPTION 'accounting reservation fields are immutable'; END IF;
  IF OLD.settlement_fingerprint IS NOT NULL AND OLD.settlement_fingerprint IS DISTINCT FROM NEW.settlement_fingerprint THEN RAISE EXCEPTION 'settlement fingerprint is immutable'; END IF;
  IF NOT ((OLD.state='reserved' AND NEW.state IN ('reserved','claimed','expired')) OR (OLD.state='claimed' AND NEW.state IN ('claimed','unknown','settled')) OR (OLD.state='unknown' AND NEW.state IN ('unknown','settled')) OR (OLD.state IN ('settled','expired','released') AND NEW.state=OLD.state)) THEN RAISE EXCEPTION 'invalid accounting reservation transition'; END IF;
  RETURN NEW;
END;
$$;
CREATE OR REPLACE FUNCTION gateway.prevent_allocation_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.reservation_id<>NEW.reservation_id OR OLD.quota_bucket_id<>NEW.quota_bucket_id OR OLD.organization_id<>NEW.organization_id OR OLD.reserved_minor<>NEW.reserved_minor THEN RAISE EXCEPTION 'accounting allocation fields are immutable'; END IF;
  IF OLD.state='held' AND NEW.state='settled' AND (NEW.settled_minor IS NULL OR NEW.usage_id IS NULL OR NEW.released_at IS NOT NULL) THEN RAISE EXCEPTION 'invalid settled allocation'; END IF;
  IF OLD.state='held' AND NEW.state='released' AND (NEW.settled_minor IS NOT NULL OR NEW.usage_id IS NOT NULL OR NEW.released_at IS NULL) THEN RAISE EXCEPTION 'invalid released allocation'; END IF;
  IF OLD.state='held' AND NEW.state='held' AND (NEW.settled_minor IS NOT NULL OR NEW.usage_id IS NOT NULL OR NEW.released_at IS NOT NULL) THEN RAISE EXCEPTION 'invalid held allocation'; END IF;
  IF OLD.state IN ('settled','released') AND (NEW.state<>OLD.state OR OLD.settled_minor IS DISTINCT FROM NEW.settled_minor OR OLD.usage_id IS DISTINCT FROM NEW.usage_id OR OLD.released_at IS DISTINCT FROM NEW.released_at) THEN RAISE EXCEPTION 'allocation state is immutable'; END IF;
  IF OLD.state NOT IN ('held','settled','released') OR NEW.state NOT IN ('held','settled','released') THEN RAISE EXCEPTION 'invalid allocation transition'; END IF;
  RETURN NEW;
END;
$$;
CREATE OR REPLACE FUNCTION gateway.prevent_trace_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF OLD.organization_id<>NEW.organization_id OR OLD.team_id IS DISTINCT FROM NEW.team_id OR OLD.service_account_id IS DISTINCT FROM NEW.service_account_id OR OLD.reservation_id<>NEW.reservation_id OR OLD.correlation_id<>NEW.correlation_id OR OLD.operation<>NEW.operation OR OLD.model<>NEW.model OR OLD.input_bound<>NEW.input_bound OR OLD.output_cap<>NEW.output_cap OR OLD.price_version<>NEW.price_version OR OLD.pricing_confidence<>NEW.pricing_confidence OR OLD.currency<>NEW.currency OR OLD.metadata<>NEW.metadata THEN RAISE EXCEPTION 'inference trace metadata is immutable'; END IF;
  RETURN NEW;
END;
$$;
CREATE TRIGGER accounting_reservation_price_immutable BEFORE UPDATE ON gateway.accounting_reservation FOR EACH ROW EXECUTE FUNCTION gateway.prevent_price_snapshot_change();
CREATE TRIGGER accounting_price_snapshot_immutable BEFORE UPDATE OR DELETE ON gateway.accounting_price_snapshot FOR EACH ROW EXECUTE FUNCTION gateway.prevent_accounting_mutation();
CREATE TRIGGER accounting_usage_append_only BEFORE UPDATE OR DELETE ON gateway.accounting_usage FOR EACH ROW EXECUTE FUNCTION gateway.prevent_accounting_mutation();
CREATE TRIGGER accounting_allocation_immutable BEFORE UPDATE ON gateway.accounting_allocation FOR EACH ROW EXECUTE FUNCTION gateway.prevent_allocation_mutation();
CREATE TRIGGER inference_trace_metadata_immutable BEFORE UPDATE ON gateway.inference_trace FOR EACH ROW EXECUTE FUNCTION gateway.prevent_trace_mutation();
