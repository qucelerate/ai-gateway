use std::env;

use gateway::{
    AccountingStore, CatalogManifest, CatalogOperation, CatalogStore, OrganizationId,
    ProviderFamily, ReservationId,
};
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
async fn catalog_apply_resolve_isolated_immutable_and_idempotent() {
    let Some(pool) = pool().await else { return };
    let organization_a = Uuid::new_v4();
    let organization_b = Uuid::new_v4();
    for (id, suffix) in [(organization_a, "a"), (organization_b, "b")] {
        sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
            .bind(id)
            .bind(format!("catalog-test-{suffix}-{id}"))
            .bind("catalog integration test")
            .execute(&pool)
            .await
            .unwrap();
    }

    let store = CatalogStore::new(pool.clone());
    let release = manifest("one", "chat");
    let digest = release.digest().unwrap();
    let applied = store
        .apply(OrganizationId(organization_a), &release, digest)
        .await
        .unwrap();
    let reapplied = store
        .apply(OrganizationId(organization_a), &release, digest)
        .await
        .unwrap();
    assert_eq!(applied.id, reapplied.id);

    let route = store
        .resolve_active(OrganizationId(organization_a), "chat", "chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(route.price.input_minor_per_token, 3);
    assert_eq!(route.price.output_minor_per_token, 7);
    assert_eq!(route.price.version, "one");
    assert_eq!(route.max_input_bytes, 4096);
    assert_eq!(route.max_output_tokens, 32);
    assert_eq!(route.reservation_ttl_seconds, 17);
    assert!(
        store
            .resolve_active(OrganizationId(organization_b), "chat", "chat")
            .await
            .unwrap()
            .is_none()
    );

    let update = sqlx::query("UPDATE gateway.catalog_deployment SET model='mutated' WHERE id=$1")
        .bind(route.deployment_id)
        .execute(&pool)
        .await;
    assert!(update.is_err());

    sqlx::query("DELETE FROM gateway.organization WHERE id IN ($1,$2)")
        .bind(organization_a)
        .bind(organization_b)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_catalog_release_seals_graph_but_allows_rollback_activation() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("catalog-seal-{organization}"))
        .bind("catalog seal integration test")
        .execute(&pool)
        .await
        .unwrap();

    let store = CatalogStore::new(pool.clone());
    let first = manifest("seal-one", "seal-chat");
    let first_digest = first.digest().unwrap();
    let applied = store
        .apply(OrganizationId(organization), &first, first_digest)
        .await
        .unwrap();

    let replacement = manifest("seal-two", "seal-chat");
    let replacement_digest = replacement.digest().unwrap();
    store
        .apply(
            OrganizationId(organization),
            &replacement,
            replacement_digest,
        )
        .await
        .unwrap();

    let sealed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gateway.catalog_release_seal WHERE organization_id=$1 AND release_id=$2)",
    )
    .bind(organization)
    .bind(applied.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(sealed);

    let mutation = sqlx::query(
        "INSERT INTO gateway.catalog_route (id,organization_id,release_id,alias,operation,deployment_id,capability_id,price_id) SELECT $1,organization_id,release_id,'seal-mutation',operation,deployment_id,capability_id,price_id FROM gateway.catalog_route WHERE organization_id=$2 AND release_id=$3 LIMIT 1",
    )
    .bind(Uuid::new_v4())
    .bind(organization)
    .bind(applied.id)
    .execute(&pool)
    .await;
    assert!(mutation.is_err(), "sealed graph rows must reject mutation");

    let rollback = store
        .apply(OrganizationId(organization), &first, first_digest)
        .await
        .unwrap();
    assert_eq!(rollback.id, applied.id);

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn catalog_activation_serializes_concurrent_releases() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("catalog-concurrent-{organization}"))
        .bind("catalog concurrent test")
        .execute(&pool)
        .await
        .unwrap();

    let store = CatalogStore::new(pool.clone());
    let first = manifest("first", "first");
    let second = manifest("second", "second");
    let first_digest = first.digest().unwrap();
    let second_digest = second.digest().unwrap();
    let (left, right) = tokio::join!(
        store.apply(OrganizationId(organization), &first, first_digest),
        store.apply(OrganizationId(organization), &second, second_digest),
    );
    assert!(left.is_ok());
    assert!(right.is_ok());
    let first_route = store
        .resolve_active(OrganizationId(organization), "first", "chat")
        .await
        .unwrap();
    let second_route = store
        .resolve_active(OrganizationId(organization), "second", "chat")
        .await
        .unwrap();
    assert_eq!(
        first_route.is_some() as u8 + second_route.is_some() as u8,
        1
    );

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn database_rejects_mismatched_route_references_and_provenance() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    let release = Uuid::new_v4();
    let provider = Uuid::new_v4();
    let secondary_provider = Uuid::new_v4();
    let deployment_a = Uuid::new_v4();
    let deployment_b = Uuid::new_v4();
    let capability_a = Uuid::new_v4();
    let capability_b = Uuid::new_v4();
    let price_a = Uuid::new_v4();
    let price_b = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("catalog-negative-{organization}"))
        .bind("catalog negative test")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.catalog_release (id,organization_id,version,digest) VALUES ($1,$2,'negative',decode(repeat('ab',32),'hex'))")
        .bind(release).bind(organization).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO gateway.catalog_provider_account (id,organization_id,release_id,adapter_instance_key,family) VALUES ($1,$2,$3,'openai-main','openai')")
        .bind(provider).bind(organization).bind(release).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO gateway.catalog_provider_account (id,organization_id,release_id,adapter_instance_key,family) VALUES ($1,$2,$3,'openai-secondary','openai')")
        .bind(secondary_provider).bind(organization).bind(release).execute(&pool).await.unwrap();
    for (id, name) in [
        (deployment_a, "deployment-a"),
        (deployment_b, "deployment-b"),
    ] {
        sqlx::query("INSERT INTO gateway.catalog_deployment (id,organization_id,release_id,provider_account_id,adapter_instance_key,name,model) VALUES ($1,$2,$3,$4,'openai-main',$5,'model')")
            .bind(id).bind(organization).bind(release).bind(provider).bind(name).execute(&pool).await.unwrap();
    }

    let cross_wired_deployment = sqlx::query("INSERT INTO gateway.catalog_deployment (id,organization_id,release_id,provider_account_id,adapter_instance_key,name,model) VALUES ($1,$2,$3,$4,'openai-secondary','cross-wired','model')")
        .bind(Uuid::new_v4()).bind(organization).bind(release).bind(provider)
        .execute(&pool).await;
    assert!(
        cross_wired_deployment.is_err(),
        "provider UUID/key pair must be bound together"
    );
    for (id, deployment) in [(capability_a, deployment_a), (capability_b, deployment_b)] {
        sqlx::query("INSERT INTO gateway.catalog_capability (id,organization_id,release_id,deployment_id,operation,max_input_bytes,max_output_tokens,max_tokens_per_byte,tokenizer,reservation_ttl_seconds,chat_input_count_mode) VALUES ($1,$2,$3,$4,'chat',4096,32,1,'configured',300,'unsupported')")
            .bind(id).bind(organization).bind(release).bind(deployment).execute(&pool).await.unwrap();
    }
    for (id, deployment) in [(price_a, deployment_a), (price_b, deployment_b)] {
        sqlx::query("INSERT INTO gateway.catalog_price (id,organization_id,release_id,deployment_id,operation,input_minor_per_token,output_minor_per_token,cached_input_minor_per_token,reasoning_minor_per_token,other_minor_per_token,currency,source,source_version,confidence) VALUES ($1,$2,$3,$4,'chat',1,1,0,0,0,'USD','test','negative','known')")
            .bind(id).bind(organization).bind(release).bind(deployment).execute(&pool).await.unwrap();
    }

    let route_sql = "INSERT INTO gateway.catalog_route (id,organization_id,release_id,alias,operation,deployment_id,capability_id,price_id) VALUES ($1,$2,$3,$4,'chat',$5,$6,$7)";
    let capability_mismatch = sqlx::query(route_sql)
        .bind(Uuid::new_v4())
        .bind(organization)
        .bind(release)
        .bind("capability-mismatch")
        .bind(deployment_a)
        .bind(capability_b)
        .bind(price_a)
        .execute(&pool)
        .await;
    assert!(
        capability_mismatch
            .unwrap_err()
            .to_string()
            .contains("catalog_route_capability")
    );
    let price_mismatch = sqlx::query(route_sql)
        .bind(Uuid::new_v4())
        .bind(organization)
        .bind(release)
        .bind("price-mismatch")
        .bind(deployment_a)
        .bind(capability_a)
        .bind(price_b)
        .execute(&pool)
        .await;
    assert!(
        price_mismatch
            .unwrap_err()
            .to_string()
            .contains("catalog_route_price")
    );

    let valid_manifest = manifest("applied", "valid");
    let valid_digest = valid_manifest.digest().unwrap();
    let store = CatalogStore::new(pool.clone());
    let applied = store
        .apply(OrganizationId(organization), &valid_manifest, valid_digest)
        .await
        .unwrap();
    let route = store
        .resolve_active(OrganizationId(organization), "valid", "chat")
        .await
        .unwrap()
        .unwrap();
    let provenance_mismatch = sqlx::query("INSERT INTO gateway.accounting_reservation (id,organization_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode) VALUES ($1,$2,'negative-provenance',$3,'reserved',1,now()+interval '5 minutes',1,3,7,0,0,0,'applied','known','USD',$4,$5,$6,$7,$8,$9,$10,$11,'embeddings',$12,$13,'unsupported')")
        .bind(Uuid::new_v4()).bind(organization).bind(vec![7_u8; 32])
        .bind(route.deployment.as_str()).bind(route.model.as_str()).bind(Uuid::new_v4())
        .bind(applied.id).bind(route.route_id).bind(route.deployment_id).bind(route.price_id)
        .bind(route.digest.0.to_vec()).bind("openai").bind("test")
        .execute(&pool).await;
    assert!(
        provenance_mismatch
            .unwrap_err()
            .to_string()
            .contains("provenance")
    );

    let reservation_id = Uuid::new_v4();
    let trace_id = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.accounting_reservation (id,organization_id,idempotency_key,fingerprint,state,reserved_minor,expires_at,output_token_cap,input_price_minor,output_price_minor,cached_input_price_minor,reasoning_price_minor,other_price_minor,price_version,pricing_confidence,currency,deployment,model,trace_id,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode) VALUES ($1,$2,'valid-provenance',$3,'reserved',1,now()+interval '5 minutes',1,3,7,0,0,0,'applied','known','USD',$4,$5,$6,$7,$8,$9,$10,$11,'chat','openai','test','unsupported')")
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
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO gateway.inference_trace (id,organization_id,reservation_id,correlation_id,operation,status,model,metadata,input_bound,output_cap,price_version,pricing_confidence,currency,catalog_release_id,catalog_route_id,catalog_deployment_id,catalog_price_id,catalog_digest,catalog_operation,catalog_family,catalog_price_source,catalog_chat_input_count_mode) VALUES ($1,$2,$3,$4,'inference','pending',$5,jsonb_build_object('input_bound',1,'output_cap',1,'currency','USD'),1,1,'applied','known','USD',$6,$7,$8,$9,$10,'chat','openai','test','unsupported')")
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
        .execute(&pool)
        .await
        .unwrap();
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
        "unsupported"
    );

    let mismatched_mode = sqlx::query("UPDATE gateway.accounting_reservation SET catalog_chat_input_count_mode='provider_api_exact' WHERE id=$1")
        .bind(reservation_id)
        .execute(&pool)
        .await;
    assert!(mismatched_mode.is_err());
    let partial_mode = sqlx::query(
        "UPDATE gateway.accounting_reservation SET catalog_chat_input_count_mode=NULL WHERE id=$1",
    )
    .bind(reservation_id)
    .execute(&pool)
    .await;
    assert!(partial_mode.is_err());
    let trace_mode = sqlx::query("UPDATE gateway.inference_trace SET catalog_chat_input_count_mode='provider_api_exact' WHERE id=$1")
        .bind(trace_id)
        .execute(&pool)
        .await;
    assert!(trace_mode.is_err());

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn anthropic_and_bedrock_catalogs_apply() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("families-{organization}"))
        .bind("families")
        .execute(&pool)
        .await
        .unwrap();
    // The family guard fires on capability rows too; it must not reference
    // price-only columns there (regression for migration 0029). Both
    // families report cache reads and writes, so may price them (0144).
    let manifest: CatalogManifest = serde_json::from_value(json!({
        "release": {"version": "families"},
        "providers": [
            {"adapter_instance_key":"anthropic-main","family":"anthropic"},
            {"adapter_instance_key":"bedrock-main","family":"bedrock"}
        ],
        "deployments": (["anthropic-main", "bedrock-main"].map(|key| json!({
            "name":key, "model":"model", "location":null, "adapter_instance_key":key,
            "capabilities":[{"operation":"chat","max_input_bytes":4096,"max_output_tokens":32,"max_tokens_per_byte":1,"tokenizer":"configured","reservation_ttl_seconds":17,"chat_input_count_mode":"unsupported"}]
        }))),
        "prices": (["anthropic-main", "bedrock-main"].map(|key| json!({
            "deployment":key, "operation":"chat",
            "input_minor_per_token":3, "output_minor_per_token":7,
            "cached_input_minor_per_token":1, "reasoning_minor_per_token":0,
            "other_minor_per_token":4, "currency":"USD", "source":"test",
            "source_version":"families", "confidence":"known"
        }))),
        "routes": [
            {"alias":"claude","operation":"chat","deployment":"anthropic-main"},
            {"alias":"bedrock","operation":"chat","deployment":"bedrock-main"}
        ]
    }))
    .unwrap();
    let store = CatalogStore::new(pool.clone());
    store
        .apply(
            OrganizationId(organization),
            &manifest,
            manifest.digest().unwrap(),
        )
        .await
        .unwrap();
    let capabilities: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gateway.catalog_capability WHERE organization_id=$1",
    )
    .bind(organization)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(capabilities, 2);

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}

/// The database family guard must accept everything the manifest validator
/// accepts. A gap surfaces as a store error, which the admin API reports as a
/// 500 instead of an invalid manifest.
#[tokio::test]
async fn database_accepts_every_supported_family_and_operation() {
    let Some(pool) = pool().await else { return };
    let organization = Uuid::new_v4();
    sqlx::query("INSERT INTO gateway.organization (id,slug,name) VALUES ($1,$2,$3)")
        .bind(organization)
        .bind(format!("matrix-{organization}"))
        .bind("matrix")
        .execute(&pool)
        .await
        .unwrap();
    let mut providers = Vec::new();
    let mut deployments = Vec::new();
    let mut prices = Vec::new();
    let mut routes = Vec::new();
    for family in ProviderFamily::ALL {
        let key = family.as_str();
        let operations: Vec<_> = CatalogOperation::ALL
            .into_iter()
            .filter(|operation| family.supports(*operation))
            .collect();
        let count_mode = if family.counts_chat_input_exactly() {
            "provider_api_exact"
        } else {
            "unsupported"
        };
        // Charge for every usage dimension the family is allowed to price.
        let (cached, reasoning, other) = match family {
            ProviderFamily::Anthropic => (0, 0, 0),
            ProviderFamily::Bedrock => (1, 0, 1),
            _ => (1, 1, 1),
        };
        providers.push(json!({"adapter_instance_key":key,"family":key}));
        deployments.push(json!({
            "name":key, "model":"model", "location":null, "adapter_instance_key":key,
            "capabilities": operations.iter().map(|operation| json!({
                "operation":operation.as_str(),"max_input_bytes":4096,"max_output_tokens":32,
                "max_tokens_per_byte":1,"tokenizer":"configured","reservation_ttl_seconds":17,
                "chat_input_count_mode":count_mode
            })).collect::<Vec<_>>()
        }));
        for operation in operations {
            prices.push(json!({
                "deployment":key, "operation":operation.as_str(),
                "input_minor_per_token":3, "output_minor_per_token":7,
                "cached_input_minor_per_token":cached, "reasoning_minor_per_token":reasoning,
                "other_minor_per_token":other, "currency":"USD", "source":"test",
                "source_version":"matrix", "confidence":"known"
            }));
            routes.push(json!({
                "alias":key, "operation":operation.as_str(), "deployment":key
            }));
        }
    }
    let manifest: CatalogManifest = serde_json::from_value(json!({
        "release": {"version": "matrix"},
        "providers": providers,
        "deployments": deployments,
        "prices": prices,
        "routes": routes
    }))
    .unwrap();
    let store = CatalogStore::new(pool.clone());
    store
        .apply(
            OrganizationId(organization),
            &manifest,
            manifest.digest().unwrap(),
        )
        .await
        .unwrap();
    for family in ProviderFamily::ALL {
        for operation in CatalogOperation::ALL {
            let route = store
                .resolve_active(
                    OrganizationId(organization),
                    family.as_str(),
                    operation.as_str(),
                )
                .await
                .unwrap();
            assert_eq!(
                route.is_some(),
                family.supports(operation),
                "{} {}",
                family.as_str(),
                operation.as_str()
            );
        }
    }

    sqlx::query("DELETE FROM gateway.organization WHERE id=$1")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
}
