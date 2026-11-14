use super::*;
use artifactory_telemetry_model::nodes::MetadataSnapshot;
use std::collections::BTreeMap;

async fn connect_snapshot(
    device: &mut Device,
    snapshot: &MetadataSnapshot,
) -> Result<
    (
        mpsc::Sender<pb::EmissaryEvent>,
        tonic::Streaming<pb::Command>,
    ),
    tonic::Status,
> {
    let (events, receiver) = mpsc::channel(16);
    events
        .send(pb::EmissaryEvent {
            emissary_id: device.id.to_string(),
            kind: "hello".into(),
            protocol_major: 1,
            payload_json: json!({"startup":snapshot}).to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let commands = device
        .client
        .connect(ReceiverStream::new(receiver))
        .await?
        .into_inner();
    Ok((events, commands))
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn startup_ack_reconnect_replace_and_retired_process_fencing() {
    let database = Database::new().await;
    let certificates = Certificates::new();
    let replica = Replica::start(database.state.clone(), &certificates).await;
    let mut device = Device::new(&database.pool, &replica, &certificates).await;
    device
        .events
        .send(pb::EmissaryEvent {
            emissary_id: device.id.to_string(),
            kind: "heartbeat".into(),
            command_id: Uuid::new_v4().to_string(),
            payload_json: json!({"capabilities":["git","bash","legacy-probe"],"ota":{}})
                .to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    device.next("emissary_health").await;
    assert_eq!(
        sql::scalar::<Value>(
            &mut database.pool.clone(),
            "SELECT capabilities FROM nodes WHERE id=$1",
            params![device.id]
        )
        .await
        .unwrap(),
        json!(["git", "bash", "legacy-probe"]),
        "legacy nodes retain their heartbeat capability updates until upgraded"
    );
    let snapshot = MetadataSnapshot::new(
        Uuid::new_v4(),
        BTreeMap::from([
            ("system.os".into(), json!("linux")),
            ("custom.site".into(), json!("lab")),
            (
                "tools.rust".into(),
                json!({"available":true,"version":"1.91"}),
            ),
        ]),
        vec!["git".into(), "bash".into()],
    )
    .unwrap();
    let (events, mut original_commands) = connect_snapshot(&mut device, &snapshot).await.unwrap();
    assert_eq!(
        original_commands.message().await.unwrap().unwrap().kind,
        "metadata_ack"
    );
    events
        .send(pb::EmissaryEvent {
            emissary_id: device.id.to_string(),
            kind: "heartbeat".into(),
            command_id: Uuid::new_v4().to_string(),
            payload_json: json!({"capabilities":["injected-capability"],"ota":{}}).to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        original_commands.message().await.unwrap().unwrap().kind,
        "emissary_health"
    );
    assert_eq!(
        sql::scalar::<Value>(
            &mut database.pool.clone(),
            "SELECT capabilities FROM nodes WHERE id=$1",
            params![device.id]
        )
        .await
        .unwrap(),
        json!(snapshot.capabilities),
        "new nodes change static capabilities only with a new startup snapshot"
    );
    let first = database
        .state
        .store
        .node_metadata(device.id)
        .await
        .unwrap()
        .unwrap();
    // Ignore the ACK and resend exactly the cached data.
    events
        .send(pb::EmissaryEvent {
            emissary_id: device.id.to_string(),
            kind: "metadata".into(),
            payload_json: serde_json::to_string(&snapshot).unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        original_commands
            .message()
            .await
            .unwrap()
            .unwrap()
            .request_id,
        snapshot.startup_id.to_string()
    );
    let (_reconnect, mut commands) = connect_snapshot(&mut device, &snapshot).await.unwrap();
    assert_eq!(
        commands.message().await.unwrap().unwrap().kind,
        "metadata_ack"
    );
    assert_eq!(
        database
            .state
            .store
            .node_metadata(device.id)
            .await
            .unwrap()
            .unwrap(),
        first,
        "reconnect and ACK retries must not rewrite the snapshot"
    );
    let replacement = MetadataSnapshot::new(
        Uuid::new_v4(),
        BTreeMap::from([("system.os".into(), json!("linux"))]),
        vec!["git".into()],
    )
    .unwrap();
    let (_current, mut commands) = connect_snapshot(&mut device, &replacement).await.unwrap();
    assert_eq!(
        commands.message().await.unwrap().unwrap().kind,
        "metadata_ack"
    );
    let metadata = database
        .state
        .store
        .node_metadata(device.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata["metadata"],
        json!({"system.os":"linux"}),
        "a new startup fully replaces old tools and custom fields"
    );
    let _ = events
        .send(pb::EmissaryEvent {
            emissary_id: device.id.to_string(),
            kind: "metadata".into(),
            payload_json: serde_json::to_string(&snapshot).unwrap(),
            ..Default::default()
        })
        .await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            original_commands.message()
        )
        .await
        .unwrap()
        .is_err(),
        "the superseded stream must not receive an ACK for old metadata"
    );
    assert!(
        connect_snapshot(&mut device, &snapshot).await.is_err(),
        "a retired process cannot recover ownership"
    );
    let admin:Value=sql::scalar(&mut database.pool.clone(),"SELECT jsonb_build_object('labels',labels,'signing_profiles',signing_profiles,'slots',slots) FROM nodes WHERE id=$1",params![device.id]).await.unwrap();
    assert_eq!(admin, json!({"labels":{},"signing_profiles":[],"slots":1}));
    replica.stop().await;
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn metadata_detail_requires_a_viewer_session_and_stays_out_of_lists() {
    let database = Database::new().await;
    let node = database.node().await;
    let user = Uuid::new_v4();
    let startup = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(), "UPDATE nodes SET startup_id=$2,metadata=jsonb_build_object('custom.site','lab'),metadata_digest=repeat('a',64),metadata_received_at=clock_timestamp() WHERE id=$1",params![node,startup]).await.unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO users(id,username,password_hash,role) VALUES($1,'metadata-viewer','unused','viewer')",params![user]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO sessions VALUES($1,$2,now()+interval '1 hour')",
        params![crypto::hash("metadata-session"), user],
    )
    .await
    .unwrap();
    sql::execute(&mut database.pool.clone(), "INSERT INTO api_tokens(id,user_id,name,token_hash,project_ids,scopes,expires_at) VALUES($1,$2,'metadata-test',$3,'{}','{read}',now()+interval '1 hour')",params![Uuid::new_v4(),user,crypto::hash("metadata-token")]).await.unwrap();
    let app = api::router(database.state.clone());
    let uri = format!("/api/v1/nodes/{node}/metadata");
    for (header, status) in [
        (None, StatusCode::UNAUTHORIZED),
        (
            Some(("authorization", "Bearer metadata-token")),
            StatusCode::FORBIDDEN,
        ),
        (
            Some(("cookie", "artifactory_session=metadata-session")),
            StatusCode::OK,
        ),
    ] {
        let mut request = Request::builder().uri(&uri);
        if let Some((name, value)) = header {
            request = request.header(name, value);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "metadata access with {header:?}");
        if status == StatusCode::OK {
            let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
                .await
                .unwrap();
            let detail: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(detail["startup_id"], json!(startup));
            assert_eq!(detail["metadata"], json!({"custom.site":"lab"}));
            assert_eq!(detail["digest"], "a".repeat(64));
            assert!(detail["received_at"].is_string());
        }
    }
    for (path, expected) in [
        ("/api/v1/nodes".into(), StatusCode::OK),
        (
            format!("/api/v1/nodes/{}/metadata", Uuid::new_v4()),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("cookie", "artifactory_session=metadata-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            let bytes = axum::body::to_bytes(response.into_body(), 128 * 1024)
                .await
                .unwrap();
            let nodes: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(nodes[0].get("metadata").is_none());
        }
    }
    database.close().await;
}

async fn planner(
    database: &Database,
    replica: &Replica,
    certificates: &Certificates,
    build: Uuid,
) -> (pb::planning_client::PlanningClient<Channel>, Uuid, Value) {
    let (certificate, key) = certificates.device();
    let worker = Uuid::new_v4();
    let run = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(),"INSERT INTO ai_workers(id,name,certificate_hash,certificate_expires_at) VALUES($1,'test',$2,now()+interval '1 day')",params![worker,pki::certificate_hash(&certificate).unwrap()]).await.unwrap();
    sql::execute(&mut database.pool.clone(),"UPDATE builds SET status='planning',goal='test',planning_context=jsonb_build_object('pool_ids',jsonb_build_array('00000000-0000-0000-0000-000000000001'),'signing_profiles','[]'::jsonb) WHERE id=$1",params![build]).await.unwrap();
    sql::execute(&mut database.pool.clone(),"INSERT INTO planning_runs(id,build_id,worker_id,purpose,state,model_config,lease_hash,lease_expires_at,deadline_at) VALUES($1,$2,$3,'initial','leased','{}',$4,now()+interval '1 minute',now()+interval '5 minutes')",params![run,build,worker,crypto::hash("lease")]).await.unwrap();
    let identity = json!({"endpoint":replica.url,"ca":certificates.ca,"certificate":certificate,"key":key,"run_id":run,"lease_token":"lease"});
    let channel = Channel::from_shared(replica.url.clone())
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(Certificate::from_pem(&certificates.ca))
                .identity(Identity::from_pem(certificate, key)),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    (
        pb::planning_client::PlanningClient::new(channel),
        run,
        identity,
    )
}
async fn node_tool(
    client: &mut pb::planning_client::PlanningClient<Channel>,
    run: Uuid,
    name: &str,
    args: Value,
) -> Value {
    let reply = client
        .tool(pb::PlannerToolRequest {
            run_id: run.to_string(),
            lease_token: "lease".into(),
            name: name.into(),
            arguments_json: args.to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    serde_json::from_str(&reply.result_json).unwrap()
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn scoped_node_queries_page_and_keep_missing_telemetry_unknown() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    let certificates = Certificates::new();
    let replica = Replica::start(database.state.clone(), &certificates).await;
    let (mut client, run, _) = planner(&database, &replica, &certificates, build).await;
    sql::execute(&mut database.pool.clone(),"INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at,metadata) SELECT gen_random_uuid(),'node-'||s,'linux','amd64','00000000-0000-0000-0000-000000000001',s::text,now()+interval '1 day','{\"custom.site\":\"lab\"}' FROM generate_series(1,120) s",params![]).await.unwrap();
    let page = node_tool(
        &mut client,
        run,
        "query_nodes",
        json!({"metadata":{"custom.site":"lab"}}),
    )
    .await;
    assert_eq!(page["nodes"].as_array().unwrap().len(), 50, "{page}");
    assert!(page["nodes"][0]["resources"].is_null());
    assert_eq!(page["nodes"][0]["resources_fresh"], false);
    assert!(page["nodes"][0].get("metadata").is_none());
    let next = node_tool(
        &mut client,
        run,
        "query_nodes",
        json!({"cursor":page["next_cursor"],"limit":100}),
    )
    .await;
    assert_eq!(next["nodes"].as_array().unwrap().len(), 70, "{next}");
    let id = page["nodes"][0]["node_id"].clone();
    let detail = node_tool(&mut client, run, "get_node", json!({"node_id":id})).await;
    assert_eq!(detail["nodes"][0]["metadata"]["custom.site"], "lab");
    assert!(
        node_tool(&mut client, run, "query_nodes", json!({"limit":101}))
            .await
            .get("error")
            .is_some()
    );
    sql::execute(&mut database.pool.clone(), "UPDATE nodes SET startup_id=gen_random_uuid(),metadata=metadata || '{\"tools.go\":{\"available\":true,\"version\":\"1.25\"}}',diagnostics='{\"tools\":{\"go\":{\"version\":\"obsolete\"}}}',signing_profiles='{private-profile}' WHERE id=$1",params![serde_json::from_value::<Uuid>(id.clone()).unwrap()]).await.unwrap();
    let tools = node_tool(&mut client, run, "query_nodes", json!({"tools":["go"]})).await;
    assert_eq!(tools["nodes"].as_array().unwrap().len(), 1);
    let summary = node_tool(&mut client, run, "summarize_nodes", json!({})).await;
    let group = summary["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["tools"]["go"]["version"] == "1.25")
        .unwrap();
    assert_eq!(group["signing_profiles"], json!([]));
    assert_eq!(group["task_slots"], 1);
    // Oversized pages end at a stable cursor and keep every node reachable.
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE nodes SET labels=jsonb_build_object('large',repeat('x',4096))",
        params![],
    )
    .await
    .unwrap();
    let mut cursor = Value::Null;
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page = node_tool(
            &mut client,
            run,
            "query_nodes",
            json!({"limit":100,"cursor":cursor}),
        )
        .await;
        assert!(serde_json::to_vec(&page).unwrap().len() <= 128 * 1024);
        for node in page["nodes"].as_array().unwrap() {
            assert!(seen.insert(node["node_id"].as_str().unwrap().to_owned()));
        }
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(seen.len() < 120);
    }
    assert_eq!(seen.len(), 120);
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE projects SET pool_ids='{}' WHERE id=(SELECT project_id FROM builds WHERE id=$1)",
        params![build],
    )
    .await
    .unwrap();
    assert_eq!(
        node_tool(&mut client, run, "get_node", json!({"node_id":id})).await["nodes"],
        json!([]),
        "current project permission overrides frozen access"
    );
    assert_eq!(
        node_tool(&mut client, run, "summarize_nodes", json!({})).await["nodes"],
        json!([])
    );
    replica.stop().await;
    database.close().await;
}

async fn resource_server(
    database: &mut Database,
    certificates: &Certificates,
    values: Arc<RwLock<Vec<Value>>>,
) -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
    resource_server_notifying(database, certificates, values, None).await
}

async fn resource_server_notifying(
    database: &mut Database,
    certificates: &Certificates,
    values: Arc<RwLock<Vec<Value>>>,
    requested: Option<Arc<tokio::sync::Notify>>,
) -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::write(directory.path().join("ca.pem"), &certificates.ca)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new().route(
        "/v1/resources",
        axum::routing::post(move |axum::Json(identities): axum::Json<Vec<Value>>| {
            let values = values.clone();
            let requested = requested.clone();
            async move {
                if let Some(requested) = requested {
                    requested.notify_one();
                }
                axum::Json(
                    values
                        .read()
                        .await
                        .iter()
                        .filter(|value| {
                            identities.iter().any(|id| {
                                id["node_id"] == value["node_id"]
                                    && id["startup_id"] == value["startup_id"]
                            })
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            }
        }),
    );
    let settings = Arc::make_mut(&mut database.state.settings);
    settings.pki_dir = directory.path().into();
    settings.telemetry = Some(artifactory_control_plane::settings::TelemetrySettings {
        endpoint: String::new(),
        query_url: format!("http://{address}"),
        token: "test".repeat(8),
    });
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (directory, task)
}
async fn placement_node(database: &Database) -> (Uuid, Uuid, Value) {
    let node = database.node().await;
    let startup = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(),"UPDATE nodes SET startup_id=$2,metadata_digest=$3,capabilities='[\"git\",\"bash\",\"source-cache-v1\",\"external-build-plan-v1\"]' WHERE id=$1",params![node,startup,"a".repeat(64)]).await.unwrap();
    (
        node,
        startup,
        json!({"node_id":node,"startup_id":startup,"sampled_at":chrono::Utc::now(),"received_at":chrono::Utc::now(),"available_memory_bytes":8192,"build_disk_available_bytes":16384,"cpu_percent":20}),
    )
}
async fn rank(database: &Database, build: Uuid, job: Uuid, ids: &[Uuid]) {
    let placement = json!({"candidates":ids.iter().map(|id|json!({"node_id":id,"metadata_digest":"a".repeat(64),"reason":"native platform and available memory"})).collect::<Vec<_>>(),"resources":{"min_available_memory_bytes":4096,"min_build_disk_available_bytes":8192,"max_cpu_percent":80}});
    sql::execute(&mut database.pool.clone(),"UPDATE builds SET goal='test',planning_context=jsonb_build_object('placement_version',1,'pool_ids',jsonb_build_array('00000000-0000-0000-0000-000000000001'),'signing_profiles','[]'::jsonb),model_config_snapshot='{\"max_calls\":20,\"timeout_seconds\":600}' WHERE id=$1",params![build]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO job_placements(job_id,placement) VALUES($1,$2)",
        params![job, placement],
    )
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn superseded_admission_does_not_start_the_unavailable_timer() {
    let mut database = Database::new().await;
    let certificates = Certificates::new();
    let (node, _, resource) = placement_node(&database).await;
    let (_, build, job) = database.fixture().await;
    rank(&database, build, job, &[node]).await;
    let values = Arc::new(RwLock::new(vec![resource]));
    let requested = Arc::new(tokio::sync::Notify::new());
    let (_directory, server) = resource_server_notifying(
        &mut database,
        &certificates,
        values.clone(),
        Some(requested.clone()),
    )
    .await;
    let resource_guard = values.write().await;
    let state = database.state.clone();
    let scheduler = tokio::spawn(async move { scheduler::schedule(&state).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    // A different replica accepts a new list while this replica fetches resources.
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE job_placements SET version=version+1 WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    drop(resource_guard);
    scheduler.await.unwrap().unwrap();
    let untouched: bool = sql::scalar(
        &mut database.pool.clone(),
        "SELECT waiting_since IS NULL AND reason IS NULL AND NOT EXISTS(SELECT 1 FROM attempts WHERE job_id=$1) FROM job_placements WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert!(
        untouched,
        "a stale admission batch is not an unavailable candidate observation"
    );
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE jobs SET dispatch_after=clock_timestamp() WHERE id=$1",
        params![job],
    )
    .await
    .unwrap();
    scheduler::schedule(&database.state).await.unwrap();
    let selected: Uuid = sql::scalar(
        &mut database.pool.clone(),
        "SELECT node_id FROM attempts WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(
        selected, node,
        "the next fresh batch must still allocate the job"
    );
    server.abort();
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ranked_placement_busy_fallback_resource_changes_and_no_unlisted_devices() {
    let mut database = Database::new().await;
    let certificates = Certificates::new();
    let (a, _, resource_a) = placement_node(&database).await;
    let (b, _, resource_b) = placement_node(&database).await;
    let (_unlisted, _, resource_c) = placement_node(&database).await;
    let resources = Arc::new(RwLock::new(vec![resource_a, resource_b, resource_c]));
    let (_directory, server) =
        resource_server(&mut database, &certificates, resources.clone()).await;
    let (_, build, job) = database.fixture().await;
    // UUID order is deliberately opposite to the model order.
    let mut order = vec![a, b];
    order.sort();
    order.reverse();
    rank(&database, build, job, &order).await;
    scheduler::schedule(&database.state).await.unwrap();
    let selected: Uuid = sql::scalar(
        &mut database.pool.clone(),
        "SELECT node_id FROM attempts WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(selected, order[0]);
    let (_, build2, job2) = database.fixture().await;
    rank(&database, build2, job2, &order).await;
    scheduler::schedule(&database.state).await.unwrap();
    let selected: Uuid = sql::scalar(
        &mut database.pool.clone(),
        "SELECT node_id FROM attempts WHERE job_id=$1",
        params![job2],
    )
    .await
    .unwrap();
    assert_eq!(
        selected, order[1],
        "busy preferred candidate must fall through in the AI order"
    );
    let (_, build3, job3) = database.fixture().await;
    rank(&database, build3, job3, &order).await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE attempts SET status='succeeded'",
        params![],
    )
    .await
    .unwrap();
    for resource in resources.write().await.iter_mut() {
        resource["available_memory_bytes"] = json!(1);
    }
    scheduler::schedule(&database.state).await.unwrap();
    let count: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts WHERE job_id=$1",
        params![job3],
    )
    .await
    .unwrap();
    assert_eq!(
        count, 0,
        "allocation must use the current resources and never an unlisted device"
    );
    assert!(
        sql::scalar::<bool>(
            &mut database.pool.clone(),
            "SELECT reason IS NOT NULL FROM job_placements WHERE job_id=$1",
            params![job3]
        )
        .await
        .unwrap()
    );
    server.abort();
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn placement_reselection_is_bounded_and_cannot_modify_scripts() {
    let database = Database::new().await;
    let certificates = Certificates::new();
    let (_, build, job) = database.fixture().await;
    let (node, _, _) = placement_node(&database).await;
    let replica = Replica::start(database.state.clone(), &certificates).await;
    let (mut client, initial_run, _) = planner(&database, &replica, &certificates, build).await;
    rank(&database, build, job, &[node]).await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET status='queued' WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE planning_runs SET state='failed' WHERE id=$1",
        params![initial_run],
    )
    .await
    .unwrap();
    let original: Value = sql::scalar(
        &mut database.pool.clone(),
        "SELECT target_config FROM jobs WHERE id=$1",
        params![job],
    )
    .await
    .unwrap();
    for round in 1..=2 {
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE job_placements SET waiting_since=now()-interval '31 seconds' WHERE job_id=$1",
            params![job],
        )
        .await
        .unwrap();
        scheduler::tick(&database.state).await.unwrap();
        let run: Uuid = sql::scalar(
            &mut database.pool.clone(),
            "SELECT run_id FROM placement_rounds WHERE build_id=$1 AND round=$2",
            params![build, round],
        )
        .await
        .unwrap();
        let config: Value = sql::scalar(
            &mut database.pool.clone(),
            "SELECT model_config FROM planning_runs WHERE id=$1",
            params![run],
        )
        .await
        .unwrap();
        assert_eq!(config["max_calls"], 5);
        assert_eq!(config["timeout_seconds"], 60);
        sql::execute(&mut database.pool.clone(),"UPDATE planning_runs SET worker_id=(SELECT worker_id FROM planning_runs WHERE id=$2),state='leased',lease_hash=$3,lease_expires_at=now()+interval '1 minute' WHERE id=$1",params![run,initial_run,crypto::hash("lease")]).await.unwrap();
        for _ in 0..5 {
            client
                .report(pb::PlannerReportRequest {
                    run_id: run.to_string(),
                    lease_token: "lease".into(),
                    kind: "model_call".into(),
                    payload_json: "{}".into(),
                })
                .await
                .unwrap();
        }
        assert_eq!(
            client
                .report(pb::PlannerReportRequest {
                    run_id: run.to_string(),
                    lease_token: "lease".into(),
                    kind: "model_call".into(),
                    payload_json: "{}".into()
                })
                .await
                .unwrap_err()
                .code(),
            tonic::Code::ResourceExhausted
        );
        let placement: Value = sql::scalar(
            &mut database.pool.clone(),
            "SELECT placement FROM job_placements WHERE job_id=$1",
            params![job],
        )
        .await
        .unwrap();
        let invalid = node_tool(
            &mut client,
            run,
            "submit_placements",
            json!({"placements":{"app":placement},"scripts":[{"name":"bad","content":"changed"}]}),
        )
        .await;
        assert!(invalid.get("error").is_some(), "{invalid}");
        let mut relaxed = placement.clone();
        relaxed["resources"]["min_available_memory_bytes"] = json!(0);
        let invalid = node_tool(
            &mut client,
            run,
            "submit_placements",
            json!({"placements":{"app":relaxed}}),
        )
        .await;
        assert!(
            invalid.get("error").is_some(),
            "resource thresholds must remain immutable: {invalid}"
        );
        let result = node_tool(
            &mut client,
            run,
            "submit_placements",
            json!({"placements":{"app":placement}}),
        )
        .await;
        assert_eq!(result["accepted"], true, "{result}");
        scheduler::tick(&database.state).await.unwrap();
        let count: i64 = sql::scalar(
            &mut database.pool.clone(),
            "SELECT count(*) FROM placement_rounds WHERE build_id=$1",
            params![build],
        )
        .await
        .unwrap();
        assert_eq!(
            count, round as i64,
            "rounds must be spaced at least 60 seconds apart"
        );
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE placement_rounds SET created_at=now()-interval '61 seconds' WHERE build_id=$1",
            params![build],
        )
        .await
        .unwrap();
    }
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE job_placements SET waiting_since=now()-interval '31 seconds' WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    scheduler::tick(&database.state).await.unwrap();
    assert_eq!(
        sql::scalar::<i64>(
            &mut database.pool.clone(),
            "SELECT count(*) FROM placement_rounds WHERE build_id=$1",
            params![build]
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        sql::scalar::<Value>(
            &mut database.pool.clone(),
            "SELECT target_config FROM jobs WHERE id=$1",
            params![job]
        )
        .await
        .unwrap(),
        original
    );
    replica.stop().await;
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL and ARTIFACTORY_SERVICE_TOKEN"]
async fn runtime_node_tools_authenticate_and_intersect_current_project_scope() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    let node = database.node().await;
    let run = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(),"UPDATE builds SET planner_backend='kubernetes',status='planning',planning_context=jsonb_build_object('pool_ids',jsonb_build_array('00000000-0000-0000-0000-000000000001'),'signing_profiles','[]'::jsonb) WHERE id=$1",params![build]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
        params![run, build],
    )
    .await
    .unwrap();
    let token = std::env::var("ARTIFACTORY_SERVICE_TOKEN")
        .expect("isolated Runtime test needs ARTIFACTORY_SERVICE_TOKEN");
    let router = api::router(database.state.clone());
    let body = json!({"run_id":run,"name":"get_node","arguments":{"node_id":node}});
    for authorized in [false, true] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/internal/runtime/tool")
            .header("content-type", "application/json");
        if authorized {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if authorized {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        if authorized {
            let value: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 131072)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(value["nodes"][0]["node_id"], json!(node));
        }
    }
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE projects SET pool_ids='{}' WHERE id=(SELECT project_id FROM builds WHERE id=$1)",
        params![build],
    )
    .await
    .unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/internal/runtime/tool")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 131072)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(value["nodes"], json!([]));
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn ranked_admission_rechecks_identity_permissions_health_and_both_resource_clocks() {
    let mut database = Database::new().await;
    let certificates = Certificates::new();
    let (node, startup, valid) = placement_node(&database).await;
    let resources = Arc::new(RwLock::new(vec![valid.clone()]));
    let (_directory, server) =
        resource_server(&mut database, &certificates, resources.clone()).await;
    for case in [
        "memory", "disk", "cpu", "sampled", "received", "startup", "digest", "project", "drain",
        "ota", "offline", "tools", "cancel",
    ] {
        let (project, build, job) = database.fixture().await;
        rank(&database, build, job, &[node]).await;
        sql::execute(&mut database.pool.clone(),"UPDATE nodes SET draining=false,ota_draining=false,last_seen=clock_timestamp(),metadata_digest=$2,capabilities='[\"git\",\"bash\",\"source-cache-v1\",\"external-build-plan-v1\"]' WHERE id=$1",params![node,"a".repeat(64)]).await.unwrap();
        let mut current = valid.clone();
        match case {
            "memory" => current["available_memory_bytes"] = Value::Null,
            "disk" => current["build_disk_available_bytes"] = Value::Null,
            "cpu" => current["cpu_percent"] = json!(99),
            "sampled" => {
                current["sampled_at"] = json!(chrono::Utc::now() - chrono::Duration::seconds(91))
            }
            "received" => {
                current["received_at"] = json!(chrono::Utc::now() - chrono::Duration::seconds(91))
            }
            "startup" => current["startup_id"] = json!(Uuid::new_v4()),
            "digest" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE nodes SET metadata_digest=$2 WHERE id=$1",
                    params![node, "b".repeat(64)],
                )
                .await
                .unwrap();
            }
            "project" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE projects SET pool_ids='{}' WHERE id=$1",
                    params![project],
                )
                .await
                .unwrap();
            }
            "drain" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE nodes SET draining=true WHERE id=$1",
                    params![node],
                )
                .await
                .unwrap();
            }
            "ota" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE nodes SET ota_draining=true WHERE id=$1",
                    params![node],
                )
                .await
                .unwrap();
            }
            "offline" => {
                sql::execute(&mut database.pool.clone(),"UPDATE nodes SET last_seen=clock_timestamp()-interval '31 seconds' WHERE id=$1",params![node]).await.unwrap();
            }
            "tools" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE nodes SET capabilities='[\"git\",\"bash\"]' WHERE id=$1",
                    params![node],
                )
                .await
                .unwrap();
            }
            "cancel" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE builds SET cancel_requested=true WHERE id=$1",
                    params![build],
                )
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        *resources.write().await = vec![current];
        scheduler::schedule(&database.state).await.unwrap();
        let count: i64 = sql::scalar(
            &mut database.pool.clone(),
            "SELECT count(*) FROM attempts WHERE job_id=$1",
            params![job],
        )
        .await
        .unwrap();
        assert_eq!(count, 0, "must reject {case}");
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE builds SET cancel_requested=true WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
    }
    assert_ne!(startup, Uuid::nil());
    server.abort();
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn three_ranked_schedulers_preserve_the_node_slot_limit() {
    let mut database = Database::new().await;
    let certificates = Certificates::new();
    let (node, _, resource) = placement_node(&database).await;
    let (_directory, server) = resource_server(
        &mut database,
        &certificates,
        Arc::new(RwLock::new(vec![resource])),
    )
    .await;
    for _ in 0..3 {
        let (_, build, job) = database.fixture().await;
        rank(&database, build, job, &[node]).await;
    }
    let first = database.state.clone();
    let second = AppState {
        coordination: Default::default(),
        ..first.clone()
    };
    let third = AppState {
        coordination: Default::default(),
        ..first.clone()
    };
    let (a, b, c) = tokio::join!(
        scheduler::schedule(&first),
        scheduler::schedule(&second),
        scheduler::schedule(&third)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    let assigned: i64 = sql::scalar(
        &mut database.pool.clone(),
        "SELECT count(*) FROM attempts WHERE node_id=$1",
        params![node],
    )
    .await
    .unwrap();
    assert_eq!(
        assigned, 1,
        "ranked allocation must hold the node lock through slot revalidation"
    );
    server.abort();
    database.close().await;
}

#[tokio::test]
#[ignore = "requires ARTIFACTORY_TEST_DATABASE_URL"]
async fn placement_results_reject_cancellation_expiry_versions_and_running_targets() {
    let database = Database::new().await;
    let (_, build, job) = database.fixture().await;
    let (node, _, _) = placement_node(&database).await;
    let certificates = Certificates::new();
    let replica = Replica::start(database.state.clone(), &certificates).await;
    let (mut client, initial, _) = planner(&database, &replica, &certificates, build).await;
    rank(&database, build, job, &[node]).await;
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE builds SET status='running' WHERE id=$1",
        params![build],
    )
    .await
    .unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE planning_runs SET state='failed' WHERE id=$1",
        params![initial],
    )
    .await
    .unwrap();
    let run = Uuid::new_v4();
    sql::execute(&mut database.pool.clone(),"INSERT INTO planning_runs(id,build_id,worker_id,purpose,state,model_config,lease_hash,lease_expires_at,deadline_at) SELECT $1,build_id,worker_id,'placement','leased','{}',$3,now()+interval '1 minute',now()+interval '1 minute' FROM planning_runs WHERE id=$2",params![run,initial,crypto::hash("lease")]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO placement_rounds(build_id,round,run_id,versions) VALUES($1,1,$2,$3)",
        params![build, run, json!({"app":{"job_id":job,"version":1}})],
    )
    .await
    .unwrap();
    let before: Value = sql::scalar(
        &mut database.pool.clone(),
        "SELECT placement FROM job_placements WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    let args = json!({"placements":{"app":before}});
    for case in ["cancel", "lease", "deadline"] {
        match case {
            "cancel" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE builds SET cancel_requested=true WHERE id=$1",
                    params![build],
                )
                .await
                .unwrap();
            }
            "lease" => {
                sql::execute(&mut database.pool.clone(),"UPDATE planning_runs SET lease_expires_at=now()-interval '1 second' WHERE id=$1",params![run]).await.unwrap();
            }
            "deadline" => {
                sql::execute(
                    &mut database.pool.clone(),
                    "UPDATE planning_runs SET deadline_at=now()-interval '1 second' WHERE id=$1",
                    params![run],
                )
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        let rejected = client
            .tool(pb::PlannerToolRequest {
                run_id: run.to_string(),
                lease_token: "lease".into(),
                name: "submit_placements".into(),
                arguments_json: args.to_string(),
            })
            .await
            .unwrap_err();
        assert_eq!(rejected.code(), tonic::Code::FailedPrecondition, "{case}");
        sql::execute(
            &mut database.pool.clone(),
            "UPDATE builds SET cancel_requested=false WHERE id=$1",
            params![build],
        )
        .await
        .unwrap();
        sql::execute(&mut database.pool.clone(),"UPDATE planning_runs SET lease_expires_at=now()+interval '1 minute',deadline_at=now()+interval '1 minute' WHERE id=$1",params![run]).await.unwrap();
    }
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE job_placements SET version=2 WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    let rejected = node_tool(&mut client, run, "submit_placements", args.clone()).await;
    assert!(
        rejected["error"]
            .as_str()
            .is_some_and(|s| s.contains("version superseded")),
        "{rejected}"
    );
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE jobs SET status='running' WHERE id=$1",
        params![job],
    )
    .await
    .unwrap();
    let accepted = node_tool(&mut client, run, "submit_placements", args).await;
    assert_eq!(accepted["updated_targets"], 0);
    let after: Value = sql::scalar(
        &mut database.pool.clone(),
        "SELECT placement FROM job_placements WHERE job_id=$1",
        params![job],
    )
    .await
    .unwrap();
    assert_eq!(before, after, "a late result cannot change running targets");
    replica.stop().await;
    database.close().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL, Node.js 24 and installed Agent Worker dependencies"]
async fn official_mcp_sdk_initializes_lists_and_queries_over_real_mtls() {
    let database = Database::new().await;
    let (_, build, _) = database.fixture().await;
    let node = database.node().await;
    let certificates = Certificates::new();
    let replica = Replica::start(database.state.clone(), &certificates).await;
    let (_, _, mut config) = planner(&database, &replica, &certificates, build).await;
    config["queries"] = json!([
        {"name":"summarize_nodes","arguments":{}},
        {"name":"query_nodes","arguments":{"os":"linux"}},
        {"name":"get_node","arguments":{"node_id":node}}
    ]);
    let report = mcp_probe(&config).await;
    assert_eq!(report["results"][1]["nodes"][0]["node_id"], json!(node));
    assert_eq!(report["results"][2]["nodes"][0]["metadata"], json!({}));
    replica.stop().await;
    database.close().await;
}

async fn mcp_probe(config: &Value) -> Value {
    use tokio::io::AsyncWriteExt;
    let mut probe = tokio::process::Command::new(
        std::env::var("ARTIFACTORY_NODE").unwrap_or_else(|_| "node".into()),
    )
    .arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../packages/agent-worker/scripts/node-mcp-probe.mjs"
    ))
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    probe
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(config).unwrap())
        .await
        .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), probe.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "MCP probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["initialized"], "artifactory-nodes");
    assert_eq!(
        report["tools"],
        json!(["summarize_nodes", "query_nodes", "get_node"])
    );
    report
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL, Node.js 24, built Agent Worker and ARTIFACTORY_SERVICE_TOKEN"]
async fn official_mcp_sdk_queries_over_real_runtime_and_rejects_stale_leases() {
    use artifactory_planner_runtime::{server, store::RuntimeStore};
    use artifactory_runtime_protocol::{Execution, ModelConfiguration, RunIntent, service::Client};
    let database = Database::new().await;
    let (project, build, _) = database.fixture().await;
    let node = database.node().await;
    let run = Uuid::new_v4();
    let token = std::env::var("ARTIFACTORY_SERVICE_TOKEN").unwrap();
    sql::execute(&mut database.pool.clone(),"UPDATE builds SET planner_backend='kubernetes',status='planning',planning_context=jsonb_build_object('pool_ids',jsonb_build_array('00000000-0000-0000-0000-000000000001'),'signing_profiles','[]'::jsonb) WHERE id=$1",params![build]).await.unwrap();
    sql::execute(
        &mut database.pool.clone(),
        "INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial','{}')",
        params![run, build],
    )
    .await
    .unwrap();
    let schema = format!("runtime_mcp_{}", Uuid::new_v4().simple());
    sql::execute(
        &mut database.pool.clone(),
        &format!("CREATE SCHEMA {schema}"),
        params![],
    )
    .await
    .unwrap();
    let url = with_options(
        &std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap(),
        &[("search_path", &schema)],
    )
    .unwrap();
    let runtime_db = connect(
        &url,
        artifactory_planner_runtime::store::models(),
        PoolSettings::new(4),
    )
    .await
    .unwrap();
    artifactory_database::migrate(&runtime_db, &artifactory_planner_runtime::store::MIGRATIONS)
        .await
        .unwrap();
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_origin = format!("http://{}", control_listener.local_addr().unwrap());
    let app = api::router(database.state.clone());
    let control = tokio::spawn(async move {
        axum::serve(control_listener, app).await.unwrap();
    });
    let mut upstream = Client::new("https://control.invalid".into(), token.clone()).unwrap();
    // Only disposable loopback fixtures use HTTP; service authorization remains active.
    upstream.origin = control_origin;
    let runtime_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", runtime_listener.local_addr().unwrap());
    let key = Arc::new([7; 32]);
    let runtime = server::Runtime {
        store: RuntimeStore::new(runtime_db.clone(), key.clone()),
        key,
        control: upstream,
        model_http: reqwest::Client::new(),
        public_url: origin.clone(),
    };
    let intent = RunIntent {
        run_id: run,
        build_id: build,
        project_id: project,
        version: 1,
        snapshot_id: Uuid::new_v4(),
        snapshot_digest: "a".repeat(64),
        pool: "default".into(),
        context: json!({}),
        model: ModelConfiguration {
            id: Uuid::new_v4(),
            api: "openai-completions".into(),
            base_url: "https://model.invalid/v1".into(),
            model: "fixture".into(),
            api_key: "fixture-private-key".into(),
            parameters: json!({}),
            max_calls: 2,
            timeout_seconds: 60,
        },
    };
    server::put_intent(&runtime, serde_json::to_value(intent).unwrap())
        .await
        .unwrap();
    let execution = Execution {
        run_id: run,
        generation: 1,
        uid: "fixture-pod".into(),
        name: "fixture-pod".into(),
        namespace: "isolated".into(),
    };
    server::register_execution(&runtime, execution.clone())
        .await
        .unwrap();
    sql::execute(
        &mut runtime_db.clone(),
        "UPDATE runtime_runs SET status='provisioning' WHERE id=$1",
        params![run],
    )
    .await
    .unwrap();
    let acquired = runtime
        .store
        .acquire_execution(execution, &origin)
        .await
        .unwrap();
    let mut stale = acquired["lease"].clone();
    stale["generation"] = json!(0);
    let app = server::router(runtime, token);
    let runner = tokio::spawn(async move {
        axum::serve(runtime_listener, app).await.unwrap();
    });
    let config = json!({"transport":"runtime","endpoint":origin,"lease":acquired["lease"],"stale_lease":stale,"queries":[
        {"name":"summarize_nodes","arguments":{}},
        {"name":"query_nodes","arguments":{"os":"linux"}},
        {"name":"get_node","arguments":{"node_id":node}}
    ]});
    let report = mcp_probe(&config).await;
    assert_eq!(report["staleLeaseRejected"], true);
    assert_eq!(report["results"][1]["nodes"][0]["node_id"], json!(node));
    assert_eq!(report["results"][2]["nodes"][0]["metadata"], json!({}));
    sql::execute(
        &mut database.pool.clone(),
        "UPDATE projects SET pool_ids='{}' WHERE id=$1",
        params![project],
    )
    .await
    .unwrap();
    let report = mcp_probe(&config).await;
    for result in report["results"].as_array().unwrap() {
        assert_eq!(
            result["nodes"],
            json!([]),
            "current project authorization is enforced through Runtime"
        );
    }
    runner.abort();
    let _ = runner.await;
    control.abort();
    let _ = control.await;
    drop(runtime_db);
    sql::execute(
        &mut database.pool.clone(),
        &format!("DROP SCHEMA {schema} CASCADE"),
        params![],
    )
    .await
    .unwrap();
    database.close().await;
}
