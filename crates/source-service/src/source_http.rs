use crate::{
    error::{ApiError, Result},
    source_cache::{self, Repository, Snapshot},
    state::AppState,
};
use axum::{
    Json,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::{StreamExt, TryStreamExt};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::io::{ReaderStream, StreamReader};
use uuid::Uuid;

struct Download {
    _permit: tokio::sync::SemaphorePermit<'static>,
    _lease: artifactory_source_core::store::SourceDownload,
    bytes: Arc<AtomicU64>,
}
async fn download(state: &AppState, repository: Uuid, lfs: bool) -> Result<Download> {
    // Long downloads retain a DB lock against eviction. Leave pool capacity for
    // lease heartbeats, cancellation and scheduling even under parallel LFS.
    static TRANSFERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);
    let permit = TRANSFERS.acquire().await.map_err(anyhow::Error::from)?;
    let lease = state.store.download_repository(repository, lfs).await?;
    Ok(Download {
        _permit: permit,
        bytes: lease.bytes.clone(),
        _lease: lease,
    })
}

pub async fn challenge(mut response: Response) -> Response {
    if response.status() == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            "www-authenticate",
            axum::http::HeaderValue::from_static("Basic realm=\"artifactory-source\""),
        );
    }
    response
}

async fn authorize(
    state: &AppState,
    snapshot: Uuid,
    entry: Uuid,
    headers: &HeaderMap,
) -> Result<(Snapshot, Repository, String)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;
    let token = if let Some(token) = auth.strip_prefix("Bearer ") {
        token.to_owned()
    } else if let Some(value) = auth.strip_prefix("Basic ") {
        let raw = STANDARD.decode(value).map_err(|_| ApiError::Unauthorized)?;
        let text = String::from_utf8(raw).map_err(|_| ApiError::Unauthorized)?;
        text.split_once(':')
            .ok_or(ApiError::Unauthorized)?
            .1
            .to_string()
    } else {
        return Err(ApiError::Unauthorized);
    };
    let result: Value = state
        .control
        .post(
            "/internal/runtime/source-authorize",
            &json!({"token":token,"snapshot_id":snapshot}),
        )
        .await?;
    let valid = result["valid"].as_bool() == Some(true);
    if !valid {
        return Err(ApiError::Unauthorized);
    }
    let snapshot = source_cache::load(state, snapshot).await?;
    let repository = snapshot
        .repositories
        .iter()
        .find(|r| r.id == entry)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    Ok((snapshot, repository, token))
}

pub async fn git(
    State(state): State<AppState>,
    Path((snapshot_id, entry_id)): Path<(Uuid, Uuid)>,
    request: Request,
) -> Result<Response> {
    let (snapshot, entry, _) = authorize(&state, snapshot_id, entry_id, request.headers()).await?;
    crate::service::restore(&state, &snapshot).await?;
    let download = download(&state, entry.repository_id, false).await?;
    // Cache acquisition may wait behind a fetch or eviction. Authorization
    // must still hold when the transfer can actually start.
    authorize(&state, snapshot_id, entry_id, request.headers()).await?;
    let path = request.uri().path();
    let suffix = if path.ends_with("/info/refs") {
        if request.uri().query() != Some("service=git-upload-pack") {
            return Err(ApiError::Forbidden);
        }
        "info/refs"
    } else if path.ends_with("/git-upload-pack") {
        "git-upload-pack"
    } else {
        return Err(ApiError::NotFound);
    };
    let repo = source_cache::repository_path(&state, entry.repository_id).await?;
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", repo.parent().unwrap())
        .env(
            "PATH_INFO",
            format!("/{}/{suffix}", repo.file_name().unwrap().to_string_lossy()),
        )
        .env(
            "GIT_NAMESPACE",
            source_cache::namespace(snapshot.id, entry.id),
        )
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "http.receivepack")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "uploadpack.allowAnySHA1InWant")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("REQUEST_METHOD", request.method().as_str())
        .env("QUERY_STRING", request.uri().query().unwrap_or(""))
        .env("REMOTE_USER", "artifactory-task")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(value) = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
    {
        command.env("CONTENT_TYPE", value);
    }
    if let Some(value) = request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
    {
        command.env("CONTENT_LENGTH", value);
    }
    if let Some(value) = request
        .headers()
        .get("git-protocol")
        .and_then(|v| v.to_str().ok())
    {
        command.env("HTTP_GIT_PROTOCOL", value);
    }
    let mut child = command.spawn().map_err(anyhow::Error::from)?;
    let mut stdin = child.stdin.take().unwrap();
    let input = request
        .into_body()
        .into_data_stream()
        .map_err(std::io::Error::other);
    let writer =
        tokio::spawn(
            async move { tokio::io::copy(&mut StreamReader::new(input), &mut stdin).await },
        );
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut response = Response::builder().header("cache-control", "no-store");
    let mut count = 0;
    loop {
        let mut line = String::new();
        let size = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            reader.read_line(&mut line),
        )
        .await
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("source backend timed out")))?
        .map_err(anyhow::Error::from)?;
        count += size;
        if size == 0 || count > 16384 {
            return Err(ApiError::Internal(anyhow::anyhow!(
                "invalid source backend headers"
            )));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.trim_end().split_once(':') {
            if key.eq_ignore_ascii_case("status") {
                let status = value
                    .split_whitespace()
                    .next()
                    .unwrap_or("500")
                    .parse::<u16>()
                    .unwrap_or(500);
                response = response.status(status);
            } else if ["content-type", "expires", "pragma"]
                .contains(&key.to_ascii_lowercase().as_str())
            {
                response = response.header(key, value.trim());
            }
        }
    }
    let stream = async_stream::try_stream! {
        let download=download;
        let _writer=writer;
        let mut data=ReaderStream::new(reader);
        while let Some(chunk)=data.next().await { let chunk=chunk?;download.bytes.fetch_add(chunk.len() as u64,Ordering::Relaxed);yield chunk; }
        let status=child.wait().await?;
        if !status.success() { Err(std::io::Error::other("source transfer failed"))?; }
    };
    let stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<axum::body::Bytes>> + Send>,
    > = Box::pin(stream);
    response
        .body(Body::from_stream(stream))
        .map_err(|e| ApiError::Internal(e.into()))
}

pub async fn lfs_batch(
    State(state): State<AppState>,
    Path((snapshot_id, entry_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Result<Response> {
    let (_, entry, token) = authorize(&state, snapshot_id, entry_id, &headers).await?;
    if input.get("operation").and_then(Value::as_str) != Some("download") {
        return Err(ApiError::Forbidden);
    }
    let requested = input
        .get("objects")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::BadRequest("objects are required".into()))?;
    if requested.len() > 1000 {
        return Err(ApiError::BadRequest("too many LFS objects".into()));
    }
    let mut objects = Vec::new();
    for object in requested {
        let oid = object.get("oid").and_then(Value::as_str).unwrap_or("");
        if let Some(item) = entry.lfs.iter().find(|item| item.oid == oid) {
            objects.push(json!({"oid":item.oid,"size":item.size,"actions":{"download":{"href":format!("{}/api/v1/source/{snapshot_id}/{entry_id}/lfs/objects/{oid}",state.origin),"header":{"Authorization":format!("Bearer {token}")},"expires_in":90}}}));
        } else {
            objects.push(json!({"oid":oid,"size":object.get("size"),"error":{"code":404,"message":"object is not part of this snapshot"}}));
        }
    }
    Ok((
        [
            ("content-type", "application/vnd.git-lfs+json"),
            ("cache-control", "no-store"),
        ],
        Json(json!({"transfer":"basic","objects":objects})),
    )
        .into_response())
}

pub async fn lfs_object(
    State(state): State<AppState>,
    Path((snapshot_id, entry_id, oid)): Path<(Uuid, Uuid, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let (snapshot, entry, _) = authorize(&state, snapshot_id, entry_id, &headers).await?;
    crate::service::restore(&state, &snapshot).await?;
    let download = download(&state, entry.repository_id, true).await?;
    authorize(&state, snapshot_id, entry_id, &headers).await?;
    let item = entry
        .lfs
        .iter()
        .find(|item| item.oid == oid)
        .ok_or(ApiError::NotFound)?;
    let repo = source_cache::repository_path(&state, entry.repository_id).await?;
    let file = tokio::fs::File::open(source_cache::lfs_path(&repo, &oid)?)
        .await
        .map_err(anyhow::Error::from)?;
    let stream = async_stream::try_stream! {
        let download=download;let mut reader=ReaderStream::new(file);
        while let Some(chunk)=reader.next().await {let chunk=chunk?;download.bytes.fetch_add(chunk.len() as u64,Ordering::Relaxed);yield chunk;}
    };
    let stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::io::Result<axum::body::Bytes>> + Send>,
    > = Box::pin(stream);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .header("content-length", item.size)
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .map_err(anyhow::Error::from)?)
}
