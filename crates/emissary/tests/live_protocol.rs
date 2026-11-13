//! These tests use real Control, PostgreSQL locks and private object storage.
//! Run against an isolated acceptance fixture; never a shared production database.
use anyhow::{Context, Result, ensure};
use artifactory_database::{PoolSettings, connect};
use artifactory_database::{params, sql};
use artifactory_emissary::{client, config::Config, platform, protocol as pb};
use futures::{StreamExt, future::join_all};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Streaming, transport::Channel};
use uuid::Uuid;

struct Environment {
    http: Client,
    base: String,
    cookie: String,
    db: toasty::Db,
    existing: Config,
    source: Value,
}
impl Environment {
    async fn load() -> Result<Self> {
        let http = Client::builder().timeout(Duration::from_secs(30)).build()?;
        let base =
            std::env::var("ARTIFACTORY_LIVE_HTTP_URL").unwrap_or("http://127.0.0.1:8080".into());
        let login=http.post(format!("{base}/api/v1/auth/login")).header("x-artifactory-csrf","1").json(&json!({"username":std::env::var("ARTIFACTORY_BOOTSTRAP_USERNAME")?,"password":std::env::var("ARTIFACTORY_BOOTSTRAP_PASSWORD")?})).send().await?;
        ensure!(login.status().is_success(), "live test login failed");
        let cookie = login
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .map(|h| {
                h.to_str()
                    .map(|s| s.split(';').next().unwrap_or("").to_owned())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("; ");
        let db = connect(
            &std::env::var("DATABASE_URL")?,
            Default::default(),
            PoolSettings::new(12).timeout(Duration::from_secs(10)),
        )
        .await?;
        let existing = Config::load(Path::new(&std::env::var(
            "ARTIFACTORY_LIVE_EMISSARY_CONFIG",
        )?))?;
        let mut env = Self {
            http,
            base,
            cookie,
            db,
            existing,
            source: Value::Null,
        };
        env.source = env
            .api(
                Method::GET,
                &format!(
                    "/projects/{}",
                    std::env::var("ARTIFACTORY_LIVE_PROJECT_ID")?
                ),
                Value::Null,
            )
            .await?;
        Ok(env)
    }
    async fn api(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        let response = self
            .http
            .request(method, format!("{}/api/v1{path}", self.base))
            .header(reqwest::header::COOKIE, &self.cookie)
            .header("x-artifactory-csrf", "1")
            .json(&body)
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "acceptance API {path} returned {}",
            response.status()
        );
        Ok(response.json().await?)
    }
}
struct Probe<'a> {
    env: &'a Environment,
    cfg: Config,
    settings: Value,
    build: Uuid,
    job: Uuid,
    assignment: pb::Command,
    control: pb::control_client::ControlClient<Channel>,
    send: mpsc::Sender<pb::EmissaryEvent>,
    stream: Streaming<pb::Command>,
    _directory: tempfile::TempDir,
}
impl<'a> Probe<'a> {
    async fn new(env: &'a Environment) -> Result<Self> {
        let pool = env
            .api(
                Method::POST,
                "/pools",
                json!({"name":format!("emissary-port-{}",Uuid::new_v4())}),
            )
            .await?["id"]
            .as_str()
            .context("missing pool")?
            .to_owned();
        let token = env
            .api(Method::POST, "/enrollment-tokens", json!({"pool_id":pool}))
            .await?["token"]
            .as_str()
            .context("missing token")?
            .to_owned();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("emissary.json");
        let cfg = client::enroll(
            &path,
            Config {
                endpoint: env.existing.endpoint.clone(),
                ca_file: env.existing.ca_file.clone(),
                root: directory.path().join("runtime"),
                name: "Rust protocol probe".into(),
                ..Default::default()
            },
            token.clone(),
        )
        .await?;
        let reuse = client::enroll(
            &directory.path().join("replay.json"),
            Config {
                root: directory.path().join("replay"),
                ..cfg.clone()
            },
            token,
        )
        .await;
        ensure!(
            reuse.is_err_and(|error| error
                .downcast_ref::<tonic::Status>()
                .is_some_and(|status| status.code() == Code::Unauthenticated)),
            "one-use enrollment replay did not reject a valid new CSR"
        );
        let profiles = env.source["signing_profiles"].clone();
        let settings = json!({"draining":false,"revoked":false,"slots":1,"labels":{},"signing_profiles":profiles,"pool_id":pool});
        env.api(
            Method::PATCH,
            &format!("/nodes/{}", cfg.emissary_id),
            settings.clone(),
        )
        .await?;
        let project=env.api(Method::POST,"/projects",json!({"name":"Rust protocol acceptance","repository_url":env.source["repository_url"],"credential_id":env.source["credential_id"],"signing_profiles":profiles,"pool_ids":[pool]})).await?["id"].as_str().context("missing project")?.parse::<Uuid>()?;
        let build = Uuid::new_v4();
        let job = Uuid::new_v4();
        let (os, arch) = platform();
        let target = json!({"os":os,"arch":arch,"executor":"native","labels":{},"capabilities":[],"signing_profiles":profiles,"timeout_seconds":120,"steps":[{"name":"probe","phase":"build","shell":if cfg!(windows){"pwsh"}else{"sh"},"run":"echo probe","timeout_seconds":60}],"artifacts":["dist/probe"]});
        // Protocol tests seed an already-planned task. Planning is exercised by
        // the Kubernetes Pi acceptance and is not bypassed by the product API.
        sql::execute(&mut env.db.clone(), "INSERT INTO builds(id,project_id,requested_ref,commit_sha,config_snapshot,status,source_repository_url) VALUES($1,$2,'probe',$3,$4,'queued',$5)", params![build, project, "a".repeat(40), json!({"version":1,"parameters":{},"targets":{"native":target},"release":{"enabled":false}}), env.source["repository_url"].as_str().context("source URL missing")?]).await?;
        sql::execute(
            &mut env.db.clone(),
            "INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,'native',$3)",
            params![job, build, target],
        )
        .await?;
        let control = client::control(&cfg).await?;
        let (send, stream) = open(&cfg, control.clone(), 1).await?;
        let mut probe = Self {
            env,
            cfg,
            settings,
            build,
            job,
            assignment: pb::Command::default(),
            control,
            send,
            stream,
            _directory: directory,
        };
        probe.assignment = probe.next("assign").await?;
        Ok(probe)
    }
    fn event(&self, kind: &str) -> pb::EmissaryEvent {
        pb::EmissaryEvent {
            emissary_id: self.cfg.emissary_id.clone(),
            kind: kind.into(),
            attempt_id: self.assignment.attempt_id.clone(),
            lease_token: self.assignment.lease_token.clone(),
            command_id: Uuid::new_v4().to_string(),
            ..Default::default()
        }
    }
    async fn next(&mut self, kind: &str) -> Result<pb::Command> {
        loop {
            let command = tokio::time::timeout(Duration::from_secs(20), self.stream.message())
                .await??
                .context("control stream closed")?;
            if command.kind == kind {
                return Ok(command);
            }
            ensure!(
                command.kind == "cancel" || command.kind == "emissary_health",
                "unexpected command {} while waiting for {kind}",
                command.kind
            );
        }
    }
    async fn start(&mut self) -> Result<()> {
        let event = self.event("accept");
        let request = event.command_id.clone();
        self.send.send(event).await?;
        let grant = self.next("start").await?;
        ensure!(
            grant.request_id == request,
            "start response lost request anchor"
        );
        Ok(())
    }
    async fn lock(&self, table: &str, id: &str) -> Result<(ProtocolLock, i32)> {
        ensure!(
            ["attempts", "nodes"].contains(&table),
            "only owned protocol rows may be locked"
        );
        let id = Uuid::parse_str(id)?;
        let table = table.to_owned();
        let mut db = self.env.db.clone();
        let (ready, initialized) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut tx = db.transaction().await?;
            let pid: i32 = sql::scalar(&mut tx, "SELECT pg_backend_pid()", params![]).await?;
            sql::scalar::<Uuid>(
                &mut tx,
                &format!("SELECT id FROM {table} WHERE id=$1 FOR UPDATE"),
                params![id],
            )
            .await?;
            let _ = ready.send(pid);
            if released.await.is_ok() {
                tx.commit().await?;
            }
            Ok(())
        });
        let pid = initialized.await?;
        Ok((ProtocolLock { release, task }, pid))
    }
    async fn waiting(&self, pid: i32) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10),async{
            loop {let blocked:bool=sql::scalar(&mut self.env.db.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND wait_event_type='Lock')", params![pid]).await?;if blocked{return Ok::<_,anyhow::Error>(());}tokio::time::sleep(Duration::from_millis(25)).await;}
        }).await??;
        Ok(())
    }
    async fn session(&self) -> Result<Uuid> {
        Ok(sql::scalar(
            &mut self.env.db.clone(),
            "SELECT active_session FROM nodes WHERE id=$1",
            params![Uuid::parse_str(&self.cfg.emissary_id)?],
        )
        .await?)
    }
    async fn session_changed(&self, old: Uuid) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.session().await? == old {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
    async fn mutate(&mut self, change: &str) -> Result<()> {
        match change {
            "revoked" => {
                self.settings["revoked"] = json!(true);
                self.env
                    .api(
                        Method::PATCH,
                        &format!("/nodes/{}", self.cfg.emissary_id),
                        self.settings.clone(),
                    )
                    .await?;
            }
            "certificate-expired" => {
                sql::execute(&mut self.env.db.clone(), "UPDATE nodes SET certificate_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1", params![Uuid::parse_str(&self.cfg.emissary_id)?]).await?;
            }
            "canceled" => {
                self.env
                    .api(
                        Method::POST,
                        &format!("/builds/{}/cancel", self.build),
                        json!({}),
                    )
                    .await?;
            }
            "session-superseded" => {
                let (tx, stream) = open(&self.cfg, self.control.clone(), 1).await?;
                self.send = tx;
                self.stream = stream;
            }
            "unchanged" => {}
            _ => anyhow::bail!("unknown fence change"),
        }
        Ok(())
    }
    async fn cleanup(&self) -> Result<()> {
        let cancellation = self
            .env
            .api(
                Method::POST,
                &format!("/builds/{}/cancel", self.build),
                json!({}),
            )
            .await;
        if let Err(error) = cancellation {
            // A successful completion/cancellation may already be terminal by
            // the time cleanup runs. Preserve all other cancellation errors.
            let build = self
                .env
                .api(Method::GET, &format!("/builds/{}", self.build), Value::Null)
                .await?;
            if !matches!(
                build["status"].as_str(),
                Some("succeeded" | "failed" | "lost" | "canceled")
            ) {
                return Err(error);
            }
        }
        sql::execute(&mut self.env.db.clone(), "UPDATE attempts SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE job_id=$1 AND status IN ('leased','running')", params![self.job]).await?;
        let mut settings = self.settings.clone();
        settings["revoked"] = json!(true);
        self.env
            .api(
                Method::PATCH,
                &format!("/nodes/{}", self.cfg.emissary_id),
                settings,
            )
            .await?;
        Ok(())
    }
    async fn counts(&self) -> Result<(i64, i64)> {
        Ok(sql::tuple(&mut self.env.db.clone(), "SELECT (SELECT count(*) FROM uploads WHERE attempt_id=$1),(SELECT count(*) FROM artifacts WHERE attempt_id=$1)", params![Uuid::parse_str(&self.assignment.attempt_id)?]).await?)
    }
    fn declaration(&self, path: &str, data: &[u8]) -> pb::ArtifactRequest {
        pb::ArtifactRequest {
            attempt_id: self.assignment.attempt_id.clone(),
            lease_token: self.assignment.lease_token.clone(),
            path: path.into(),
            size: data.len() as u64,
            sha256: hex::encode(Sha256::digest(data)),
        }
    }
    async fn put(&self, ticket: &pb::UploadTicket, data: &[u8]) -> Result<()> {
        ensure!(
            self.env
                .http
                .put(&ticket.url)
                .body(data.to_vec())
                .send()
                .await?
                .status()
                .is_success(),
            "staging upload failed"
        );
        Ok(())
    }
}
async fn open(
    cfg: &Config,
    mut control: pb::control_client::ControlClient<Channel>,
    major: u32,
) -> Result<(mpsc::Sender<pb::EmissaryEvent>, Streaming<pb::Command>)> {
    let (tx, rx) = mpsc::channel(64);
    tx.send(pb::EmissaryEvent {
        emissary_id: cfg.emissary_id.clone(),
        kind: "hello".into(),
        protocol_major: major,
        payload_json: json!({"capabilities":["git","bash","sh","pwsh","cc","codesign"]})
            .to_string(),
        ..Default::default()
    })
    .await?;
    let stream = control.connect(ReceiverStream::new(rx)).await?.into_inner();
    Ok((tx, stream))
}

#[tokio::test]
#[ignore = "requires isolated Control, PostgreSQL and private S3 acceptance fixture"]
async fn live_emissary_protocol() -> Result<()> {
    let env = Environment::load().await?;
    basic(&env).await?;
    for table in ["attempts", "nodes"] {
        for kind in ["accept", "finished"] {
            for change in ["unchanged", "canceled"] {
                cancellation(&env, table, kind, change).await?;
            }
        }
    }
    for kind in ["heartbeat", "accept", "finished"] {
        for change in [
            "unchanged",
            "revoked",
            "certificate-expired",
            "session-superseded",
        ] {
            authorization(&env, kind, change).await?;
        }
    }
    for stage in ["prepare", "complete"] {
        for change in [
            "unchanged",
            "revoked",
            "certificate-expired",
            "canceled",
            "lease-expired",
        ] {
            artifact_fence(&env, stage, change).await?;
        }
    }
    concurrent_artifacts(&env).await?;
    storage_stalls(&env).await?;
    replay_completion(&env).await?;
    println!(
        "PASS live Rust Emissary: mTLS, replay, real database lock fences, concurrent artifacts, cancellation and immutable promotion"
    );
    Ok(())
}
async fn basic(env: &Environment) -> Result<()> {
    let channel = client::dial(&env.existing, true).await?;
    let result = pb::control_client::ControlClient::new(channel.clone())
        .prepare_artifact(pb::ArtifactRequest::default())
        .await;
    ensure!(
        result.is_err_and(|s| s.code() == Code::Unauthenticated),
        "certificate-free artifact request accepted"
    );
    let result = pb::enrollment_client::EnrollmentClient::new(channel)
        .enroll(pb::EnrollRequest {
            protocol_major: 999,
            ..Default::default()
        })
        .await;
    ensure!(
        result.is_err_and(|s| s.code() == Code::FailedPrecondition),
        "incompatible enrollment accepted"
    );
    let mut probe = Probe::new(env).await?;
    let result = async {
        ensure!(
            open(&probe.cfg, probe.control.clone(), 999).await.is_err(),
            "incompatible hello accepted"
        );
        for _ in 0..2 {
            probe.start().await?;
        }
        for (sequence, data, code) in [
            (1, "first", Code::Ok),
            (1, "first", Code::Ok),
            (1, "different", Code::AlreadyExists),
            (3, "gap", Code::FailedPrecondition),
            (2, "second", Code::Ok),
        ] {
            let result = probe
                .control
                .upload_logs(tokio_stream::iter([pb::LogChunk {
                    attempt_id: probe.assignment.attempt_id.clone(),
                    lease_token: probe.assignment.lease_token.clone(),
                    sequence,
                    step: "protocol".into(),
                    data: data.as_bytes().into(),
                }]))
                .await;
            match result {
                Ok(ack) => ensure!(
                    code == Code::Ok && ack.get_ref().through_sequence == sequence,
                    "invalid log acknowledgement"
                ),
                Err(s) => ensure!(
                    s.code() == code,
                    "log ordering/replay contract changed: {}",
                    s.code()
                ),
            }
        }
        let response = env
            .http
            .get(format!("{}/api/v1/builds/{}/events", env.base, probe.build))
            .header(reqwest::header::COOKIE, &env.cookie)
            .send()
            .await?
            .error_for_status()?;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let replay = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let chunk = stream.next().await.context("SSE closed before replay")??;
                bytes.extend_from_slice(&chunk);
                ensure!(bytes.len() < 4 << 20, "SSE replay exceeded test limit");
                let text = String::from_utf8_lossy(&bytes);
                let messages = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
                    .filter_map(|event| event["text"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>();
                if messages.len() >= 2 {
                    return Ok::<_, anyhow::Error>(messages);
                }
            }
        })
        .await??;
        ensure!(
            replay == ["first", "second"],
            "SSE replay changed accepted log content"
        );
        let data = b"verified immutable artifact";
        let request = probe.declaration("dist/probe", data);
        let ticket = probe
            .control
            .prepare_artifact(request.clone())
            .await?
            .into_inner();
        let complete = pb::CompleteArtifactRequest {
            attempt_id: probe.assignment.attempt_id.clone(),
            lease_token: probe.assignment.lease_token.clone(),
            upload_id: ticket.upload_id.clone(),
        };
        probe.put(&ticket, &vec![b'x'; data.len()]).await?;
        ensure!(
            probe
                .control
                .complete_artifact(complete.clone())
                .await
                .is_err_and(|s| s.code() == Code::FailedPrecondition),
            "corrupt upload accepted"
        );
        probe.put(&ticket, data).await?;
        let artifact = probe
            .control
            .complete_artifact(complete.clone())
            .await?
            .into_inner();
        ensure!(
            probe
                .control
                .complete_artifact(complete)
                .await?
                .into_inner()
                .artifact_id
                == artifact.artifact_id,
            "artifact completion not idempotent"
        );
        probe.put(&ticket, &vec![b'z'; data.len()]).await?;
        let body = env
            .http
            .get(format!(
                "{}/api/v1/artifacts/{}/download",
                env.base, artifact.artifact_id
            ))
            .header(reqwest::header::COOKIE, &env.cookie)
            .send()
            .await?
            .bytes()
            .await?;
        ensure!(
            body.as_ref() == data,
            "staging PUT changed immutable artifact"
        );
        probe.mutate("canceled").await?;
        ensure!(
            probe
                .control
                .prepare_artifact(request)
                .await
                .is_err_and(|s| s.code() == Code::FailedPrecondition),
            "canceled artifact accepted"
        );
        let mut finished = probe.event("finished");
        finished.sequence = 1;
        finished.payload_json = json!({"status":"failed","error":"protocol probe"}).to_string();
        for _ in 0..2 {
            probe.send.send(finished.clone()).await?;
            ensure!(
                probe.next("event_ack").await?.ack_sequence == 1,
                "completion replay acknowledgement changed"
            );
        }
        finished.sequence = 2;
        finished.payload_json = json!({"status":"succeeded"}).to_string();
        probe.send.send(finished).await?;
        ensure!(
            probe.next("event_ack").await.is_err(),
            "late success changed canceled job"
        );
        probe.mutate("revoked").await?;
        probe.settings["revoked"] = json!(false);
        env.api(
            Method::PATCH,
            &format!("/nodes/{}", probe.cfg.emissary_id),
            probe.settings.clone(),
        )
        .await?;
        ensure!(
            probe
                .control
                .prepare_artifact(pb::ArtifactRequest::default())
                .await
                .is_err_and(|s| s.code() == Code::Unauthenticated),
            "revoked certificate revived"
        );
        Ok(())
    }
    .await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}
async fn cancellation(env: &Environment, table: &str, kind: &str, change: &str) -> Result<()> {
    let mut probe = Probe::new(env).await?;
    let result = async {
        if kind == "finished" {
            probe.start().await?;
        }
        let id = if table == "nodes" {
            probe.cfg.emissary_id.clone()
        } else {
            probe.assignment.attempt_id.clone()
        };
        let (tx, pid) = probe.lock(table, &id).await?;
        let mut event = probe.event(kind);
        if kind == "finished" {
            event.sequence = 1;
            event.payload_json =
                json!({"status":"failed","error":"owned cancellation probe"}).to_string();
        }
        probe.send.send(event).await?;
        probe.waiting(pid).await?;
        probe.mutate(change).await?;
        tx.commit().await?;
        if kind == "accept" && change == "canceled" {
            ensure!(
                probe.next("start").await.is_err(),
                "cancellation did not fence waiting acceptance"
            );
        } else {
            let command = probe
                .next(if kind == "accept" {
                    "start"
                } else {
                    "event_ack"
                })
                .await?;
            if kind == "finished" {
                ensure!(
                    command.ack_sequence == 1,
                    "wrong completion acknowledgement"
                );
            }
            let status: String = sql::scalar(
                &mut env.db.clone(),
                "SELECT status FROM jobs WHERE id=$1",
                params![probe.job],
            )
            .await?;
            ensure!(
                status
                    == if kind == "accept" {
                        "running"
                    } else if change == "canceled" {
                        "canceled"
                    } else {
                        "failed"
                    },
                "waiting event used stale cancellation state"
            );
        }
        println!("PASS cancellation {kind}/{table}/{change}");
        Ok(())
    }
    .await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}
async fn authorization(env: &Environment, kind: &str, change: &str) -> Result<()> {
    let mut probe = Probe::new(env).await?;
    let result=async {
        if kind!="accept" {probe.start().await?;}
        let before:Value=sql::scalar(&mut env.db.clone(), "SELECT jsonb_build_object('lease',lease_expires_at,'status',status,'sequence',last_sequence) FROM attempts WHERE id=$1", params![Uuid::parse_str(&probe.assignment.attempt_id)?]).await?;
        let(tx,pid)=probe.lock("attempts",&probe.assignment.attempt_id).await?;
        let mut event=probe.event(kind);
        if kind=="heartbeat" {event.attempt_id.clear();event.lease_token.clear();event.payload_json=json!({"active_attempts":[{"attempt_id":probe.assignment.attempt_id,"lease_token":probe.assignment.lease_token}],"capabilities":[]}).to_string();}
        if kind=="finished" {event.sequence=1;event.payload_json=json!({"status":"failed","error":"owned authorization probe"}).to_string();}
        probe.send.send(event).await?;probe.waiting(pid).await?;
        // A fresh stream must be kept separately while the blocked old stream is observed.
        let mut fresh=None;
        if change=="session-superseded" {
            let old=probe.session().await?;
            fresh=Some(open(&probe.cfg,probe.control.clone(),1).await?);
            probe.session_changed(old).await?;
        }else{probe.mutate(change).await?;}
        tx.commit().await?;
        let reply=probe.next(match kind{"heartbeat"=>"renew","accept"=>"start",_=>"event_ack"}).await;
        let after:Value=sql::scalar(&mut env.db.clone(), "SELECT jsonb_build_object('lease',lease_expires_at,'status',status,'sequence',last_sequence) FROM attempts WHERE id=$1", params![Uuid::parse_str(&probe.assignment.attempt_id)?]).await?;
        if change=="unchanged" {ensure!(reply.is_ok() && before!=after,"authorized waiting event failed");}else {ensure!(before==after && reply.is_err(),"invalid authorization committed a waiting event");}
        drop(fresh);println!("PASS authorization {kind}/{change}");Ok(())
    }.await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}
async fn artifact_fence(env: &Environment, stage: &str, change: &str) -> Result<()> {
    let mut probe = Probe::new(env).await?;
    let result=async {
        probe.start().await?;let data=b"owned artifact authorization probe";let declaration=probe.declaration("dist/fenced",data);
        let ticket=if stage=="complete" {let ticket=probe.control.prepare_artifact(declaration.clone()).await?.into_inner();probe.put(&ticket,data).await?;Some(ticket)}else{None};
        let before=probe.counts().await?;
        if change=="lease-expired" {sql::execute(&mut env.db.clone(), "UPDATE attempts SET lease_expires_at=clock_timestamp()+interval '2 seconds' WHERE id=$1", params![Uuid::parse_str(&probe.assignment.attempt_id)?]).await?;}
        let(tx,pid)=probe.lock("attempts",&probe.assignment.attempt_id).await?;
        let mut control=probe.control.clone();let attempt=probe.assignment.attempt_id.clone();let token=probe.assignment.lease_token.clone();
        let rpc=tokio::spawn(async move {
            if let Some(ticket)=ticket {control.complete_artifact(pb::CompleteArtifactRequest{attempt_id:attempt,lease_token:token,upload_id:ticket.upload_id}).await.map(|r|r.into_inner().artifact_id)}else{control.prepare_artifact(declaration).await.map(|r|r.into_inner().upload_id)}
        });
        probe.waiting(pid).await?;
        if change=="lease-expired" {tokio::time::sleep(Duration::from_millis(2100)).await;}else{probe.mutate(change).await?;}
        tx.commit().await?;let outcome=tokio::time::timeout(Duration::from_secs(20),rpc).await??;
        let after=probe.counts().await?;
        if change=="unchanged" {ensure!(outcome.is_ok() && after==if stage=="prepare"{(1,0)}else{(1,1)},"authorized waiting artifact failed");}
        else {ensure!(outcome.is_err_and(|s|matches!(s.code(),Code::FailedPrecondition|Code::Unauthenticated)) && before==after,"invalid waiting artifact committed records");}
        println!("PASS artifact {stage}/{change}");Ok(())
    }.await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}
async fn concurrent_artifacts(env: &Environment) -> Result<()> {
    let mut probe = Probe::new(env).await?;
    let result = async {
        probe.start().await?;
        let data = b"concurrent immutable artifact";
        let request = probe.declaration("dist/concurrent", data);
        let results = join_all((0..8).map(|_| {
            let mut control = probe.control.clone();
            let request = request.clone();
            async move { control.prepare_artifact(request).await }
        }))
        .await;
        let tickets = results
            .into_iter()
            .map(|r| r.map(tonic::Response::into_inner))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            tickets.iter().all(|t| t.upload_id == tickets[0].upload_id),
            "concurrent prepare created duplicate tickets"
        );
        probe.put(&tickets[0], data).await?;
        let complete = pb::CompleteArtifactRequest {
            attempt_id: probe.assignment.attempt_id.clone(),
            lease_token: probe.assignment.lease_token.clone(),
            upload_id: tickets[0].upload_id.clone(),
        };
        let mut heartbeat = probe.event("heartbeat");
        heartbeat.attempt_id.clear();
        heartbeat.lease_token.clear();
        heartbeat.payload_json = json!({"active_attempts":[{
            "attempt_id":probe.assignment.attempt_id,
            "lease_token":probe.assignment.lease_token
        }]}).to_string();
        let request_id = heartbeat.command_id.clone();
        let send = probe.send.clone();
        let operations = join_all((0..16).map(|index| {
            let mut control = probe.control.clone();
            let request = request.clone();
            let complete = complete.clone();
            async move {
                if index % 2 == 0 {
                    Ok(control.prepare_artifact(request).await?.into_inner().upload_id)
                } else {
                    let artifact = control.complete_artifact(complete).await?.into_inner();
                    ensure!(artifact.size == request.size && artifact.sha256 == request.sha256,
                        "concurrent completion changed artifact content");
                    Ok::<_, anyhow::Error>(artifact.artifact_id)
                }
            }
        }));
        let (results, heartbeat_sent) = tokio::join!(operations, send.send(heartbeat));
        heartbeat_sent?;
        let records = results.into_iter().collect::<Result<Vec<_>>>()?;
        ensure!(
            records
                .iter()
                .all(|id| *id == tickets[0].upload_id)
                && probe.counts().await? == (1, 1),
            "concurrent completion created duplicate artifacts"
        );
        let renewal = probe.next("renew").await?;
        ensure!(renewal.request_id == request_id && renewal.attempt_id == probe.assignment.attempt_id,
            "mixed artifact requests prevented the anchored heartbeat renewal");
        println!("PASS eight concurrent preparations and 17 mixed artifact/heartbeat requests preserve one artifact");
        Ok(())
    }
    .await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}
async fn replay_completion(env: &Environment) -> Result<()> {
    let mut probe = Probe::new(env).await?;
    let result = async {
        probe.start().await?;
        probe.settings["draining"] = json!(true);
        env.api(
            Method::PATCH,
            &format!("/nodes/{}", probe.cfg.emissary_id),
            probe.settings.clone(),
        )
        .await?;
        let mut finished = probe.event("finished");
        finished.sequence = 1;
        finished.payload_json =
            json!({"status":"failed","error":"replay probe","failure_kind":"emissary"}).to_string();
        probe.send.send(finished.clone()).await?;
        probe.next("event_ack").await?;
        let failure: String = sql::scalar(
            &mut env.db.clone(),
            "SELECT failure_kind FROM attempts WHERE id=$1",
            params![Uuid::parse_str(&probe.assignment.attempt_id)?],
        )
        .await?;
        ensure!(
            failure == "emissary",
            "Emissary failure classification was not persisted"
        );
        env.api(
            Method::POST,
            &format!("/jobs/{}/retry", probe.job),
            json!({}),
        )
        .await?;
        let (tx, pid) = probe.lock("nodes", &probe.cfg.emissary_id).await?;
        probe.send.send(finished).await?;
        probe.waiting(pid).await?;
        tx.commit().await?;
        ensure!(
            probe.next("event_ack").await?.ack_sequence == 1,
            "replay was not acknowledged"
        );
        let status: String = sql::scalar(
            &mut env.db.clone(),
            "SELECT status FROM jobs WHERE id=$1",
            params![probe.job],
        )
        .await?;
        ensure!(status == "queued", "old completion changed retried job");
        Ok(())
    }
    .await;
    let cleanup = probe.cleanup().await;
    result.and(cleanup)
}

struct PausedStorage {
    prefix: Vec<String>,
    target: String,
    resumed: bool,
}
impl PausedStorage {
    fn pause() -> Result<Option<Self>> {
        let (prefix, target) = if let Ok(container) = std::env::var("ARTIFACTORY_LIVE_S3_CONTAINER")
        {
            (Vec::new(), container)
        } else if let Ok(node) = std::env::var("ARTIFACTORY_LIVE_S3_NODE") {
            let task = std::env::var("ARTIFACTORY_LIVE_S3_TASK")?;
            ensure!(
                task.len() == 64 && task.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid isolated storage task"
            );
            (
                vec![
                    "exec".into(),
                    node,
                    "ctr".into(),
                    "--namespace".into(),
                    "k8s.io".into(),
                    "tasks".into(),
                ],
                task,
            )
        } else {
            println!("SKIP storage pause: no dedicated storage task configured");
            return Ok(None);
        };
        let storage = Self {
            prefix,
            target,
            resumed: false,
        };
        storage.run("pause")?;
        Ok(Some(storage))
    }
    fn run(&self, action: &str) -> Result<()> {
        let output = std::process::Command::new("docker")
            .args(&self.prefix)
            .arg(action)
            .arg(&self.target)
            .output()?;
        ensure!(
            output.status.success(),
            "isolated storage {action} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        self.run(if self.prefix.is_empty() {
            "unpause"
        } else {
            "resume"
        })?;
        self.resumed = true;
        Ok(())
    }
}
impl Drop for PausedStorage {
    fn drop(&mut self) {
        if !self.resumed {
            let _ = self.resume();
        }
    }
}
async fn storage_stalls(env: &Environment) -> Result<()> {
    if std::env::var_os("ARTIFACTORY_LIVE_S3_CONTAINER").is_none()
        && std::env::var_os("ARTIFACTORY_LIVE_S3_NODE").is_none()
    {
        println!("SKIP storage pause: no dedicated storage task configured");
        return Ok(());
    }
    for stage in ["log", "artifact"] {
        let mut probe = Probe::new(env).await?;
        let result=async {
            probe.start().await?;
            let data=b"storage stall must not hold lease transaction";
            let ticket=if stage=="artifact"{let ticket=probe.control.prepare_artifact(probe.declaration("dist/stall",data)).await?.into_inner();probe.put(&ticket,data).await?;Some(ticket)}else{None};
            let mut storage=PausedStorage::pause()?.context("missing dedicated storage task")?;
            let mut control=client::control(&probe.cfg).await?;
            let attempt=probe.assignment.attempt_id.clone();let token=probe.assignment.lease_token.clone();
            let rpc=tokio::spawn(async move {
                if let Some(ticket)=ticket {
                    control.complete_artifact(pb::CompleteArtifactRequest{attempt_id:attempt,lease_token:token,upload_id:ticket.upload_id}).await.map(|_|())
                }else{
                    control.upload_logs(tokio_stream::iter([pb::LogChunk{attempt_id:attempt,lease_token:token,sequence:1,step:"stall".into(),data:data.to_vec()}])).await.map(|_|())
                }
            });
            tokio::time::sleep(Duration::from_millis(750)).await;
            ensure!(!rpc.is_finished(),"{stage} storage request did not stall");
            let mut heartbeat=probe.event("heartbeat");heartbeat.attempt_id.clear();heartbeat.lease_token.clear();
            heartbeat.payload_json=json!({"active_attempts":[{"attempt_id":probe.assignment.attempt_id,"lease_token":probe.assignment.lease_token}]}).to_string();
            let request=heartbeat.command_id.clone();probe.send.send(heartbeat).await?;
            let grant=tokio::time::timeout(Duration::from_secs(3),probe.next("renew")).await??;
            ensure!(grant.request_id==request && grant.attempt_id==probe.assignment.attempt_id,"storage stall returned wrong renewal");
            storage.resume()?;
            tokio::time::timeout(Duration::from_secs(30),rpc).await???;
            println!("PASS {stage} storage stall preserves independent heartbeat");Ok(())
        }.await;
        probe.cleanup().await?;
        result?;
    }
    Ok(())
}

// The task owns its Db and native transaction; cancellation rolls it back.
struct ProtocolLock {
    release: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<()>>,
}
impl ProtocolLock {
    async fn commit(self) -> Result<()> {
        let _ = self.release.send(());
        self.task.await?
    }
}
