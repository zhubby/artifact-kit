use crate::{
    config::{Config, private_dir, private_write},
    platform,
    protocol::{self as pb, control_client::ControlClient},
};
use anyhow::{Context, Result, ensure};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use std::{fs, io::BufReader, path::Path, sync::Arc, time::Duration};
use tonic::transport::{Channel, Endpoint};

pub async fn dial(cfg: &Config, enrollment: bool) -> Result<Channel> {
    let address = if cfg.endpoint.starts_with("https://") {
        cfg.endpoint.clone()
    } else {
        ensure!(!cfg.endpoint.contains("://"), "TLS endpoint required");
        format!("https://{}", cfg.endpoint)
    };
    let address = url::Url::parse(&address)?;
    let host = address
        .host_str()
        .context("TLS endpoint has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    ensure!(
        address.username().is_empty()
            && address.password().is_none()
            && address.path() == "/"
            && address.query().is_none()
            && address.fragment().is_none(),
        "TLS endpoint must contain only a host and port"
    );
    let port = address
        .port_or_known_default()
        .context("TLS endpoint has no port")?;
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())?;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_pemfile::certs(&mut BufReader::new(fs::File::open(&cfg.ca_file)?)) {
        roots.add(certificate?)?;
    }
    ensure!(!roots.is_empty(), "pinned CA file is empty");
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots);
    let mut tls = if enrollment {
        builder.with_no_client_auth()
    } else {
        let certificates =
            rustls_pemfile::certs(&mut BufReader::new(fs::File::open(&cfg.certificate_file)?))
                .collect::<std::io::Result<Vec<_>>>()?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(fs::File::open(&cfg.key_file)?))?
            .context("client private key missing")?;
        builder.with_client_auth_cert(certificates, key)?
    };
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    // The custom connector always encrypts the socket. The internal HTTP URI
    // prevents Tonic from attempting a second TLS handshake with its defaults.
    let authority = &address[url::Position::BeforeHost..url::Position::AfterPort];
    Ok(Endpoint::from_shared(format!("http://{authority}"))?
        .connect_timeout(Duration::from_secs(10))
        .connect_with_connector(tower::service_fn(move |_| {
            let connector = connector.clone();
            let server_name = server_name.clone();
            let host = host.clone();
            async move {
                let socket = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                socket.set_nodelay(true)?;
                let socket = connector.connect(server_name, socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(socket))
            }
        }))
        .await?)
}
pub async fn control(cfg: &Config) -> Result<ControlClient<Channel>> {
    Ok(ControlClient::new(dial(cfg, false).await?).max_decoding_message_size(512 * 1024))
}

/// Randomize retry phase without extending the configured maximum backoff.
pub(crate) fn retry_delay(seconds: u64) -> Duration {
    let ceiling = seconds.clamp(1, 30) * 1000;
    let half = ceiling / 2;
    let random = (uuid::Uuid::new_v4().as_u128() % u128::from(half + 1)) as u64;
    Duration::from_millis(half + random)
}
pub async fn enroll(path: &Path, mut cfg: Config, token: String) -> Result<Config> {
    ensure!(
        !path.try_exists()?,
        "configuration already exists; use a new configuration and root"
    );
    for name in ["node.pem", "node.key"] {
        ensure!(
            !cfg.root.join(name).try_exists()?,
            "managed root already contains a device identity"
        );
    }
    cfg.root = std::path::absolute(&cfg.root)?;
    private_dir(&cfg.root)?;
    let key = KeyPair::generate()?;
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, &cfg.name);
    let csr = params.serialize_request(&key)?;
    let (os, arch) = platform();
    let mut client = pb::enrollment_client::EnrollmentClient::new(dial(&cfg, true).await?);
    let mut request = tonic::Request::new(pb::EnrollRequest {
        token,
        name: cfg.name.clone(),
        os: os.into(),
        arch: arch.into(),
        csr_pem: csr.pem()?,
        protocol_major: 1,
    });
    request.set_timeout(Duration::from_secs(30));
    let reply = client
        .enroll(request)
        .await
        .context("device enrollment rejected")?
        .into_inner();
    ensure!(
        uuid::Uuid::parse_str(&reply.emissary_id).is_ok(),
        "invalid registered Emissary identity"
    );
    cfg.emissary_id = reply.emissary_id;
    cfg.telemetry_endpoint = reply.telemetry_endpoint;
    cfg.key_file = cfg.root.join("node.key");
    cfg.certificate_file = cfg.root.join("node.pem");
    private_write(&cfg.key_file, key.serialize_pem().as_bytes(), false, true)?;
    private_write(
        &cfg.certificate_file,
        reply.certificate_pem.as_bytes(),
        false,
        true,
    )?;
    cfg.save(path)?;
    Ok(cfg)
}
