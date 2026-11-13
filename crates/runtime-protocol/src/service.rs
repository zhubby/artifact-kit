//! Shared authenticated transport. Service credentials never enter CRDs or runner Pods.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::StreamExt;
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder,
    service::TowerToHyperService,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{env, sync::Arc, time::Duration};
use tokio_rustls::TlsAcceptor;

pub fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("{name} is required"))
}
pub fn key(name: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(required(name)?)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("{name} must contain 32 base64 encoded bytes"))
}
pub fn insecure() -> bool {
    env::var("ARTIFACTORY_INSECURE_DEV").as_deref() == Ok("true")
}
pub fn endpoint(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.host_str().is_some() && url.username().is_empty() && url.password().is_none(),
        "invalid service endpoint"
    );
    ensure!(
        url.scheme() == "https" || (insecure() && url.scheme() == "http"),
        "service HTTPS required"
    );
    Ok(())
}
/// Validate an advertised download base URL before attaching snapshot paths.
pub fn download_origin(value: &str) -> Result<String> {
    endpoint(value)?;
    let url = reqwest::Url::parse(value)?;
    ensure!(
        !value.chars().any(char::is_whitespace)
            && url.query().is_none()
            && url.fragment().is_none(),
        "download URL cannot contain whitespace, a query or a fragment"
    );
    Ok(value.trim_end_matches('/').to_owned())
}
pub fn client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Budget reservations and outboxes own retries, including protocol NACKs.
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10));
    if let Ok(path) = env::var("ARTIFACTORY_SERVICE_CA") {
        builder =
            builder.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
    }
    Ok(builder.build()?)
}
#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub origin: String,
    token: String,
}
impl Client {
    pub fn new(origin: String, token: String) -> Result<Self> {
        endpoint(&origin)?;
        ensure!(
            token.len() >= 32,
            "service token must contain at least 32 characters"
        );
        Ok(Self {
            http: client()?,
            origin: origin.trim_end_matches('/').into(),
            token,
        })
    }
    pub async fn post<T: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        path: &str,
        value: &T,
    ) -> Result<R> {
        let response = self
            .http
            .post(format!("{}{path}", self.origin))
            .bearer_auth(&self.token)
            .json(value)
            .timeout(Duration::from_secs(330))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "service rejected request ({})",
            response.status()
        );
        decode(response, 2 * 1024 * 1024).await
    }
}
pub async fn decode<T: DeserializeOwned>(response: reqwest::Response, limit: usize) -> Result<T> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        ensure!(
            bytes.len() + chunk.len() <= limit,
            "service response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn protected(router: Router, token: String) -> Router {
    router.layer(middleware::from_fn(
        move |request: Request<Body>, next: Next| {
            let token = token.clone();
            async move {
                let supplied = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .unwrap_or("");
                // Compare digests so malformed and wrong-length tokens follow the same comparison.
                let a = crate::crypto::hash(supplied);
                let b = crate::crypto::hash(&token);
                if a.bytes()
                    .zip(b.bytes())
                    .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                    != 0
                {
                    return Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Body::empty())
                        .unwrap();
                }
                next.run(request).await
            }
        },
    ))
}
pub async fn serve(bind: &str, router: Router) -> Result<()> {
    let router = router.layer(middleware::from_fn(crate::metrics::observe));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    if insecure() && env::var("ARTIFACTORY_SERVICE_TLS_CERT").is_err() {
        axum::serve(listener, router).await?;
        return Ok(());
    }
    let cert = std::fs::read(required("ARTIFACTORY_SERVICE_TLS_CERT")?)?;
    let private = std::fs::read(required("ARTIFACTORY_SERVICE_TLS_KEY")?)?;
    let certs = rustls_pemfile::certs(&mut cert.as_slice()).collect::<std::io::Result<Vec<_>>>()?;
    let private =
        rustls_pemfile::private_key(&mut private.as_slice())?.context("missing TLS private key")?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, private)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    loop {
        let (socket, _) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            if let Ok(Ok(tls)) =
                tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await
            {
                let _ = Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(
                        TokioIo::new(tls),
                        TowerToHyperService::new(router),
                    )
                    .await;
            }
        });
    }
}
