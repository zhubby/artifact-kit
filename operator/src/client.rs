use anyhow::Result;
use artifactory_runtime_protocol::{
    service,
    wire::{JsonRequest, runtime_client::RuntimeClient},
};
use serde_json::Value;
pub async fn call(method: &str, value: &Value) -> Result<Value> {
    let url = service::required("ARTIFACTORY_RUNTIME_GRPC_URL")?;
    let mut endpoint = tonic::transport::Endpoint::from_shared(url.clone())?
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15));
    if url.starts_with("https://") {
        let mut tls = tonic::transport::ClientTlsConfig::new().with_enabled_roots();
        if let Ok(path) = std::env::var("ARTIFACTORY_SERVICE_CA") {
            tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(
                tokio::fs::read(path).await?,
            ));
        }
        endpoint = endpoint.tls_config(tls)?;
    }
    let mut client = RuntimeClient::new(endpoint.connect().await?);
    let mut request = tonic::Request::new(JsonRequest {
        json: value.to_string(),
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", service::required("ARTIFACTORY_SERVICE_TOKEN")?).parse()?,
    );
    let response = if method == "get" {
        client.get_run(request).await?
    } else {
        client.register_execution(request).await?
    };
    Ok(serde_json::from_str(&response.into_inner().json)?)
}
