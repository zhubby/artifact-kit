//! Opt-in load harness: three actual Control processes, real mTLS device streams.
use super::*;
use base64::Engine;
use futures::{StreamExt, stream};
use serde_json::Value;
use std::{path::Path, sync::Mutex};
use tokio_util::sync::CancellationToken;

pub(super) struct ProcessReplica {
    child: tokio::process::Child,
    grpc: String,
    pub(super) http: String,
    log: std::path::PathBuf,
}

enum Role {
    Control,
    Source,
}

impl ProcessReplica {
    async fn start(
        database: &Database,
        certificates: &Certificates,
        root: &Path,
        storage: &str,
        index: usize,
    ) -> Self {
        Self::start_role(database, certificates, root, storage, index, Role::Control).await
    }

    pub(super) async fn source(
        database: &Database,
        certificates: &Certificates,
        root: &Path,
        storage: &str,
        index: usize,
    ) -> Self {
        Self::start_role(database, certificates, root, storage, index, Role::Source).await
    }

    async fn start_role(
        database: &Database,
        certificates: &Certificates,
        root: &Path,
        storage: &str,
        index: usize,
        role: Role,
    ) -> Self {
        let directory = root.join(index.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("ca.pem"), &certificates.ca).unwrap();
        std::fs::write(directory.join("server.pem"), &certificates.server).unwrap();
        std::fs::write(directory.join("server.key"), &certificates.server_key).unwrap();
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let grpc = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let http_address = http.local_addr().unwrap();
        let grpc_address = grpc.local_addr().unwrap();
        drop(http);
        drop(grpc);
        let mut database_url = url::Url::parse(&database.state.settings.database_url).unwrap();
        database_url.query_pairs_mut().append_pair(
            "options",
            &format!("-c search_path={},public", database.schema),
        );
        let log = std::fs::File::create(directory.join("control.log")).unwrap();
        let mut command =
            tokio::process::Command::new(env!("CARGO_BIN_EXE_artifactory-control-plane"));
        command
            .env("DATABASE_URL", database_url.as_str())
            .env("ARTIFACTORY_HTTP_BIND", http_address.to_string())
            .env("ARTIFACTORY_GRPC_BIND", grpc_address.to_string())
            .env("ARTIFACTORY_PUBLIC_URL", format!("http://{http_address}"))
            .env("ARTIFACTORY_INSECURE_DEV", "true")
            .env("ARTIFACTORY_PKI_DIR", &directory)
            .env("ARTIFACTORY_DATA_DIR", &directory)
            .env(
                "ARTIFACTORY_MASTER_KEY",
                base64::engine::general_purpose::STANDARD.encode([1; 32]),
            )
            .env("ARTIFACTORY_S3_ENDPOINT", storage)
            .env("ARTIFACTORY_S3_PUBLIC_ENDPOINT", storage)
            .env("ARTIFACTORY_S3_ACCESS_KEY", "load-fixture")
            .env("ARTIFACTORY_S3_SECRET_KEY", "load-fixture")
            .env(
                "ARTIFACTORY_DB_MAX_CONNECTIONS",
                std::env::var("ARTIFACTORY_LOAD_DB_CONNECTIONS").unwrap_or_else(|_| "20".into()),
            )
            .env("ARTIFACTORY_CONTROL_DRAIN_SECONDS", "1")
            .env_remove("ARTIFACTORY_RUNTIME_URL")
            .env_remove("ARTIFACTORY_SOURCE_URLS")
            .env_remove("ARTIFACTORY_LOCAL_SOURCE_URL")
            .env_remove("ARTIFACTORY_TELEMETRY_ENDPOINT")
            .env(
                "RUST_LOG",
                std::env::var("ARTIFACTORY_LOAD_LOG")
                    .unwrap_or_else(|_| "artifactory_control_plane=warn".into()),
            )
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true);
        let scheme = match role {
            Role::Control => {
                command.arg("serve");
                "http"
            }
            Role::Source => {
                command
                    .arg("serve-source")
                    .env("ARTIFACTORY_SERVICE_TLS_CERT", directory.join("server.pem"))
                    .env("ARTIFACTORY_SERVICE_TLS_KEY", directory.join("server.key"));
                "https"
            }
        };
        let child = command.spawn().unwrap();
        let replica = Self {
            child,
            grpc: format!("https://{grpc_address}"),
            http: format!("{scheme}://{http_address}"),
            log: directory.join("control.log"),
        };
        let http = reqwest::Client::builder()
            .add_root_certificate(
                reqwest::Certificate::from_pem(certificates.ca.as_bytes()).unwrap(),
            )
            .build()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if http
                    .get(format!("{}/readyz", replica.http))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            let log = std::fs::read_to_string(directory.join("control.log")).unwrap();
            // The fixture uses no deployment credentials. Still omit URI-bearing
            // lines from startup diagnostics so database URLs are never printed.
            for line in log.lines().filter(|line| !line.contains("://")) {
                eprintln!("control startup: {line}");
            }
            panic!("isolated control process failed readiness");
        });
        replica
    }

    pub(super) async fn terminate(&mut self) {
        let status = std::process::Command::new("kill")
            .args(["-TERM", &self.child.id().unwrap().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

impl Drop for ProcessReplica {
    fn drop(&mut self) {
        if let Ok(output) = std::env::var("ARTIFACTORY_LOAD_OUTPUT") {
            let output = std::path::PathBuf::from(output);
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            let sanitized = log
                .lines()
                .filter(|line| !line.contains("://"))
                .collect::<Vec<_>>()
                .join("\n");
            let name = self
                .log
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy();
            let _ = std::fs::write(
                output.parent().unwrap().join(format!("control-{name}.log")),
                sanitized,
            );
        }
    }
}

#[derive(Default)]
struct Metrics {
    heartbeat_ms: Mutex<Vec<f64>>,
    startup_heartbeat_ms: Mutex<Vec<f64>>,
    heartbeat_phases: [Mutex<Vec<f64>>; 3],
    reconnect_ms: Mutex<Vec<f64>>,
    phase: AtomicUsize,
    measurement_begin: Mutex<Option<Instant>>,
    api_ms: Mutex<Vec<f64>>,
    reconnects: AtomicUsize,
    transport_retries: AtomicUsize,
    reconnect_errors: Mutex<HashMap<String, usize>>,
    errors: AtomicUsize,
}

async fn run_device(
    mut device: Device,
    fallback: String,
    ca: String,
    metrics: Arc<Metrics>,
    stop: CancellationToken,
    phase: usize,
) {
    let mut active: Option<pb::Command> = None;
    let mut pending = HashMap::<String, (Instant, usize)>::new();
    let start = tokio::time::Instant::now()
        + Duration::from_millis(u64::try_from(phase % 100).unwrap() * 100);
    let mut heartbeat = tokio::time::interval_at(start, Duration::from_secs(10));
    loop {
        tokio::select! {
            _=stop.cancelled()=>break,
            _=heartbeat.tick()=>{
                let request=Uuid::new_v4().to_string();
                pending.insert(request.clone(),(Instant::now(),metrics.phase.load(Ordering::Relaxed)));
                if pending.len()>16 {metrics.errors.fetch_add(1,Ordering::Relaxed); break;}
                let attempts=active.as_ref().map(|a|vec![json!({"attempt_id":a.attempt_id,"lease_token":a.lease_token})]).unwrap_or_default();
                let event=pb::EmissaryEvent {kind:"heartbeat".into(),emissary_id:device.id.to_string(),command_id:request,
                    payload_json:json!({"capabilities":["git","bash"],"diagnostics":{"emissary_version":"load-fixture"},"ota":{},"active_attempts":attempts}).to_string(),..Default::default()};
                let _=device.events.send(event).await;
            }
            message=device.commands.message()=>match message {
                Ok(Some(command))=>match command.kind.as_str() {
                    "assign" if active.as_ref().is_none_or(|a|a.attempt_id!=command.attempt_id) =>{
                            let _=device.events.send(pb::EmissaryEvent {kind:"accept".into(),emissary_id:device.id.to_string(),attempt_id:command.attempt_id.clone(),lease_token:command.lease_token.clone(),command_id:Uuid::new_v4().to_string(),..Default::default()}).await;
                            active=Some(command);
                    },
                    "emissary_health"=>if let Some((sent,phase))=pending.remove(&command.request_id) {
                        let measuring=metrics.measurement_begin.lock().unwrap().is_some_and(|begin|sent>=begin);
                        let samples=if measuring {&metrics.heartbeat_ms}else{&metrics.startup_heartbeat_ms};
                        let elapsed=sent.elapsed().as_secs_f64()*1000.0;
                        samples.lock().unwrap().push(elapsed);
                        if measuring {metrics.heartbeat_phases[phase].lock().unwrap().push(elapsed);}
                    },
                    "cancel"=>{metrics.errors.fetch_add(1,Ordering::Relaxed); break;},
                    _=>{},
                },
                _=>{
                    metrics.reconnects.fetch_add(1,Ordering::Relaxed);
                    pending.clear();
                    let reconnect_started=Instant::now();
                    let connection=tokio::time::timeout(Duration::from_secs(20),async {
                        loop {
                            let jitter=(Uuid::new_v4().as_u128()%500) as u64+50;
                            tokio::time::sleep(Duration::from_millis(jitter)).await;
                            match Device::try_open_metadata(device.id,&device.certificate,&device.key,&fallback,&ca,device.startup.as_ref()).await {
                                Ok(connection)=>break connection,
                                Err(error)=>{
                                    metrics.transport_retries.fetch_add(1,Ordering::Relaxed);
                                    let kind=if let Some(status)=error.downcast_ref::<tonic::Status>() {
                                        format!("rpc_{:?}",status.code())
                                    } else if let Some(error)=error.chain().find_map(|cause|cause.downcast_ref::<std::io::Error>()) {
                                        format!("io_{:?}",error.kind())
                                    } else {"transport_other".into()};
                                    *metrics.reconnect_errors.lock().unwrap().entry(kind).or_default()+=1;
                                },
                            }
                        }
                    }).await.expect("device exceeded the 20s reconnect budget");
                    let (client,events,commands)=connection;
                    device.client=client;device.events=events;device.commands=commands;
                    metrics.reconnect_ms.lock().unwrap().push(reconnect_started.elapsed().as_secs_f64()*1000.0);
                }
            }
        }
    }
}

fn percentile(samples: &Mutex<Vec<f64>>, percent: usize) -> f64 {
    let mut samples = samples.lock().unwrap().clone();
    assert!(!samples.is_empty(), "no latency samples recorded");
    samples.sort_by(f64::total_cmp);
    samples[((samples.len() - 1) * percent) / 100]
}

fn latency_summary(samples: &Mutex<Vec<f64>>) -> Value {
    let count = samples.lock().unwrap().len();
    if count == 0 {
        return json!({"samples":0});
    }
    json!({"samples":count,"p50_ms":percentile(samples,50),"p95_ms":percentile(samples,95),"p99_ms":percentile(samples,99),"max_ms":percentile(samples,100)})
}

async fn probe_database(pool: toasty::Db, metrics: Arc<Metrics>, stop: CancellationToken) -> Value {
    let mut peaks = [[0_i64; 5]; 3];
    let mut samples = [0_usize; 3];
    let mut maximum_gap = 0.0_f64;
    let mut previous = Instant::now();
    let mut lock_details = json!([]);
    let mut maximum_locks = 0;
    let mut active_details = json!([]);
    let mut maximum_active = 0;
    let mut errors = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _=stop.cancelled()=>break, _=tick.tick()=>{} }
        maximum_gap = maximum_gap.max(previous.elapsed().as_secs_f64() * 1000.0);
        previous = Instant::now();
        let row: Result<(i64,i64,i64,i64,i64),_> = sql::tuple(&mut pool.clone(), "SELECT count(*),count(*) FILTER(WHERE state='active'),count(*) FILTER(WHERE state='active' AND wait_event_type='Lock'),count(*) FILTER(WHERE state='active' AND wait_event_type='IO'),count(*) FILTER(WHERE state='idle in transaction') FROM pg_stat_activity WHERE datname=current_database()", params![]).await;
        let Ok((connections, active, locks, io, idle_tx)) = row else {
            errors += 1;
            continue;
        };
        let phase = metrics.phase.load(Ordering::Relaxed);
        samples[phase] += 1;
        for (peak, value) in peaks[phase]
            .iter_mut()
            .zip([connections, active, locks, io, idle_tx])
        {
            *peak = (*peak).max(value);
        }
        if locks > maximum_locks {
            maximum_locks = locks;
            lock_details=sql::scalar(&mut pool.clone(), "SELECT coalesce(jsonb_agg(jsonb_build_object('wait_event',a.wait_event,'query',left(a.query,180),'blockers',(SELECT array_agg(left(b.query,180)) FROM pg_stat_activity b WHERE b.pid=ANY(pg_blocking_pids(a.pid))))),'[]') FROM pg_stat_activity a WHERE a.datname=current_database() AND a.state='active' AND a.wait_event_type='Lock'", params![]).await.unwrap_or(json!([]));
        }
        if active > maximum_active {
            maximum_active = active;
            active_details=sql::scalar(&mut pool.clone(), "SELECT coalesce(jsonb_agg(to_jsonb(q)),'[]') FROM (SELECT left(query,160) AS query,wait_event_type,wait_event,count(*) AS connections FROM pg_stat_activity WHERE datname=current_database() AND state='active' AND pid<>pg_backend_pid() GROUP BY 1,2,3 ORDER BY count(*) DESC) q", params![]).await.unwrap_or(json!([]));
        }
    }
    let phases=peaks.iter().enumerate().map(|(index,values)| json!({"samples":samples[index],"connections_peak":values[0],"active_queries_peak":values[1],"lock_waits_peak":values[2],"io_waits_peak":values[3],"idle_transactions_peak":values[4]})).collect::<Vec<_>>();
    json!({"connections_peak":peaks.iter().map(|p|p[0]).max(),"active_queries_peak":maximum_active,"active_query_peak_details":active_details,"lock_waits_peak":maximum_locks,"lock_wait_peak_details":lock_details,"phases":phases,"maximum_probe_gap_ms":maximum_gap,"probe_errors":errors})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL; run tests/control/load.py"]
async fn device_fleet_load() {
    use anyhow::Context;

    let Some(nodes) = std::env::var("ARTIFACTORY_LOAD_NODES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return;
    };
    let active = std::env::var("ARTIFACTORY_LOAD_ACTIVE")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let seconds = std::env::var("ARTIFACTORY_LOAD_SECONDS")
        .unwrap_or_else(|_| "30".into())
        .parse::<u64>()
        .unwrap();
    let database_connections = std::env::var("ARTIFACTORY_LOAD_DB_CONNECTIONS")
        .unwrap_or_else(|_| "20".into())
        .parse::<u32>()
        .unwrap();
    assert!(
        (10..=10000).contains(&nodes)
            && active > 0
            && active <= nodes
            && (20..=300).contains(&seconds)
            && (8..=256).contains(&database_connections)
    );
    let database = Database::new().await;
    let database_configuration: Value = sql::scalar(&mut database.pool.clone(), "SELECT jsonb_build_object('server_version',current_setting('server_version'),'max_connections',current_setting('max_connections')::integer,'fsync',current_setting('fsync'),'synchronous_commit',current_setting('synchronous_commit'),'full_page_writes',current_setting('full_page_writes'),'shared_buffers',current_setting('shared_buffers'),'wal_buffers',current_setting('wal_buffers'))", params![]).await.unwrap();
    let root = tempfile::tempdir().unwrap();
    let certificates = Certificates::new();
    let user = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "INSERT INTO users(id,username,password_hash,role) VALUES($1,'load-viewer','unused','viewer')", params![user]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("load-session"), user],
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let storage_server = tokio::spawn(async move {
        // This peer accepts bucket creation. Artifact bytes are tested separately.
        let router =
            axum::Router::new().route("/{*key}", axum::routing::put(|| async { StatusCode::OK }));
        axum::serve(listener, router).await.unwrap();
    });
    let mut replicas = Vec::new();
    for index in 0..3 {
        replicas.push(
            ProcessReplica::start(&database, &certificates, root.path(), &endpoint, index).await,
        );
    }
    let metrics = Arc::new(Metrics::default());
    let log_filter = std::env::var("ARTIFACTORY_LOAD_LOG")
        .unwrap_or_else(|_| "artifactory_control_plane=warn".into());
    let stop = CancellationToken::new();
    let mut actors = tokio::task::JoinSet::new();
    let urls = replicas.iter().map(|r| r.grpc.clone()).collect::<Vec<_>>();
    let bootstrap_stop = CancellationToken::new();
    let bootstrap_probe = tokio::spawn(probe_database(
        database.admin.clone(),
        metrics.clone(),
        bootstrap_stop.clone(),
    ));
    let connected = Instant::now();
    let bootstrap = async {
    let mut devices=stream::iter(0..nodes).map(|index|{
        let certificates=&certificates;
        let urls=&urls;
        let pool=&database.pool;
        async move {
            let id=Uuid::new_v4();
            let (certificate,key)=certificates.device();
            sql::execute(&mut pool.clone(), "INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at) VALUES($1,$2,'linux','amd64','00000000-0000-0000-0000-000000000001',$3,now()+interval '1 day')", params![id, format!("load-{index}"), pki::certificate_hash(&certificate).unwrap()]).await.unwrap();
            let startup=Some(artifactory_telemetry_model::nodes::MetadataSnapshot::new(Uuid::new_v4(),std::collections::BTreeMap::from([("system.os".into(),json!("linux")),("system.arch".into(),json!("amd64")),("hardware.logical_cpus".into(),json!(8)),("custom.site".into(),json!("load-fixture"))]),vec!["git".into(),"bash".into()]).unwrap());
            let (client,events,commands)=Device::try_open_metadata(id,&certificate,&key,&urls[index%3],&certificates.ca,startup.as_ref()).await.context("establishing the mTLS fleet")?;
            Ok::<_,anyhow::Error>((index,Device{id,certificate,key,client,events,commands,startup}))
        }
    }).buffer_unordered(32);
    while let Some(device) = devices.next().await {
        let (index,device)=device?;
        actors.spawn(run_device(
            device,
            urls[1].clone(),
            certificates.ca.clone(),
            metrics.clone(),
            stop.clone(),
            index,
        ));
    }
    drop(devices);
    let connect_seconds = connected.elapsed().as_secs_f64();
    wait_connected(&database.pool, nodes as i64).await;
    println!("CONTROL_LOAD_STAGE {}", json!({"stage":"connected","nodes":nodes,"seconds":connect_seconds}));
    for _ in 0..active {
        database.fixture().await;
    }
    println!("CONTROL_LOAD_STAGE {}", json!({"stage":"jobs_created","jobs":active,"seconds":connected.elapsed().as_secs_f64()}));
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let count: i64 =
                sql::scalar(&mut database.pool.clone(), "SELECT count(*) FROM attempts WHERE status='running'", params![])
                    .await
                    .unwrap();
            if count == active as i64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("control replicas did not start the requested attempts")?;
    Ok::<_,anyhow::Error>(connect_seconds)
    }.await;
    bootstrap_stop.cancel();
    let bootstrap_database = bootstrap_probe.await.unwrap();
    let connect_seconds = match bootstrap {
        Ok(seconds) => seconds,
        Err(error) => {
            let startup_snapshots:i64=sql::scalar(&mut database.pool.clone(),"SELECT count(*) FROM nodes WHERE startup_id IS NOT NULL AND metadata_digest IS NOT NULL",params![]).await.unwrap();
            let startup_history: i64 = sql::scalar(
                &mut database.pool.clone(),
                "SELECT count(*) FROM node_startups",
                params![],
            )
            .await
            .unwrap();
            let metadata_preserved =
                startup_snapshots == nodes as i64 && startup_history == nodes as i64;
            let evidence = json!({"metadata_snapshots":startup_snapshots,"metadata_history":startup_history,"metadata_preserved":metadata_preserved,"nodes":nodes,"active_attempts":active,"replicas":3,"passed":false,"stage":"bootstrap",
                "bootstrap_error":error.to_string(),"bootstrap_seconds":connected.elapsed().as_secs_f64(),"devices_started":actors.len(),
                "startup_heartbeat_latency":latency_summary(&metrics.startup_heartbeat_ms),"database_probe":bootstrap_database,
                "control_log_filter":log_filter,
                "database_connections_per_replica":database_connections,
                "database_configuration":database_configuration,
                "build_profile":if cfg!(debug_assertions) {"debug"} else {"release"}});
            if let Ok(path) = std::env::var("ARTIFACTORY_LOAD_OUTPUT") {
                std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            }
            println!("CONTROL_LOAD_EVIDENCE {evidence}");
            panic!("load bootstrap failed: {error:#}");
        }
    };
    let initial_attempts: Vec<Uuid> = sql::scalars(
        &mut database.pool.clone(),
        "SELECT id FROM attempts ORDER BY id",
        params![],
    )
    .await
    .unwrap();
    println!(
        "CONTROL_LOAD_STAGE {}",
        json!({"stage":"running","attempts":initial_attempts.len(),"seconds":connected.elapsed().as_secs_f64()})
    );
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let started = Instant::now();
    *metrics.measurement_begin.lock().unwrap() = Some(started);
    let probe_stop = CancellationToken::new();
    let probe = tokio::spawn(probe_database(
        database.admin.clone(),
        metrics.clone(),
        probe_stop.clone(),
    ));
    let mut api_errors = 0;
    let mut hard_killed = false;
    let mut graceful_stopped = false;
    while started.elapsed() < Duration::from_secs(seconds) {
        if !hard_killed && started.elapsed() > Duration::from_secs(seconds / 3) {
            metrics.phase.store(1, Ordering::Relaxed);
            replicas[0].child.kill().await.unwrap();
            hard_killed = true;
        }
        if !graceful_stopped && started.elapsed() > Duration::from_secs(seconds * 2 / 3) {
            metrics.phase.store(2, Ordering::Relaxed);
            replicas[2].terminate().await;
            graceful_stopped = true;
        }
        for path in ["/api/v1/overview", "/api/v1/nodes?limit=100"] {
            let sent = Instant::now();
            let response = http
                .get(format!("{}{path}", replicas[1].http))
                .header("cookie", "artifactory_session=load-session")
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    if response.bytes().await.is_err() {
                        api_errors += 1;
                    }
                }
                _ => api_errors += 1,
            }
            metrics
                .api_ms
                .lock()
                .unwrap()
                .push(sent.elapsed().as_secs_f64() * 1000.0);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let final_attempts: Vec<Uuid> = sql::scalars(
        &mut database.pool.clone(),
        "SELECT id FROM attempts ORDER BY id",
        params![],
    )
    .await
    .unwrap();
    let attempts_preserved = initial_attempts == final_attempts;
    let running: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts WHERE status='running' AND lease_expires_at>now()",
        params![],
    )
    .await
    .unwrap();
    let recovered=tokio::time::timeout(Duration::from_secs(30),async {
        loop {
            let recovered:bool=sql::scalar(&mut database.pool.clone(), "SELECT count(*)=$1 AND count(DISTINCT connection_owner)=1 AND bool_and(last_seen>now()-interval '15 seconds' AND connection_owner IS NOT NULL) FROM nodes", params![nodes as i64]).await.unwrap();
            if recovered {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.is_ok();
    probe_stop.cancel();
    let database_metrics = probe.await.unwrap();
    let heartbeat_p99 = percentile(&metrics.heartbeat_ms, 99);
    let api_p95 = percentile(&metrics.api_ms, 95);
    let sample_count = metrics.heartbeat_ms.lock().unwrap().len();
    let startup_samples = metrics.startup_heartbeat_ms.lock().unwrap().len();
    let startup_p99 = (startup_samples > 0).then(|| percentile(&metrics.startup_heartbeat_ms, 99));
    let resource = std::process::Command::new("ps")
        .args([
            "-o",
            "rss=,%cpu=",
            "-p",
            &replicas[1].child.id().unwrap().to_string(),
        ])
        .output()
        .unwrap();
    let fields = String::from_utf8(resource.stdout).unwrap();
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    stop.cancel();
    while let Some(result) = actors.join_next().await {
        if result.is_err() {
            metrics.errors.fetch_add(1, Ordering::Relaxed);
        }
    }
    let errors = metrics.errors.load(Ordering::Relaxed);
    let startup_snapshots: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM nodes WHERE startup_id IS NOT NULL AND metadata_digest IS NOT NULL",
        params![],
    )
    .await
    .unwrap();
    let startup_history: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM node_startups",
        params![],
    )
    .await
    .unwrap();
    let metadata_preserved = startup_snapshots == nodes as i64 && startup_history == nodes as i64;
    let evidence = json!({"metadata_snapshots":startup_snapshots,"metadata_history":startup_history,"metadata_preserved":metadata_preserved,"nodes":nodes,"active_attempts":active,"replicas":3,"seconds":seconds,"connect_seconds":connect_seconds,
        "bootstrap_database_probe":bootstrap_database,
        "heartbeat_samples":sample_count,"heartbeat_p99_ms":heartbeat_p99,"startup_heartbeat_samples":startup_samples,"startup_heartbeat_p99_ms":startup_p99,"api_p95_ms":api_p95,"api_samples":metrics.api_ms.lock().unwrap().len(),
        "passed":metadata_preserved && errors==0 && api_errors==0 && attempts_preserved && recovered && running==active as i64 && heartbeat_p99<2000.0 && api_p95<1000.0 && sample_count>=nodes*2,
        "reconnects":metrics.reconnects.load(Ordering::Relaxed),"transient_transport_retries":metrics.transport_retries.load(Ordering::Relaxed),"errors":errors,"api_errors":api_errors,"database_connections_peak_including_harness":database_metrics["connections_peak"],
        "reconnect_errors":*metrics.reconnect_errors.lock().unwrap(),
        "database_active_queries_peak_including_harness":database_metrics["active_queries_peak"],"database_lock_waits_peak":database_metrics["lock_waits_peak"],
        "database_lock_wait_peak_details":database_metrics["lock_wait_peak_details"],"database_probe":database_metrics,
        "heartbeat_phases":{"steady":latency_summary(&metrics.heartbeat_phases[0]),"after_sigkill":latency_summary(&metrics.heartbeat_phases[1]),"after_sigterm":latency_summary(&metrics.heartbeat_phases[2])},
        "reconnect_latency":latency_summary(&metrics.reconnect_ms),"build_profile":if cfg!(debug_assertions) {"debug"} else {"release"},
        "control_log_filter":log_filter,
        "database_connections_per_replica":database_connections,
        "database_configuration":database_configuration,
        "cargo_config_overrides":std::env::var("ARTIFACTORY_LOAD_CARGO_CONFIGS").ok().and_then(|value|serde_json::from_str::<Value>(&value).ok()).unwrap_or(json!([])),
        "survivor_rss_kib":fields[0].parse::<u64>().unwrap(),"survivor_cpu_percent_snapshot":fields[1].parse::<f64>().unwrap(),
        "hard_kill":hard_killed,"sigterm_drain":graceful_stopped,"attempt_ids_preserved":attempts_preserved,"valid_running_attempts":running,"all_nodes_recovered":recovered,
        "scope":"real Control child processes and mTLS; virtual executors; controlled S3 peer; no telemetry ingest or artifact throughput load"});
    if let Ok(path) = std::env::var("ARTIFACTORY_LOAD_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
    }
    println!("CONTROL_LOAD_EVIDENCE {evidence}");
    replicas[1].terminate().await;
    storage_server.abort();
    drop(replicas);
    database.close().await;
    assert!(
        metadata_preserved,
        "reconnects must reuse each node startup snapshot"
    );
    assert_eq!(errors, 0);
    assert_eq!(api_errors, 0, "all API probes must succeed");
    assert!(
        attempts_preserved,
        "replica loss must not create replacement attempts"
    );
    assert_eq!(
        running, active as i64,
        "all started attempts must retain valid leases"
    );
    assert!(
        recovered,
        "all nodes must recover onto the surviving replica"
    );
    assert!(
        heartbeat_p99 < 2000.0,
        "heartbeat P99 exceeded 2s: {heartbeat_p99}"
    );
    assert!(api_p95 < 1000.0, "API P95 exceeded 1s: {api_p95}");
    assert!(
        sample_count >= nodes * 2,
        "sampling window must include at least two fleet heartbeat cycles"
    );
}
