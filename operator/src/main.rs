use artifactory_operator::{
    controller::{self, Context},
    leadership::Leadership,
};
use artifactory_runtime_protocol::service;
use std::{sync::Arc, time::Duration};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let namespace = service::required("ARTIFACTORY_NAMESPACE")?;
    let client = kube::Client::try_default().await?;
    let leader = Leadership {
        api: kube::Api::namespaced(client.clone(), &namespace),
        identity: Arc::new(
            std::env::var("POD_UID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        ),
        name: "artifactory-operator".into(),
    };
    let ctx = Arc::new(Context {
        client,
        namespace,
        runtime: service::Client::new(
            service::required("ARTIFACTORY_RUNTIME_URL")?,
            service::required("ARTIFACTORY_SERVICE_TOKEN")?,
        )?,
        leader,
    });
    let election = ctx.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = election.leader.renew().await {
                tracing::warn!(%error,"operator lease renewal failed");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    let background = ctx.clone();
    tokio::spawn(async move {
        loop {
            if background.leader.ensure().await.is_ok()
                && let Err(error) = controller::dispatch(&background).await
            {
                tracing::warn!(%error,"operator dispatch failed");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    controller::controller(ctx).await;
    Ok(())
}
