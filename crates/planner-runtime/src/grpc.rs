use crate::server::{self, Runtime};
use artifactory_runtime_protocol::{
    crypto, service,
    wire::{
        JsonReply, JsonRequest,
        runtime_server::{Runtime as Contract, RuntimeServer},
    },
};
use tonic::{Request, Response, Status};
#[derive(Clone)]
pub struct Gateway {
    pub state: Runtime,
    pub token: String,
}
impl Gateway {
    fn input(
        &self,
        request: Request<JsonRequest>,
        internal: bool,
    ) -> Result<serde_json::Value, Status> {
        if internal {
            let auth = request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if crypto::hash(auth) != crypto::hash(&self.token) {
                return Err(Status::unauthenticated("service identity required"));
            }
        }
        let text = request.into_inner().json;
        if text.len() > 2 * 1024 * 1024 {
            return Err(Status::resource_exhausted("request exceeds limit"));
        }
        serde_json::from_str(&text).map_err(|_| Status::invalid_argument("invalid request"))
    }
}
fn reply(value: anyhow::Result<serde_json::Value>) -> Result<Response<JsonReply>, Status> {
    value
        .map(|v| {
            Response::new(JsonReply {
                json: v.to_string(),
            })
        })
        .map_err(|_| Status::failed_precondition("run request rejected or service unavailable"))
}
#[tonic::async_trait]
impl Contract for Gateway {
    async fn put_intent(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, true)?;
        reply(server::put_intent(&self.state, v).await)
    }
    async fn get_run(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, true)?;
        reply(
            server::read_run(
                &self.state,
                server::id(&v, "run_id").map_err(|_| Status::invalid_argument("invalid run"))?,
            )
            .await,
        )
    }
    async fn register_execution(
        &self,
        r: Request<JsonRequest>,
    ) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, true)?;
        reply(
            server::register_execution(
                &self.state,
                serde_json::from_value(v)
                    .map_err(|_| Status::invalid_argument("invalid execution"))?,
            )
            .await,
        )
    }
    async fn acquire(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, false)?;
        reply(server::acquire_execution(&self.state, v).await)
    }
    async fn heartbeat(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, false)?;
        reply(
            server::heartbeat_run(
                &self.state,
                serde_json::from_value(v).map_err(|_| Status::invalid_argument("invalid lease"))?,
            )
            .await,
        )
    }
    async fn tool(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, false)?;
        reply(server::tool_call(&self.state, v).await)
    }
    async fn report(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, false)?;
        reply(server::report_event(&self.state, v).await)
    }
    async fn validate_grant(&self, r: Request<JsonRequest>) -> Result<Response<JsonReply>, Status> {
        let v = self.input(r, true)?;
        reply(
            server::validate_grant(
                &self.state,
                serde_json::from_value(v).map_err(|_| Status::invalid_argument("invalid lease"))?,
            )
            .await,
        )
    }
}
pub async fn serve(state: Runtime) -> anyhow::Result<()> {
    let mut builder = tonic::transport::Server::builder();
    if !service::insecure() {
        let cert = tokio::fs::read(service::required("ARTIFACTORY_SERVICE_TLS_CERT")?).await?;
        let key = tokio::fs::read(service::required("ARTIFACTORY_SERVICE_TLS_KEY")?).await?;
        builder = builder.tls_config(
            tonic::transport::ServerTlsConfig::new()
                .identity(tonic::transport::Identity::from_pem(cert, key)),
        )?;
    }
    builder
        .add_service(
            RuntimeServer::new(Gateway {
                state,
                token: service::required("ARTIFACTORY_SERVICE_TOKEN")?,
            })
            .max_decoding_message_size(2 * 1024 * 1024),
        )
        .serve(
            std::env::var("ARTIFACTORY_RUNTIME_GRPC_BIND")
                .unwrap_or("0.0.0.0:8092".into())
                .parse()?,
        )
        .await?;
    Ok(())
}
