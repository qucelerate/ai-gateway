-- Administrative audit events retain the historical team scope even after the
-- scoped resource is deleted. The team id is an immutable historical value,
-- not a live-resource dependency, so a foreign key would make legitimate team
-- deletion impossible once scoped audits exist.
ALTER TABLE gateway.audit_event
    DROP CONSTRAINT IF EXISTS audit_event_team_fk;

-- 0023 backfilled versions before later test-created or externally-created
-- bindings could appear. Seed every existing binding that is missing one while
-- retaining any version already used by a live deployment.
INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
SELECT organization_id, 'role_binding', id
FROM gateway.admin_role_binding
ON CONFLICT DO NOTHING;
