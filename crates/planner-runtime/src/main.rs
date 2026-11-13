use artifactory_planner_runtime::server;
use artifactory_runtime_protocol::service;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let state = server::from_env().await?;
    let background = state.clone();
    let grpc = state.clone();
    tokio::spawn(async move {
        if let Err(error) = artifactory_planner_runtime::grpc::serve(grpc).await {
            tracing::error!(%error,"runtime gRPC stopped");
        }
    });
    tokio::spawn(async move {
        loop {
            if let Err(error) = server::maintain(&background).await {
                tracing::warn!(%error,"runtime reconciliation failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
    service::serve(
        &std::env::var("ARTIFACTORY_RUNTIME_BIND").unwrap_or("0.0.0.0:8091".into()),
        server::router(state, service::required("ARTIFACTORY_SERVICE_TOKEN")?),
    )
    .await
}
