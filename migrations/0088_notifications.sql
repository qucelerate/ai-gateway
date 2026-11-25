-- Notifications (Phase 10, D-024): what the gateway did, or needs from
-- people, in an organization. Each carries the permission its subject needs,
-- so members see only what their role can.
--
-- A notification is either an event (`resolvable = false`: prices adopted)
-- or a condition (`resolvable = true`: a price source failing), which stays
-- open until what it is about is fixed or acted on. One condition is open
-- per (organization, kind, subject), and one event is kept per subject, so
-- raising either again never adds another.
CREATE TABLE gateway.notification (
    id UUID PRIMARY KEY,
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$' AND length(kind) <= 64),
    -- What it is about, within its kind: a price source, a provider account,
    -- a deployment's price.
    subject TEXT NOT NULL CHECK (length(subject) BETWEEN 1 AND 256),
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'action')),
    resolvable BOOLEAN NOT NULL,
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    body TEXT NOT NULL CHECK (length(body) <= 2000),
    -- A console path.
    link TEXT CHECK (link IS NULL OR (length(link) <= 512 AND link LIKE '/admin/%')),
    required_permission TEXT NOT NULL CHECK (required_permission ~ '^[a-z_]{1,64}$'),
    data JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (octet_length(data::text) <= 16384),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMPTZ,
    CHECK (resolvable OR resolved_at IS NULL)
);

CREATE UNIQUE INDEX notification_open_condition_uq
    ON gateway.notification (organization_id, kind, subject)
    WHERE resolvable AND resolved_at IS NULL;
CREATE UNIQUE INDEX notification_event_uq
    ON gateway.notification (organization_id, kind, subject)
    WHERE NOT resolvable;
-- Listing, newest first.
CREATE INDEX notification_organization_created_idx
    ON gateway.notification (organization_id, created_at DESC, id DESC);
-- Retention.
CREATE INDEX notification_resolved_idx ON gateway.notification (resolved_at)
    WHERE resolved_at IS NOT NULL;
CREATE INDEX notification_event_created_idx ON gateway.notification (created_at)
    WHERE NOT resolvable;

-- Each member's read state is their own: notifications they marked read one
-- by one, and a mark below which (by creation, then id) all are read.
CREATE TABLE gateway.notification_read (
    notification_id UUID NOT NULL REFERENCES gateway.notification (id) ON DELETE CASCADE,
    user_account_id UUID NOT NULL REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    read_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (notification_id, user_account_id)
);
CREATE INDEX notification_read_user_idx ON gateway.notification_read (user_account_id);

CREATE TABLE gateway.notification_read_mark (
    organization_id UUID NOT NULL REFERENCES gateway.organization (id) ON DELETE CASCADE,
    user_account_id UUID NOT NULL REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    through_created_at TIMESTAMPTZ NOT NULL,
    through_id UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (organization_id, user_account_id)
);
