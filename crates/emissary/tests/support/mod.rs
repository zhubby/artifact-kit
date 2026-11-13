//! Loopback mTLS peer for runtime lifecycle tests; no business scheduler is simulated.
use artifactory_emissary::{config::Config, protocol as pb};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};

pub struct Connection {
    pub hello: pb::EmissaryEvent,
    pub commands: mpsc::Sender<pb::Command>,
    pub events: mpsc::Receiver<pb::EmissaryEvent>,
}
impl Connection {
    pub async fn event(&mut self, kind: &str) -> pb::EmissaryEvent {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let event = self
                    .events
                    .recv()
                    .await
                    .expect("runtime disconnected before event");
                if event.kind == kind {
                    return event;
                }
            }
        })
        .await
        .expect("runtime event timed out")
    }
    pub async fn closed(&mut self) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while self.events.recv().await.is_some() {}
        })
        .await
        .expect("runtime did not disconnect");
    }
}
#[derive(Clone)]
struct Peer {
    connected: mpsc::Sender<Connection>,
    logs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    ack_delay: Duration,
}
#[tonic::async_trait]
impl pb::control_server::Control for Peer {
    type ConnectStream = ReceiverStream<Result<pb::Command, Status>>;
    async fn connect(
        &self,
        request: Request<Streaming<pb::EmissaryEvent>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        if request.extensions().get::<tonic::transport::server::TlsConnectInfo<tonic::transport::server::TcpConnectInfo>>()
            .and_then(|info|info.peer_certs()).is_none() {
            return Err(Status::unauthenticated("client certificate required"));
        }
        let mut incoming = request.into_inner();
        let hello = incoming
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("hello required"))?;
        let (commands, mut input) = mpsc::channel(32);
        let (events, receiver) = mpsc::channel(128);
        let (output, response) = mpsc::channel(64);
        self.connected
            .send(Connection {
                hello: hello.clone(),
                commands,
                events: receiver,
            })
            .await
            .map_err(|_| Status::unavailable("test completed"))?;
        let ack_delay = self.ack_delay;
        tokio::spawn(async move {
            let mut next = Some(hello);
            loop {
                let event = if let Some(event) = next.take() {
                    event
                } else {
                    tokio::select! {
                        result = incoming.message()=>match result {Ok(Some(event))=>event,_=>break},
                        command = input.recv()=>{match command {Some(command)=>if output.send(Ok(command)).await.is_err(){break},None=>break} continue;},
                        _=output.closed()=>break,
                    }
                };
                let command = if event.kind == "accept" {
                    Some(pb::Command {
                        kind: "start".into(),
                        attempt_id: event.attempt_id.clone(),
                        lease_token: event.lease_token.clone(),
                        request_id: event.command_id.clone(),
                        lease_seconds: 90,
                        ..Default::default()
                    })
                } else if event.sequence > 0 {
                    Some(pb::Command {
                        kind: "event_ack".into(),
                        attempt_id: event.attempt_id.clone(),
                        ack_sequence: event.sequence,
                        ..Default::default()
                    })
                } else {
                    None
                };
                let payload: serde_json::Value =
                    serde_json::from_str(&event.payload_json).unwrap_or_default();
                if matches!(event.kind.as_str(), "hello" | "heartbeat") {
                    if !payload["ota"].is_null()
                        && output
                            .send(Ok(pb::Command {
                                kind: "emissary_health".into(),
                                request_id: event.command_id.clone(),
                                ..Default::default()
                            }))
                            .await
                            .is_err()
                    {
                        break;
                    }
                    if let Some(active) = payload["active_attempts"].as_array() {
                        for attempt in active {
                            if output
                                .send(Ok(pb::Command {
                                    kind: "renew".into(),
                                    attempt_id: attempt["attempt_id"].as_str().unwrap().into(),
                                    lease_token: attempt["lease_token"].as_str().unwrap().into(),
                                    request_id: event.command_id.clone(),
                                    lease_seconds: 90,
                                    ..Default::default()
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                if let Some(command) = command {
                    tokio::time::sleep(ack_delay).await;
                    if output.send(Ok(command)).await.is_err() {
                        break;
                    }
                }
                if events.send(event).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(response)))
    }
    async fn upload_logs(
        &self,
        request: Request<Streaming<pb::LogChunk>>,
    ) -> Result<Response<pb::LogAck>, Status> {
        let mut stream = request.into_inner();
        let mut sequence = 0;
        while let Some(chunk) = stream.message().await? {
            self.logs
                .lock()
                .expect("logs mutex poisoned")
                .entry(chunk.attempt_id)
                .or_default()
                .extend(chunk.data);
            sequence = chunk.sequence;
        }
        Ok(Response::new(pb::LogAck {
            through_sequence: sequence,
        }))
    }
    async fn prepare_artifact(
        &self,
        _: Request<pb::ArtifactRequest>,
    ) -> Result<Response<pb::UploadTicket>, Status> {
        Err(Status::unimplemented("no artifacts in lifecycle fixture"))
    }
    async fn complete_artifact(
        &self,
        _: Request<pb::CompleteArtifactRequest>,
    ) -> Result<Response<pb::ArtifactRecord>, Status> {
        Err(Status::unimplemented("no artifacts in lifecycle fixture"))
    }
}
pub struct Fixture {
    pub cfg: Config,
    pub connections: mpsc::Receiver<Connection>,
    #[cfg(unix)]
    logs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn start(
        directory: &Path,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Self {
        Self::with_ack_delay(directory, versions, Duration::ZERO).await
    }
    pub async fn with_ack_delay(
        directory: &Path,
        versions: &[&'static rustls::SupportedProtocolVersion],
        ack_delay: Duration,
    ) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::from_params(&params, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut params =
            CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.signed_by(&server_key, &issuer).unwrap();
        let client_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::default();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = params.signed_by(&client_key, &issuer).unwrap();
        fs::write(directory.join("ca.pem"), ca.pem()).unwrap();
        fs::write(directory.join("client.pem"), client.pem()).unwrap();
        fs::write(directory.join("client.key"), client_key.serialize_pem()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let mut tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
            )
            .unwrap();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let incoming =
            futures::stream::unfold((listener, acceptor), |(listener, acceptor)| async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    if let Ok(tls) = acceptor.accept(socket).await {
                        return Some((Ok::<_, std::io::Error>(tls), (listener, acceptor)));
                    }
                }
            });
        let (connected, connections) = mpsc::channel(8);
        let logs = Arc::new(Mutex::new(HashMap::new()));
        let peer = Peer {
            connected,
            logs: logs.clone(),
            ack_delay,
        };
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(pb::control_server::ControlServer::new(peer))
                .serve_with_incoming_shutdown(incoming, shutdown.cancelled())
                .await
                .unwrap();
        });
        Self {
            cfg: Config {
                endpoint,
                ca_file: directory.join("ca.pem"),
                certificate_file: directory.join("client.pem"),
                key_file: directory.join("client.key"),
                emissary_id: uuid::Uuid::new_v4().to_string(),
                root: directory.join("managed"),
                allow_http_uploads: true,
                ..Default::default()
            },
            connections,
            #[cfg(unix)]
            logs,
            cancel,
            server,
        }
    }
    pub async fn connection(&mut self) -> Connection {
        tokio::time::timeout(Duration::from_secs(30), self.connections.recv())
            .await
            .unwrap()
            .unwrap()
    }
    #[cfg(unix)]
    pub fn logs(&self, id: &str) -> Vec<u8> {
        self.logs
            .lock()
            .expect("logs mutex poisoned")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server.abort();
    }
}
