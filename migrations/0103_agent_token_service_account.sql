-- An agent token limited to one service account (D-028): it reaches only
-- that account's end-user settings, plans and users, so an app's backend
-- manages its own end users and nothing else. NULL: the agent's whole
-- organization, as before.
ALTER TABLE gateway.agent_token
    ADD COLUMN service_account_id UUID,
    ADD CONSTRAINT agent_token_service_account_fk FOREIGN KEY (organization_id, service_account_id)
        REFERENCES gateway.service_account (organization_id, id) ON DELETE CASCADE;
