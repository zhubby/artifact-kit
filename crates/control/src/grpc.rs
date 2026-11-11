use crate::{
    crypto, pki,
    protocol::{self, AgentEvent, Command},
    recipe::safe_relative_path,
    state::{AppState, Connection},
};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request, Response, Status, Streaming,
    transport::server::{TcpConnectInfo, TlsConnectInfo},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct AgentService(pub AppState);

const VALID_ATTEMPT_QUERY: &str = "SELECT EXISTS(SELECT 1 FROM attempts a JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status='running' AND a.lease_expires_at>clock_timestamp() AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND ($4 OR NOT b.cancel_requested))";

fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(%error,"agent request failed");
    Status::internal("control plane operation failed")
}

async fn identity<T>(state: &AppState, request: &Request<T>) -> Result<Uuid, Status> {
    let certificate = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(|info| info.peer_certs())
        .and_then(|certificates| certificates.first().cloned())
        .ok_or_else(|| Status::unauthenticated("node certificate required"))?;
    sqlx::query_scalar("SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()")
        .bind(crypto::hash(certificate.as_ref())).fetch_optional(&state.pool).await.map_err(internal)?
        .ok_or_else(|| Status::unauthenticated("node certificate revoked, expired or unknown"))
}

async fn valid_node<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())")
        .bind(node).bind(session).fetch_one(executor).await.map_err(internal)?;
    if valid {
        Ok(())
    } else {
        Err(Status::unauthenticated(
            "node session revoked or superseded",
        ))
    }
}

async fn lock_authorized_node(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    // Acquire this after the attempt lock, then validate with a fresh clock and
    // snapshot. Keep revocation/session replacement serialized with the write.
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR SHARE")
        .bind(node)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    valid_node(&mut **transaction, node, session).await
}

async fn valid_attempt<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    node: Uuid,
    attempt: &str,
    token: &str,
    allow_canceled: bool,
) -> Result<Uuid, Status> {
    let attempt =
        Uuid::parse_str(attempt).map_err(|_| Status::invalid_argument("invalid attempt ID"))?;
    let valid: bool = sqlx::query_scalar(VALID_ATTEMPT_QUERY)
        .bind(attempt)
        .bind(node)
        .bind(token)
        .bind(allow_canceled)
        .fetch_one(executor)
        .await
        .map_err(internal)?;
    if valid {
        Ok(attempt)
    } else {
        Err(Status::failed_precondition(
            "attempt lease expired, canceled or superseded",
        ))
    }
}

async fn lock_artifact_attempt(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: Uuid,
    attempt: Uuid,
    token: &str,
) -> Result<(), Status> {
    // Shared locks coexist with insertion's foreign-key KEY SHARE lock. Check
    // again after waiting; hold authorization stable through the short commit.
    sqlx::query("SELECT id FROM attempts WHERE id=$1 FOR SHARE")
        .bind(attempt)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR SHARE")
        .bind(node)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    valid_attempt(&mut **transaction, node, &attempt.to_string(), token, false).await?;
    Ok(())
}

#[tonic::async_trait]
impl protocol::enrollment_server::Enrollment for AgentService {
    async fn enroll(
        &self,
        request: Request<protocol::EnrollRequest>,
    ) -> Result<Response<protocol::EnrollReply>, Status> {
        let input = request.into_inner();
        let state = &self.0;
        if input.protocol_major != 1 {
            return Err(Status::failed_precondition("unsupported protocol major"));
        }
        if input.token.len() != 64
            || input.name.is_empty()
            || input.name.len() > 100
            || !["linux", "darwin", "windows"].contains(&input.os.as_str())
            || !["amd64", "arm64"].contains(&input.arch.as_str())
        {
            return Err(Status::invalid_argument("invalid enrollment request"));
        }
        let mut transaction = state.pool.begin().await.map_err(internal)?;
        let token=sqlx::query("SELECT id,pool_id FROM enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>now() FOR UPDATE")
            .bind(crypto::hash(&input.token)).fetch_optional(&mut *transaction).await.map_err(internal)?
            .ok_or_else(|| Status::unauthenticated("invalid or expired enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = pki::sign(&state.settings, id, &input.csr_pem)
            .await
            .map_err(|_| Status::invalid_argument("CSR rejected or issuer unavailable"))?;
        let hash = pki::certificate_hash(&certificate).map_err(internal)?;
        sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(id).bind(input.name).bind(input.os).bind(input.arch).bind(token.get::<Uuid,_>("pool_id")).bind(hash).bind(Utc::now()+ChronoDuration::days(30)).execute(&mut *transaction).await.map_err(internal)?;
        sqlx::query("UPDATE enrollment_tokens SET used_at=now() WHERE id=$1")
            .bind(token.get::<Uuid, _>("id"))
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        state
            .audit("enrollment", "node_enrolled", id, json!({}))
            .await
            .map_err(internal)?;
        Ok(Response::new(protocol::EnrollReply {
            agent_id: id.to_string(),
            certificate_pem: certificate,
            ca_pem: tokio::fs::read_to_string(state.settings.pki_dir.join("ca.pem"))
                .await
                .map_err(internal)?,
            heartbeat_seconds: 10,
            lease_seconds: 90,
        }))
    }
}

#[tonic::async_trait]
impl protocol::control_server::Control for AgentService {
    type ConnectStream = ReceiverStream<Result<Command, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<AgentEvent>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let node = identity(&self.0, &request).await?;
        let mut stream = request.into_inner();
        let hello = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("hello required"))??
            .ok_or_else(|| Status::invalid_argument("hello required"))?;
        if hello.kind != "hello" || hello.agent_id != node.to_string() || hello.protocol_major != 1
        {
            return Err(Status::failed_precondition(
                "invalid hello or protocol version",
            ));
        }
        let session = Uuid::new_v4();
        sqlx::query("UPDATE nodes SET active_session=$2,last_seen=now() WHERE id=$1")
            .bind(node)
            .bind(session)
            .execute(&self.0.pool)
            .await
            .map_err(internal)?;
        let (sender, receiver) = mpsc::channel(128);
        let connection = Connection {
            session,
            sender: sender.clone(),
        };
        if let Some(previous) = self.0.connections.write().await.insert(node, connection) {
            let _ = previous
                .sender
                .try_send(Err(Status::aborted("node connection superseded")));
        }
        let state = self.0.clone();
        tokio::spawn(async move {
            let result = async {
                heartbeat(&state, node, session, &hello, &sender).await?;
                loop {
                    let event = tokio::time::timeout(Duration::from_secs(35), stream.message())
                        .await
                        .map_err(|_| Status::deadline_exceeded("heartbeat timeout"))??;
                    let Some(event) = event else {
                        return Ok::<(), Status>(());
                    };
                    valid_node(&state.pool, node, session).await?;
                    if event.agent_id != node.to_string() {
                        return Err(Status::permission_denied("node ID mismatch"));
                    }
                    if event.kind == "heartbeat" {
                        heartbeat(&state, node, session, &event, &sender).await?;
                    } else {
                        apply_event(&state, node, session, &event, &sender).await?;
                    }
                }
            }
            .await;
            if let Err(error) = result {
                let _ = sender.try_send(Err(error));
            }
            let mut connections = state.connections.write().await;
            if connections
                .get(&node)
                .is_some_and(|entry| entry.session == session)
            {
                connections.remove(&node);
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn upload_logs(
        &self,
        request: Request<Streaming<protocol::LogChunk>>,
    ) -> Result<Response<protocol::LogAck>, Status> {
        let node = identity(&self.0, &request).await?;
        let mut stream = request.into_inner();
        let mut through = 0;
        let mut chunks = 0;
        while let Some(chunk) = stream.message().await? {
            chunks += 1;
            if chunks > 64
                || chunk.data.len() > 65536
                || chunk.data.is_empty()
                || chunk.sequence == 0
                || chunk.step.len() > 120
            {
                return Err(Status::invalid_argument("invalid log chunk"));
            }
            let attempt = valid_attempt(
                &self.0.pool,
                node,
                &chunk.attempt_id,
                &chunk.lease_token,
                true,
            )
            .await?;
            let hash = crypto::hash(&chunk.data);
            let key = format!("logs/{attempt}/{}/{hash}", chunk.sequence);
            let size = chunk.data.len() as i64;
            self.0
                .storage
                .put(&key, chunk.data)
                .await
                .map_err(internal)?;
            let mut transaction = self.0.pool.begin().await.map_err(internal)?;
            sqlx::query("SELECT id FROM attempts WHERE id=$1 FOR UPDATE")
                .bind(attempt)
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
            let valid: bool = sqlx::query_scalar(VALID_ATTEMPT_QUERY)
                .bind(attempt)
                .bind(node)
                .bind(&chunk.lease_token)
                .bind(true)
                .fetch_one(&mut *transaction)
                .await
                .map_err(internal)?;
            if !valid {
                return Err(Status::failed_precondition(
                    "execution lease is no longer valid",
                ));
            }
            if let Some(existing) = sqlx::query_scalar::<_, String>(
                "SELECT sha256 FROM log_chunks WHERE attempt_id=$1 AND sequence=$2",
            )
            .bind(attempt)
            .bind(chunk.sequence as i64)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(internal)?
            {
                if existing != hash {
                    return Err(Status::already_exists("log sequence content mismatch"));
                }
            } else {
                let previous: i64 = sqlx::query_scalar(
                    "SELECT coalesce(max(sequence),0) FROM log_chunks WHERE attempt_id=$1",
                )
                .bind(attempt)
                .fetch_one(&mut *transaction)
                .await
                .map_err(internal)?;
                if chunk.sequence != previous as u64 + 1 {
                    return Err(Status::failed_precondition(format!(
                        "expected log sequence {}",
                        previous + 1
                    )));
                }
                sqlx::query("INSERT INTO log_chunks(attempt_id,sequence,step,object_key,sha256,size) VALUES($1,$2,$3,$4,$5,$6)")
                    .bind(attempt).bind(chunk.sequence as i64).bind(chunk.step).bind(key).bind(hash).bind(size).execute(&mut *transaction).await.map_err(internal)?;
            }
            transaction.commit().await.map_err(internal)?;
            through = chunk.sequence;
        }
        Ok(Response::new(protocol::LogAck {
            through_sequence: through,
        }))
    }

    async fn prepare_artifact(
        &self,
        request: Request<protocol::ArtifactRequest>,
    ) -> Result<Response<protocol::UploadTicket>, Status> {
        let node = identity(&self.0, &request).await?;
        let input = request.into_inner();
        let attempt = valid_attempt(
            &self.0.pool,
            node,
            &input.attempt_id,
            &input.lease_token,
            false,
        )
        .await?;
        if !safe_relative_path(&input.path)
            || input.size == 0
            || input.size > 20 * 1024 * 1024 * 1024
            || input.sha256.len() != 64
            || !input
                .sha256
                .bytes()
                .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase())
        {
            return Err(Status::invalid_argument("invalid artifact declaration"));
        }
        let id = Uuid::new_v4();
        let key = format!("uploads/{id}");
        let mut transaction = self.0.pool.begin().await.map_err(internal)?;
        sqlx::query("INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,$3,$4,$5,$6,now()+interval '15 minutes') ON CONFLICT(attempt_id,path) DO NOTHING")
            .bind(id).bind(attempt).bind(&input.path).bind(key).bind(input.size as i64).bind(&input.sha256).execute(&mut *transaction).await.map_err(internal)?;
        let row=sqlx::query("SELECT u.*,EXISTS(SELECT 1 FROM artifacts a WHERE a.id=u.id) AS completed FROM uploads u WHERE attempt_id=$1 AND path=$2").bind(attempt).bind(input.path).fetch_one(&mut *transaction).await.map_err(internal)?;
        if row.get::<i64, _>("expected_size") != input.size as i64
            || row.get::<String, _>("expected_sha256") != input.sha256
        {
            return Err(Status::already_exists(
                "artifact path already declared with different content",
            ));
        }
        lock_artifact_attempt(&mut transaction, node, attempt, &input.lease_token).await?;
        transaction.commit().await.map_err(internal)?;
        let url = if row.get::<bool, _>("completed") {
            String::new()
        } else {
            self.0
                .storage
                .upload_url(&row.get::<String, _>("object_key"))
        };
        Ok(Response::new(protocol::UploadTicket {
            upload_id: row.get::<Uuid, _>("id").to_string(),
            url,
            expires_seconds: 900,
        }))
    }

    async fn complete_artifact(
        &self,
        request: Request<protocol::CompleteArtifactRequest>,
    ) -> Result<Response<protocol::ArtifactRecord>, Status> {
        let node = identity(&self.0, &request).await?;
        let input = request.into_inner();
        let attempt = valid_attempt(
            &self.0.pool,
            node,
            &input.attempt_id,
            &input.lease_token,
            false,
        )
        .await?;
        let id = Uuid::parse_str(&input.upload_id)
            .map_err(|_| Status::invalid_argument("invalid upload ID"))?;
        let mut transaction = self.0.pool.begin().await.map_err(internal)?;
        let row = sqlx::query("SELECT * FROM uploads WHERE id=$1 AND attempt_id=$2 FOR UPDATE")
            .bind(id)
            .bind(attempt)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("upload not found"))?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(internal)?;
        let final_key = format!("artifacts/{id}");
        if !exists {
            self.0
                .storage
                .verify_and_promote(
                    &row.get::<String, _>("object_key"),
                    &final_key,
                    row.get("expected_size"),
                    &row.get::<String, _>("expected_sha256"),
                )
                .await
                .map_err(|_| {
                    Status::failed_precondition("artifact verification or promotion failed")
                })?;
        }
        // Network verification completes before taking the attempt/node locks,
        // so a slow S3 request does not block heartbeat renewal.
        lock_artifact_attempt(&mut transaction, node, attempt, &input.lease_token).await?;
        if !exists {
            sqlx::query("INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(id).bind(attempt).bind(row.get::<String,_>("path")).bind(final_key).bind(row.get::<i64,_>("expected_size")).bind(row.get::<String,_>("expected_sha256")).execute(&mut *transaction).await.map_err(internal)?;
        }
        transaction.commit().await.map_err(internal)?;
        Ok(Response::new(protocol::ArtifactRecord {
            artifact_id: id.to_string(),
            path: row.get("path"),
            sha256: row.get("expected_sha256"),
            size: row.get::<i64, _>("expected_size") as u64,
        }))
    }
}

async fn heartbeat(
    state: &AppState,
    node: Uuid,
    session: Uuid,
    event: &AgentEvent,
    sender: &mpsc::Sender<Result<Command, Status>>,
) -> Result<(), Status> {
    valid_node(&state.pool, node, session).await?;
    if event.payload_json.len() > 65536 {
        return Err(Status::invalid_argument("oversized heartbeat"));
    }
    let payload: Value = serde_json::from_str(&event.payload_json)
        .map_err(|_| Status::invalid_argument("invalid heartbeat"))?;
    let capabilities = payload.get("capabilities").cloned().unwrap_or(json!([]));
    if !capabilities.is_array() || capabilities.as_array().unwrap().len() > 256 {
        return Err(Status::invalid_argument("invalid capabilities"));
    }
    sqlx::query("UPDATE nodes SET last_seen=now(),capabilities=$3,diagnostics=$4 WHERE id=$1 AND active_session=$2")
        .bind(node).bind(session).bind(capabilities).bind(payload.get("diagnostics").cloned().unwrap_or(json!({}))).execute(&state.pool).await.map_err(internal)?;
    if let Some(active) = payload.get("active_attempts").and_then(Value::as_array) {
        if active.len() > 32 {
            return Err(Status::invalid_argument("too many active attempts"));
        }
        for entry in active {
            let Some(id) = entry
                .get("attempt_id")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            let token = entry
                .get("lease_token")
                .and_then(Value::as_str)
                .unwrap_or("");
            let mut transaction = state.pool.begin().await.map_err(internal)?;
            sqlx::query("SELECT id FROM attempts WHERE id=$1 AND node_id=$2 FOR UPDATE")
                .bind(id)
                .bind(node)
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
            lock_authorized_node(&mut transaction, node, session).await?;
            let row=sqlx::query("UPDATE attempts a SET lease_expires_at=clock_timestamp()+interval '90 seconds' FROM jobs j,builds b,nodes n WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND j.id=a.job_id AND b.id=j.build_id AND NOT b.cancel_requested AND n.id=a.node_id AND n.active_session=$4 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() RETURNING a.status")
                .bind(id).bind(node).bind(token).bind(session).fetch_optional(&mut *transaction).await.map_err(internal)?;
            transaction.commit().await.map_err(internal)?;
            let command = Command {
                command_id: Uuid::new_v4().to_string(),
                kind: if row.is_some() { "renew" } else { "cancel" }.into(),
                attempt_id: id.to_string(),
                lease_token: token.into(),
                lease_seconds: if row.is_some() { 90 } else { 0 },
                request_id: event.command_id.clone(),
                message: if row.is_some() {
                    String::new()
                } else {
                    "lease_invalid".into()
                },
                ..Default::default()
            };
            sender
                .send(Ok(command))
                .await
                .map_err(|_| Status::unavailable("connection closed"))?;
        }
    }
    let ota_healthy = crate::ota::report(state, node, session, &payload)
        .await
        .map_err(internal)?;
    if ota_healthy && event.kind == "heartbeat" {
        valid_node(&state.pool, node, session).await?;
        sender
            .send(Ok(Command {
                kind: "agent_health".into(),
                request_id: event.command_id.clone(),
                ..Default::default()
            }))
            .await
            .map_err(|_| Status::unavailable("connection closed"))?;
    }
    Ok(())
}

async fn apply_event(
    state: &AppState,
    node: Uuid,
    session: Uuid,
    event: &AgentEvent,
    sender: &mpsc::Sender<Result<Command, Status>>,
) -> Result<(), Status> {
    let id = Uuid::parse_str(&event.attempt_id)
        .map_err(|_| Status::invalid_argument("invalid attempt ID"))?;
    if event.payload_json.len() > 16384 {
        return Err(Status::invalid_argument("oversized task event"));
    }
    let mut transaction = state.pool.begin().await.map_err(internal)?;
    let row=sqlx::query("SELECT a.*,j.target_config FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.lease_expires_at>now() FOR UPDATE OF a,j")
        .bind(id).bind(node).bind(&event.lease_token).fetch_optional(&mut *transaction).await.map_err(internal)?
        .ok_or_else(|| Status::failed_precondition("attempt lease expired or unknown"))?;
    lock_authorized_node(&mut transaction, node, session).await?;
    // Cancellation can commit while either row lock waits. Read it with the
    // current lease clock after both waits, rather than using the old snapshot.
    let current = sqlx::query("SELECT a.lease_expires_at>clock_timestamp() AS lease_valid,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1")
        .bind(id).fetch_one(&mut *transaction).await.map_err(internal)?;
    if !current.get::<bool, _>("lease_valid") {
        return Err(Status::failed_precondition(
            "attempt lease expired while waiting for its lock",
        ));
    }
    let canceled: bool = current.get("cancel_requested");
    let status: String = row.get("status");
    if event.kind == "accept" {
        if !["leased", "running"].contains(&status.as_str()) || canceled {
            return Err(Status::failed_precondition("attempt cannot start"));
        }
        sqlx::query("UPDATE attempts SET status='running',started_at=coalesce(started_at,now()),lease_expires_at=now()+interval '90 seconds' WHERE id=$1").bind(id).execute(&mut *transaction).await.map_err(internal)?;
        sqlx::query("UPDATE jobs SET status='running' WHERE id=$1")
            .bind(row.get::<Uuid, _>("job_id"))
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        sender
            .send(Ok(Command {
                command_id: Uuid::new_v4().to_string(),
                kind: "start".into(),
                attempt_id: id.to_string(),
                lease_token: event.lease_token.clone(),
                lease_seconds: 90,
                request_id: event.command_id.clone(),
                ..Default::default()
            }))
            .await
            .map_err(|_| Status::unavailable("connection closed"))?;
        return Ok(());
    }
    let previous: i64 = row.get("last_sequence");
    if event.sequence <= previous as u64 {
        sender
            .send(Ok(Command {
                kind: "event_ack".into(),
                attempt_id: id.to_string(),
                ack_sequence: previous as u64,
                ..Default::default()
            }))
            .await
            .map_err(|_| Status::unavailable("connection closed"))?;
        return Ok(());
    }
    if status != "running" || event.sequence != previous as u64 + 1 {
        return Err(Status::failed_precondition(
            "task event out of order or terminal",
        ));
    }
    let payload: Value = serde_json::from_str(&event.payload_json)
        .map_err(|_| Status::invalid_argument("invalid event payload"))?;
    let target: crate::recipe::Target =
        serde_json::from_value(row.get("target_config")).map_err(internal)?;
    match event.kind.as_str() {
        "source_cache" => {
            if payload.get("hit").and_then(Value::as_bool).is_none()
                || payload.get("path").and_then(Value::as_str).is_none()
                || payload
                    .get("commit")
                    .and_then(Value::as_str)
                    .is_none_or(|value| !crate::source_cache::valid_commit(value))
            {
                return Err(Status::invalid_argument("invalid source cache event"));
            }
            let updated=sqlx::query("UPDATE source_repositories r SET agent_cache_hits=agent_cache_hits+CASE WHEN $2 THEN 1 ELSE 0 END,agent_cache_misses=agent_cache_misses+CASE WHEN $2 THEN 0 ELSE 1 END WHERE r.id IN (SELECT (entry->>'repository_id')::uuid FROM source_snapshots s CROSS JOIN LATERAL jsonb_array_elements(s.manifest->'repositories') entry JOIN attempts a ON a.source_snapshot_id=s.id WHERE a.id=$1 AND entry->>'path'=$3 AND entry->>'commit'=$4)")
                .bind(id).bind(payload["hit"].as_bool().unwrap()).bind(payload["path"].as_str().unwrap()).bind(payload["commit"].as_str().unwrap()).execute(&mut *transaction).await.map_err(internal)?;
            if updated.rows_affected() != 1 {
                return Err(Status::invalid_argument(
                    "source cache event does not match attempt snapshot",
                ));
            }
        }
        "step_started" => {
            let index = payload
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| Status::invalid_argument("step index required"))?
                as usize;
            let step = target
                .steps
                .get(index)
                .ok_or_else(|| Status::invalid_argument("unknown step"))?;
            let complete: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
            )
            .bind(id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(internal)?;
            if complete != index as i64 {
                return Err(Status::failed_precondition("previous steps not complete"));
            }
            sqlx::query("INSERT INTO step_runs(attempt_id,step_index,name,phase,status) VALUES($1,$2,$3,$4,'running')")
                .bind(id).bind(index as i32).bind(&step.name).bind(&step.phase).execute(&mut *transaction).await.map_err(internal)?;
        }
        "step_finished" => {
            let index = payload
                .get("index")
                .and_then(Value::as_i64)
                .ok_or_else(|| Status::invalid_argument("step index required"))?;
            let exit = payload
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or(-1);
            let changed=sqlx::query("UPDATE step_runs SET status=$3,exit_code=$4,finished_at=now() WHERE attempt_id=$1 AND step_index=$2 AND status='running'")
                .bind(id).bind(index as i32).bind(if exit==0 {"succeeded"} else {"failed"}).bind(exit as i32).execute(&mut *transaction).await.map_err(internal)?;
            if changed.rows_affected() != 1 {
                return Err(Status::failed_precondition("step not running"));
            }
        }
        "finished" => {
            let requested = payload
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            if !["succeeded", "failed", "canceled"].contains(&requested) {
                return Err(Status::invalid_argument("invalid result status"));
            }
            let final_status = if canceled { "canceled" } else { requested };
            if final_status == "succeeded" {
                let complete: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
                )
                .bind(id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(internal)?;
                let artifacts: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE attempt_id=$1")
                        .bind(id)
                        .fetch_one(&mut *transaction)
                        .await
                        .map_err(internal)?;
                if complete != target.steps.len() as i64 || artifacts == 0 {
                    return Err(Status::failed_precondition(
                        "steps or verified artifacts missing",
                    ));
                }
            }
            let error = payload.get("error").and_then(Value::as_str).unwrap_or("");
            let failure = payload
                .get("failure_kind")
                .and_then(Value::as_str)
                .unwrap_or("execution");
            if ![
                "execution",
                "source",
                "authorization",
                "artifact_transport",
                "agent",
            ]
            .contains(&failure)
            {
                return Err(Status::invalid_argument("invalid failure kind"));
            }
            sqlx::query("UPDATE attempts SET status=$2,error=$3,failure_kind=$4,finished_at=now() WHERE id=$1")
                .bind(id)
                .bind(final_status)
                .bind(error)
                .bind(failure)
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
            sqlx::query("UPDATE jobs SET status=$2 WHERE id=$1")
                .bind(row.get::<Uuid, _>("job_id"))
                .bind(final_status)
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
        }
        _ => return Err(Status::invalid_argument("unknown task event")),
    }
    sqlx::query("INSERT INTO attempt_events(attempt_id,sequence,kind,payload) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(event.sequence as i64)
        .bind(&event.kind)
        .bind(payload)
        .execute(&mut *transaction)
        .await
        .map_err(internal)?;
    sqlx::query("UPDATE attempts SET last_sequence=$2 WHERE id=$1")
        .bind(id)
        .bind(event.sequence as i64)
        .execute(&mut *transaction)
        .await
        .map_err(internal)?;
    transaction.commit().await.map_err(internal)?;
    sender
        .send(Ok(Command {
            kind: "event_ack".into(),
            attempt_id: id.to_string(),
            ack_sequence: event.sequence,
            ..Default::default()
        }))
        .await
        .map_err(|_| Status::unavailable("connection closed"))?;
    Ok(())
}
