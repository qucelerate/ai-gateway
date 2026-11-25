-- A person's photo, from the identity provider at sign-in, for display only.
-- Kept apart from user_account so that rows read on every request stay small.
CREATE TABLE gateway.user_profile_photo (
    user_id UUID PRIMARY KEY REFERENCES gateway.user_account (id) ON DELETE CASCADE,
    content_type TEXT NOT NULL CHECK (content_type IN ('image/jpeg', 'image/png')),
    photo BYTEA NOT NULL CHECK (octet_length(photo) BETWEEN 1 AND 262144),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
