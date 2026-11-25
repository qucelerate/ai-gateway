-- OIDC callback replay fencing.  A claimed callback owns the row for a
-- short, pre-dispatch lease; exchange_started and completed are terminal
-- fences and are never reclaimed.
ALTER TABLE gateway.oidc_login_transaction
    ADD COLUMN status TEXT NOT NULL DEFAULT 'pending',
    ADD COLUMN claim_id UUID,
    ADD COLUMN claimed_at TIMESTAMPTZ,
    ADD COLUMN claim_lease_until TIMESTAMPTZ,
    ADD COLUMN exchange_started_at TIMESTAMPTZ,
    ADD COLUMN completed_at TIMESTAMPTZ;

UPDATE gateway.oidc_login_transaction
SET status = 'completed',
    completed_at = COALESCE(consumed_at, CURRENT_TIMESTAMP)
WHERE consumed_at IS NOT NULL;

ALTER TABLE gateway.oidc_login_transaction
    ADD CONSTRAINT oidc_login_transaction_status_ck
        CHECK (status IN ('pending', 'claimed', 'exchange_started', 'completed')),
    ADD CONSTRAINT oidc_login_transaction_state_shape_ck
        CHECK (
            (status = 'pending'
                AND claim_id IS NULL
                AND claimed_at IS NULL
                AND claim_lease_until IS NULL
                AND exchange_started_at IS NULL
                AND completed_at IS NULL
                AND consumed_at IS NULL)
            OR (status = 'claimed'
                AND claim_id IS NOT NULL
                AND claimed_at IS NOT NULL
                AND claim_lease_until IS NOT NULL
                AND exchange_started_at IS NULL
                AND completed_at IS NULL
                AND consumed_at IS NULL)
            OR (status = 'exchange_started'
                AND claim_id IS NOT NULL
                AND claimed_at IS NOT NULL
                AND claim_lease_until IS NOT NULL
                AND exchange_started_at IS NOT NULL
                AND completed_at IS NULL
                AND consumed_at IS NULL)
            OR (status = 'completed'
                AND completed_at IS NOT NULL
                AND consumed_at IS NOT NULL
                AND claim_lease_until IS NULL
                AND ((claim_id IS NULL AND claimed_at IS NULL
                      AND exchange_started_at IS NULL)
                     OR (claim_id IS NOT NULL AND claimed_at IS NOT NULL)))
        );

CREATE INDEX oidc_login_transaction_claim_idx
    ON gateway.oidc_login_transaction (status, claim_lease_until)
    WHERE status = 'claimed';

-- The security-event constraint is intentionally changed only in this
-- forward migration.  Metadata remains the existing identifier-free,
-- method-only object; issuer, subject, state and claim values never enter it.
ALTER TABLE gateway.security_event
    DROP CONSTRAINT IF EXISTS security_event_type_ck,
    ADD CONSTRAINT security_event_type_ck CHECK (
        event_type IN (
            'login.succeeded',
            'login.replay_or_invalid_state',
            'login.backend_unavailable',
            'login.backend_rejected',
            'login.claims_rejected',
            'login.unprovisioned',
            'login.disabled',
            'login.identity_unavailable',
            'logout.requested'
        )
    );
