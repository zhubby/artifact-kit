use crate::{
    client,
    config::{Config, atomic_json, private_dir},
    doctor, ota, process, protocol as pb, source,
    worker::{Event, Input, Lease},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
    task::JoinSet,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

struct Task {
    token: String,
    assignment: pb::Assignment,
    lease: Lease,
    started: bool,
    control: Option<mpsc::Sender<Input>>,
    pending: Vec<pb::EmissaryEvent>,
    sequence: u64,
    sent_sequence: u64,
    replay_at: Option<Instant>,
    last_accept_at: Option<Instant>,
    log_sequence: u64,
    finished_at: Option<Instant>,
    buffer_error: bool,
    logs_abandoned: bool,
}
impl Task {
    fn new(token: String, assignment: pb::Assignment, seconds: u32, now: Instant) -> Self {
        Self {
            token,
            assignment,
            lease: Lease::new(now, seconds),
            started: false,
            control: None,
            pending: vec![],
            sequence: 0,
            sent_sequence: 0,
            replay_at: None,
            last_accept_at: None,
            log_sequence: 0,
            finished_at: None,
            buffer_error: false,
            logs_abandoned: false,
        }
    }
}
#[derive(Serialize, Deserialize)]
struct LogRecord {
    sequence: u64,
    step: String,
    data: Vec<u8>,
}
struct Spool {
    root: PathBuf,
    bytes: u64,
    limit: u64,
}
impl Spool {
    fn new(root: PathBuf, limit: u64) -> Result<Self> {
        private_dir(&root.join("spool"))?;
        let bytes = walkdir::WalkDir::new(root.join("spool"))
            .into_iter()
            .try_fold(0, |sum, entry| {
                let entry = entry?;
                Ok::<_, anyhow::Error>(
                    sum + if entry.file_type().is_file() {
                        entry.metadata()?.len()
                    } else {
                        0
                    },
                )
            })?;
        Ok(Self { root, bytes, limit })
    }
    fn append(&mut self, id: &str, sequence: &mut u64, step: &str, data: &[u8]) -> Result<()> {
        let record = LogRecord {
            sequence: *sequence + 1,
            step: step.into(),
            data: data.into(),
        };
        let bytes = serde_json::to_vec(&record)?;
        ensure!(
            self.bytes + bytes.len() as u64 <= self.limit,
            "log buffer exhausted"
        );
        crate::config::atomic_bytes(
            &self
                .root
                .join("spool")
                .join(id)
                .join(format!("{:020}.json", record.sequence)),
            &bytes,
            false,
        )?;
        *sequence = record.sequence;
        self.bytes += bytes.len() as u64;
        Ok(())
    }
    fn batch(&self, id: &str) -> Result<Vec<LogRecord>> {
        let dir = self.root.join("spool").join(id);
        if !dir.try_exists()? {
            return Ok(vec![]);
        }
        let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        entries
            .into_iter()
            .take(32)
            .map(|e| Ok(serde_json::from_slice(&fs::read(e.path())?)?))
            .collect()
    }
    fn empty(&self, id: &str) -> bool {
        fs::read_dir(self.root.join("spool").join(id))
            .map(|mut e| e.next().is_none())
            .unwrap_or(false)
            || !self.root.join("spool").join(id).exists()
    }
    fn ack(&mut self, id: &str, through: u64) -> Result<()> {
        let dir = self.root.join("spool").join(id);
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .strip_suffix(".json")
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|seq| seq <= through)
            {
                let size = entry.metadata()?.len();
                fs::remove_file(entry.path())?;
                self.bytes = self.bytes.saturating_sub(size);
            }
        }
        Ok(())
    }
}
struct State {
    cfg: Config,
    tasks: HashMap<String, Task>,
    anchors: HashMap<String, Instant>,
    spool: Spool,
    diagnostics: Value,
    capabilities: Vec<String>,
    workers: usize,
    stopping: bool,
    ota_directory: Option<PathBuf>,
    ota_session: String,
    health_at: Option<Instant>,
    drain_request: String,
    drain_update: String,
    telemetry: Option<crate::telemetry::Handle>,
}
enum Action {
    Start(String, u32),
    Control(String, Box<Input>),
}
impl State {
    fn new(cfg: Config, diagnostics: Value, capabilities: Vec<String>) -> Result<Self> {
        let spool = Spool::new(cfg.root.clone(), cfg.log_buffer_bytes)?;
        Ok(Self {
            cfg,
            tasks: HashMap::new(),
            anchors: HashMap::new(),
            spool,
            diagnostics,
            capabilities,
            workers: 0,
            stopping: false,
            ota_directory: std::env::var_os("ARTIFACTORY_OTA_DIRECTORY").map(PathBuf::from),
            ota_session: uuid::Uuid::new_v4().to_string(),
            health_at: None,
            drain_request: String::new(),
            drain_update: String::new(),
            telemetry: None,
        })
    }
    fn heartbeat(&mut self, kind: &str) -> pb::EmissaryEvent {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let ota = self.ota_snapshot(&id);
        self.anchors
            .retain(|_, at| now.saturating_duration_since(*at) < Duration::from_secs(120));
        self.anchors.insert(id.clone(), now);
        let mut metrics = json!({});
        if let Some(snapshot) = self.telemetry.as_ref().and_then(|h| h.snapshot()) {
            self.diagnostics = snapshot.diagnostics;
            self.capabilities = snapshot.capabilities;
            metrics = snapshot.metrics;
            if self.ota_directory.is_some()
                && !self.capabilities.iter().any(|c| c == "emissary-ota-v1")
            {
                self.capabilities.push("emissary-ota-v1".into());
            }
        }
        let mut diagnostics = self.diagnostics.clone();
        diagnostics["metrics"] = metrics;
        diagnostics["buffered_log_bytes"] = json!(self.spool.bytes);
        pb::EmissaryEvent{emissary_id:self.cfg.emissary_id.clone(),kind:kind.into(),command_id:id,protocol_major:1,payload_json:json!({"active_attempts":self.tasks.iter().map(|(id,t)|json!({"attempt_id":id,"lease_token":t.token})).collect::<Vec<_>>(),"capabilities":self.capabilities,"diagnostics":diagnostics,"ota":ota}).to_string(),..Default::default()}
    }
    fn ota_snapshot(&mut self, request: &str) -> Option<ota::State> {
        let directory = self.ota_directory.as_ref()?;
        let mut state = ota::snapshot(directory).ok()?;
        if state.phase == "draining"
            && self.tasks.is_empty()
            && self.workers == 0
            && self.spool.bytes == 0
        {
            state.phase = "activating".into();
            state.activation_pending = true;
            self.drain_request = request.into();
            self.drain_update = state.update_id.clone();
        }
        Some(state)
    }
    fn command(&mut self, command: pb::Command, now: Instant) -> Result<Vec<Action>> {
        if let Some(directory) = &self.ota_directory {
            match command.kind.as_str() {
                "emissary_update" => {
                    ota::submit(directory, &serde_json::from_str(&command.payload_json)?)?;
                    return Ok(vec![]);
                }
                "emissary_health" => {
                    if let Some(anchor) =
                        self.anchors.get(&command.request_id).copied().filter(|at| {
                            now.saturating_duration_since(*at) < Duration::from_secs(30)
                                && self.health_at.is_none_or(|last| *at > last)
                        })
                    {
                        self.health_at = Some(anchor);
                        if command.request_id == self.drain_request {
                            let state = ota::snapshot(directory)?;
                            if state.phase == "draining" && state.update_id == self.drain_update {
                                atomic_json(
                                    &directory.join("drained.json"),
                                    &json!({"id":state.update_id}),
                                )?;
                            }
                        }
                        ota::confirm(directory, &self.ota_session, &command.request_id)?;
                    }
                    return Ok(vec![]);
                }
                _ => {}
            }
        }
        let id = command.attempt_id;
        if command.kind == "assign" {
            ensure!(source::valid_id(&id), "invalid attempt ID");
            if !self.stopping
                && self.tasks.len() < 32
                && !self.tasks.contains_key(&id)
                && command.lease_seconds > 0
                && let Some(assignment) = command.assignment
            {
                self.tasks.insert(
                    id,
                    Task::new(command.lease_token, assignment, command.lease_seconds, now),
                );
            }
            return Ok(vec![]);
        }
        let Some(task) = self.tasks.get_mut(&id) else {
            return Ok(vec![]);
        };
        if command.kind == "event_ack" {
            let terminal = task
                .pending
                .iter()
                .any(|e| e.kind == "finished" && e.sequence <= command.ack_sequence);
            task.pending.retain(|e| e.sequence > command.ack_sequence);
            if terminal {
                self.tasks.remove(&id);
            }
            return Ok(vec![]);
        }
        ensure!(command.lease_token == task.token, "lease token mismatch");
        match command.kind.as_str() {
            "start" | "renew" => {
                if self.stopping && !task.started {
                    return Ok(vec![]);
                }
                let Some(anchor) = self.anchors.get(&command.request_id) else {
                    return Ok(vec![]);
                };
                if !task.lease.renew(now, *anchor, command.lease_seconds) {
                    self.drop_task(&id, "execution lease expired")?;
                    return Ok(vec![]);
                }
                let seconds = task
                    .lease
                    .deadline()
                    .saturating_duration_since(now)
                    .as_secs() as u32;
                if seconds == 0 {
                    self.drop_task(&id, "execution lease expired")?;
                    return Ok(vec![]);
                }
                if command.kind == "start" && !task.started {
                    task.started = true;
                    Ok(vec![Action::Start(id, seconds)])
                } else {
                    Ok(vec![Action::Control(
                        id,
                        Box::new(Input {
                            kind: "renew".into(),
                            lease_seconds: seconds,
                            ..Default::default()
                        }),
                    )])
                }
            }
            "cancel" => {
                if command.message == "lease_invalid" {
                    self.drop_task(&id, "execution lease expired or revoked")?;
                    Ok(vec![])
                } else {
                    Ok(vec![Action::Control(
                        id,
                        Box::new(Input {
                            kind: "cancel".into(),
                            reason: "cancel requested".into(),
                            ..Default::default()
                        }),
                    )])
                }
            }
            _ => Ok(vec![]),
        }
    }
    fn drop_task(&mut self, id: &str, reason: &str) -> Result<()> {
        if let Some(mut task) = self.tasks.remove(id)
            && let Some(control) = task.control.take()
        {
            let _ = control.try_send(Input {
                kind: "cancel".into(),
                reason: reason.into(),
                ..Default::default()
            });
        }
        atomic_json(
            &self.cfg.root.join("lost").join(format!("{id}.json")),
            &json!({"attempt_id":id,"reason":reason,"time":chrono::Utc::now()}),
        )
    }
    fn event(&mut self, id: &str, mut event: Event) -> Result<()> {
        let Some(task) = self.tasks.get_mut(id) else {
            return Ok(());
        };
        if task.finished_at.is_some() {
            return Ok(());
        }
        if event.kind == "log" {
            if self
                .spool
                .append(id, &mut task.log_sequence, &event.step, &event.data)
                .is_err()
            {
                task.buffer_error = true;
                if let Some(control) = &task.control {
                    let _ = control.try_send(Input {
                        kind: "cancel".into(),
                        reason: "log buffer exhausted or unavailable".into(),
                        ..Default::default()
                    });
                }
            }
            return Ok(());
        }
        if event.kind == "finished" {
            task.finished_at = Some(Instant::now());
            if task.buffer_error {
                event.payload = json!({"status":"failed","error":"log buffer exhausted or unavailable","failure_kind":"emissary"});
            }
        }
        task.sequence += 1;
        task.pending.push(pb::EmissaryEvent {
            emissary_id: self.cfg.emissary_id.clone(),
            kind: event.kind,
            attempt_id: id.into(),
            lease_token: task.token.clone(),
            sequence: task.sequence,
            payload_json: event.payload.to_string(),
            ..Default::default()
        });
        Ok(())
    }
    fn pending(&mut self) -> Vec<pb::EmissaryEvent> {
        let mut events = Vec::new();
        let now = Instant::now();
        for (id, task) in &mut self.tasks {
            if !task.started {
                if self.stopping
                    || task.last_accept_at.is_some_and(|at| {
                        now.saturating_duration_since(at) < Duration::from_secs(1)
                    })
                {
                    continue;
                }
                task.last_accept_at = Some(now);
                let request = uuid::Uuid::new_v4().to_string();
                self.anchors.insert(request.clone(), now);
                events.push(pb::EmissaryEvent {
                    emissary_id: self.cfg.emissary_id.clone(),
                    kind: "accept".into(),
                    attempt_id: id.clone(),
                    lease_token: task.token.clone(),
                    command_id: request,
                    ..Default::default()
                });
            } else {
                // Replay unacknowledged events on a timer. Replaying on every
                // acknowledgement amplifies a slow peer's queue and can starve
                // later step/terminal events and heartbeat processing.
                if task.replay_at.is_none_or(|at| now >= at) {
                    task.sent_sequence = 0;
                    task.replay_at = Some(now + Duration::from_secs(5));
                }
                for event in &task.pending {
                    if event.sequence <= task.sent_sequence {
                        continue;
                    }
                    if event.kind == "finished" && !task.logs_abandoned && !self.spool.empty(id) {
                        break;
                    }
                    events.push(event.clone());
                }
            }
        }
        events
    }
    fn stop(&mut self) {
        self.stopping = true;
        for task in self.tasks.values_mut() {
            if let Some(control) = task.control.take() {
                let _ = control.try_send(Input {
                    kind: "cancel".into(),
                    reason: "Emissary stopping".into(),
                    ..Default::default()
                });
            }
        }
    }
}
enum Message {
    Connected(String, mpsc::Sender<pb::EmissaryEvent>),
    Disconnected(String),
    Command(String, Box<pb::Command>),
    Event(String, Event),
    Exit(String),
    Logs(String, Option<u64>),
}
async fn connect(
    cfg: Config,
    session: String,
    hello: pb::EmissaryEvent,
    send: mpsc::Sender<Message>,
    cancel: CancellationToken,
) {
    let result = async {
        let mut control = client::control(&cfg).await?;
        let (tx, rx) = mpsc::channel(256);
        tx.send(hello).await?;
        let mut stream = control.connect(ReceiverStream::new(rx)).await?.into_inner();
        send.send(Message::Connected(session.clone(), tx)).await?;
        while let Some(command) = stream.message().await? {
            send.send(Message::Command(session.clone(), Box::new(command)))
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        _=cancel.cancelled()=>{},
        outcome=result=>if let Err(error)=outcome {
            eprintln!("control stream ended: {error}");
        },
    }
    let _ = send.send(Message::Disconnected(session)).await;
}
async fn worker_task(
    cfg: Config,
    id: String,
    token: String,
    assignment: pb::Assignment,
    seconds: u32,
    (mut controls, registry): (mpsc::Receiver<Input>, crate::telemetry::Registry),
    send: mpsc::Sender<Message>,
) -> Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = process::spawn(command)?;
    let attempt = uuid::Uuid::parse_str(&id)?;
    let _managed = if let Some(pid) = child.id() {
        registry.register(attempt, pid).await.ok()
    } else {
        None
    };
    let mut stdin = child.stdin().take().context("worker stdin missing")?;
    let stdout = child.stdout().take().context("worker stdout missing")?;
    let initial = Input {
        kind: "start".into(),
        config: cfg,
        attempt_id: id.clone(),
        lease_token: token,
        lease_seconds: seconds,
        assignment: Some(assignment),
        ..Default::default()
    };
    let mut bytes = serde_json::to_vec(&initial)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    let writer = tokio::spawn(async move {
        while let Some(input) = controls.recv().await {
            let mut bytes = serde_json::to_vec(&input)?;
            bytes.push(b'\n');
            stdin.write_all(&bytes).await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        let event: Event = serde_json::from_str(&line)?;
        if event.kind == "step_started" {
            registry.step(
                attempt,
                event.payload["index"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok()),
            );
        }
        if event.kind == "step_finished" {
            registry.step(attempt, None);
        }
        send.send(Message::Event(id.clone(), event)).await?;
    }
    writer.abort();
    let _ = writer.await;
    child.wait().await?;
    Ok(())
}
pub async fn run(cfg: Config, cancel: CancellationToken) -> Result<()> {
    ensure!(
        source::valid_id(&cfg.emissary_id),
        "Emissary must be registered first"
    );
    private_dir(&cfg.root)?;
    let (diagnostics, mut capabilities) = doctor::doctor(&cfg).await;
    if std::env::var_os("ARTIFACTORY_OTA_DIRECTORY").is_some() {
        capabilities.push("emissary-ota-v1".into());
    }
    let mut state = State::new(cfg.clone(), diagnostics, capabilities)?;
    if !cfg.telemetry_endpoint.is_empty() {
        state.telemetry = Some(crate::telemetry::Handle::start(
            cfg.clone(),
            state.diagnostics.clone(),
            state.capabilities.clone(),
        ));
    }
    let (send, mut recv) = mpsc::channel(256);
    let mut jobs = JoinSet::new();
    let transport = CancellationToken::new();
    let mut outbound: Option<mpsc::Sender<pb::EmissaryEvent>> = None;
    let mut connecting = false;
    let mut session = String::new();
    let mut retry = Instant::now();
    let mut backoff = 1;
    let mut log_busy = HashSet::new();
    // Tonic reconnects this independent HTTP/2 channel. Share cheap client
    // clones across upload batches instead of rebuilding TLS for every batch.
    let log_transport = std::sync::Arc::new(tokio::sync::OnceCell::new());
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut heartbeat = Instant::now();
    let mut shutdown: Option<Instant> = None;
    loop {
        if !connecting && outbound.is_none() && Instant::now() >= retry && shutdown.is_none() {
            connecting = true;
            session = uuid::Uuid::new_v4().to_string();
            state.ota_session = session.clone();
            state.health_at = None;
            if let Some(directory) = &state.ota_directory {
                ota::confirm(directory, &session, "")?;
            }
            let hello = state.heartbeat("hello");
            let cfg = cfg.clone();
            let send = send.clone();
            let session = session.clone();
            let token = transport.clone();
            jobs.spawn(connect(cfg, session, hello, send, token));
        }
        tokio::select! {
            _=cancel.cancelled(),if shutdown.is_none()=>{state.stop();shutdown=Some(Instant::now()+Duration::from_secs(5));},
            _=tick.tick()=>{
                let now=Instant::now();
                let expired=state.tasks.iter().filter(|(_,task)|task.lease.expired(now)).map(|(id,_)|id.clone()).collect::<Vec<_>>();
                for id in expired {state.drop_task(&id,"execution lease expired")?;}
                for task in state.tasks.values_mut() {
                    if task.finished_at.is_some_and(|at|now.saturating_duration_since(at)>Duration::from_secs(120)) && !task.logs_abandoned {
                        task.logs_abandoned=true;
                        for event in &mut task.pending {if event.kind=="finished" {event.payload_json=json!({"status":"failed","error":"log delivery timed out; unacknowledged chunks retained on Emissary"}).to_string();}}
                    }
                }
                for (id,task) in &state.tasks {
                    if !log_busy.contains(id)
                        && let Ok(records)=state.spool.batch(id)&& !records.is_empty() {
                            log_busy.insert(id.clone());let cfg=cfg.clone();let id=id.clone();let token=task.token.clone();let send=send.clone();let log_transport=log_transport.clone();
                            jobs.spawn(async move {
                                let operation=async {
                                    let mut control=log_transport.get_or_try_init(||client::control(&cfg)).await?.clone();
                                    let max=records.last().map(|r|r.sequence).unwrap_or(0);
                                    let chunks=records.into_iter().map(|r|pb::LogChunk{attempt_id:id.clone(),lease_token:token.clone(),sequence:r.sequence,step:r.step,data:r.data}).collect::<Vec<_>>();
                                    let ack=control.upload_logs(tokio_stream::iter(chunks)).await?.into_inner();
                                    Ok::<_,anyhow::Error>(ack.through_sequence.min(max))
                                };
                                let through=tokio::time::timeout(Duration::from_secs(20),operation).await.ok().and_then(Result::ok);
                                let _=send.send(Message::Logs(id,through)).await;
                            });
                        }
                }
                if now>=heartbeat {if let Some(tx)=&outbound {let _=tx.try_send(state.heartbeat("heartbeat"));}heartbeat=now+Duration::from_secs(10);}
                if let Some(deadline)=shutdown&& (now>=deadline || (state.workers==0 && state.tasks.values().all(|t|!t.started) && state.spool.bytes==0)) {break;}
            },
            Some(message)=recv.recv()=>match message {
                Message::Connected(id,tx) if id==session=>{
                    outbound=Some(tx);connecting=false;backoff=1;
                    for task in state.tasks.values_mut() {
                        task.sent_sequence=0;task.replay_at=None;task.last_accept_at=None;
                    }
                },
                Message::Disconnected(id) if id==session=>{outbound=None;connecting=false;retry=Instant::now()+client::retry_delay(backoff);backoff=(backoff*2).min(30);if shutdown.is_none(){eprintln!("control connection interrupted; reconnecting");}},
                Message::Command(id,command) if id==session=>{
                    let actions=state.command(*command,Instant::now())?;
                    for action in actions {match action {
                        Action::Control(id,input)=>if let Some(task)=state.tasks.get(&id)&& let Some(tx)=&task.control {let _=tx.try_send(*input);},
                        Action::Start(id,seconds)=>{
                            let task=state.tasks.get_mut(&id).context("missing started task")?;
                            let (tx,rx)=mpsc::channel(32);task.control=Some(tx);state.workers+=1;
                            let cfg=cfg.clone();let token=task.token.clone();let assignment=task.assignment.clone();let send=send.clone();
                            let registry=state.telemetry.as_ref().map(|h|h.registry.clone()).unwrap_or_default();
                            jobs.spawn(async move {
                                if worker_task(cfg,id.clone(),token,assignment,seconds,(rx,registry),send.clone()).await.is_err() {
                                    let _=send.send(Message::Event(id.clone(),Event{kind:"finished".into(),payload:json!({"status":"failed","error":"isolated worker exited without a terminal result","failure_kind":"emissary"}),..Default::default()})).await;
                                }
                                let _=send.send(Message::Exit(id)).await;
                            });
                        },
                    }}
                },
                Message::Event(id,event)=>state.event(&id,event)?,
                Message::Exit(id)=>{
                    state.workers=state.workers.saturating_sub(1);
                    if state.tasks.get(&id).is_some_and(|t|t.finished_at.is_none()) {state.event(&id,Event{kind:"finished".into(),payload:json!({"status":"failed","error":"worker exited without a terminal result","failure_kind":"emissary"}),..Default::default()})?;}
                },
                Message::Logs(id,through)=>{log_busy.remove(&id);if let Some(through)=through {state.spool.ack(&id,through)?;}},
                _=>{},
            },
            Some(_)=jobs.join_next(),if !jobs.is_empty()=>{},
        }
        if let Some(tx) = &outbound {
            for event in state.pending() {
                let sequence = event.sequence;
                let id = event.attempt_id.clone();
                if tx.try_send(event).is_err() {
                    break;
                }
                if let Some(task) = state.tasks.get_mut(&id) {
                    task.sent_sequence = task.sent_sequence.max(sequence);
                }
            }
        }
    }
    transport.cancel();
    state.stop();
    // Give EOF-triggered worker cleanup a chance even when the control plane is down.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while jobs.join_next().await.is_some() {}
    })
    .await;
    jobs.shutdown().await;
    Ok(())
}
#[cfg(test)]
mod tests;
