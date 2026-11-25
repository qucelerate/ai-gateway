#![allow(clippy::await_holding_lock)] // deliberate DB-guard serialization pattern in tests
use std::{
    env,
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration as StdDuration,
};

use chrono::{Duration, Utc};
use gateway::{
    AdminIdempotencyKey, AdminMutationRequest, AdminOperation, AdminPermission, AdminScope,
    AdminStore, ApiKeyPepper, ApiKeyScope, AuthStore, IdempotencyResultMetadata, IdentityStore,
    IfMatch, MembershipRole, MutationStart, ResourceVersion,
    domain::{
        audit::{
            AdminAuditEvent, AuditAction, AuditActor, AuditOutcome, AuditSnapshot,
            AuditTargetResource,
        },
        auth::AuthCrypto,
        identity::{ApiKeyId, ServiceAccountId},
    },
    team_membership_version_id,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

static DATABASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn database_guard() -> MutexGuard<'static, ()> {
    DATABASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tokio::test]
async fn admin_authorization_revalidates_org_and_precise_team_membership() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("skipping admin authorization regression: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"admin-authorization-regression-pepper").unwrap(),
    );
    let admin = AdminStore::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let org_a = store
        .create_organization(&format!("admin-a-{suffix}"), "A")
        .await
        .unwrap();
    let org_b = store
        .create_organization(&format!("admin-b-{suffix}"), "B")
        .await
        .unwrap();
    let team_a = store.create_team(org_a.id, "A team").await.unwrap();
    let team_b = store.create_team(org_a.id, "B team").await.unwrap();
    let owner = store
        .create_user(org_a.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    store
        .add_organization_membership(org_a.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    store
        .add_organization_membership(org_b.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    backfill_admin_role_bindings(&pool).await;
    assert!(
        !admin
            .authorization_context(owner.id, org_a.id)
            .await
            .unwrap()
            .bindings
            .is_empty()
    );
    assert!(
        !admin
            .authorization_context(owner.id, org_b.id)
            .await
            .unwrap()
            .bindings
            .is_empty()
    );
    store
        .set_organization_membership_enabled(org_a.id, owner.id, false)
        .await
        .unwrap();
    assert!(
        admin
            .authorization_context(owner.id, org_a.id)
            .await
            .unwrap()
            .bindings
            .is_empty()
    );
    assert!(
        !admin
            .authorization_context(owner.id, org_b.id)
            .await
            .unwrap()
            .bindings
            .is_empty()
    );

    let team_user = store
        .create_user(org_a.id, None, None, None, Some("team admin"))
        .await
        .unwrap();
    store
        .add_organization_membership(org_a.id, team_user.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_team_membership(org_a.id, team_a.id, team_user.id, MembershipRole::Admin)
        .await
        .unwrap();
    backfill_admin_role_bindings(&pool).await;
    let context = admin
        .authorization_context(team_user.id, org_a.id)
        .await
        .unwrap();
    assert!(context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_a.id
        }
    ));
    assert!(!context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_b.id
        }
    ));
    assert!(!context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Organization(org_a.id)
    ));
    // A team role cannot read/write the organization or another team.
    assert!(!context.allows(
        AdminPermission::OrganizationRead,
        AdminScope::Organization(org_a.id)
    ));
    assert!(!context.allows(
        AdminPermission::ApiKeysList,
        AdminScope::Organization(org_a.id)
    ));
    assert!(!context.allows(
        AdminPermission::TeamRead,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_b.id
        }
    ));
    assert!(!context.allows(
        AdminPermission::ApiKeyRead,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_b.id
        }
    ));
    assert!(!context.allows(
        AdminPermission::MemberMutate,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_b.id
        }
    ));
    // Valid team-scoped reads and writes still succeed on the bound team.
    assert!(context.allows(
        AdminPermission::TeamRead,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_a.id
        }
    ));
    assert!(context.allows(
        AdminPermission::ApiKeyRead,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_a.id
        }
    ));
    assert!(context.allows(
        AdminPermission::ServiceAccountMutate,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_a.id
        }
    ));
    store
        .set_team_membership_enabled(org_a.id, team_a.id, team_user.id, false)
        .await
        .unwrap();
    let disabled_team_context = admin
        .authorization_context(team_user.id, org_a.id)
        .await
        .unwrap();
    assert!(!disabled_team_context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Team {
            organization_id: org_a.id,
            team_id: team_a.id
        }
    ));
    pool.close().await;
}

#[tokio::test]
async fn admin_role_binding_constraints_and_backfill_are_fail_closed() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "skipping admin binding constraint regression: GATEWAY_TEST_DATABASE_URL unset"
            );
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"admin-binding-constraint-pepper").unwrap(),
    );
    let admin = AdminStore::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = store
        .create_organization(&format!("rbac-{suffix}"), "RBAC")
        .await
        .unwrap();
    let team = store
        .create_team(organization.id, "constraints team")
        .await
        .unwrap();
    let owner = store
        .create_user(organization.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    let auditor = store
        .create_user(organization.id, None, None, None, Some("auditor"))
        .await
        .unwrap();
    let member = store
        .create_user(organization.id, None, None, None, Some("member"))
        .await
        .unwrap();
    let teammate = store
        .create_user(organization.id, None, None, None, Some("teammate"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, owner.id, MembershipRole::Owner)
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, auditor.id, MembershipRole::Auditor)
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, member.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, teammate.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_team_membership(organization.id, team.id, owner.id, MembershipRole::Admin)
        .await
        .unwrap();
    store
        .add_team_membership(
            organization.id,
            team.id,
            teammate.id,
            MembershipRole::Member,
        )
        .await
        .unwrap();

    // Re-run the migration's compatibility backfill against this fixture so
    // the backfill semantics are verified without needing a fresh database.
    backfill_admin_role_bindings(&pool).await;

    // Backfill grants explicit admin roles only; a generic member gets no
    // org-wide read binding.
    let owner_roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL",
    )
    .bind(organization.id.0)
    .bind(owner.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(owner_roles, vec!["organization_owner"]);
    let auditor_roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id IS NULL",
    )
    .bind(organization.id.0)
    .bind(auditor.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(auditor_roles, vec!["auditor"]);
    let member_bindings: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(organization.id.0)
    .bind(member.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(member_bindings, 0);
    let owner_team_roles: Vec<String> = sqlx::query_scalar(
        "SELECT role FROM gateway.admin_role_binding
         WHERE organization_id = $1 AND user_id = $2 AND team_id = $3",
    )
    .bind(organization.id.0)
    .bind(owner.id.0)
    .bind(team.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(owner_team_roles, vec!["team_admin"]);

    // No duplicate org-wide binding is possible despite team_id being NULL.
    assert!(
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
             VALUES ($1,$2,$3,'organization_owner')",
        )
        .bind(Uuid::new_v4())
        .bind(organization.id.0)
        .bind(owner.id.0)
        .execute(&pool)
        .await
        .is_err()
    );

    // A team binding must reference a real membership of that user in that team.
    assert!(
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role)
             VALUES ($1,$2,$3,$4,'team_admin')",
        )
        .bind(Uuid::new_v4())
        .bind(organization.id.0)
        .bind(member.id.0)
        .bind(team.id.0)
        .execute(&pool)
        .await
        .is_err()
    );

    // Role scope validity: team bindings must be team_admin and org-wide
    // bindings must never be team_admin.
    assert!(
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role)
             VALUES ($1,$2,$3,$4,'operator')",
        )
        .bind(Uuid::new_v4())
        .bind(organization.id.0)
        .bind(owner.id.0)
        .bind(team.id.0)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
             VALUES ($1,$2,$3,'team_admin')",
        )
        .bind(Uuid::new_v4())
        .bind(organization.id.0)
        .bind(owner.id.0)
        .execute(&pool)
        .await
        .is_err()
    );

    // A valid team binding is accepted, then disabling the team membership
    // immediately removes that team scope while the org-wide grant persists.
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role)
         VALUES ($1,$2,$3,$4,'team_admin')",
    )
    .bind(Uuid::new_v4())
    .bind(organization.id.0)
    .bind(teammate.id.0)
    .bind(team.id.0)
    .execute(&pool)
    .await
    .unwrap();
    let team_context = admin
        .authorization_context(teammate.id, organization.id)
        .await
        .unwrap();
    assert!(team_context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Team {
            organization_id: organization.id,
            team_id: team.id
        }
    ));
    assert!(!team_context.allows(
        AdminPermission::OrganizationRead,
        AdminScope::Organization(organization.id)
    ));
    store
        .set_team_membership_enabled(organization.id, team.id, teammate.id, false)
        .await
        .unwrap();
    let disabled_context = admin
        .authorization_context(teammate.id, organization.id)
        .await
        .unwrap();
    assert!(!disabled_context.allows(
        AdminPermission::ApiKeyRotate,
        AdminScope::Team {
            organization_id: organization.id,
            team_id: team.id
        }
    ));
    assert!(disabled_context.bindings.is_empty());

    let owner_context = admin
        .authorization_context(owner.id, organization.id)
        .await
        .unwrap();
    assert!(owner_context.allows(
        AdminPermission::OrganizationRead,
        AdminScope::Organization(organization.id)
    ));
    pool.close().await;
}

#[tokio::test]
async fn admin_coordinator_rolls_back_audit_failure_and_fences_replays() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("skipping admin coordinator regression: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"admin-coordinator-regression-pepper").unwrap(),
    );
    let admin = AdminStore::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = store
        .create_organization(&format!("coordinator-{suffix}"), "Before")
        .await
        .unwrap();
    let actor = store
        .create_user(organization.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, actor.id, MembershipRole::Owner)
        .await
        .unwrap();
    backfill_admin_role_bindings(&pool).await;
    // The deploy-time migration backfills version rows only for resources that
    // already exist; this fixture was created after migrations, so seed the
    // organization's optimistic-concurrency row the coordinator requires.
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
         VALUES ($1, 'organization', $1)
         ON CONFLICT DO NOTHING",
    )
    .bind(organization.id.0)
    .execute(&pool)
    .await
    .unwrap();
    let operation = AdminOperation::new("organization.update").unwrap();
    let key = AdminIdempotencyKey::new("coordinator-key").unwrap();
    let invalid_request = AdminMutationRequest {
        actor_id: actor.id,
        organization_id: organization.id,
        scope: AdminScope::Organization(organization.id),
        permission: AdminPermission::OrganizationMutate,
        route: "/api/admin/v1/organizations/{id}".into(),
        method: "PATCH".into(),
        operation: operation.clone(),
        idempotency_key: key.clone(),
        target_id: organization.id.0,
        parent_id: None,
        target: AuditTargetResource::Organization,
        audit_action: AuditAction::new("organization.updated").unwrap(),
        if_match: IfMatch::Exact(ResourceVersion::INITIAL),
        request_id: Uuid::nil(),
        body: serde_json::json!({"name":"Must roll back"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(invalid_request).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("first request cannot replay"),
    };
    sqlx::query("UPDATE gateway.organization SET name = 'Must roll back' WHERE id = $1")
        .bind(organization.id.0)
        .execute(&mut **mutation.transaction())
        .await
        .unwrap();
    assert!(
        mutation
            .commit(None, Some(AuditSnapshot::default()), None)
            .await
            .is_err()
    );
    let name: String = sqlx::query_scalar("SELECT name FROM gateway.organization WHERE id = $1")
        .bind(organization.id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Before");
    assert_eq!(
        admin
            .current_version(organization.id, "organization", organization.id.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );

    let valid_request = AdminMutationRequest {
        actor_id: actor.id,
        organization_id: organization.id,
        scope: AdminScope::Organization(organization.id),
        permission: AdminPermission::OrganizationMutate,
        route: "/api/admin/v1/organizations/{id}".into(),
        method: "PATCH".into(),
        operation,
        idempotency_key: key,
        target_id: organization.id.0,
        parent_id: None,
        target: AuditTargetResource::Organization,
        audit_action: AuditAction::new("organization.updated").unwrap(),
        if_match: IfMatch::Exact(ResourceVersion::INITIAL),
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"name":"After"}),
    };
    let mut mutation = match coordinator.begin(valid_request.clone()).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("first valid request cannot replay"),
    };
    sqlx::query("UPDATE gateway.organization SET name = 'After' WHERE id = $1")
        .bind(organization.id.0)
        .execute(&mut **mutation.transaction())
        .await
        .unwrap();
    let next_version = mutation.next_version().get();
    mutation
        .commit(
            None,
            Some(AuditSnapshot {
                version: Some(next_version),
                ..Default::default()
            }),
            Some(IdempotencyResultMetadata {
                resource_id: organization.id.0,
                version: Some(2),
                secret_unavailable_on_replay: false,
                snapshot: None,
            }),
        )
        .await
        .unwrap();
    let different_body = AdminMutationRequest {
        body: serde_json::json!({"name":"Different"}),
        ..valid_request.clone()
    };
    assert!(matches!(
        admin.coordinator().begin(different_body).await,
        Err(gateway::AdminStoreError::IdempotencyConflict)
    ));
    let stale = AdminMutationRequest {
        idempotency_key: AdminIdempotencyKey::new("stale-key").unwrap(),
        if_match: IfMatch::Exact(ResourceVersion::INITIAL),
        ..valid_request
    };
    assert!(matches!(
        admin.coordinator().begin(stale).await,
        Err(gateway::AdminStoreError::VersionConflict(_))
    ));
    pool.close().await;
}

/// Seed an owner-run admin fixture: an organization, an owner actor with the
/// backfilled org-wide binding, and the organization's version row (the
/// migration backfills version rows only for pre-existing resources).
async fn admin_owner_fixture(
    pool: &PgPool,
    slug: &str,
) -> (
    IdentityStore,
    AdminStore,
    gateway::OrganizationId,
    gateway::UserAccountId,
) {
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"admin-create-path-fixture-pepper").unwrap(),
    );
    let admin = AdminStore::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = store
        .create_organization(&format!("{slug}-{suffix}"), slug)
        .await
        .unwrap();
    let actor = store
        .create_user(organization.id, None, None, None, Some("owner"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, actor.id, MembershipRole::Owner)
        .await
        .unwrap();
    backfill_admin_role_bindings(pool).await;
    sqlx::query(
        "INSERT INTO gateway.admin_resource_version (organization_id, resource_type, resource_id)
         VALUES ($1, 'organization', $1)
         ON CONFLICT DO NOTHING",
    )
    .bind(organization.id.0)
    .execute(pool)
    .await
    .unwrap();
    (store, admin, organization.id, actor.id)
}

#[tokio::test]
async fn admin_key_create_uses_pregenerated_id_and_version_one() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("skipping admin key create regression: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let (store, admin, organization, actor) = admin_owner_fixture(&pool, "key-create").await;
    let service_account = store
        .create_service_account(organization, actor, None, "key create service")
        .await
        .unwrap();

    // The coordinator and the identity primitive share one pre-generated id so
    // target/version/audit/idempotency all reference the actual created key.
    let key_id = ApiKeyId::new();
    let request = AdminMutationRequest {
        actor_id: actor,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::ApiKeyCreate,
        route: "/api/admin/v1/service-accounts/{id}/keys".into(),
        method: "POST".into(),
        operation: AdminOperation::new("api_key.create").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("create-key-1").unwrap(),
        target_id: key_id.0,
        parent_id: Some(service_account.id.0),
        target: AuditTargetResource::ApiKey,
        audit_action: AuditAction::new("api_key.created").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"scopes":["inference"]}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(request.clone()).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh create cannot replay"),
    };
    assert_eq!(
        mutation.next_version(),
        ResourceVersion::INITIAL,
        "the first created version must be 1, not 2"
    );
    let created = store
        .create_api_key_in_transaction(
            &mut mutation.transaction(),
            organization,
            service_account.id,
            &[ApiKeyScope::Inference],
            None,
            None,
            key_id,
        )
        .await
        .unwrap();
    assert_eq!(created.id, key_id);
    mutation
        .commit(
            None,
            Some(AuditSnapshot {
                version: Some(1),
                ..Default::default()
            }),
            Some(IdempotencyResultMetadata {
                resource_id: key_id.0,
                version: Some(1),
                secret_unavailable_on_replay: false,
                snapshot: None,
            }),
        )
        .await
        .unwrap();

    let row_id: Uuid = sqlx::query_scalar("SELECT id FROM gateway.api_key WHERE id = $1")
        .bind(key_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        row_id, key_id.0,
        "api_key row must use the coordinator target id"
    );
    assert_eq!(
        admin
            .current_version(organization, "api_key", key_id.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );
    let (entity_id, outcome): (Uuid, String) = sqlx::query_as(
        "SELECT entity_id, outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND entity_type = 'api_key' AND request_id = $2",
    )
    .bind(organization.0)
    .bind(request.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        entity_id, key_id.0,
        "audit target must equal the created key id"
    );
    assert_eq!(outcome, "succeeded");
    let (resource_id, response_version): (Option<Uuid>, Option<serde_json::Value>) =
        sqlx::query_as(
            "SELECT resource_id, response_metadata->'version' FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND idempotency_key = $2",
        )
        .bind(organization.0)
        .bind("create-key-1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        resource_id,
        Some(key_id.0),
        "idempotency must reference the created key"
    );
    assert_eq!(response_version, Some(serde_json::json!(1)));
    pool.close().await;
}

#[tokio::test]
async fn admin_key_rotate_seeds_version_and_successor_is_revocable() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("skipping admin key rotate regression: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let (store, admin, organization, actor) = admin_owner_fixture(&pool, "key-rotate").await;
    let service_account = store
        .create_service_account(organization, actor, None, "rotate service")
        .await
        .unwrap();
    let original = store
        .create_api_key(
            organization,
            service_account.id,
            &[ApiKeyScope::Inference],
            None,
            actor,
        )
        .await
        .unwrap();
    let successor = store
        .rotate_api_key(organization, original.id, actor)
        .await
        .unwrap();
    assert_ne!(successor.id, original.id);
    let (status, revoked_at): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT status, revoked_at FROM gateway.api_key WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization.0)
    .bind(original.id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status, "active",
        "rotation must leave the predecessor active"
    );
    assert!(revoked_at.is_none());
    // The successor's version row is seeded atomically with creation.
    assert_eq!(
        admin
            .current_version(organization, "api_key", successor.id.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );

    // A coordinator revoke of the successor resolves its seeded version row.
    let revoke = AdminMutationRequest {
        actor_id: actor,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::ApiKeyRevoke,
        route: "/api/admin/v1/api-keys/{id}".into(),
        method: "POST".into(),
        operation: AdminOperation::new("api_key.revoke").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("revoke-successor").unwrap(),
        target_id: successor.id.0,
        parent_id: None,
        target: AuditTargetResource::ApiKey,
        audit_action: AuditAction::new("api_key.revoked").unwrap(),
        if_match: IfMatch::Exact(ResourceVersion::INITIAL),
        request_id: Uuid::new_v4(),
        body: serde_json::json!({}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(revoke).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh revoke cannot replay"),
    };
    sqlx::query(
        "UPDATE gateway.api_key
         SET status = 'revoked', revoked_at = CURRENT_TIMESTAMP
         WHERE organization_id = $1 AND id = $2 AND status = 'active'",
    )
    .bind(organization.0)
    .bind(successor.id.0)
    .execute(&mut **mutation.transaction())
    .await
    .unwrap();
    mutation.commit(None, None, None).await.unwrap();
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM gateway.api_key WHERE organization_id = $1 AND id = $2")
            .bind(organization.0)
            .bind(successor.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "revoked");
    assert_eq!(
        admin
            .current_version(organization, "api_key", successor.id.0)
            .await
            .unwrap(),
        ResourceVersion::new(2)
    );
    pool.close().await;
}

#[tokio::test]
async fn admin_team_scoped_creates_are_gated_by_scope() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!("skipping admin team create regression: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let store = IdentityStore::new(
        pool.clone(),
        ApiKeyPepper::from_bytes(b"admin-team-create-pepper").unwrap(),
    );
    let admin = AdminStore::new(pool.clone());
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = store
        .create_organization(&format!("team-create-{suffix}"), "Team create")
        .await
        .unwrap();
    let team_a = store.create_team(organization.id, "team a").await.unwrap();
    let team_b = store.create_team(organization.id, "team b").await.unwrap();
    let admin_user = store
        .create_user(organization.id, None, None, None, Some("team admin"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, admin_user.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_team_membership(
            organization.id,
            team_a.id,
            admin_user.id,
            MembershipRole::Admin,
        )
        .await
        .unwrap();
    backfill_admin_role_bindings(&pool).await;

    // Service-account create succeeds only in the team the actor administers.
    let sa_id = ServiceAccountId::new();
    let sa_create = AdminMutationRequest {
        actor_id: admin_user.id,
        organization_id: organization.id,
        scope: AdminScope::Team {
            organization_id: organization.id,
            team_id: team_a.id,
        },
        permission: AdminPermission::ServiceAccountMutate,
        route: "/api/admin/v1/teams/{id}/service-accounts".into(),
        method: "POST".into(),
        operation: AdminOperation::new("service_account.create").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("team-a-sa").unwrap(),
        target_id: sa_id.0,
        parent_id: Some(admin_user.id.0),
        target: AuditTargetResource::ServiceAccount,
        audit_action: AuditAction::new("service_account.created").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"name":"team a service"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(sa_create.clone()).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh team SA create cannot replay"),
    };
    assert_eq!(mutation.next_version(), ResourceVersion::INITIAL);
    sqlx::query(
        "INSERT INTO gateway.service_account (id, organization_id, team_id, owner_user_id, name)
         VALUES ($1,$2,$3,$4,'team a service')",
    )
    .bind(sa_id.0)
    .bind(organization.id.0)
    .bind(team_a.id.0)
    .bind(admin_user.id.0)
    .execute(&mut **mutation.transaction())
    .await
    .unwrap();
    mutation
        .commit(
            None,
            Some(AuditSnapshot::default()),
            Some(IdempotencyResultMetadata {
                resource_id: sa_id.0,
                version: Some(1),
                secret_unavailable_on_replay: false,
                snapshot: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        admin
            .current_version(organization.id, "service_account", sa_id.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );
    // The same create under the wrong team scope fails (no team_b binding and
    // the owner holds no team_b membership to revalidate against).
    let wrong_team = AdminMutationRequest {
        scope: AdminScope::Team {
            organization_id: organization.id,
            team_id: team_b.id,
        },
        idempotency_key: AdminIdempotencyKey::new("wrong-team-sa").unwrap(),
        ..sa_create
    };
    assert!(admin.coordinator().begin(wrong_team).await.is_err());

    // Membership create succeeds in the right team and is versioned under the
    // migration's team-membership identity (md5(team:user)).
    let member = store
        .create_user(organization.id, None, None, None, Some("new member"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, member.id, MembershipRole::Member)
        .await
        .unwrap();
    let membership_create = AdminMutationRequest {
        actor_id: admin_user.id,
        organization_id: organization.id,
        scope: AdminScope::Team {
            organization_id: organization.id,
            team_id: team_a.id,
        },
        permission: AdminPermission::MemberMutate,
        route: "/api/admin/v1/teams/{id}/members".into(),
        method: "POST".into(),
        operation: AdminOperation::new("membership.create").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("team-a-member").unwrap(),
        target_id: member.id.0,
        parent_id: None,
        target: AuditTargetResource::Membership,
        audit_action: AuditAction::new("membership.created").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"role":"member"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(membership_create).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh membership create cannot replay"),
    };
    sqlx::query(
        "INSERT INTO gateway.team_membership (organization_id, team_id, user_id, role)
         VALUES ($1,$2,$3,'member')",
    )
    .bind(organization.id.0)
    .bind(team_a.id.0)
    .bind(member.id.0)
    .execute(&mut **mutation.transaction())
    .await
    .unwrap();
    mutation.commit(None, None, None).await.unwrap();
    let version_id = team_membership_version_id(team_a.id, member.id);
    assert_eq!(
        admin
            .current_version(organization.id, "membership", version_id)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL),
        "team memberships must be versioned under the migration's md5 identity"
    );
    pool.close().await;
}

#[tokio::test]
async fn admin_org_wide_creates_and_denials_are_audited() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "skipping admin org create/denial regression: GATEWAY_TEST_DATABASE_URL unset"
            );
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let (store, admin, organization, actor) = admin_owner_fixture(&pool, "org-create").await;
    let subject = store
        .create_user(organization, None, None, None, Some("subject"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization, subject.id, MembershipRole::Member)
        .await
        .unwrap();

    // Role-binding create at org scope seeds its version row at 1.
    let binding_id = Uuid::new_v4();
    let binding_create = AdminMutationRequest {
        actor_id: actor,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::RoleBindingMutate,
        route: "/api/admin/v1/role-bindings".into(),
        method: "POST".into(),
        operation: AdminOperation::new("role_binding.create").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("create-binding").unwrap(),
        target_id: binding_id,
        parent_id: Some(subject.id.0),
        target: AuditTargetResource::RoleBinding,
        audit_action: AuditAction::new("role_binding.created").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"role":"operator"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(binding_create).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh binding create cannot replay"),
    };
    assert_eq!(mutation.next_version(), ResourceVersion::INITIAL);
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role)
         VALUES ($1,$2,$3,'operator')",
    )
    .bind(binding_id)
    .bind(organization.0)
    .bind(subject.id.0)
    .execute(&mut **mutation.transaction())
    .await
    .unwrap();
    mutation.commit(None, None, None).await.unwrap();
    assert_eq!(
        admin
            .current_version(organization, "role_binding", binding_id)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );

    // Membership create at org scope seeds its version row at 1.
    let new_member = store
        .create_user(organization, None, None, None, Some("new member"))
        .await
        .unwrap();
    let member_create = AdminMutationRequest {
        actor_id: actor,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::MemberMutate,
        route: "/api/admin/v1/members".into(),
        method: "POST".into(),
        operation: AdminOperation::new("membership.create").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("create-org-member").unwrap(),
        target_id: new_member.id.0,
        parent_id: None,
        target: AuditTargetResource::Membership,
        audit_action: AuditAction::new("membership.created").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"role":"member"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(member_create).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh membership create cannot replay"),
    };
    sqlx::query(
        "INSERT INTO gateway.organization_membership (organization_id, user_id, role)
         VALUES ($1,$2,'member')",
    )
    .bind(organization.0)
    .bind(new_member.id.0)
    .execute(&mut **mutation.transaction())
    .await
    .unwrap();
    mutation.commit(None, None, None).await.unwrap();
    assert_eq!(
        admin
            .current_version(organization, "membership", new_member.id.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL)
    );

    // A denied authorization is audited on its own transaction and the
    // resource version is untouched.
    let denied_request = AdminMutationRequest {
        actor_id: subject.id,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::OrganizationMutate,
        route: "/api/admin/v1/organizations/{id}".into(),
        method: "PATCH".into(),
        operation: AdminOperation::new("organization.update").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("denied-org-update").unwrap(),
        target_id: organization.0,
        parent_id: None,
        target: AuditTargetResource::Organization,
        audit_action: AuditAction::new("organization.updated").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"name":"nope"}),
    };
    assert!(matches!(
        admin.coordinator().begin(denied_request.clone()).await,
        Err(gateway::AdminStoreError::Authorization(_))
    ));
    let (outcome, actor_type): (String, String) = sqlx::query_as(
        "SELECT outcome, actor_type FROM gateway.audit_event
         WHERE organization_id = $1 AND request_id = $2",
    )
    .bind(organization.0)
    .bind(denied_request.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outcome, "denied");
    assert_eq!(actor_type, "user");
    assert_eq!(
        admin
            .current_version(organization, "organization", organization.0)
            .await
            .unwrap(),
        Some(ResourceVersion::INITIAL),
        "a denied mutation must not change resource state"
    );

    // The standalone audit path accepts every actor variant without secrets.
    admin
        .record_audit(AdminAuditEvent {
            actor: AuditActor::System,
            organization_id: organization,
            team_id: None,
            target: AuditTargetResource::Organization,
            action: AuditAction::new("organization.denied").unwrap(),
            target_id: organization.0,
            outcome: AuditOutcome::Denied,
            request_id: Uuid::new_v4(),
            before: None,
            after: None,
        })
        .await
        .unwrap();
    let system_row: (String, String) = sqlx::query_as(
        "SELECT actor_type, outcome FROM gateway.audit_event
         WHERE organization_id = $1 AND actor_type = 'system' AND action = 'organization.denied'",
    )
    .bind(organization.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(system_row, ("system".to_owned(), "denied".to_owned()));
    pool.close().await;
}

#[tokio::test]
async fn admin_idempotency_none_version_metadata_is_omitted() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "skipping admin idempotency metadata regression: GATEWAY_TEST_DATABASE_URL unset"
            );
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let (_, admin, organization, actor) = admin_owner_fixture(&pool, "meta-omit").await;
    let request = AdminMutationRequest {
        actor_id: actor,
        organization_id: organization,
        scope: AdminScope::Organization(organization),
        permission: AdminPermission::OrganizationMutate,
        route: "/api/admin/v1/organizations/{id}".into(),
        method: "PATCH".into(),
        operation: AdminOperation::new("organization.update").unwrap(),
        idempotency_key: AdminIdempotencyKey::new("meta-none-version").unwrap(),
        target_id: organization.0,
        parent_id: None,
        target: AuditTargetResource::Organization,
        audit_action: AuditAction::new("organization.updated").unwrap(),
        if_match: IfMatch::Any,
        request_id: Uuid::new_v4(),
        body: serde_json::json!({"name":"no version"}),
    };
    let coordinator = admin.coordinator();
    let mut mutation = match coordinator.begin(request).await.unwrap() {
        MutationStart::New(mutation) => mutation,
        MutationStart::Replay(_) => panic!("a fresh update cannot replay"),
    };
    sqlx::query("UPDATE gateway.organization SET name = 'no version' WHERE id = $1")
        .bind(organization.0)
        .execute(&mut **mutation.transaction())
        .await
        .unwrap();
    mutation
        .commit(
            None,
            None,
            Some(IdempotencyResultMetadata {
                resource_id: organization.0,
                version: None,
                secret_unavailable_on_replay: false,
                snapshot: None,
            }),
        )
        .await
        .unwrap();
    let (metadata,): (Option<serde_json::Value>,) = sqlx::query_as(
        "SELECT response_metadata FROM gateway.admin_idempotency_record
         WHERE organization_id = $1 AND idempotency_key = $2",
    )
    .bind(organization.0)
    .bind("meta-none-version")
    .fetch_one(&pool)
    .await
    .unwrap();
    let metadata = metadata.expect("response metadata must be stored");
    assert!(
        !metadata.as_object().unwrap().contains_key("version"),
        "absent version must be omitted, not serialized as null: {metadata}"
    );
    assert_eq!(metadata["resource_id"], organization.0.to_string());
    pool.close().await;
}

/// Re-run the migration's compatibility backfill for memberships created
/// after migrations were applied. This mirrors the deploy-time backfill so
/// tests exercise the real backfill semantics instead of hand-inserted rows.
async fn backfill_admin_role_bindings(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role, enabled)
         SELECT md5(organization_id::text || ':' || user_id::text)::uuid, organization_id, user_id,
                CASE role
                    WHEN 'owner' THEN 'organization_owner'
                    WHEN 'admin' THEN 'organization_admin'
                    ELSE 'auditor'
                END,
                enabled
         FROM gateway.organization_membership
         WHERE role IN ('owner', 'admin', 'auditor')
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, team_id, role, enabled)
         SELECT md5(organization_id::text || ':' || team_id::text || ':' || user_id::text)::uuid,
                organization_id, user_id, team_id, 'team_admin', enabled
         FROM gateway.team_membership
         WHERE role IN ('owner', 'admin')
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn connect_test_pool(database_url: &str) -> PgPool {
    let mut last_error = None;
    for _ in 0..15 {
        match PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(StdDuration::from_secs(2))
            .connect(database_url)
            .await
        {
            Ok(pool) => return pool,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(StdDuration::from_secs(1)).await;
            }
        }
    }
    panic!("GATEWAY_TEST_DATABASE_URL remained unreachable: {last_error:?}");
}

#[tokio::test]
async fn identity_is_scoped_and_key_verification_fails_closed() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) if env::var_os("CI").is_some() || env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL is required in CI")
        }
        Err(_) => {
            eprintln!("skipping identity integration test: GATEWAY_TEST_DATABASE_URL is unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;

    let pepper = ApiKeyPepper::from_bytes(b"integration-test-api-key-pepper").unwrap();
    let store = IdentityStore::new(pool.clone(), pepper);
    let suffix = Uuid::new_v4().simple().to_string();
    let organization_a = store
        .create_organization(&format!("org-a-{suffix}"), "Organization A")
        .await
        .unwrap();
    let organization_b = store
        .create_organization(&format!("org-b-{suffix}"), "Organization B")
        .await
        .unwrap();
    let team_a = store
        .create_team(organization_a.id, "Team A")
        .await
        .unwrap();
    let team_b = store
        .create_team(organization_b.id, "Team B")
        .await
        .unwrap();

    // One global user can belong to multiple organizations. Membership state,
    // rather than global user state, controls verification in each tenant.
    let shared_user = store
        .create_user(
            organization_a.id,
            Some("https://issuer.example"),
            Some(&suffix),
            Some("profile-only@example.invalid"),
            Some("Shared User"),
        )
        .await
        .unwrap();
    store
        .add_organization_membership(organization_a.id, shared_user.id, MembershipRole::Owner)
        .await
        .unwrap();
    store
        .add_organization_membership(organization_b.id, shared_user.id, MembershipRole::Owner)
        .await
        .unwrap();
    store
        .add_team_membership(
            organization_a.id,
            team_a.id,
            shared_user.id,
            MembershipRole::Owner,
        )
        .await
        .unwrap();
    store
        .add_team_membership(
            organization_b.id,
            team_b.id,
            shared_user.id,
            MembershipRole::Owner,
        )
        .await
        .unwrap();
    let other_user = store
        .create_user(organization_b.id, None, None, None, Some("B only"))
        .await
        .unwrap();
    store
        .add_organization_membership(organization_b.id, other_user.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_organization_membership(organization_a.id, other_user.id, MembershipRole::Member)
        .await
        .unwrap();
    store
        .add_team_membership(
            organization_a.id,
            team_a.id,
            other_user.id,
            MembershipRole::Admin,
        )
        .await
        .unwrap();

    assert!(
        store
            .add_team_membership(
                organization_a.id,
                team_b.id,
                shared_user.id,
                MembershipRole::Member
            )
            .await
            .is_err()
    );
    assert!(
        store
            .create_service_account(
                organization_a.id,
                shared_user.id,
                Some(team_b.id),
                "wrong parent"
            )
            .await
            .is_err()
    );

    let service_account_a = store
        .create_service_account(
            organization_a.id,
            shared_user.id,
            Some(team_a.id),
            "A service",
        )
        .await
        .unwrap();
    let service_account_b = store
        .create_service_account(
            organization_b.id,
            shared_user.id,
            Some(team_b.id),
            "B service",
        )
        .await
        .unwrap();
    assert!(
        store
            .find_service_account(organization_b.id, service_account_a.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .set_service_account_enabled(organization_b.id, service_account_a.id, false)
            .await
            .unwrap()
    );

    let key_a = store
        .create_api_key(
            organization_a.id,
            service_account_a.id,
            &[ApiKeyScope::Inference],
            None,
            shared_user.id,
        )
        .await
        .unwrap();
    let key_b = store
        .create_api_key(
            organization_b.id,
            service_account_b.id,
            &[ApiKeyScope::Inference],
            None,
            shared_user.id,
        )
        .await
        .unwrap();
    assert!(
        store
            .create_api_key(
                organization_a.id,
                service_account_a.id,
                &[ApiKeyScope::Inference],
                None,
                other_user.id,
            )
            .await
            .is_err()
    );
    let verified = store
        .verify_bearer_key(&key_a.bearer, ApiKeyScope::Inference)
        .await
        .unwrap()
        .expect("fresh key should verify");
    assert_eq!(verified.organization_id, organization_a.id);
    assert_eq!(verified.service_account_id, service_account_a.id);
    assert_eq!(verified.team_id, Some(team_a.id));
    assert_eq!(verified.api_key_id, Some(key_a.id));
    assert!(
        store
            .verify_bearer_key(&key_a.bearer, ApiKeyScope::Embeddings)
            .await
            .unwrap()
            .is_none()
    );

    // Suspending the shared user's A membership must not affect B.
    assert!(
        store
            .set_organization_membership_enabled(organization_a.id, shared_user.id, false)
            .await
            .unwrap()
    );
    assert!(
        store
            .verify_bearer_key(&key_a.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .verify_bearer_key(&key_b.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .set_organization_membership_enabled(organization_a.id, shared_user.id, true)
            .await
            .unwrap()
    );

    let mut bad_bearer = key_a.bearer.clone().into_bytes();
    let last = bad_bearer.len() - 1;
    bad_bearer[last] = if bad_bearer[last] == b'0' { b'1' } else { b'0' };
    let bad_bearer = String::from_utf8(bad_bearer).unwrap();
    assert!(
        store
            .verify_bearer_key(&bad_bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );

    // An actor outside A cannot create a key there; the key insert is rolled
    // back when the transactional audit insert rejects that actor.
    let before_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.api_key WHERE organization_id = $1")
            .bind(organization_a.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        store
            .create_api_key(
                organization_a.id,
                service_account_a.id,
                &[ApiKeyScope::Inference],
                None,
                other_user.id,
            )
            .await
            .is_err()
    );
    let after_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.api_key WHERE organization_id = $1")
            .bind(organization_a.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before_count, after_count);

    assert!(
        !store
            .revoke_api_key(organization_b.id, key_a.id, shared_user.id)
            .await
            .unwrap()
    );
    assert!(
        store
            .revoke_api_key(organization_a.id, key_a.id, shared_user.id)
            .await
            .unwrap()
    );
    assert!(
        store
            .verify_bearer_key(&key_a.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );

    let expiring = store
        .create_api_key(
            organization_a.id,
            service_account_a.id,
            &[ApiKeyScope::Inference],
            Some(Utc::now() + Duration::milliseconds(100)),
            shared_user.id,
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        store
            .verify_bearer_key(&expiring.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );

    let disabled = store
        .create_api_key(
            organization_a.id,
            service_account_a.id,
            &[ApiKeyScope::Inference],
            None,
            shared_user.id,
        )
        .await
        .unwrap();
    assert!(
        store
            .disable_api_key(organization_a.id, disabled.id, shared_user.id)
            .await
            .unwrap()
    );
    assert!(
        store
            .verify_bearer_key(&disabled.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );

    let account_disabled = store
        .create_api_key(
            organization_b.id,
            service_account_b.id,
            &[ApiKeyScope::Inference],
            None,
            shared_user.id,
        )
        .await
        .unwrap();
    assert!(
        store
            .set_service_account_enabled(organization_b.id, service_account_b.id, false)
            .await
            .unwrap()
    );
    assert!(
        store
            .verify_bearer_key(&account_disabled.bearer, ApiKeyScope::Inference)
            .await
            .unwrap()
            .is_none()
    );

    let audit_rows: Vec<(String, Uuid, Uuid, Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT action, organization_id, actor_user_id, entity_id, metadata FROM gateway.audit_event WHERE organization_id = $1 AND entity_type = 'api_key'",
    )
    .bind(organization_a.id.0)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(audit_rows.iter().any(|row| row.0 == "api_key.created"));
    assert!(audit_rows.iter().any(|row| row.0 == "api_key.revoked"));
    assert!(audit_rows.iter().any(|row| row.0 == "api_key.disabled"));
    for (action, organization_id, actor_id, entity_id, metadata) in &audit_rows {
        assert!(action.starts_with("api_key."));
        assert_eq!(*organization_id, organization_a.id.0);
        assert_eq!(*actor_id, shared_user.id.0);
        let expected_entity_id = entity_id.to_string();
        assert_eq!(
            metadata["api_key_id"].as_str(),
            Some(expected_entity_id.as_str())
        );
        let metadata_text = metadata.to_string();
        assert!(!metadata_text.contains("integration-test-api-key-pepper"));
        assert!(!metadata_text.contains("digest"));
        assert!(!metadata_text.contains("bearer"));
    }

    let api_key_json: String =
        sqlx::query_scalar("SELECT to_jsonb(api_key)::text FROM gateway.api_key WHERE id = $1")
            .bind(key_a.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!api_key_json.contains(&key_a.bearer));
    pool.close().await;
}

#[tokio::test]
async fn auth_replay_expiry_binding_membership_and_logout_are_fail_closed() {
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) if env::var_os("CI").is_some() || env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL required in CI")
        }
        Err(_) => {
            eprintln!("skipping auth integration test: GATEWAY_TEST_DATABASE_URL unset");
            return;
        }
    };
    let _guard = database_guard();
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let pepper = ApiKeyPepper::from_bytes(b"auth-integration-api-key-pepper").unwrap();
    let store = IdentityStore::new(pool.clone(), pepper);
    let auth = AuthStore::new(pool.clone());
    let crypto =
        AuthCrypto::new(b"auth-integration-session-hmac-key-32-bytes", &[9_u8; 32]).unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let organization = store
        .create_organization(&format!("auth-org-{suffix}"), "Auth organization")
        .await
        .unwrap();
    let subject = format!("subject-{suffix}");
    let user = store
        .create_user(
            organization.id,
            Some("https://issuer.example"),
            Some(&subject),
            None,
            None,
        )
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, user.id, MembershipRole::Owner)
        .await
        .unwrap();
    let moved_user = store
        .create_user(
            organization.id,
            Some("https://other-issuer.example"),
            Some(&format!("other-subject-{suffix}")),
            None,
            None,
        )
        .await
        .unwrap();
    store
        .add_organization_membership(organization.id, moved_user.id, MembershipRole::Owner)
        .await
        .unwrap();

    let (transaction, state, _, _) = crypto
        .new_login_transaction("browser-a", std::time::Duration::from_secs(60))
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(transaction.id)
    .bind(&transaction.state_digest)
    .bind(&transaction.nonce_digest)
    .bind(&transaction.verifier_ciphertext)
    .bind(&transaction.browser_binding_digest)
    .bind(transaction.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        auth.claim_login_callback(&crypto.digest(state.as_str()), &crypto.digest("browser-b"),)
            .await
            .unwrap()
            .is_none()
    );
    let (claimed_transaction, claim_id) = auth
        .claim_login_callback(&crypto.digest(state.as_str()), &crypto.digest("browser-a"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        auth.start_login_exchange(claimed_transaction.id, claim_id)
            .await
            .unwrap()
    );
    let session = crypto.new_session(
        user.id,
        "browser-rotated",
        std::time::Duration::from_secs(60),
    );
    assert!(
        auth.finish_login_callback_success_claimed(
            claimed_transaction.id,
            claim_id,
            "https://issuer.example",
            &subject,
            &session,
        )
        .await
        .unwrap()
    );
    assert!(
        auth.claim_login_callback(&crypto.digest(state.as_str()), &crypto.digest("browser-a"),)
            .await
            .unwrap()
            .is_none()
    );

    let (expired, expired_state, _, _) = crypto
        .new_login_transaction("browser-a", std::time::Duration::ZERO)
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(expired.id)
    .bind(&expired.state_digest)
    .bind(&expired.nonce_digest)
    .bind(&expired.verifier_ciphertext)
    .bind(&expired.browser_binding_digest)
    .bind(expired.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        auth.claim_login_callback(
            &crypto.digest(expired_state.as_str()),
            &crypto.digest("browser-a"),
        )
        .await
        .unwrap()
        .is_none()
    );

    let (failed, failed_state, _, _) = crypto
        .new_login_transaction("browser-a", std::time::Duration::from_secs(60))
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(failed.id)
    .bind(&failed.state_digest)
    .bind(&failed.nonce_digest)
    .bind(&failed.verifier_ciphertext)
    .bind(&failed.browser_binding_digest)
    .bind(failed.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();
    let (failed_transaction, failed_claim) = auth
        .claim_login_callback(
            &crypto.digest(failed_state.as_str()),
            &crypto.digest("browser-a"),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        auth.finish_login_callback_failure_claimed(
            failed_transaction.id,
            failed_claim,
            gateway::domain::auth::LoginFailure::ClaimsRejected,
        )
        .await
        .unwrap()
    );
    let failed_state: String =
        sqlx::query_scalar("SELECT status FROM gateway.oidc_login_transaction WHERE id = $1")
            .bind(failed_transaction.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(failed_state, "completed");
    let failed_audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.security_event
         WHERE user_id IS NULL AND event_type = 'login.claims_rejected'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(failed_audits >= 1);

    // Mutating the pre-provisioned user before finalization must consume the
    // exchange fence without creating a session.
    let sessions_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.session WHERE user_id = $1")
            .bind(user.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (mutated, mutated_state, _, _) = crypto
        .new_login_transaction("browser-a", std::time::Duration::from_secs(60))
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(mutated.id)
    .bind(&mutated.state_digest)
    .bind(&mutated.nonce_digest)
    .bind(&mutated.verifier_ciphertext)
    .bind(&mutated.browser_binding_digest)
    .bind(mutated.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();
    let (mutated_transaction, mutated_claim) = auth
        .claim_login_callback(
            &crypto.digest(mutated_state.as_str()),
            &crypto.digest("browser-a"),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        auth.start_login_exchange(mutated_transaction.id, mutated_claim)
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE gateway.user_account
         SET oidc_issuer = NULL, oidc_subject = NULL
         WHERE id = $1",
    )
    .bind(user.id.0)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE gateway.user_account
         SET oidc_issuer = $1, oidc_subject = $2
         WHERE id = $3",
    )
    .bind("https://issuer.example")
    .bind(&subject)
    .bind(moved_user.id.0)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        !auth
            .finish_login_callback_success_claimed(
                mutated_transaction.id,
                mutated_claim,
                "https://issuer.example",
                &subject,
                &session,
            )
            .await
            .unwrap()
    );
    let sessions_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.session WHERE user_id = $1")
            .bind(user.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sessions_after, sessions_before);
    let moved_sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gateway.session WHERE user_id = $1")
            .bind(moved_user.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(moved_sessions, 0);
    sqlx::query("UPDATE gateway.user_account SET enabled = true WHERE id = $1")
        .bind(user.id.0)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        auth.find_session(&session.token_digest, &session.browser_binding_digest)
            .await
            .unwrap(),
        Some(user.id)
    );
    assert!(
        auth.logout_session(&crypto.digest(&session.token))
            .await
            .unwrap()
    );
    pool.close().await;
}

#[tokio::test]
async fn oidc_callback_claims_are_concurrent_fenced_and_terminal_after_dispatch() {
    let _database_guard = database_guard();
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) if env::var_os("CI").is_some() || env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => return,
    };
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;
    let auth = AuthStore::new(pool.clone());
    let crypto = AuthCrypto::new(b"claim-integration-session-hmac-key-32", &[8_u8; 32]).unwrap();

    let (transaction, state, _, _) = crypto
        .new_login_transaction("claim-browser", std::time::Duration::from_secs(120))
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(transaction.id)
    .bind(&transaction.state_digest)
    .bind(&transaction.nonce_digest)
    .bind(&transaction.verifier_ciphertext)
    .bind(&transaction.browser_binding_digest)
    .bind(transaction.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();

    let state_digest = crypto.digest(state.as_str());
    let binding_digest = crypto.digest("claim-browser");
    let (claim_a, claim_b) = tokio::join!(
        auth.claim_login_callback(&state_digest, &binding_digest),
        auth.claim_login_callback(&state_digest, &binding_digest),
    );
    let claim_a = claim_a.unwrap();
    let claim_b = claim_b.unwrap();
    assert_eq!(claim_a.is_some() as u8 + claim_b.is_some() as u8, 1);
    let (claimed_transaction, claim_id) = claim_a.or(claim_b).unwrap();

    assert!(
        auth.start_login_exchange(claimed_transaction.id, claim_id)
            .await
            .unwrap()
    );
    sqlx::query(
        "UPDATE gateway.oidc_login_transaction
         SET claim_lease_until = CURRENT_TIMESTAMP - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(claimed_transaction.id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        auth.claim_login_callback(&state_digest, &binding_digest)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !auth
            .start_login_exchange(claimed_transaction.id, Uuid::new_v4())
            .await
            .unwrap()
    );
    assert!(
        auth.finish_login_callback_failure_claimed(
            claimed_transaction.id,
            claim_id,
            gateway::domain::auth::LoginFailure::BackendRejected,
        )
        .await
        .unwrap()
    );
    assert!(
        auth.claim_login_callback(&state_digest, &binding_digest)
            .await
            .unwrap()
            .is_none()
    );

    // A pre-dispatch stale lease can be replaced, but the old fence cannot
    // advance or release the replacement claim.
    let (stale, stale_state, _, _) = crypto
        .new_login_transaction("claim-browser", std::time::Duration::from_secs(120))
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.oidc_login_transaction
         (id, state_digest, nonce_digest, verifier_ciphertext,
          browser_binding_digest, expires_at)
         VALUES ($1,$2,$3,$4,$5,to_timestamp($6))",
    )
    .bind(stale.id)
    .bind(&stale.state_digest)
    .bind(&stale.nonce_digest)
    .bind(&stale.verifier_ciphertext)
    .bind(&stale.browser_binding_digest)
    .bind(stale.expires_at as i64)
    .execute(&pool)
    .await
    .unwrap();
    let stale_digest = crypto.digest(stale_state.as_str());
    let stale_binding = crypto.digest("claim-browser");
    let (stale_transaction, stale_claim) = auth
        .claim_login_callback(&stale_digest, &stale_binding)
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE gateway.oidc_login_transaction
         SET claim_lease_until = CURRENT_TIMESTAMP - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(stale_transaction.id)
    .execute(&pool)
    .await
    .unwrap();
    let (replacement, replacement_claim) = auth
        .claim_login_callback(&stale_digest, &stale_binding)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(stale_claim, replacement_claim);
    assert!(
        !auth
            .start_login_exchange(stale_transaction.id, stale_claim)
            .await
            .unwrap()
    );
    assert!(
        auth.release_login_callback_claim(replacement.id, replacement_claim)
            .await
            .unwrap()
    );
    let (reclaimed_transaction, reclaimed_claim) = auth
        .claim_login_callback(&stale_digest, &stale_binding)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reclaimed_transaction.id, replacement.id);
    assert_ne!(reclaimed_claim, replacement_claim);
    assert!(
        !auth
            .start_login_exchange(reclaimed_transaction.id, replacement_claim)
            .await
            .unwrap()
    );
    assert!(
        auth.finish_login_callback_failure_claimed(
            reclaimed_transaction.id,
            reclaimed_claim,
            gateway::domain::auth::LoginFailure::BackendUnavailable,
        )
        .await
        .unwrap()
    );

    pool.close().await;
}

#[tokio::test]
async fn audit_constraints_reject_invalid_new_rows() {
    let _database_guard = database_guard();
    let database_url = match env::var("GATEWAY_TEST_DATABASE_URL") {
        Ok(value) => value,
        Err(_) if env::var_os("CI").is_some() || env::var_os("GITHUB_ACTIONS").is_some() => {
            panic!("GATEWAY_TEST_DATABASE_URL must be set in CI")
        }
        Err(_) => return,
    };
    let pool = connect_test_pool(&database_url).await;
    apply_migrations(&pool).await;

    let organization_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, $3)")
        .bind(organization_id)
        .bind(format!("audit-constraint-test-{organization_id}"))
        .bind("Audit constraint test")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.user_account (id) VALUES ($1)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO gateway.organization_membership (organization_id, user_id, role) \
         VALUES ($1, $2, 'owner')",
    )
    .bind(organization_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let valid_metadata = serde_json::json!({ "api_key_id": entity_id.to_string() });
    // The action grammar allows lowercase `[a-z0-9._:-]` (matching the Rust
    // `AuditAction` contract), so use a genuinely invalid action here.
    let invalid_action = sqlx::query(
        "INSERT INTO gateway.audit_event \
         (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata) \
         VALUES ($1, $2, $3, 'NOT_A_VALID_ACTION', 'api_key', $4, $5)",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id)
    .bind(user_id)
    .bind(entity_id)
    .bind(&valid_metadata)
    .execute(&pool)
    .await;
    assert!(invalid_action.is_err());

    let invalid_metadata = sqlx::query(
        "INSERT INTO gateway.audit_event \
         (id, organization_id, actor_user_id, action, entity_type, entity_id, metadata) \
         VALUES ($1, $2, $3, 'api_key.created', 'api_key', $4, '{}'::JSONB)",
    )
    .bind(Uuid::new_v4())
    .bind(organization_id)
    .bind(user_id)
    .bind(entity_id)
    .execute(&pool)
    .await;
    assert!(invalid_metadata.is_err());

    let not_valid: (bool, bool) = sqlx::query_as(
        "SELECT bool_and(NOT convalidated), bool_and(convalidated = false) \
         FROM pg_constraint \
         WHERE conrelid = 'gateway.audit_event'::regclass \
           AND conname IN ('audit_event_action_ck', 'audit_event_metadata_shape_ck')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(not_valid, (true, true));

    pool.close().await;
}

async fn apply_migrations(pool: &PgPool) {
    gateway::persistence::migrations::run(pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
}
