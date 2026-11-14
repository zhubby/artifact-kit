//! Business/runtime handoff. Only stage transitions cross this boundary.
use crate::{crypto, execution_plan::ExecutionPlan, source_cache, state::AppState};
use anyhow::{Context, Result, ensure};
use artifactory_runtime_protocol::{
    ModelConfiguration, RunIntent,
    service::{self, Client},
};
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
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
    let origin = s.store.source_project_origin(project).await?;
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
        let encrypted = s.store.credential_ciphertext(id).await?;
        let revision = crypto::hash(&encrypted);
        let hash = crypto::hash(format!("{project}:{id}:{revision}"));
        let key = Uuid::parse_str(&hash[..32])?;
        mapped = Some(key);
        credentials.push(json!({"id":key,"revision":revision,"value":serde_json::from_slice::<Value>(&crypto::open(&s.settings.master_key,&encrypted)?)?}));
    }
    source(origin)?.post("/v1/refs",&json!({"operation_id":Uuid::new_v4(),"project_id":project,"repository_url":url,"reference":"HEAD","credential_id":mapped,"credentials":credentials})).await
}
pub async fn source_origin(state: &AppState, snapshot: Uuid) -> Result<Option<String>> {
    state.store.source_origin(snapshot).await
}
pub async fn source_download_origin(state: &AppState, snapshot: Uuid) -> Result<Option<String>> {
    state.store.source_download_origin(snapshot).await
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
    let valid = s
        .store
        .authorize_source_download(
            crypto::hash(v["token"].as_str().unwrap_or("")),
            id(&v, "snapshot_id")?,
        )
        .await?;
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
    let build = s.store.runtime_tool_build(id(&v, "run_id")?).await?;
    Ok(Json(match v["name"].as_str() {
        Some(name @ ("node_capabilities" | "summarize_nodes" | "query_nodes" | "get_node")) => {
            crate::node_query::tool(&s, build, name, &v["arguments"]).await?
        }
        Some("read_logs") => crate::planning::read_logs(&s, build, &v["arguments"]).await?,
        _ => json!({"error":"tool not permitted"}),
    }))
}
pub async fn accept_event(s: &AppState, v: Value) -> Result<Value> {
    let event = id(&v, "event_id")?;
    let run = id(&v, "run_id")?;
    let hash = crypto::hash(serde_json::to_vec(&v)?);
    if let Some(receipt) = s.store.runtime_receipt(event).await? {
        ensure!(receipt.input_hash == hash, "event ID reused");
        return Ok(receipt.result);
    }
    let kind = v["kind"].as_str().context("missing event kind")?;
    let payload = &v["payload"];
    let run_source = s.store.runtime_run_source(run).await?;
    let mut validation = None;
    if kind == "proposal" {
        let remote: Value = runtime()?.post("/v1/get", &json!({"run_id":run})).await?;
        ensure!(
            remote["lifecycle"]["generation"] == payload["generation"],
            "stale runtime generation"
        );
        let digest = s
            .store
            .source_snapshot_digest(run_source.snapshot_id)
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
                    let snapshot: Uuid = run_source.snapshot_id;
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
    s.store
        .commit_runtime_event(
            crate::store::RuntimeEventCommit {
                event,
                run,
                build: run_source.build_id,
                hash,
                kind,
                payload,
                validation,
            },
            |context| async move {
                crate::planning::prepare_plan(
                    s,
                    context,
                    payload["payload"]
                        .get("plan")
                        .unwrap_or(&payload["payload"])
                        .clone(),
                    true,
                )
                .await
            },
        )
        .await
}
async fn event(
    State(s): State<AppState>,
    Json(v): Json<Value>,
) -> artifactory_runtime_protocol::error::Result<Json<Value>> {
    Ok(Json(accept_event(&s, v).await?))
}
pub async fn deliver_intents(s: &AppState) -> Result<()> {
    let rows = s.store.runtime_deliveries().await?;
    for row in rows {
        let event: i64 = row.id;
        let run: Uuid = row.run_id;
        let value = if row.kind == "cancel" {
            json!({"run_id":run,"version":row.version,"canceled":true})
        } else {
            let ciphertext = if let Some(value) = row.ciphertext {
                value
            } else {
                let intent = make_intent(s, run).await?;
                let sealed = crypto::seal(&s.settings.master_key, &serde_json::to_vec(&intent)?)?;
                s.store.save_runtime_payload(event, sealed).await?
            };
            serde_json::from_slice(&crypto::open(&s.settings.master_key, &ciphertext)?)?
        };
        let _: Value = runtime()?.post("/v1/intent", &value).await?;
        s.store.runtime_delivered(event).await?;
    }
    Ok(())
}
async fn make_intent(s: &AppState, run: Uuid) -> Result<RunIntent> {
    let input = s.store.runtime_intent_context(run).await?;
    let build = input.build_id;
    let mut model = input.model;
    model["api_key"] = json!(String::from_utf8(crypto::open(
        &s.settings.master_key,
        model["key_ciphertext"]
            .as_str()
            .context("model credential missing")?
    )?)?);
    model.as_object_mut().unwrap().remove("key_ciphertext");
    let context = input.context;
    Ok(RunIntent {
        run_id: run,
        build_id: build,
        project_id: input.project_id,
        version: 1,
        snapshot_id: input.snapshot_id,
        snapshot_digest: input.digest,
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
    let Some(input) = s.store.remote_capture_input(build).await? else {
        return Ok(());
    };
    let project = input.project_id;
    let origin = input.origin.clone();
    let ciphertext = if let Some(value) = input.ciphertext {
        value
    } else {
        let bindings: Vec<source_cache::CredentialBinding> = serde_json::from_value(
            input
                .planning_context
                .get("source_credentials")
                .cloned()
                .unwrap_or(json!([])),
        )?;
        let primary: Option<Uuid> = input.credential_id;
        let mut ids: Vec<Uuid> = bindings.iter().map(|b| b.credential_id).collect();
        ids.extend(primary);
        ids.sort();
        ids.dedup();
        let mut credentials = Vec::new();
        let mut mapped = std::collections::HashMap::new();
        for id in ids {
            let encrypted = s.store.credential_ciphertext(id).await?;
            let revision = crypto::hash(&encrypted);
            let hash = crypto::hash(format!("{project}:{id}:{revision}"));
            let mapped_id = Uuid::parse_str(&hash[..32])?;
            mapped.insert(id, mapped_id);
            credentials.push(json!({"id":mapped_id,"revision":revision,"value":serde_json::from_slice::<Value>(&crypto::open(&s.settings.master_key,&encrypted)?)?}));
        }
        let input = json!({"operation_id":build,"project_id":project,"repository_url":input.repository_url,"reference":input.requested_ref,"credential_id":primary.map(|id|mapped[&id]),"bindings":bindings.iter().map(|b|json!({"repository_url":b.repository_url,"credential_id":mapped[&b.credential_id]})).collect::<Vec<_>>(),"credentials":credentials});
        let sealed = crypto::seal(&s.settings.master_key, &serde_json::to_vec(&input)?)?;
        s.store.save_remote_source_payload(build, sealed).await?
    };
    let model = input.model_config_snapshot;
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
    s.store
        .save_remote_snapshot(crate::store::RemoteSnapshot {
            build,
            project,
            snapshot: &snapshot,
            root_repository,
            result: &result,
            origin,
            public_origin,
            model,
        })
        .await
}
pub async fn resolve_remote_next(s: &AppState) -> Result<()> {
    let candidates = s.store.remote_source_candidates().await?;
    for (build, project) in candidates {
        let origin: String = if let Some(origin) = s.store.source_project_origin(project).await? {
            origin
        } else {
            let origins = service::required("ARTIFACTORY_SOURCE_URLS")?;
            let origins: Vec<_> = origins.split(',').collect();
            let index = (project.as_u128() % origins.len() as u128) as usize;
            s.store
                .route_source_project(project, origins[index])
                .await?
        };
        if !s.store.claim_remote_source(build, origin).await? {
            continue;
        }
        let result = capture_remote(s, build).await;
        s.store.finish_remote_source(build, result.is_err()).await?;
        return result;
    }
    Ok(())
}
