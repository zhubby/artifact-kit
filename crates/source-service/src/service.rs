use anyhow::{Context, Result, ensure};
use artifactory_runtime_protocol::{
    crypto,
    objects::Objects,
    service::{self, Client},
};
use artifactory_source_core::{self as core, CredentialBinding, Snapshot};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::{ops::Deref, path::PathBuf, sync::Arc};
use uuid::Uuid;
type ApiResult<T> = artifactory_runtime_protocol::error::Result<Json<T>>;

#[derive(Clone)]
pub struct AppState {
    pub core: core::AppState,
    pub objects: Objects,
    pub control: Client,
    pub runtime: Client,
    pub origin: String,
}
impl Deref for AppState {
    type Target = core::AppState;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}
#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub id: Uuid,
    pub revision: String,
    pub value: core::source::GitCredential,
}
#[derive(Serialize, Deserialize)]
pub struct Capture {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub repository_url: String,
    pub reference: String,
    pub credential_id: Option<Uuid>,
    #[serde(default)]
    pub bindings: Vec<CredentialBinding>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
}
pub fn router(state: AppState, token: String) -> Router {
    let internal = Router::new()
        .route("/metrics", get(metrics))
        .route("/v1/capture", post(capture))
        .route("/v1/metadata", post(metadata))
        .route("/v1/evidence", post(evidence))
        .route("/v1/inspect", post(inspect))
        .route("/v1/refs", post(refs))
        .route("/v1/retain", post(retain))
        .with_state(state.clone());
    let public = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/tool", post(tool))
        .route(
            "/api/v1/source/{snapshot}/{entry}/git/info/refs",
            get(crate::source_http::git),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/git/git-upload-pack",
            post(crate::source_http::git),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/lfs/objects/batch",
            post(crate::source_http::lfs_batch),
        )
        .route(
            "/api/v1/source/{snapshot}/{entry}/lfs/objects/{oid}",
            get(crate::source_http::lfs_object),
        )
        .with_state(state)
        .layer(axum::middleware::map_response(
            crate::source_http::challenge,
        ));
    public
        .merge(service::protected(internal, token))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
}
async fn metrics(State(s): State<AppState>) -> artifactory_runtime_protocol::error::Result<String> {
    let mut output = artifactory_runtime_protocol::metrics::render();
    let (ready,incomplete):(i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE durable),count(*) FILTER(WHERE NOT durable) FROM source_snapshots").fetch_one(&s.pool).await?;
    let references: i64 = sqlx::query_scalar("SELECT count(*) FROM source_retentions")
        .fetch_one(&s.pool)
        .await?;
    output.push_str(&format!("artifactory_source_snapshots{{state=\"durable\"}} {ready}\nartifactory_source_snapshots{{state=\"incomplete\"}} {incomplete}\nartifactory_source_retention_references {references}\n"));
    Ok(output)
}
pub async fn capture_operation(state: &AppState, input: Capture) -> Result<Value> {
    static CAPTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let _capacity = CAPTURES.acquire().await?;
    let hash = crypto::hash(serde_json::to_vec(&input)?);
    sqlx::query(
        "INSERT INTO source_operations(id,input_hash) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(input.operation_id)
    .bind(&hash)
    .execute(&state.pool)
    .await?;
    let mut lock = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,2))")
        .bind(input.operation_id.to_string())
        .execute(&mut *lock)
        .await?;
    let row = sqlx::query("SELECT input_hash,snapshot_id FROM source_operations WHERE id=$1")
        .bind(input.operation_id)
        .fetch_one(&mut *lock)
        .await?;
    ensure!(
        row.get::<String, _>("input_hash") == hash,
        "source operation ID reused with different input"
    );
    let snapshot = if let Some(id) = row.get::<Option<Uuid>, _>("snapshot_id") {
        core::load(state, id).await?
    } else {
        for credential in &input.credentials {
            ensure!(
                credential.revision.len() == 64,
                "invalid credential revision"
            );
            let ciphertext = crypto::seal(
                &state.settings.master_key,
                &serde_json::to_vec(&credential.value)?,
            )?;
            sqlx::query("INSERT INTO credentials(id,revision,ciphertext) VALUES($1,$2,$3) ON CONFLICT(id) DO NOTHING").bind(credential.id).bind(&credential.revision).bind(ciphertext).execute(&state.pool).await?;
            let existing: String =
                sqlx::query_scalar("SELECT revision FROM credentials WHERE id=$1")
                    .bind(credential.id)
                    .fetch_one(&state.pool)
                    .await?;
            ensure!(
                existing == credential.revision,
                "credential revision conflict"
            );
        }
        let snapshot = core::capture(
            state,
            input.project_id,
            &input.repository_url,
            &input.reference,
            input.credential_id,
            input.bindings,
            input.operation_id,
        )
        .await?;
        // Persist the exact manifest before publishing Ready. A lost acknowledgement reuses it.
        sqlx::query("UPDATE source_operations SET snapshot_id=$2 WHERE id=$1")
            .bind(input.operation_id)
            .bind(snapshot.id)
            .execute(&state.pool)
            .await?;
        snapshot
    };
    persist(state, &snapshot).await?;
    sqlx::query(
        "INSERT INTO source_retentions(snapshot_id,owner) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(snapshot.id)
    .bind(format!("operation:{}", input.operation_id))
    .execute(&state.pool)
    .await?;
    lock.commit().await?;
    describe(state, &snapshot).await
}
async fn capture(State(state): State<AppState>, Json(input): Json<Capture>) -> ApiResult<Value> {
    Ok(Json(capture_operation(&state, input).await?))
}
pub async fn describe(state: &AppState, snapshot: &Snapshot) -> Result<Value> {
    let digest: String =
        sqlx::query_scalar("SELECT digest FROM source_snapshots WHERE id=$1 AND durable")
            .bind(snapshot.id)
            .fetch_one(&state.pool)
            .await?;
    let ids: Vec<Uuid> = snapshot
        .repositories
        .iter()
        .map(|r| r.repository_id)
        .collect();
    let repositories:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'repository_url',repository_url,'cache_key',cache_key) FROM source_repositories WHERE id=ANY($1)").bind(ids).fetch_all(&state.pool).await?;
    Ok(
        json!({"snapshot":snapshot,"digest":digest,"repositories":repositories,"origin":state.origin,"analysis_version":1}),
    )
}
async fn metadata(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult<Value> {
    let snapshot = core::load(&state, id(&value, "snapshot_id")?).await?;
    Ok(Json(describe(&state, &snapshot).await?))
}
pub fn id(value: &Value, key: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        value[key].as_str().context("missing identifier")?,
    )?)
}
async fn inspect(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult<Value> {
    let snapshot = core::load(&state, id(&value, "snapshot_id")?).await?;
    restore(&state, &snapshot).await?;
    Ok(Json(
        core::inspect_tool(
            &state,
            snapshot.id,
            value["name"].as_str().context("missing tool")?,
            &value["arguments"],
        )
        .await?,
    ))
}
async fn evidence(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult<Value> {
    let snapshot = core::load(&state, id(&value, "snapshot_id")?).await?;
    restore(&state, &snapshot).await?;
    let paths: Vec<String> = serde_json::from_value(value["paths"].clone())?;
    if paths.len() > 64 {
        return Err(artifactory_runtime_protocol::error::ApiError::BadRequest(
            "too many evidence paths".into(),
        ));
    }
    match core::validate_evidence(&state, &snapshot, &paths).await {
        Ok(()) => Ok(Json(json!({"valid":true}))),
        Err(_) => Ok(Json(
            json!({"valid":false,"error":"evidence must reference existing snapshot files"}),
        )),
    }
}
async fn tool(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult<Value> {
    let auth: Value = state.runtime.post("/v1/validate", &value["lease"]).await?;
    let snapshot = core::load(&state, id(&auth, "snapshot_id")?).await?;
    if snapshot.project_id != id(&auth, "project_id")? {
        return Err(artifactory_runtime_protocol::error::ApiError::Forbidden);
    }
    restore(&state, &snapshot).await?;
    // Restore may wait behind I/O; a grant must still hold when the read begins.
    let _: Value = state.runtime.post("/v1/validate", &value["lease"]).await?;
    let result = core::inspect_tool(
        &state,
        snapshot.id,
        value["name"].as_str().context("missing tool")?,
        &value["arguments"],
    )
    .await;
    Ok(Json(match result {
        Ok(v) => v,
        Err(e) => json!({"error":e.to_string()}),
    }))
}
async fn refs(State(state): State<AppState>, Json(input): Json<Capture>) -> ApiResult<Value> {
    for auth in input.credentials {
        let encrypted = crypto::seal(
            &state.settings.master_key,
            &serde_json::to_vec(&auth.value)?,
        )?;
        sqlx::query("INSERT INTO credentials(id,revision,ciphertext) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(auth.id).bind(auth.revision).bind(encrypted).execute(&state.pool).await?;
    }
    Ok(Json(serde_json::to_value(
        core::list_refs(&state, &input.repository_url, input.credential_id).await?,
    )?))
}
async fn retain(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult<Value> {
    sqlx::query(
        "INSERT INTO source_retentions(snapshot_id,owner) VALUES($1,$2) ON CONFLICT DO NOTHING",
    )
    .bind(id(&value, "snapshot_id")?)
    .bind(value["owner"].as_str().context("missing retention owner")?)
    .execute(&state.pool)
    .await?;
    Ok(Json(json!({"ok":true})))
}
pub async fn persist(state: &AppState, snapshot: &Snapshot) -> Result<()> {
    let durable: bool = sqlx::query_scalar("SELECT durable FROM source_snapshots WHERE id=$1")
        .bind(snapshot.id)
        .fetch_one(&state.pool)
        .await?;
    if durable {
        return Ok(());
    }
    for entry in &snapshot.repositories {
        let repo = core::repository_path(state, entry.repository_id).await?;
        let mut lock = state.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock_shared(hashtextextended($1,0))")
            .bind(repo.file_stem().unwrap().to_string_lossy().as_ref())
            .execute(&mut *lock)
            .await?;
        let temp = tempfile::tempdir_in(&state.settings.data_dir)?;
        let bundle = temp.path().join("source.bundle");
        let prefix = format!(
            "refs/namespaces/{}/refs",
            core::namespace(snapshot.id, entry.id)
        );
        let mut refs = vec![format!("{prefix}/heads/source")];
        refs.extend(entry.tags.keys().map(|t| format!("{prefix}/tags/{t}")));
        let mut args = vec![
            "bundle",
            "create",
            bundle.to_str().context("invalid bundle path")?,
        ];
        args.extend(refs.iter().map(String::as_str));
        core::git(&repo, &args).await?;
        state
            .objects
            .put_file(
                &format!("sources/{}/{}/source.bundle", snapshot.id, entry.id),
                &bundle,
            )
            .await?;
        for lfs in &entry.lfs {
            state
                .objects
                .put_file(
                    &format!("sources/{}/{}/lfs/{}", snapshot.id, entry.id, lfs.oid),
                    &core::lfs_path(&repo, &lfs.oid)?,
                )
                .await?;
        }
        lock.commit().await?;
    }
    state
        .objects
        .put(
            &format!("sources/{}/manifest.json", snapshot.id),
            serde_json::to_vec(snapshot)?,
        )
        .await?;
    sqlx::query("UPDATE source_snapshots SET durable=true WHERE id=$1")
        .bind(snapshot.id)
        .execute(&state.pool)
        .await?;
    Ok(())
}
pub async fn restore(state: &AppState, snapshot: &Snapshot) -> Result<()> {
    for entry in &snapshot.repositories {
        let repo = core::repository_path(state, entry.repository_id).await?;
        let mut lock = state.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(repo.file_stem().unwrap().to_string_lossy().as_ref())
            .execute(&mut *lock)
            .await?;
        let mut missing = !repo.join("HEAD").exists()
            || core::git(&repo, &["cat-file", "-e", &entry.commit])
                .await
                .is_err();
        for tag in entry.tags.values() {
            missing |= core::git(&repo, &["cat-file", "-e", tag]).await.is_err();
        }
        if missing {
            let durable: bool =
                sqlx::query_scalar("SELECT durable FROM source_snapshots WHERE id=$1")
                    .bind(snapshot.id)
                    .fetch_one(&state.pool)
                    .await?;
            ensure!(durable, "snapshot persistence incomplete");
            tokio::fs::create_dir_all(&repo).await?;
            core::git(&repo, &["init", "--bare"]).await?;
            let temp = tempfile::tempdir_in(&state.settings.data_dir)?;
            let bundle = temp.path().join("source.bundle");
            state
                .objects
                .get_file(
                    &format!("sources/{}/{}/source.bundle", snapshot.id, entry.id),
                    &bundle,
                    50 * 1024 * 1024 * 1024,
                )
                .await?;
            core::git(&repo, &["bundle", "verify", bundle.to_str().unwrap()]).await?;
            core::git(&repo, &["bundle", "unbundle", bundle.to_str().unwrap()]).await?;
        }
        for item in &entry.lfs {
            let path = core::lfs_path(&repo, &item.oid)?;
            if !path.exists() {
                tokio::fs::create_dir_all(path.parent().unwrap()).await?;
                let temp = path.with_extension("partial");
                state
                    .objects
                    .get_file(
                        &format!("sources/{}/{}/lfs/{}", snapshot.id, entry.id, item.oid),
                        &temp,
                        item.size,
                    )
                    .await?;
                tokio::fs::rename(temp, path).await?;
            }
        }
        lock.commit().await?;
    }
    // This only binds frozen refs and verifies SHA-256. All needed objects came from durable storage.
    core::ensure_available(state, snapshot).await
}
pub async fn from_env() -> Result<AppState> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(24)
        .connect(&service::required("DATABASE_URL")?)
        .await?;
    artifactory_database::require_timescale(&pool).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    let data_dir = PathBuf::from(service::required("ARTIFACTORY_SOURCE_DATA_DIR")?);
    tokio::fs::create_dir_all(&data_dir).await?;
    let token = service::required("ARTIFACTORY_SERVICE_TOKEN")?;
    Ok(AppState {
        core: core::AppState {
            pool,
            settings: Arc::new(core::Settings {
                data_dir,
                master_key: service::key("ARTIFACTORY_SERVICE_KEY")?,
            }),
        },
        objects: Objects::from_env()?,
        control: Client::new(service::required("ARTIFACTORY_CONTROL_URL")?, token.clone())?,
        runtime: Client::new(service::required("ARTIFACTORY_RUNTIME_URL")?, token)?,
        origin: service::download_origin(&service::required("ARTIFACTORY_SOURCE_PUBLIC_URL")?)?,
    })
}
