use artifactory_database::{PoolSettings, connect, with_database, with_options};
use artifactory_database::{params, sql};
use artifactory_telemetry_model::{Batch, HostSample, Inventory, ProcessSample};
use artifactory_telemetry_service::{Service, query, store};
use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL on an isolated TimescaleDB administrator database"]
async fn durable_replay_ordering_nulls_columnstore_and_rollups() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let mut admin = connect(&url, Default::default(), PoolSettings::new(2))
        .await
        .unwrap();
    let name = format!("telemetry_test_{}", Uuid::new_v4().simple());
    sql::execute(&mut admin, &format!("CREATE DATABASE {name}"), params![])
        .await
        .unwrap();
    let scoped = with_database(&url, &name).unwrap();
    let pool = connect(&scoped, store::models(), PoolSettings::new(8))
        .await
        .unwrap();
    artifactory_database::require_timescale(&pool)
        .await
        .unwrap();
    artifactory_database::migrate(&pool, &store::MIGRATIONS)
        .await
        .unwrap();
    let node = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let at = Utc::now() - Duration::minutes(10);
    let mut batch = Batch {
        id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        sampled_at: at,
        monotonic_ms: 1,
        dropped_batches: 0,
        omitted_processes: 0,
        host: Some(HostSample {
            cpu_percent: Some(25.0),
            memory_total_bytes: Some(1024),
            memory_used_bytes: None,
            ..HostSample::default()
        }),
        processes: vec![ProcessSample {
            attempt_id: attempt,
            pid: 17,
            started_at: at.timestamp() as u64 - 100,
            parent_pid: None,
            step: Some(1),
            cpu_percent: None,
            rss_bytes: Some(30),
            read_bytes: None,
            written_bytes: None,
            status: "running".into(),
        }],
        inventory: Some(Inventory {
            os: "linux".into(),
            arch: "arm64".into(),
            ..Inventory::default()
        }),
    };
    let bytes = serde_json::to_vec(&batch).unwrap();
    let (a, b) = tokio::join!(
        store::ingest(&pool, node, &batch, &bytes, Utc::now()),
        store::ingest(&pool, node, &batch, &bytes, Utc::now())
    );
    assert_ne!(
        a.unwrap(),
        b.unwrap(),
        "exactly one simultaneous delivery may insert"
    );
    let count: i64 = sql::scalar(
        &mut pool.clone(),
        "SELECT count(*) FROM process_samples",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    let result = store::ingest(&pool, node, &batch, b"conflicting contents", Utc::now()).await;
    assert_eq!(result.unwrap_err().code(), tonic::Code::AlreadyExists);
    batch.id = Uuid::new_v4();
    batch.sampled_at = at + Duration::seconds(10);
    batch.host.as_mut().unwrap().cpu_percent = Some(75.0);
    store::ingest(
        &pool,
        node,
        &batch,
        &serde_json::to_vec(&batch).unwrap(),
        Utc::now(),
    )
    .await
    .unwrap();
    let newest = batch.sampled_at;
    batch.id = Uuid::new_v4();
    batch.sampled_at = at - Duration::minutes(5);
    batch.host.as_mut().unwrap().cpu_percent = None;
    store::ingest(
        &pool,
        node,
        &batch,
        &serde_json::to_vec(&batch).unwrap(),
        Utc::now(),
    )
    .await
    .unwrap();
    let latest: chrono::DateTime<Utc> = sql::scalar(
        &mut pool.clone(),
        "SELECT sampled_at FROM latest_hosts WHERE node_id=$1",
        params![node],
    )
    .await
    .unwrap();
    assert_eq!(latest, newest, "old replay must not replace latest state");
    let count: i64 = sql::scalar(
        &mut pool.clone(),
        "SELECT count(*) FROM inventory_versions",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 1, "unchanged inventories share one body");
    let unknown: bool = sql::scalar(
        &mut pool.clone(),
        "SELECT bool_and(cpu_percent IS NULL AND read_bytes IS NULL) FROM build_samples",
        params![],
    )
    .await
    .unwrap();
    assert!(unknown, "unknown process metrics must not become zero");
    for view in ["host_minutes", "build_minutes", "host_hours", "build_hours"] {
        sql::execute(
            &mut pool.clone(),
            &format!("CALL refresh_continuous_aggregate('{view}',$1::timestamptz,$2::timestamptz)"),
            params![at - Duration::hours(2), Utc::now() + Duration::hours(1)],
        )
        .await
        .unwrap();
    }
    let avg: f64 = sql::scalar(
        &mut pool.clone(),
        "SELECT sum(cpu_sum)/sum(cpu_samples) FROM host_hours WHERE node_id=$1",
        params![node],
    )
    .await
    .unwrap();
    assert_eq!(avg, 50.0);
    let chunks:Vec<String>=sql::scalars(&mut pool.clone(), "SELECT format('%I.%I',chunk_schema,chunk_name) FROM timescaledb_information.chunks WHERE hypertable_name IN ('host_samples','build_samples','process_samples')", params![]).await.unwrap();
    for chunk in chunks {
        sql::execute(
            &mut pool.clone(),
            &format!("CALL convert_to_columnstore('{chunk}'::regclass)"),
            params![],
        )
        .await
        .unwrap();
    }
    batch.id = Uuid::new_v4();
    batch.sampled_at = at + Duration::seconds(20);
    store::ingest(
        &pool,
        node,
        &batch,
        &serde_json::to_vec(&batch).unwrap(),
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(
        store::ingest(
            &pool,
            node,
            &batch,
            &serde_json::to_vec(&batch).unwrap(),
            Utc::now()
        )
        .await
        .unwrap(),
        "duplicate ACK after columnstore conversion"
    );
    use tower::ServiceExt;
    let router = query::router(
        Service {
            store: store::TelemetryStore::new(pool.clone(), pool.clone()),
            authorization: store::BusinessAuthorizationRepository::new(pool.clone()),
        },
        "x".repeat(32),
    );
    let request = axum::http::Request::builder()
        .uri(format!("/v1/nodes/{node}/latest"))
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(router.clone().oneshot(request).await.unwrap().status(), 401);
    let request = axum::http::Request::builder()
        .uri(format!("/v1/nodes/{node}/latest"))
        .header("authorization", format!("Bearer {}", "x".repeat(32)))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let latest: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        latest["host"]["fresh"], false,
        "replayed historical data must not mark the host fresh"
    );
    sql::execute(&mut pool.clone(), "INSERT INTO inventory_observations(node_id,sampled_at,digest) SELECT node_id,now()-interval '1 hour'-i*interval '1 second',digest FROM inventory_versions CROSS JOIN generate_series(1,101) i WHERE node_id=$1", params![node]).await.unwrap();
    let request = axum::http::Request::builder()
        .uri(format!("/v1/nodes/{node}/inventory"))
        .header("authorization", format!("Bearer {}", "x".repeat(32)))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let inventory: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inventory["samples"].as_array().unwrap().len(), 100);
    assert_eq!(inventory["truncated"], true);
    grpc_authorization(&pool, node, attempt).await;
    drop(pool);
    sql::execute(
        &mut admin,
        &format!("DROP DATABASE {name} WITH (FORCE)"),
        params![],
    )
    .await
    .unwrap();
}

async fn grpc_authorization(pool: &toasty::Db, node: Uuid, attempt: Uuid) {
    use artifactory_emissary_protocol::{
        TelemetryBatch, telemetry_client::TelemetryClient, telemetry_server::TelemetryServer,
    };
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use tonic::transport::{
        Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig,
    };
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let server_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server = params.signed_by(&server_key, &issuer).unwrap();
    let client_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::default();
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client = params.signed_by(&client_key, &issuer).unwrap();
    sql::execute(&mut pool.clone(), &format!("DO $fixture$ BEGIN {}; END $fixture$","CREATE TABLE nodes(id uuid PRIMARY KEY,certificate_hash text,revoked boolean NOT NULL DEFAULT false,certificate_expires_at timestamptz NOT NULL DEFAULT now()+interval '1 day'); CREATE TABLE attempts(id uuid PRIMARY KEY,node_id uuid NOT NULL)"), params![]).await.unwrap();
    sql::execute(
        &mut pool.clone(),
        "INSERT INTO nodes(id,certificate_hash) VALUES($1,$2)",
        params![
            node,
            artifactory_runtime_protocol::crypto::hash(client.der())
        ],
    )
    .await
    .unwrap();
    sql::execute(
        &mut pool.clone(),
        "INSERT INTO attempts(id,node_id) VALUES($1,$2)",
        params![attempt, node],
    )
    .await
    .unwrap();
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let name: String = sql::scalar(
        &mut pool.clone(),
        "SELECT current_database()::text",
        params![],
    )
    .await
    .unwrap();
    let scoped = with_database(&url, &name).unwrap();
    let readonly = connect(
        &with_options(&scoped, &[("default_transaction_read_only", "on")]).unwrap(),
        store::models(),
        PoolSettings::new(2),
    )
    .await
    .unwrap();
    assert!(
        sql::execute(
            &mut readonly.clone(),
            "UPDATE nodes SET revoked=true",
            params![]
        )
        .await
        .is_err(),
        "authorization pool cannot write business state"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = Service {
        store: store::TelemetryStore::new(pool.clone(), readonly.clone()),
        authorization: store::BusinessAuthorizationRepository::new(readonly.clone()),
    };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(server.pem(), server_key.serialize_pem()))
        .client_ca_root(Certificate::from_pem(ca.pem()));
    let task = tokio::spawn(async move {
        Server::builder()
            .tls_config(tls)
            .unwrap()
            .add_service(TelemetryServer::new(service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap();
    });
    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(ca.pem()))
        .identity(Identity::from_pem(client.pem(), client_key.serialize_pem()));
    let channel = Endpoint::from_shared(format!("https://{address}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut rpc = TelemetryClient::new(channel);
    let mut batch = Batch {
        id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        sampled_at: Utc::now(),
        monotonic_ms: 1,
        dropped_batches: 0,
        omitted_processes: 0,
        host: Some(HostSample::default()),
        processes: vec![],
        inventory: None,
    };
    let request = |b: &Batch| TelemetryBatch {
        json: serde_json::to_vec(b).unwrap(),
    };
    assert!(
        !rpc.upload(request(&batch))
            .await
            .unwrap()
            .into_inner()
            .duplicate
    );
    assert!(
        rpc.upload(request(&batch))
            .await
            .unwrap()
            .into_inner()
            .duplicate,
        "ACK loss replays the original receipt"
    );
    batch.id = Uuid::new_v4();
    batch.sampled_at = Utc::now() + Duration::minutes(10);
    assert_eq!(
        rpc.upload(request(&batch)).await.unwrap_err().code(),
        tonic::Code::OutOfRange
    );
    batch.sampled_at = Utc::now() - Duration::days(3);
    assert_eq!(
        rpc.upload(request(&batch)).await.unwrap_err().code(),
        tonic::Code::OutOfRange
    );
    batch.sampled_at = Utc::now();
    batch.processes.push(ProcessSample {
        attempt_id: Uuid::new_v4(),
        pid: 200,
        started_at: 100,
        parent_pid: None,
        step: None,
        cpu_percent: None,
        rss_bytes: None,
        read_bytes: None,
        written_bytes: None,
        status: "running".into(),
    });
    assert_eq!(
        rpc.upload(request(&batch)).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );
    batch.processes[0].attempt_id = attempt;
    rpc.upload(request(&batch)).await.unwrap();
    sql::execute(
        &mut pool.clone(),
        "UPDATE nodes SET revoked=true WHERE id=$1",
        params![node],
    )
    .await
    .unwrap();
    batch.id = Uuid::new_v4();
    assert_eq!(
        rpc.upload(request(&batch)).await.unwrap_err().code(),
        tonic::Code::Unauthenticated,
        "revocation applies to an existing telemetry connection"
    );
    drop(rpc);
    let _ = stop_tx.send(());
    task.await.unwrap();
    drop(readonly);
}

#[tokio::test]
#[ignore = "requires isolated TimescaleDB; reports telemetry store p95"]
async fn telemetry_store_latency() {
    let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
    let mut admin = connect(&url, Default::default(), PoolSettings::new(2))
        .await
        .unwrap();
    let name = format!("perf_{}", Uuid::new_v4().simple());
    sql::execute(&mut admin, &format!("CREATE DATABASE {name}"), params![])
        .await
        .unwrap();
    let scoped = with_database(&url, &name).unwrap();
    let pool = connect(&scoped, store::models(), PoolSettings::new(12))
        .await
        .unwrap();
    artifactory_database::migrate(&pool, &store::MIGRATIONS)
        .await
        .unwrap();
    let store = store::TelemetryStore::new(pool.clone(), pool.clone());
    let node = Uuid::new_v4();
    let mut write = Vec::new();
    let mut read = Vec::new();
    let at = Utc::now() - Duration::minutes(10);
    let range = query::Range {
        from: at - Duration::seconds(1),
        to: at + Duration::minutes(5),
        resolution: query::Resolution::Raw,
    };
    for index in 0..288 {
        let batch = Batch {
            id: Uuid::new_v4(),
            session_id: node,
            sampled_at: at + Duration::seconds(index),
            monotonic_ms: index as u64,
            dropped_batches: 0,
            omitted_processes: 0,
            host: Some(HostSample {
                cpu_percent: Some(50.0),
                ..HostSample::default()
            }),
            processes: vec![],
            inventory: None,
        };
        let bytes = serde_json::to_vec(&batch).unwrap();
        let started = std::time::Instant::now();
        store
            .ingest(node, &batch, &bytes, Utc::now())
            .await
            .unwrap();
        let write_us = started.elapsed().as_micros() as u64;
        let started = std::time::Instant::now();
        assert_eq!(
            store
                .history(node, &range)
                .await
                .unwrap_or_else(|_| panic!("history query failed"))
                .samples
                .len(),
            index as usize + 1
        );
        let read_us = started.elapsed().as_micros() as u64;
        if index >= 32 {
            write.push(write_us);
            read.push(read_us);
        }
    }
    let p95 = |mut values: Vec<u64>| {
        values.sort_unstable();
        values[(values.len() * 95).div_ceil(100) - 1]
    };
    println!(
        "PERF {}",
        serde_json::json!({"telemetry_write_us":p95(write),"telemetry_read_us":p95(read),"samples":256})
    );
    drop(pool);
    sql::execute(
        &mut admin,
        &format!("DROP DATABASE {name} WITH (FORCE)"),
        params![],
    )
    .await
    .unwrap();
}
