//! Business/runtime handoff. Only stage transitions cross this boundary.
use crate::{crypto, execution_plan::ExecutionPlan, source_cache, state::AppState};
use anyhow::{Context, Result, ensure};
use artifactory_runtime_protocol::{
    ModelConfiguration, RunIntent,
    service::{self, Client},
};
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use sqlx::{Acquire, Row};
use uuid::Uuid;

pub fn runtime() -> Result<Client> {
    Client::new(
        service::required("ARTIFACTORY_RUNTIME_URL")?,
        service::required("ARTIFACTORY_SERVICE_TOKEN")?,
    )
}
pub fn source(origin: String) -> Result<Client> {
    Client::new(origin, service::required("ARTIFACTORY_SERVICE_TOKEN")?)
}
pub async fn remote_refs(
    s: &AppState,
    project: Uuid,
    url: &str,
    credential: Option<Uuid>,
) -> Result<Value> {
    let origin: Option<String> =
        sqlx::query_scalar("SELECT origin FROM source_project_routes WHERE project_id=$1")
            .bind(project)
            .fetch_optional(&s.pool)
            .await?;
    let origin = origin.unwrap_or(
        service::required("ARTIFACTORY_SOURCE_URLS")?
            .split(',')
            .next()
            .context("source pool empty")?
            .into(),
    );
    let mut credentials = Vec::new();
    let mut mapped = None;
    if let Some(id) = credential {
        let encrypted: String =
            sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
                .bind(id)
                .fetch_one(&s.pool)
                .await?;
        let revision = crypto::hash(&encrypted);
        let hash = crypto::hash(format!("{project}:{id}:{revision}"));
        let key = Uuid::parse_str(&hash[..32])?;
        mapped = Some(key);
        credentials.push(json!({"id":key,"revision":revision,"value":serde_json::from_slice::<Value>(&crypto::open(&s.settings.master_key,&encrypted)?)?}));
    }
    source(origin)?.post("/v1/refs",&json!({"operation_id":Uuid::new_v4(),"project_id":project,"repository_url":url,"reference":"HEAD","credential_id":mapped,"credentials":credentials})).await
}
pub async fn source_origin(state: &AppState, snapshot: Uuid) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT service_origin FROM source_snapshots WHERE id=$1")
            .bind(snapshot)
            .fetch_one(&state.pool)
            .await?,
    )
}
pub async fn source_download_origin(state: &AppState, snapshot: Uuid) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(public_origin,service_origin) FROM source_snapshots WHERE id=$1",
    )
    .bind(snapshot)
    .fetch_one(&state.pool)
    .await?)
}
pub fn routes(state: AppState) -> Router {
    let Some(token) = std::env::var("ARTIFACTORY_SERVICE_TOKEN")
        .ok()
        .filter(|v| v.len() >= 32)
    else {
        return Router::new();
    };
    service::protected(
        Router::new()
            .route("/internal/runtime/event", post(event))
            .route("/internal/runtime/tool", post(tool))
            .route("/internal/runtime/source-authorize", post(source_authorize))
            .with_state(state),
        token,
    )
}
async fn source_authorize(
    State(s): State<AppState>,
    Json(v): Json<Value>,
) -> artifactory_runtime_protocol::error::Result<Json<Value>> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM source_tokens t JOIN attempts a ON a.id=t.attempt_id JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id JOIN source_snapshots ss ON ss.id=t.snapshot_id WHERE t.token_hash=$1 AND t.snapshot_id=$2 AND b.source_snapshot_id=ss.id AND b.project_id=ss.project_id AND t.expires_at>clock_timestamp() AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND NOT b.cancel_requested AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp())").bind(crypto::hash(v["token"].as_str().unwrap_or(""))).bind(id(&v,"snapshot_id")?).fetch_one(&s.pool).await?;
    Ok(Json(json!({"valid":valid})))
}
fn id(v: &Value, key: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        v[key].as_str().context("missing identifier")?,
    )?)
}
async fn tool(
    State(s): State<AppState>,
    Json(v): Json<Value>,
) -> artifactory_runtime_protocol::error::Result<Json<Value>> {
    let build:Uuid=sqlx::query_scalar("SELECT b.id FROM builds b JOIN planning_runs r ON r.build_id=b.id WHERE r.id=$1 AND r.authority='kubernetes' AND NOT b.cancel_requested AND b.status IN ('planning','replanning')").bind(id(&v,"run_id")?).fetch_one(&s.pool).await?;
    Ok(Json(match v["name"].as_str() {
        Some("node_capabilities") => json!({"nodes":crate::planning::nodes(&s,build).await?}),
        Some("read_logs") => crate::planning::read_logs(&s, build, &v["arguments"]).await?,
        _ => json!({"error":"tool not permitted"}),
    }))
}
pub async fn accept_event(s: &AppState, v: Value) -> Result<Value> {
    let event = id(&v, "event_id")?;
    let run = id(&v, "run_id")?;
    let hash = crypto::hash(serde_json::to_vec(&v)?);
    if let Some(row) =
        sqlx::query("SELECT input_hash,result FROM runtime_receipts WHERE event_id=$1")
            .bind(event)
            .fetch_optional(&s.pool)
            .await?
    {
        ensure!(
            row.get::<String, _>("input_hash") == hash,
            "event ID reused"
        );
        return Ok(row.get("result"));
    }
    let kind = v["kind"].as_str().context("missing event kind")?;
    let payload = &v["payload"];
    let run_row=sqlx::query("SELECT r.build_id,b.source_snapshot_id FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 AND r.authority='kubernetes'").bind(run).fetch_one(&s.pool).await?;
    let mut validation = None;
    if kind == "proposal" {
        let remote: Value = runtime()?.post("/v1/get", &json!({"run_id":run})).await?;
        ensure!(
            remote["lifecycle"]["generation"] == payload["generation"],
            "stale runtime generation"
        );
        let digest: String = sqlx::query_scalar("SELECT digest FROM source_snapshots WHERE id=$1")
            .bind(run_row.get::<Uuid, _>("source_snapshot_id"))
            .fetch_one(&s.pool)
            .await?;
        ensure!(
            payload["snapshot_digest"] == digest,
            "snapshot digest changed"
        );
        if payload["kind"] == "submit_plan" {
            let plan = payload["payload"]
                .get("plan")
                .unwrap_or(&payload["payload"]);
            match ExecutionPlan::parse(plan.clone()) {
                Err(error) => validation = Some(error.to_string()),
                Ok(plan) => {
                    let snapshot: Uuid = run_row.get("source_snapshot_id");
                    let origin = source_origin(s, snapshot)
                        .await?
                        .context("remote source origin missing")?;
                    let evidence:Value=source(origin)?.post("/v1/evidence",&json!({"snapshot_id":snapshot,"paths":plan.evidence.iter().map(|e|&e.path).collect::<Vec<_>>()})).await?;
                    if evidence["valid"] != true {
                        validation = Some("evidence must reference existing snapshot files".into());
                    }
                }
            }
        }
    }
    let mut tx = s.pool.begin().await?;
    let build: Uuid = run_row.get("build_id");
    sqlx::query("SELECT id FROM builds WHERE id=$1 FOR UPDATE")
        .bind(build)
        .execute(&mut *tx)
        .await?;
    let row=sqlx::query("SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.target_requirements,b.planning_context,b.parameters,b.selected_targets,b.current_plan_id,b.revision_count,b.release_version,b.cancel_requested,b.status AS build_status FROM planning_runs r JOIN builds b ON b.id=r.build_id WHERE r.id=$1 FOR UPDATE OF r").bind(run).fetch_one(&mut *tx).await?;
    if let Some(prior) =
        sqlx::query_scalar::<_, Value>("SELECT result FROM runtime_receipts WHERE event_id=$1")
            .bind(event)
            .fetch_optional(&mut *tx)
            .await?
    {
        return Ok(prior);
    }
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM plan_revisions WHERE run_id=$1")
            .bind(run)
            .fetch_optional(&mut *tx)
            .await?;
    let result = if let Some(plan) = existing {
        json!({"accepted":true,"plan_revision_id":plan})
    } else if row.get::<bool, _>("cancel_requested")
        || !["planning", "replanning"].contains(&row.get::<String, _>("build_status").as_str())
    {
        json!({"error":"build canceled or planning superseded"})
    } else if kind == "failed" {
        sqlx::query(
            "UPDATE planning_runs SET state='failed',error=$2,finished_at=now() WHERE id=$1",
        )
        .bind(run)
        .bind(payload["error"].as_str().unwrap_or("Pi planning failed"))
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1")
            .bind(build)
            .bind(payload["error"].as_str().unwrap_or("Pi planning failed"))
            .execute(&mut *tx)
            .await?;
        json!({"ok":true})
    } else if kind != "proposal" {
        json!({"error":"unknown runtime event"})
    } else if payload["deadline_ms"]
        .as_i64()
        .is_none_or(|d| d <= chrono::Utc::now().timestamp_millis())
    {
        json!({"error":"planning deadline reached"})
    } else if let Some(error) = validation {
        json!({"error":error})
    } else if payload["kind"] == "request_input" {
        let question = payload["payload"]["question"].as_str().unwrap_or("");
        if question.trim().is_empty() || question.len() > 8000 {
            json!({"error":"a concise question is required"})
        } else {
            sqlx::query("UPDATE builds SET status='needs_input',clarification=$2 WHERE id=$1")
                .bind(build)
                .bind(json!({"question":question}))
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE planning_runs SET state='needs_input',finished_at=now() WHERE id=$1",
            )
            .bind(run)
            .execute(&mut *tx)
            .await?;
            json!({"accepted":true,"needs_input":true})
        }
    } else if payload["kind"] == "submit_plan" {
        let mut savepoint = tx.begin().await?;
        let result = crate::planning::submit(
            s,
            &mut savepoint,
            &row,
            payload["payload"]
                .get("plan")
                .unwrap_or(&payload["payload"])
                .clone(),
            true,
        )
        .await;
        match result {
            Ok(value) => {
                savepoint.commit().await?;
                value
            }
            Err(error) => {
                savepoint.rollback().await?;
                json!({"error":error.to_string()})
            }
        }
    } else {
        json!({"error":"invalid proposal kind"})
    };
    sqlx::query("INSERT INTO runtime_receipts(event_id,input_hash,result,run_id,generation) VALUES($1,$2,$3,$4,$5)").bind(event).bind(hash).bind(&result).bind(run).bind(payload["generation"].as_i64()).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO planning_events(run_id,kind,payload) VALUES($1,'runtime_result',$2)")
        .bind(run)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
async fn event(
    State(s): State<AppState>,
    Json(v): Json<Value>,
) -> artifactory_runtime_protocol::error::Result<Json<Value>> {
    Ok(Json(accept_event(&s, v).await?))
}
pub async fn deliver_intents(s: &AppState) -> Result<()> {
    let rows = sqlx::query(
        "SELECT * FROM runtime_intents WHERE delivered_at IS NULL ORDER BY id LIMIT 64",
    )
    .fetch_all(&s.pool)
    .await?;
    for row in rows {
        let event: i64 = row.get("id");
        let run: Uuid = row.get("run_id");
        let value = if row.get::<String, _>("kind") == "cancel" {
            json!({"run_id":run,"version":row.get::<i64,_>("version"),"canceled":true})
        } else {
            let ciphertext = if let Some(value) = row.get::<Option<String>, _>("payload_ciphertext")
            {
                value
            } else {
                let intent = make_intent(s, run).await?;
                let sealed = crypto::seal(&s.settings.master_key, &serde_json::to_vec(&intent)?)?;
                sqlx::query_scalar("UPDATE runtime_intents SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE id=$1 RETURNING payload_ciphertext").bind(event).bind(sealed).fetch_one(&s.pool).await?
            };
            serde_json::from_slice(&crypto::open(&s.settings.master_key, &ciphertext)?)?
        };
        let _: Value = runtime()?.post("/v1/intent", &value).await?;
        sqlx::query("UPDATE runtime_intents SET delivered_at=now() WHERE id=$1")
            .bind(event)
            .execute(&s.pool)
            .await?;
    }
    Ok(())
}
async fn make_intent(s: &AppState, run: Uuid) -> Result<RunIntent> {
    let row=sqlx::query("SELECT r.*,b.project_id,b.source_snapshot_id,b.goal,b.requested_ref,b.commit_sha,b.parameters,b.target_requirements,b.planning_context,b.revision_count,ss.digest,ss.service_origin FROM planning_runs r JOIN builds b ON b.id=r.build_id JOIN source_snapshots ss ON ss.id=b.source_snapshot_id WHERE r.id=$1").bind(run).fetch_one(&s.pool).await?;
    let build: Uuid = row.get("build_id");
    let mut model = row.get::<Value, _>("model_config");
    model["api_key"] = json!(String::from_utf8(crypto::open(
        &s.settings.master_key,
        model["key_ciphertext"]
            .as_str()
            .context("model credential missing")?
    )?)?);
    model.as_object_mut().unwrap().remove("key_ciphertext");
    let current:Option<Value>=sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=(SELECT current_plan_id FROM builds WHERE id=$1)").bind(build).fetch_optional(&s.pool).await?;
    let failed:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('target',j.target,'error',a.error,'attempt_id',a.id) FROM jobs j JOIN LATERAL (SELECT id,error FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status IN ('failed','lost')").bind(build).fetch_all(&s.pool).await?;
    let context = json!({"build_id":build,"goal":row.get::<Option<String>,_>("goal"),"ref":row.get::<String,_>("requested_ref"),"commit":row.get::<Option<String>,_>("commit_sha"),"parameters":row.get::<Value,_>("parameters"),"target_requirements":row.get::<Value,_>("target_requirements"),"project_context":row.get::<Value,_>("planning_context"),"snapshot_id":row.get::<Uuid,_>("source_snapshot_id"),"revision":row.get::<i32,_>("revision_count"),"current_plan":current,"failed_targets":failed,"nodes":crate::planning::nodes(s,build).await?,"source_url":row.get::<Option<String>,_>("service_origin")});
    Ok(RunIntent {
        run_id: run,
        build_id: build,
        project_id: row.get("project_id"),
        version: 1,
        snapshot_id: row.get("source_snapshot_id"),
        snapshot_digest: row.get("digest"),
        context,
        model: ModelConfiguration {
            id: Uuid::parse_str(model["id"].as_str().context("profile ID missing")?)?,
            api: model["api"]
                .as_str()
                .context("model protocol missing")?
                .into(),
            base_url: model["base_url"]
                .as_str()
                .context("model URL missing")?
                .into(),
            model: model["model"].as_str().context("model missing")?.into(),
            api_key: model["api_key"].as_str().unwrap_or_default().into(),
            parameters: model["parameters"].clone(),
            max_calls: model["max_calls"].as_u64().unwrap_or(20) as u32,
            timeout_seconds: model["timeout_seconds"].as_u64().unwrap_or(600) as u32,
        },
        pool: std::env::var("ARTIFACTORY_AGENT_POOL").unwrap_or("default".into()),
    })
}
pub async fn capture_remote(s: &AppState, build: Uuid) -> Result<()> {
    let row=sqlx::query("SELECT b.*,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM builds b WHERE id=$1 AND planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested").bind(build).fetch_optional(&s.pool).await?;
    let Some(row) = row else {
        return Ok(());
    };
    let project: Uuid = row.get("project_id");
    let request = sqlx::query(
        "SELECT origin,payload_ciphertext FROM remote_source_requests WHERE build_id=$1",
    )
    .bind(build)
    .fetch_one(&s.pool)
    .await?;
    let origin: String = request.get("origin");
    let ciphertext = if let Some(value) = request.get::<Option<String>, _>("payload_ciphertext") {
        value
    } else {
        let bindings: Vec<source_cache::CredentialBinding> = serde_json::from_value(
            row.get::<Value, _>("planning_context")
                .get("source_credentials")
                .cloned()
                .unwrap_or(json!([])),
        )?;
        let primary: Option<Uuid> = row.get("credential_id");
        let mut ids: Vec<Uuid> = bindings.iter().map(|b| b.credential_id).collect();
        ids.extend(primary);
        ids.sort();
        ids.dedup();
        let mut credentials = Vec::new();
        let mut mapped = std::collections::HashMap::new();
        for id in ids {
            let encrypted: String =
                sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id=$1")
                    .bind(id)
                    .fetch_one(&s.pool)
                    .await?;
            let revision = crypto::hash(&encrypted);
            let hash = crypto::hash(format!("{project}:{id}:{revision}"));
            let mapped_id = Uuid::parse_str(&hash[..32])?;
            mapped.insert(id, mapped_id);
            credentials.push(json!({"id":mapped_id,"revision":revision,"value":serde_json::from_slice::<Value>(&crypto::open(&s.settings.master_key,&encrypted)?)?}));
        }
        let input = json!({"operation_id":build,"project_id":project,"repository_url":row.get::<String,_>("repository_url"),"reference":row.get::<String,_>("requested_ref"),"credential_id":primary.map(|id|mapped[&id]),"bindings":bindings.iter().map(|b|json!({"repository_url":b.repository_url,"credential_id":mapped[&b.credential_id]})).collect::<Vec<_>>(),"credentials":credentials});
        let sealed = crypto::seal(&s.settings.master_key, &serde_json::to_vec(&input)?)?;
        sqlx::query_scalar("UPDATE remote_source_requests SET payload_ciphertext=coalesce(payload_ciphertext,$2) WHERE build_id=$1 RETURNING payload_ciphertext").bind(build).bind(sealed).fetch_one(&s.pool).await?
    };
    let input: Value = serde_json::from_slice(&crypto::open(&s.settings.master_key, &ciphertext)?)?;
    let result: Value = source(origin.clone())?.post("/v1/capture", &input).await?;
    let snapshot: source_cache::Snapshot = serde_json::from_value(result["snapshot"].clone())?;
    ensure!(snapshot.project_id == project, "source project mismatch");
    let public_origin = service::download_origin(result["origin"].as_str().unwrap_or(&origin))?;
    let root_repository = snapshot
        .repositories
        .first()
        .context("root repository missing")?
        .repository_id;
    let mut tx = s.pool.begin().await?;
    for repository in result["repositories"]
        .as_array()
        .context("repository metadata missing")?
    {
        sqlx::query("INSERT INTO source_repositories(id,project_id,repository_url,cache_key) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO NOTHING").bind(id(repository,"id")?).bind(project).bind(repository["repository_url"].as_str().context("repository URL missing")?).bind(repository["cache_key"].as_str().context("cache key missing")?).execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO source_snapshots(id,project_id,root_repository_id,commit_sha,manifest,digest,service_origin,public_origin) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(id) DO NOTHING").bind(snapshot.id).bind(project).bind(root_repository).bind(&snapshot.commit).bind(&result["snapshot"]).bind(result["digest"].as_str().context("snapshot digest missing")?).bind(origin).bind(public_origin).execute(&mut *tx).await?;
    let changed=sqlx::query("UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1 AND status='resolving' AND NOT cancel_requested").bind(build).bind(&snapshot.commit).bind(snapshot.id).execute(&mut *tx).await?;
    if changed.rows_affected() == 1 {
        sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial',$3)").bind(Uuid::new_v4()).bind(build).bind(row.get::<Option<Value>,_>("model_config_snapshot").context("model configuration missing")?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
pub async fn resolve_remote_next(s: &AppState) -> Result<()> {
    let candidates:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT id,project_id FROM builds WHERE planner_backend='kubernetes' AND status='resolving' AND NOT cancel_requested AND goal IS NOT NULL ORDER BY created_at LIMIT 64").fetch_all(&s.pool).await?;
    for (build, project) in candidates {
        let origin: String = if let Some(origin) =
            sqlx::query_scalar("SELECT origin FROM source_project_routes WHERE project_id=$1")
                .bind(project)
                .fetch_optional(&s.pool)
                .await?
        {
            origin
        } else {
            let origins = service::required("ARTIFACTORY_SOURCE_URLS")?;
            let origins: Vec<_> = origins.split(',').collect();
            let index = (project.as_u128() % origins.len() as u128) as usize;
            sqlx::query_scalar("INSERT INTO source_project_routes(project_id,origin) VALUES($1,$2) ON CONFLICT(project_id) DO UPDATE SET project_id=excluded.project_id RETURNING origin").bind(project).bind(origins[index]).fetch_one(&s.pool).await?
        };
        sqlx::query("INSERT INTO remote_source_requests(build_id,origin) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(build).bind(origin).execute(&s.pool).await?;
        let claimed=sqlx::query("UPDATE remote_source_requests SET lease_until=now()+interval '360 seconds' WHERE build_id=$1 AND (lease_until IS NULL OR lease_until<now())").bind(build).execute(&s.pool).await?;
        if claimed.rows_affected() == 0 {
            continue;
        }
        let result = capture_remote(s, build).await;
        sqlx::query("UPDATE remote_source_requests SET lease_until=now()+interval '3 seconds',error=$2 WHERE build_id=$1").bind(build).bind(if result.is_err(){Some("source service unavailable or capture incomplete")}else{None}).execute(&s.pool).await?;
        // Recoverable source/network faults are retried against the saved operation, bounded to 20 minutes.
        if result.is_err() {
            sqlx::query("UPDATE builds SET status='failed',error='source capture exceeded provisioning deadline',finished_at=now() WHERE id=$1 AND status='resolving' AND created_at<now()-interval '20 minutes'").bind(build).execute(&s.pool).await?;
        }
        return result;
    }
    Ok(())
}
