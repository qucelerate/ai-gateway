//! Upgrade test for migrations 0015-0018. It drops and recreates triggers,
//! constraints and columns on shared tables, so it lives in its own test
//! binary: parallel tests inserting into those tables would deadlock with it.

use std::env;

use gateway::{AccountingStore, CatalogManifest, CatalogStore, OrganizationId, ReservationId};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = env::var("GATEWAY_TEST_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap_or_else(|error| {
            panic!("GATEWAY_TEST_DATABASE_URL is configured but unreachable: {error}")
        });
    gateway::persistence::migrations::run(&pool)
        .await
        .unwrap_or_else(|error| panic!("configured test database migration failed: {error}"));
    Some(pool)
}

fn manifest(version: &str, alias: &str) -> CatalogManifest {
    serde_json::from_value(json!({
        "release": {"version": version},
        "providers": [{"adapter_instance_key":"openai-main","family":"openai"}],
        "deployments": [{
            "name":"deployment", "model":"model", "location":null,
            "adapter_instance_key":"openai-main",
            "capabilities":[{"operation":"chat","max_input_bytes":4096,"max_output_tokens":32,"max_tokens_per_byte":1,"tokenizer":"configured","reservation_ttl_seconds":17,"chat_input_count_mode":"unsupported"}]
        }],
        "prices": [{
            "deployment":"deployment", "operation":"chat",
            "input_minor_per_token":3, "output_minor_per_token":7,
            "cached_input_minor_per_token":0, "reasoning_minor_per_token":0,
            "other_minor_per_token":0, "currency":"USD", "source":"test",
            "source_version":version, "confidence":"known"
        }],
        "routes":[{"alias":alias,"operation":"chat","deployment":"deployment"}]
    }))
    .unwrap()
}

#[tokio::test]
async fn catalog_count_mode_upgrade_backfills_pre_0018_accounting_rows() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("catalog-upgrade-{organization}"))
        .bind("catalog upgrade test")
        .execute(&pool)
        .await
        .unwrap();

    let store = CatalogStore::new(pool.clone());
    let release_manifest = manifest("upgrade", "upgrade");
    let digest = release_manifest.digest().unwrap();
    let applied = store
        .apply(OrganizationId(organization), &release_manifest, digest)
        .await
        .unwrap();
    let route = store
        .resolve_active(OrganizationId(organization), "upgrade", "chat")
        .await
        .unwrap()
        .unwrap();
    let reservation_id = Uuid::new_v4();
    let trace_id = Uuid::new_v4();
    let mut connection = pool.acquire().await.unwrap();
    let mut tx = sqlx::Acquire::begin(&mut connection).await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('gateway-catalog-0018-proof', 0))")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.accounting_reservation (id,organization_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode) VALUES ($1,$2,'pre-0018',$3,'reserved',1,now()+interval '5 minutes',1,3,7,0,0,0,'upgrade','known','USD',$4,$5,$6,$7,$8,$9,$10,$11,'chat','openai','test','unsupported')")
        .bind(reservation_id)
        .bind(organization)
        .bind(vec![8_u8; 32])
        .bind(route.deployment.as_str())
        .bind(route.model.as_str())
        .bind(trace_id)
        .bind(applied.id)
        .bind(route.route_id)
        .bind(route.deployment_id)
        .bind(route.price_id)
        .bind(route.digest.0.to_vec())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.inference_trace (id,organization_id,reservation_id,correlation_id,operation,status,model,metadata,input_bound,output_cap,price_version,pricing_confidence,currency,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode) VALUES ($1,$2,$3,$4,'inference','pending',$5,jsonb_build_object('input_bound',1,'output_cap',1,'currency','USD'),1,1,'upgrade','known','USD',$6,$7,$8,$9,$10,'chat','openai','test','unsupported')")
        .bind(trace_id)
        .bind(organization)
        .bind(reservation_id)
        .bind(Uuid::new_v4())
        .bind(route.model.as_str())
        .bind(applied.id)
        .bind(route.route_id)
        .bind(route.deployment_id)
        .bind(route.price_id)
        .bind(route.digest.0.to_vec())
        .execute(&mut *tx)
        .await
        .unwrap();

    // Recreate the pre-0018 schema shape while retaining the seeded rows, then
    // execute the real forward files. The fresh Compose database has already
    // applied these files, so this is isolated to this test's organization and
    // restores the current schema in-place.
    sqlx::raw_sql("DROP TRIGGER accounting_reservation_catalog_provenance_revalidate ON gateway.accounting_reservation; DROP TRIGGER inference_trace_catalog_provenance_revalidate ON gateway.inference_trace; DROP TRIGGER accounting_reservation_catalog_count_mode_revalidate ON gateway.accounting_reservation; DROP TRIGGER inference_trace_catalog_count_mode_revalidate ON gateway.inference_trace; DROP TRIGGER accounting_reservation_catalog_count_mode_validate ON gateway.accounting_reservation; DROP TRIGGER inference_trace_catalog_count_mode_validate ON gateway.inference_trace; DROP TRIGGER accounting_reservation_catalog_provenance_immutable ON gateway.accounting_reservation; DROP TRIGGER inference_trace_catalog_provenance_immutable ON gateway.inference_trace; ALTER TABLE gateway.accounting_reservation DROP CONSTRAINT accounting_reservation_catalog_provenance_ck; ALTER TABLE gateway.inference_trace DROP CONSTRAINT inference_trace_catalog_provenance_ck; ALTER TABLE gateway.accounting_reconciliation DROP CONSTRAINT accounting_reconciliation_reason_check; ALTER TABLE gateway.accounting_reconciliation ADD CONSTRAINT accounting_reconciliation_reason_check CHECK (reason IN ('provider-unknown','expired-after-claim','bound-breach','settlement-failed','input-count-drift')); ALTER TABLE gateway.accounting_reservation DROP COLUMN catalog_chat_input_count_mode; ALTER TABLE gateway.inference_trace DROP COLUMN catalog_chat_input_count_mode; ALTER TABLE gateway.catalog_capability DROP COLUMN chat_input_count_mode; ALTER TABLE gateway.accounting_reservation ADD CONSTRAINT accounting_reservation_catalog_provenance_ck CHECK ((catalog_release_id IS NULL AND catalog_route_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL AND catalog_operation IS NULL AND catalog_family IS NULL AND catalog_price_source IS NULL) OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest)=32 AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL AND catalog_price_source IS NOT NULL)); ALTER TABLE gateway.inference_trace ADD CONSTRAINT inference_trace_catalog_provenance_ck CHECK ((catalog_release_id IS NULL AND catalog_route_id IS NULL AND catalog_deployment_id IS NULL AND catalog_price_id IS NULL AND catalog_digest IS NULL AND catalog_operation IS NULL AND catalog_family IS NULL AND catalog_price_source IS NULL) OR (catalog_release_id IS NOT NULL AND catalog_route_id IS NOT NULL AND catalog_deployment_id IS NOT NULL AND catalog_price_id IS NOT NULL AND catalog_digest IS NOT NULL AND octet_length(catalog_digest)=32 AND catalog_operation IS NOT NULL AND catalog_family IS NOT NULL AND catalog_price_source IS NOT NULL));")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (migration_number, migration) in [
        include_str!("../migrations/0015_catalog_chat_count_mode.sql"),
        include_str!("../migrations/0016_accounting_chat_count_provenance.sql"),
        include_str!("../migrations/0017_validate_chat_count_mode.sql"),
        include_str!("../migrations/0018_catalog_count_provenance_backfill_and_update_guards.sql"),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::raw_sql(migration).execute(&mut *tx).await.unwrap();
        if migration_number == 0 {
            // The seeded release is already active, so use a narrowly scoped
            // trigger suspension to recreate its provider_api_exact live
            // capability while exercising the forward upgrade files.
            sqlx::query("ALTER TABLE gateway.catalog_capability DISABLE TRIGGER catalog_capability_immutable")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE gateway.catalog_capability SET chat_input_count_mode='provider_api_exact' WHERE organization_id=$1")
                .bind(organization)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("ALTER TABLE gateway.catalog_capability ENABLE TRIGGER catalog_capability_immutable")
                .execute(&mut *tx)
                .await
                .unwrap();
        }
    }
    // 0062 runs this check as the application role; re-running 0017 reset it.
    sqlx::raw_sql("ALTER FUNCTION gateway.validate_catalog_chat_count_mode() SECURITY DEFINER SET search_path = pg_catalog, pg_temp")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER accounting_reservation_catalog_provenance_immutable BEFORE UPDATE ON gateway.accounting_reservation FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_provenance_mutation(); CREATE TRIGGER inference_trace_catalog_provenance_immutable BEFORE UPDATE ON gateway.inference_trace FOR EACH ROW EXECUTE FUNCTION gateway.prevent_catalog_provenance_mutation();")
        .execute(&mut *tx)
        .await
        .unwrap();

    tx.commit().await.unwrap();

    let capability_mode: String = sqlx::query_scalar(
        "SELECT chat_input_count_mode FROM gateway.catalog_capability WHERE organization_id=$1 LIMIT 1",
    )
    .bind(organization)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(capability_mode, "provider_api_exact");

    let reservation = AccountingStore::new(pool.clone())
        .find_reservation(OrganizationId(organization), ReservationId(reservation_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reservation
            .catalog_provenance
            .as_ref()
            .unwrap()
            .chat_input_count_mode,
        "historical_unverified"
    );
    let trace_mode: String = sqlx::query_scalar(
        "SELECT catalog_chat_input_count_mode FROM gateway.inference_trace WHERE id=$1",
    )
    .bind(trace_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(trace_mode, "historical_unverified");

    // Historical provenance survives ordinary accounting and trace lifecycle
    // transitions while the provider_api_exact capability remains live.
    let reservation_lifecycle =
        sqlx::query("UPDATE gateway.accounting_reservation SET state='claimed' WHERE id=$1")
            .bind(reservation_id)
            .execute(&pool)
            .await;
    assert!(reservation_lifecycle.is_ok());

    let trace_lifecycle =
        sqlx::query("UPDATE gateway.inference_trace SET status='finalized' WHERE id=$1")
            .bind(trace_id)
            .execute(&pool)
            .await;
    assert!(trace_lifecycle.is_ok());

    let illicit_reservation_upgrade = sqlx::query(
        "UPDATE gateway.accounting_reservation SET catalog_chat_input_count_mode='provider_api_exact' WHERE id=$1",
    )
    .bind(reservation_id)
    .execute(&pool)
    .await;
    assert!(illicit_reservation_upgrade.is_err());

    let illicit_trace_upgrade = sqlx::query(
        "UPDATE gateway.inference_trace SET catalog_chat_input_count_mode='provider_api_exact' WHERE id=$1",
    )
    .bind(trace_id)
    .execute(&pool)
    .await;
    assert!(illicit_trace_upgrade.is_err());

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}
