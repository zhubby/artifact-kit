use artifactory_runtime_protocol::service;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let state = artifactory_source_service::service::from_env().await?;
    service::serve(
        &std::env::var("ARTIFACTORY_SOURCE_BIND").unwrap_or("0.0.0.0:8090".into()),
        artifactory_source_service::service::router(
            state,
            service::required("ARTIFACTORY_SERVICE_TOKEN")?,
        ),
    )
    .await
}
