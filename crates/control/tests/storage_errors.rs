use artifactory_control::{settings::Settings, storage::Storage};
use std::{process::Command, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const CONTENT: &[u8] = b"verified diagnostic fixture";
const HASH: &str = "264c1c132ef2c39d633aded55e4bd87ea5fcd5760826cbc9fd84de922f48569c";
const ACCESS_KEY: &str = "DIAGNOSTIC_ONLY_ACCESS_KEY";
const SECRET_KEY: &str = "diagnostic-only-secret-not-a-real-credential";

#[derive(Clone, Copy, Debug)]
enum Operation {
    Initialize,
    PutLog,
    GetLog,
    ReadArtifact,
    WriteArtifact,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Disconnect,
    Forbidden,
    TruncatedBody,
}

#[test]
fn storage_transport_errors_hide_presigned_urls() {
    run_probe("transport");
}

#[test]
fn storage_status_and_body_errors_hide_presigned_urls() {
    run_probe("responses");
}

fn run_probe(mode: &str) {
    // Isolate proxy settings without changing the process environment of other
    // tests. Only synthetic credentials and an ephemeral loopback port are used.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "storage_error_probe_helper", "--nocapture"])
        .env("ARTIFACTORY_TEST_STORAGE_ERROR_MODE", mode)
        .env("NO_PROXY", "localhost,127.0.0.1")
        .env("no_proxy", "localhost,127.0.0.1")
        .output()
        .expect("run actual Storage error probes");
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "storage {mode} diagnostics failed");
}

#[tokio::test]
async fn storage_error_probe_helper() {
    let mode = match std::env::var("ARTIFACTORY_TEST_STORAGE_ERROR_MODE") {
        Ok(mode) if mode == "transport" || mode == "responses" => mode,
        _ => return,
    };
    let operations = [
        Operation::Initialize,
        Operation::PutLog,
        Operation::GetLog,
        Operation::ReadArtifact,
        Operation::WriteArtifact,
    ];
    let fault = if mode == "transport" {
        Fault::Disconnect
    } else {
        Fault::Forbidden
    };
    let mut cases = operations.map(|operation| (operation, fault)).to_vec();
    if mode == "responses" {
        cases.extend([
            (Operation::GetLog, Fault::TruncatedBody),
            (Operation::ReadArtifact, Fault::TruncatedBody),
        ]);
    }
    let mut leaked = Vec::new();
    for (operation, fault) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(serve_fault(listener, operation, fault));
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::new(&Settings {
            database_url: "unused".into(),
            http_bind: "unused".into(),
            grpc_bind: "unused".into(),
            public_url: endpoint.clone(),
            insecure_dev: true,
            registration_enabled: true,
            pki_dir: directory.path().into(),
            data_dir: directory.path().into(),
            master_key: [0; 32],
            s3_endpoint: endpoint.clone(),
            s3_public_endpoint: endpoint,
            s3_bucket: "diagnostic-probe".into(),
            s3_region: "us-east-1".into(),
            s3_access_key: ACCESS_KEY.into(),
            s3_secret_key: SECRET_KEY.into(),
            telemetry: None,
        })
        .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            match operation {
                Operation::Initialize => storage.initialize().await,
                Operation::PutLog => storage.put("private-log", CONTENT.to_vec()).await,
                Operation::GetLog => storage.get_log("private-log").await.map(|_| ()),
                Operation::ReadArtifact | Operation::WriteArtifact => {
                    storage
                        .verify_and_promote(
                            "private-upload",
                            "private-final",
                            CONTENT.len() as i64,
                            HASH,
                        )
                        .await
                }
            }
        })
        .await
        .expect("actual Storage call must finish after the loopback fault");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("fault server must finish")
            .expect("fault server must observe the real signed requests");
        let error = result.expect_err("the actual transport/status/body fault must be reported");
        let request_error = error.downcast_ref::<reqwest::Error>();
        match fault {
            Fault::Disconnect => {
                let cause = request_error.expect("retain typed transport cause");
                assert!(cause.is_request() || cause.is_connect());
                assert!(std::error::Error::source(cause).is_some());
            }
            Fault::Forbidden if matches!(operation, Operation::Initialize) => {
                assert!(format!("{error:#}").contains("403"));
            }
            Fault::Forbidden => {
                assert_eq!(request_error.unwrap().status().unwrap().as_u16(), 403);
            }
            Fault::TruncatedBody => {
                let cause = request_error.expect("retain typed response-body cause");
                assert!(cause.is_body() || cause.is_decode());
            }
        }
        // Cover the normal tracing display, anyhow's complete cause chain and
        // startup/debug diagnostics, without flattening or hiding the real cause.
        let diagnostics = format!("{error}\n{error:#}\n{error:?}");
        let exposes_url = request_error.is_some_and(|error| error.url().is_some());
        let exposes_capability = [
            ACCESS_KEY,
            SECRET_KEY,
            "X-Amz-Credential",
            "X-Amz-Signature",
        ]
        .iter()
        .any(|marker| diagnostics.contains(marker));
        if exposes_url || exposes_capability {
            leaked.push(format!("{operation:?}/{fault:?}"));
        }
        println!(
            "CASE {operation:?}/{fault:?}: signed URL hidden={} url_field={exposes_url} capability_text={exposes_capability}",
            !exposes_url && !exposes_capability
        );
    }
    assert!(leaked.is_empty(), "signed URL leaked in cases: {leaked:?}");
}

async fn serve_fault(listener: TcpListener, operation: Operation, fault: Fault) {
    if matches!(operation, Operation::WriteArtifact) {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        assert!(request.starts_with("GET /diagnostic-probe/private-upload?"));
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    CONTENT.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(CONTENT).await.unwrap();
        socket.shutdown().await.unwrap();
    }
    let (mut socket, _) = listener.accept().await.unwrap();
    let request = read_request(&mut socket).await;
    let expected = match operation {
        Operation::Initialize => "PUT /diagnostic-probe",
        Operation::PutLog => "PUT /diagnostic-probe/private-log?",
        Operation::GetLog => "GET /diagnostic-probe/private-log?",
        Operation::ReadArtifact => "GET /diagnostic-probe/private-upload?",
        Operation::WriteArtifact => "PUT /diagnostic-probe/private-final?",
    };
    assert!(request.starts_with(expected), "unexpected request location");
    match fault {
        Fault::Disconnect => {}
        Fault::Forbidden => {
            socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
        Fault::TruncatedBody => {
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\nshort",
                        CONTENT.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    }
    socket.shutdown().await.unwrap();
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert!(count > 0, "request closed before all headers arrived");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 16 * 1024, "unexpected fixture request size");
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..index].to_vec()).unwrap();
            assert!(headers.contains(ACCESS_KEY) && headers.contains("X-Amz-Signature="));
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length <= CONTENT.len(), "unexpected fixture request body");
            let received = bytes.len() - index - 4;
            if received < length {
                let mut remainder = vec![0; length - received];
                socket.read_exact(&mut remainder).await.unwrap();
            }
            return headers;
        }
    }
}
