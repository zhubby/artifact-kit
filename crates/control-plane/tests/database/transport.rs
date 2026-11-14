//! Real Tonic/mTLS connections against isolated database schemas and controlled storage.
use super::*;
use artifactory_control_plane::{dispatch, grpc::EmissaryService, pki, protocol as pb};
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, Server, ServerTlsConfig};

#[path = "device_load.rs"]
mod load;
#[path = "local_source.rs"]
mod local_source;
#[path = "node_metadata.rs"]
mod node_metadata;

struct Certificates {
    ca: String,
    issuer: rcgen::Issuer<'static, KeyPair>,
    server: String,
    server_key: String,
}

impl Certificates {
    fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "Artifactory test CA");
        let ca = params.self_signed(&key).unwrap().pem();
        let issuer = rcgen::Issuer::new(params, key);
        let key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = params.signed_by(&key, &issuer).unwrap().pem();
        Self {
            ca,
            issuer,
            server,
            server_key: key.serialize_pem(),
        }
    }

    fn device(&self) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "Artifactory test device");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        (
            params.signed_by(&key, &self.issuer).unwrap().pem(),
            key.serialize_pem(),
        )
    }
}

struct Replica {
    state: AppState,
    url: String,
    server: tokio::task::JoinHandle<()>,
}

impl Replica {
    async fn start(state: AppState, certificates: &Certificates) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let incoming = futures::stream::unfold(listener, |listener| async {
            let next = listener.accept().await.map(|(socket, _)| socket);
            Some((next, listener))
        });
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(
                &certificates.server,
                &certificates.server_key,
            ))
            .client_ca_root(Certificate::from_pem(&certificates.ca));
        let service = EmissaryService(state.clone());
        let planning = artifactory_control_plane::planning::PlanningService(state.clone());
        let server = tokio::spawn(async move {
            Server::builder()
                .tls_config(tls)
                .unwrap()
                .add_service(pb::control_server::ControlServer::new(service))
                .add_service(pb::planning_server::PlanningServer::new(planning))
                .serve_with_incoming(incoming)
                .await
                .unwrap();
        });
        Self { state, url, server }
    }

    async fn stop(self) {
        self.state.coordination.stop();
        self.server.abort();
        let _ = self.server.await;
    }
}

struct Device {
    id: Uuid,
    certificate: String,
    key: String,
    client: pb::control_client::ControlClient<Channel>,
    events: mpsc::Sender<pb::EmissaryEvent>,
    commands: tonic::Streaming<pb::Command>,
    startup: Option<artifactory_telemetry_model::nodes::MetadataSnapshot>,
}

impl Device {
    async fn new(pool: &toasty::Db, server: &Replica, certificates: &Certificates) -> Self {
        let id = Uuid::new_v4();
        let (certificate, key) = certificates.device();
        sql::execute(&mut pool.clone(), "INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at) VALUES($1,$2,'linux','amd64','00000000-0000-0000-0000-000000000001',$3,now()+interval '1 day')", params![id, format!("load-{id}"), pki::certificate_hash(&certificate).unwrap()]).await.unwrap();
        let (client, events, commands) =
            Self::open(id, &certificate, &key, &server.url, &certificates.ca).await;
        Self {
            id,
            certificate,
            key,
            client,
            events,
            commands,
            startup: None,
        }
    }

    async fn open(
        id: Uuid,
        certificate: &str,
        key: &str,
        url: &str,
        ca: &str,
    ) -> (
        pb::control_client::ControlClient<Channel>,
        mpsc::Sender<pb::EmissaryEvent>,
        tonic::Streaming<pb::Command>,
    ) {
        Self::try_open(id, certificate, key, url, ca).await.unwrap()
    }

    async fn try_open(
        id: Uuid,
        certificate: &str,
        key: &str,
        url: &str,
        ca: &str,
    ) -> anyhow::Result<(
        pb::control_client::ControlClient<Channel>,
        mpsc::Sender<pb::EmissaryEvent>,
        tonic::Streaming<pb::Command>,
    )> {
        Self::try_open_metadata(id, certificate, key, url, ca, None).await
    }

    async fn try_open_metadata(
        id: Uuid,
        certificate: &str,
        key: &str,
        url: &str,
        ca: &str,
        startup: Option<&artifactory_telemetry_model::nodes::MetadataSnapshot>,
    ) -> anyhow::Result<(
        pb::control_client::ControlClient<Channel>,
        mpsc::Sender<pb::EmissaryEvent>,
        tonic::Streaming<pb::Command>,
    )> {
        let channel = Channel::from_shared(url.to_owned())?
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(ca))
                    .identity(Identity::from_pem(certificate, key)),
            )?
            .connect()
            .await?;
        let mut client = pb::control_client::ControlClient::new(channel);
        let (events, stream) = mpsc::channel(32);
        events
            .send(pb::EmissaryEvent {
                kind: "hello".into(),
                emissary_id: id.to_string(),
                protocol_major: 1,
                payload_json:
                    json!({"startup":startup,"capabilities":["git","bash"],"active_attempts":[]})
                        .to_string(),
                ..Default::default()
            })
            .await?;
        let commands = client
            .connect(ReceiverStream::new(stream))
            .await?
            .into_inner();
        Ok((client, events, commands))
    }

    async fn reconnect(&mut self, server: &Replica, ca: &str) {
        let (client, events, commands) =
            Self::open(self.id, &self.certificate, &self.key, &server.url, ca).await;
        self.client = client;
        self.events = events;
        self.commands = commands;
    }

    async fn next(&mut self, kind: &str) -> pb::Command {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let command = self
                    .commands
                    .message()
                    .await
                    .unwrap()
                    .expect("control stream closed");
                if command.kind == kind {
                    return command;
                }
            }
        })
        .await
        .expect("expected command timed out")
    }

    async fn renew(&mut self, assignment: &pb::Command) -> Duration {
        let started = Instant::now();
        let request = Uuid::new_v4().to_string();
        self.events.send(pb::EmissaryEvent {kind:"heartbeat".into(),emissary_id:self.id.to_string(),command_id:request.clone(),
            payload_json:json!({"capabilities":["git","bash"],"active_attempts":[{"attempt_id":assignment.attempt_id,"lease_token":assignment.lease_token}]}).to_string(),..Default::default()}).await.unwrap();
        loop {
            let response = self.next("renew").await;
            if response.request_id == request {
                return started.elapsed();
            }
        }
    }
}

async fn wait_connected(pool: &toasty::Db, count: i64) {
    tokio::time::timeout(Duration::from_secs(30),async {
        loop {
            let online:i64=sql::scalar(&mut pool.clone(), "SELECT count(*) FROM nodes WHERE connection_owner IS NOT NULL AND capabilities @> '[\"git\",\"bash\"]'", params![]).await.unwrap();
            if online==count {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn session_registration_requires_the_claimed_nodes_current_certificate() {
    let database = Database::new().await;
    let certificates = Certificates::new();
    let server = Replica::start(database.state.clone(), &certificates).await;
    let first = Device::new(&database.pool, &server, &certificates).await;
    let second = Device::new(&database.pool, &server, &certificates).await;
    let before: Vec<(Uuid, Option<Uuid>, Option<Uuid>)> = sql::tuples(
        &mut database.pool.clone(),
        "SELECT id,active_session,connection_owner FROM nodes ORDER BY id",
        params![],
    )
    .await
    .unwrap();
    for claimed in [second.id, Uuid::new_v4()] {
        let result = Device::try_open(
            claimed,
            &first.certificate,
            &first.key,
            &server.url,
            &certificates.ca,
        )
        .await;
        assert!(
            result.is_err(),
            "a certificate must not claim node {claimed}"
        );
    }
    let (unregistered, key) = certificates.device();
    assert!(
        Device::try_open(first.id, &unregistered, &key, &server.url, &certificates.ca)
            .await
            .is_err(),
        "an unregistered certificate must not replace a session"
    );
    for condition in ["revoked=true", "revoked=false,certificate_expires_at=now()"] {
        sql::execute(
            &mut database.pool.clone(),
            &format!("UPDATE nodes SET {condition} WHERE id=$1"),
            params![first.id],
        )
        .await
        .unwrap();
        assert!(
            Device::try_open(
                first.id,
                &first.certificate,
                &first.key,
                &server.url,
                &certificates.ca,
            )
            .await
            .is_err(),
            "a revoked or expired node must not register a session: {condition}"
        );
    }
    let after: Vec<(Uuid, Option<Uuid>, Option<Uuid>)> = sql::tuples(
        &mut database.pool.clone(),
        "SELECT id,active_session,connection_owner FROM nodes ORDER BY id",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(
        before, after,
        "rejected identities must leave sessions intact"
    );
    server.stop().await;
    drop((first, second));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ordinary_health_rejects_superseded_revoked_and_expired_streams() {
    let database = Database::new().await;
    let certificates = Certificates::new();
    let first = Replica::start(database.state.clone(), &certificates).await;
    let second = Replica::start(
        AppState {
            coordination: Default::default(),
            connections: Default::default(),
            ..database.state.clone()
        },
        &certificates,
    )
    .await;
    let mut old = Device::new(&database.pool, &first, &certificates).await;
    let (client, events, commands) = Device::open(
        old.id,
        &old.certificate,
        &old.key,
        &second.url,
        &certificates.ca,
    )
    .await;
    let mut current = Device {
        id: old.id,
        certificate: old.certificate.clone(),
        key: old.key.clone(),
        client,
        events,
        commands,
        startup: None,
    };
    let before: Value = sql::scalar(&mut database.pool.clone(), "SELECT jsonb_build_object('session',active_session,'owner',connection_owner,'last_seen',last_seen,'diagnostics',diagnostics) FROM nodes WHERE id=$1", params![old.id]).await.unwrap();
    let pulse = |node: Uuid| {
        pb::EmissaryEvent {
        kind: "heartbeat".into(),
        emissary_id: node.to_string(),
        command_id: Uuid::new_v4().to_string(),
        payload_json: json!({"capabilities":["git","bash"],"diagnostics":{"fencing_probe":true},"ota":{},"active_attempts":[]}).to_string(),
        ..Default::default()
    }
    };
    old.events.send(pulse(old.id)).await.unwrap();
    let stale = tokio::time::timeout(Duration::from_secs(1), old.commands.message())
        .await
        .expect("a superseded heartbeat must close its old stream")
        .unwrap_err();
    assert_eq!(stale.code(), tonic::Code::Unauthenticated);
    let after: Value = sql::scalar(&mut database.pool.clone(), "SELECT jsonb_build_object('session',active_session,'owner',connection_owner,'last_seen',last_seen,'diagnostics',diagnostics) FROM nodes WHERE id=$1", params![old.id]).await.unwrap();
    assert_eq!(
        before, after,
        "a superseded health report must not write node state"
    );
    let event = pulse(current.id);
    let request = event.command_id.clone();
    current.events.send(event).await.unwrap();
    assert_eq!(current.next("emissary_health").await.request_id, request);
    for restriction in ["revoked=true", "certificate_expires_at=now()"] {
        let mut device = Device::new(&database.pool, &second, &certificates).await;
        sql::execute(
            &mut database.pool.clone(),
            &format!("UPDATE nodes SET {restriction} WHERE id=$1"),
            params![device.id],
        )
        .await
        .unwrap();
        device.events.send(pulse(device.id)).await.unwrap();
        let rejected = tokio::time::timeout(Duration::from_secs(1), device.commands.message())
            .await
            .expect("an unauthorized heartbeat must close its stream")
            .unwrap_err();
        assert_eq!(
            rejected.code(),
            tonic::Code::Unauthenticated,
            "{restriction}"
        );
    }
    first.stop().await;
    second.stop().await;
    drop((old, current));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn health_snapshots_preserve_durable_sessions_and_lease_commits() {
    let database = Database::new().await;
    database.fixture().await;
    let policy: String = sql::scalar(
        &mut database.pool.clone(),
        "SELECT current_setting('synchronous_commit')",
        params![],
    )
    .await
    .unwrap();
    for statement in [
        "CREATE TABLE durability_observations(kind text,policy text)",
        "CREATE FUNCTION observe_durability() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE kind text; BEGIN IF TG_TABLE_NAME='nodes' THEN IF OLD.active_session IS DISTINCT FROM NEW.active_session THEN kind:='session'; ELSE kind:='health'; END IF; ELSE kind:='lease'; END IF; INSERT INTO durability_observations VALUES(kind,current_setting('synchronous_commit')); RETURN NEW; END; $$",
        "CREATE TRIGGER node_durability BEFORE UPDATE ON nodes FOR EACH ROW EXECUTE FUNCTION observe_durability()",
        "CREATE TRIGGER lease_durability BEFORE INSERT OR UPDATE ON attempts FOR EACH ROW EXECUTE FUNCTION observe_durability()",
    ] {
        sql::execute(&mut database.pool.clone(), statement, params![])
            .await
            .unwrap();
    }
    let certificates = Certificates::new();
    let server = Replica::start(database.state.clone(), &certificates).await;
    let mut device = Device::new(&database.pool, &server, &certificates).await;
    scheduler::schedule(&database.state).await.unwrap();
    dispatch::deliver(&database.state).await.unwrap();
    let assignment = device.next("assign").await;
    device
        .events
        .send(pb::EmissaryEvent {
            kind: "accept".into(),
            emissary_id: device.id.to_string(),
            attempt_id: assignment.attempt_id.clone(),
            lease_token: assignment.lease_token.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    device.next("start").await;
    device.renew(&assignment).await;
    let observations: Vec<(String, String)> = sql::tuples(
        &mut database.pool.clone(),
        "SELECT kind,policy FROM durability_observations",
        params![],
    )
    .await
    .unwrap();
    let mut connections = Vec::new();
    let mut subsequent = Vec::<String>::new();
    for _ in 0..10 {
        let mut connection = database.pool.connection().await.unwrap();
        subsequent.push(
            sql::scalar(
                &mut connection,
                "SELECT current_setting('synchronous_commit')",
                params![],
            )
            .await
            .unwrap(),
        );
        connections.push(connection);
    }
    drop(connections);
    server.stop().await;
    drop(device);
    database.close().await;
    for kind in ["session", "health", "lease"] {
        let samples: Vec<_> = observations
            .iter()
            .filter(|entry| entry.0 == kind)
            .collect();
        assert!(!samples.is_empty(), "missing {kind} durability observation");
        let expected = if kind == "health" { "off" } else { &policy };
        assert!(
            samples.iter().all(|entry| entry.1 == expected),
            "{kind} used unexpected commit policy: {samples:?}"
        );
    }
    assert!(
        subsequent.iter().all(|setting| setting == &policy),
        "health policy must not escape its transaction: {subsequent:?}"
    );
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn mtls_owner_failover_replays_unstarted_work_and_fences_old_stream() {
    let database = Database::new().await;
    database.fixture().await;
    let certificates = Certificates::new();
    let first = Replica::start(database.state.clone(), &certificates).await;
    let second = Replica::start(
        AppState {
            coordination: Default::default(),
            connections: Default::default(),
            ..database.state.clone()
        },
        &certificates,
    )
    .await;
    let mut device = Device::new(&database.pool, &first, &certificates).await;
    wait_connected(&database.pool, 1).await;
    scheduler::schedule(&second.state).await.unwrap();
    dispatch::deliver(&first.state).await.unwrap();
    let assignment = device.next("assign").await;
    let before = device.renew(&assignment).await;
    assert!(before < Duration::from_secs(2));
    first.stop().await;
    device.reconnect(&second, &certificates.ca).await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE device_commands SET next_delivery_at=now()",
        params![],
    )
    .await
    .unwrap();
    dispatch::deliver(&second.state).await.unwrap();
    let replay = device.next("assign").await;
    assert_eq!(assignment.command_id, replay.command_id);
    assert_eq!(assignment.attempt_id, replay.attempt_id);
    device.renew(&assignment).await;
    second.stop().await;
    drop(device);
    database.close().await;
}

#[derive(Clone)]
struct SlowStorage {
    release: tokio::sync::watch::Receiver<bool>,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn blocked_reconnects_leave_pool_capacity_for_existing_heartbeats() {
    let database = Database::new().await;
    let certificates = Certificates::new();
    let server = Replica::start(database.state.clone(), &certificates).await;
    let mut existing = Device::new(&database.pool, &server, &certificates).await;
    wait_connected(&database.pool, 1).await;
    let mut blocked = Vec::new();
    for _ in 0..24 {
        let id = Uuid::new_v4();
        let (certificate, key) = certificates.device();
        sql::execute(&mut database.pool.clone(), "INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at) VALUES($1,'reconnect','linux','amd64','00000000-0000-0000-0000-000000000001',$2,now()+interval '1 day')", params![id, pki::certificate_hash(&certificate).unwrap()]).await.unwrap();
        blocked.push((id, certificate, key));
    }
    let mut lock_db = database.pool.clone();
    let mut lock = lock_db.transaction().await.unwrap();
    sql::execute(
        &mut lock,
        "SELECT id FROM nodes WHERE name='reconnect' FOR UPDATE",
        params![],
    )
    .await
    .unwrap();
    let mut connections = tokio::task::JoinSet::new();
    for (id, certificate, key) in blocked {
        let url = server.url.clone();
        let ca = certificates.ca.clone();
        connections.spawn(async move { Device::try_open(id, &certificate, &key, &url, &ca).await });
    }
    let mut observed: i64 = 0;
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // Use the independent observer pool, including while the business
            // pool is full. Restrict the probe to this fixture's nodes relation.
            observed = sql::scalar(&mut database.admin.clone(),
                "SELECT count(DISTINCT a.pid) FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid
                 JOIN pg_class c ON c.oid=l.relation JOIN pg_namespace n ON n.oid=c.relnamespace
                 WHERE n.nspname=$1 AND c.relname='nodes' AND a.wait_event_type='Lock'", params![&database.schema])
            .await
            .unwrap();
            if observed >= 9 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let healthy = tokio::time::timeout(Duration::from_secs(1), async {
        existing
            .events
            .send(pb::EmissaryEvent {
                kind: "heartbeat".into(),
                emissary_id: existing.id.to_string(),
                payload_json: json!({"capabilities":["git","bash"],"ota":{},"active_attempts":[]})
                    .to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        existing.next("emissary_health").await;
    })
    .await;
    lock.rollback().await.unwrap();
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    server.stop().await;
    drop(existing);
    database.close().await;
    assert!(
        observed > 0,
        "the reconnects must actually wait on node locks"
    );
    assert!(
        healthy.is_ok(),
        "reconnects must not exhaust heartbeat connections"
    );
}

async fn slow_object(
    axum::extract::State(state): axum::extract::State<SlowStorage>,
) -> &'static str {
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.maximum.fetch_max(active, Ordering::SeqCst);
    let mut release = state.release.clone();
    while !*release.borrow_and_update() {
        release.changed().await.unwrap();
    }
    state.active.fetch_sub(1, Ordering::SeqCst);
    "artifact"
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn twenty_stalled_artifact_rpcs_leave_heartbeats_and_api_responsive() {
    let database = Database::new().await;
    database.fixture().await;
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = tokio::sync::watch::channel(false);
    let storage = SlowStorage {
        release: gate,
        active: Default::default(),
        maximum: Default::default(),
    };
    let router = axum::Router::new()
        .route(
            "/{*key}",
            axum::routing::get(slow_object).put(|| async { StatusCode::OK }),
        )
        .with_state(storage.clone());
    let storage_server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut settings = (*database.state.settings).clone();
    settings.s3_endpoint = endpoint.clone();
    settings.s3_public_endpoint = endpoint;
    settings.data_dir = directory.path().into();
    settings.control.artifact_verifications = 4;
    let state = AppState {
        storage: Arc::new(Storage::new(&settings).unwrap()),
        settings: Arc::new(settings),
        ..database.state.clone()
    };
    let certificates = Certificates::new();
    let server = Replica::start(state.clone(), &certificates).await;
    let mut device = Device::new(&database.pool, &server, &certificates).await;
    wait_connected(&database.pool, 1).await;
    scheduler::schedule(&state).await.unwrap();
    dispatch::deliver(&state).await.unwrap();
    let assignment = device.next("assign").await;
    let attempt = Uuid::parse_str(&assignment.attempt_id).unwrap();
    device
        .events
        .send(pb::EmissaryEvent {
            kind: "accept".into(),
            emissary_id: device.id.to_string(),
            attempt_id: assignment.attempt_id.clone(),
            lease_token: assignment.lease_token.clone(),
            command_id: Uuid::new_v4().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    device.next("start").await;
    let mut completions = tokio::task::JoinSet::new();
    for index in 0..20 {
        let upload = Uuid::new_v4();
        sql::execute(&mut database.pool.clone(), "INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,$3,$4,8,$5,now()+interval '1 hour')", params![upload, attempt, format!("dist/app-{index}"), format!("uploads/{upload}"), crypto::hash("artifact")]).await.unwrap();
        let mut client = device.client.clone();
        let request = pb::CompleteArtifactRequest {
            upload_id: upload.to_string(),
            attempt_id: assignment.attempt_id.clone(),
            lease_token: assignment.lease_token.clone(),
        };
        if index == 0 {
            let mut duplicate = device.client.clone();
            let request = request.clone();
            completions.spawn(async move { duplicate.complete_artifact(request).await.unwrap() });
        }
        completions.spawn(async move { client.complete_artifact(request).await.unwrap() });
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while storage.active.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut slowest = Duration::ZERO;
    for _ in 0..10 {
        slowest = slowest.max(device.renew(&assignment).await);
        let request = Request::builder()
            .uri("/readyz")
            .body(Body::empty())
            .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            api::router(state.clone()).oneshot(request),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert!(
        slowest < Duration::from_secs(2),
        "slow storage delayed lease renewal: {slowest:?}"
    );
    assert_eq!(
        storage.maximum.load(Ordering::SeqCst),
        4,
        "verification concurrency must respect the configured budget"
    );
    release.send(true).unwrap();
    while let Some(result) = completions.join_next().await {
        result.unwrap();
    }
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM artifacts",
        params![],
    )
    .await
    .unwrap();
    assert_eq!(count, 20);
    // Cancellation during the external S3 wait must be rechecked by the short
    // final transaction, even though object promotion can already have occurred.
    release.send(false).unwrap();
    let upload = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,'dist/canceled',$3,8,$4,now()+interval '1 hour')", params![upload, attempt, format!("uploads/{upload}"), crypto::hash("artifact")]).await.unwrap();
    let mut client = device.client.clone();
    let request = pb::CompleteArtifactRequest {
        upload_id: upload.to_string(),
        attempt_id: assignment.attempt_id.clone(),
        lease_token: assignment.lease_token.clone(),
    };
    let canceled = tokio::spawn(async move { client.complete_artifact(request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while storage.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "UPDATE builds SET cancel_requested=true WHERE id=(SELECT build_id FROM jobs j JOIN attempts a ON a.job_id=j.id WHERE a.id=$1)", params![attempt]).await.unwrap();
    release.send(true).unwrap();
    assert_eq!(
        canceled.await.unwrap().unwrap_err().code(),
        tonic::Code::FailedPrecondition
    );
    let visible: bool = sql::scalar(
        &mut database.pool.clone(),
        "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1)",
        params![upload],
    )
    .await
    .unwrap();
    assert!(
        !visible,
        "cancellation during verification must reject artifact commit"
    );
    println!(
        "slow_storage: pending_artifact_rpcs=21 unique_uploads=20 verification_limit=4 slowest_renewal_ms={:.2}",
        slowest.as_secs_f64() * 1000.0
    );
    server.stop().await;
    storage_server.abort();
    drop(device);
    drop(state);
    database.close().await;
}
