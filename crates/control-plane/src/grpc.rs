use crate::{
    crypto, pki,
    protocol::{self, Command, EmissaryEvent},
    recipe::safe_relative_path,
    state::{AppState, Connection},
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request, Response, Status, Streaming,
    transport::server::{TcpConnectInfo, TlsConnectInfo},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct EmissaryService(pub AppState);

pub(crate) fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(%error,"emissary request failed");
    Status::internal("control plane operation failed")
}

async fn identity<T>(state: &AppState, request: &Request<T>) -> Result<Uuid, Status> {
    let certificate = request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(|info| info.peer_certs())
        .and_then(|certificates| certificates.first().cloned())
        .ok_or_else(|| Status::unauthenticated("node certificate required"))?;
    state
        .store
        .node_identity(crypto::hash(certificate.as_ref()))
        .await
}

#[tonic::async_trait]
impl protocol::enrollment_server::Enrollment for EmissaryService {
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
        let enrolled = state
            .store
            .enroll_node(
                crate::store::NodeEnrollment {
                    token: &input.token,
                    name: &input.name,
                    os: &input.os,
                    arch: &input.arch,
                },
                |id| async move {
                    pki::sign(&state.settings, id, &input.csr_pem)
                        .await
                        .map_err(|_| Status::invalid_argument("CSR rejected or issuer unavailable"))
                },
            )
            .await?;
        let id = enrolled.id;
        let certificate = enrolled.certificate;
        state
            .audit("enrollment", "node_enrolled", id, json!({}))
            .await
            .map_err(internal)?;
        Ok(Response::new(protocol::EnrollReply {
            emissary_id: id.to_string(),
            certificate_pem: certificate,
            ca_pem: tokio::fs::read_to_string(state.settings.pki_dir.join("ca.pem"))
                .await
                .map_err(internal)?,
            heartbeat_seconds: 10,
            lease_seconds: 90,
            telemetry_endpoint: state
                .settings
                .telemetry
                .as_ref()
                .map(|t| t.endpoint.clone())
                .unwrap_or_default(),
        }))
    }
}

#[tonic::async_trait]
impl protocol::control_server::Control for EmissaryService {
    type ConnectStream = ReceiverStream<Result<Command, Status>>;

    async fn connect(
        &self,
        request: Request<Streaming<EmissaryEvent>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let node = identity(&self.0, &request).await?;
        let mut stream = request.into_inner();
        let hello = tokio::time::timeout(Duration::from_secs(10), stream.message())
            .await
            .map_err(|_| Status::deadline_exceeded("hello required"))??
            .ok_or_else(|| Status::invalid_argument("hello required"))?;
        if hello.kind != "hello"
            || hello.emissary_id != node.to_string()
            || hello.protocol_major != 1
        {
            return Err(Status::failed_precondition(
                "invalid hello or protocol version",
            ));
        }
        let session = Uuid::new_v4();
        self.0.store.connect_node(node, session).await?;
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
                    state.store.validate_node_session(node, session).await?;
                    if event.emissary_id != node.to_string() {
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
            let attempt = self
                .0
                .store
                .validate_attempt(node, &chunk.attempt_id, &chunk.lease_token, true)
                .await?;
            let hash = crypto::hash(&chunk.data);
            let key = format!("logs/{attempt}/{}/{hash}", chunk.sequence);
            let size = chunk.data.len() as i64;
            self.0
                .storage
                .put(&key, chunk.data)
                .await
                .map_err(internal)?;
            self.0
                .store
                .commit_log_chunk(
                    node,
                    attempt,
                    crate::store::LogCommit {
                        token: &chunk.lease_token,
                        sequence: chunk.sequence,
                        step: &chunk.step,
                        key: &key,
                        hash: &hash,
                        size,
                    },
                )
                .await?;
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
        let attempt = self
            .0
            .store
            .validate_attempt(node, &input.attempt_id, &input.lease_token, false)
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
        let upload = self
            .0
            .store
            .prepare_upload(
                node,
                attempt,
                crate::store::ArtifactDeclaration {
                    path: &input.path,
                    size: input.size,
                    sha256: &input.sha256,
                    lease_token: &input.lease_token,
                },
            )
            .await?;
        let url = if upload.completed {
            String::new()
        } else {
            self.0.storage.upload_url(&upload.object_key)
        };
        Ok(Response::new(protocol::UploadTicket {
            upload_id: upload.id.to_string(),
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
        let attempt = self
            .0
            .store
            .validate_attempt(node, &input.attempt_id, &input.lease_token, false)
            .await?;
        let id = Uuid::parse_str(&input.upload_id)
            .map_err(|_| Status::invalid_argument("invalid upload ID"))?;
        let artifact = self
            .0
            .store
            .complete_upload(node, attempt, id, &input.lease_token, |upload| async move {
                self.0
                    .storage
                    .verify_and_promote(
                        &upload.object_key,
                        &upload.final_key,
                        upload.expected_size,
                        &upload.expected_sha256,
                    )
                    .await
                    .map_err(|_| {
                        Status::failed_precondition("artifact verification or promotion failed")
                    })
            })
            .await?;
        Ok(Response::new(protocol::ArtifactRecord {
            artifact_id: artifact.id.to_string(),
            path: artifact.path,
            sha256: artifact.sha256,
            size: artifact.size,
        }))
    }
}

async fn heartbeat(
    state: &AppState,
    node: Uuid,
    session: Uuid,
    event: &EmissaryEvent,
    sender: &mpsc::Sender<Result<Command, Status>>,
) -> Result<(), Status> {
    state.store.validate_node_session(node, session).await?;
    if event.payload_json.len() > 65536 {
        return Err(Status::invalid_argument("oversized heartbeat"));
    }
    let payload: Value = serde_json::from_str(&event.payload_json)
        .map_err(|_| Status::invalid_argument("invalid heartbeat"))?;
    let capabilities = payload.get("capabilities").cloned().unwrap_or(json!([]));
    if !capabilities.is_array() || capabilities.as_array().unwrap().len() > 256 {
        return Err(Status::invalid_argument("invalid capabilities"));
    }
    state
        .store
        .record_heartbeat(
            node,
            session,
            capabilities,
            payload.get("diagnostics").cloned().unwrap_or(json!({})),
        )
        .await?;
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
            let renewed = state.store.renew_attempt(node, session, id, token).await?;
            let command = Command {
                command_id: Uuid::new_v4().to_string(),
                kind: if renewed { "renew" } else { "cancel" }.into(),
                attempt_id: id.to_string(),
                lease_token: token.into(),
                lease_seconds: if renewed { 90 } else { 0 },
                request_id: event.command_id.clone(),
                message: if renewed {
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
        state.store.validate_node_session(node, session).await?;
        sender
            .send(Ok(Command {
                kind: "emissary_health".into(),
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
    event: &EmissaryEvent,
    sender: &mpsc::Sender<Result<Command, Status>>,
) -> Result<(), Status> {
    let id = Uuid::parse_str(&event.attempt_id)
        .map_err(|_| Status::invalid_argument("invalid attempt ID"))?;
    if event.payload_json.len() > 16384 {
        return Err(Status::invalid_argument("oversized task event"));
    }
    let receipt = state
        .store
        .submit_execution_event(
            node,
            session,
            id,
            crate::store::ExecutionEvent {
                kind: &event.kind,
                sequence: event.sequence,
                lease_token: &event.lease_token,
                payload_json: &event.payload_json,
            },
        )
        .await?;
    let command = match receipt {
        crate::store::EventReceipt::Start => Command {
            command_id: Uuid::new_v4().to_string(),
            kind: "start".into(),
            attempt_id: id.to_string(),
            lease_token: event.lease_token.clone(),
            lease_seconds: 90,
            request_id: event.command_id.clone(),
            ..Default::default()
        },
        crate::store::EventReceipt::Ack(sequence) => Command {
            kind: "event_ack".into(),
            attempt_id: id.to_string(),
            ack_sequence: sequence,
            ..Default::default()
        },
    };
    sender
        .send(Ok(command))
        .await
        .map_err(|_| Status::unavailable("connection closed"))?;
    Ok(())
}
