//! Node identity, execution leases, events and upload persistence.
use super::ControlStore;
use crate::{crypto, pki};
use artifactory_database::{params, sql};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use toasty::stmt::Type;

use tonic::Status;
use uuid::Uuid;
fn internal(error: impl std::fmt::Display) -> Status {
    crate::grpc::internal(error)
}
const VALID_ATTEMPT_QUERY: &str = "SELECT EXISTS(SELECT 1 FROM attempts a JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status='running' AND a.lease_expires_at>clock_timestamp() AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND ($4 OR NOT b.cancel_requested))";

async fn valid_node(
    executor: &mut dyn toasty::Executor,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    let valid:bool=sql::scalar(executor, "SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())", params![node, session]).await.map_err(internal)?;
    if valid {
        Ok(())
    } else {
        Err(Status::unauthenticated(
            "node session revoked or superseded",
        ))
    }
}

async fn lock_authorized_node(
    transaction: &mut toasty::db::Transaction<'_>,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    // Acquire this after the attempt lock, then validate with a fresh clock and
    // snapshot. Keep revocation/session replacement serialized with the write.
    sql::execute(
        transaction,
        "SELECT id FROM nodes WHERE id=$1 FOR SHARE",
        params![node],
    )
    .await
    .map_err(internal)?;
    valid_node(transaction, node, session).await
}

async fn valid_attempt(
    executor: &mut dyn toasty::Executor,
    node: Uuid,
    attempt: &str,
    token: &str,
    allow_canceled: bool,
) -> Result<Uuid, Status> {
    let attempt =
        Uuid::parse_str(attempt).map_err(|_| Status::invalid_argument("invalid attempt ID"))?;
    let valid: bool = sql::scalar(
        executor,
        VALID_ATTEMPT_QUERY,
        params![attempt, node, token, allow_canceled],
    )
    .await
    .map_err(internal)?;
    if valid {
        Ok(attempt)
    } else {
        Err(Status::failed_precondition(
            "attempt lease expired, canceled or superseded",
        ))
    }
}

async fn lock_artifact_attempt(
    transaction: &mut toasty::db::Transaction<'_>,
    node: Uuid,
    attempt: Uuid,
    token: &str,
) -> Result<(), Status> {
    // Shared locks coexist with insertion's foreign-key KEY SHARE lock. Check
    // again after waiting; hold authorization stable through the short commit.
    sql::execute(
        transaction,
        "SELECT id FROM attempts WHERE id=$1 FOR SHARE",
        params![attempt],
    )
    .await
    .map_err(internal)?;
    sql::execute(
        transaction,
        "SELECT id FROM nodes WHERE id=$1 FOR SHARE",
        params![node],
    )
    .await
    .map_err(internal)?;
    valid_attempt(transaction, node, &attempt.to_string(), token, false).await?;
    Ok(())
}

pub(crate) struct NodeEnrollment<'a> {
    pub token: &'a str,
    pub name: &'a str,
    pub os: &'a str,
    pub arch: &'a str,
}
pub(crate) struct EnrolledNode {
    pub id: Uuid,
    pub certificate: String,
}
pub(crate) struct LogCommit<'a> {
    pub token: &'a str,
    pub sequence: u64,
    pub step: &'a str,
    pub key: &'a str,
    pub hash: &'a str,
    pub size: i64,
}
pub(crate) struct ArtifactDeclaration<'a> {
    pub path: &'a str,
    pub size: u64,
    pub sha256: &'a str,
    pub lease_token: &'a str,
}
pub(crate) struct PreparedUpload {
    pub id: Uuid,
    pub object_key: String,
    pub completed: bool,
}
pub(crate) struct UploadVerification {
    pub object_key: String,
    pub final_key: String,
    pub expected_size: i64,
    pub expected_sha256: String,
}
pub(crate) struct CompletedArtifact {
    pub id: Uuid,
    pub path: String,
    pub sha256: String,
    pub size: u64,
}
pub(crate) struct ExecutionEvent<'a> {
    pub kind: &'a str,
    pub sequence: u64,
    pub lease_token: &'a str,
    pub payload_json: &'a str,
}
pub(crate) enum EventReceipt {
    Start,
    Ack(u64),
}

pub(crate) enum SessionUpdate {
    Current,
    Register {
        owner: Uuid,
        certificate_hash: String,
    },
}

impl ControlStore {
    pub(crate) async fn node_identity(&self, hash: String) -> Result<Uuid, Status> {
        sql::optional_scalar(&mut self.pool.clone(), "SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()", params![hash]).await.map_err(internal)?
        .ok_or_else(|| Status::unauthenticated("node certificate revoked, expired or unknown"))
    }

    pub(crate) async fn validate_attempt(
        &self,
        node: Uuid,
        attempt: &str,
        token: &str,
        canceled: bool,
    ) -> Result<Uuid, Status> {
        valid_attempt(&mut self.pool.clone(), node, attempt, token, canceled).await
    }

    pub(crate) async fn enroll_node<F, Fut>(
        &self,
        input: NodeEnrollment<'_>,
        issue: F,
    ) -> Result<EnrolledNode, Status>
    where
        F: FnOnce(Uuid) -> Fut,
        Fut: std::future::Future<Output = Result<String, Status>>,
    {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await.map_err(internal)?;
        let token=sql::optional_record(&mut transaction, "SELECT id,pool_id FROM enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>now() FOR UPDATE", params![crypto::hash(input.token)], &[("id", Type::Uuid),("pool_id", Type::Uuid)]).await.map_err(internal)?
            .ok_or_else(|| Status::unauthenticated("invalid or expired enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = issue(id).await?;
        let hash = pki::certificate_hash(&certificate).map_err(internal)?;
        super::models::Node::create()
            .id(id)
            .name(input.name)
            .os(input.os)
            .arch(input.arch)
            .pool_id(token.decode::<Uuid>("pool_id").map_err(internal)?)
            .certificate_hash(hash)
            .certificate_expires_at(
                sql::timestamp(Utc::now() + ChronoDuration::days(30)).map_err(internal)?,
            )
            .exec(&mut transaction)
            .await
            .map_err(internal)?;
        sql::execute(
            &mut transaction,
            "UPDATE enrollment_tokens SET used_at=now() WHERE id=$1",
            params![token.decode::<Uuid>("id").map_err(internal)?],
        )
        .await
        .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(EnrolledNode { id, certificate })
    }

    pub(crate) async fn commit_log_chunk(
        &self,
        node: Uuid,
        attempt: Uuid,
        input: LogCommit<'_>,
    ) -> Result<(), Status> {
        let LogCommit {
            key, hash, size, ..
        } = input;
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await.map_err(internal)?;
        sql::execute(
            &mut transaction,
            "SELECT id FROM attempts WHERE id=$1 FOR UPDATE",
            params![attempt],
        )
        .await
        .map_err(internal)?;
        let valid: bool = sql::scalar(
            &mut transaction,
            VALID_ATTEMPT_QUERY,
            params![attempt, node, input.token, true],
        )
        .await
        .map_err(internal)?;
        if !valid {
            return Err(Status::failed_precondition(
                "execution lease is no longer valid",
            ));
        }
        if let Some(existing) = sql::optional_scalar::<String>(
            &mut transaction,
            "SELECT sha256 FROM log_chunks WHERE attempt_id=$1 AND sequence=$2",
            params![attempt, input.sequence as i64],
        )
        .await
        .map_err(internal)?
        {
            if existing != hash {
                return Err(Status::already_exists("log sequence content mismatch"));
            }
        } else {
            let previous: i64 = sql::scalar(
                &mut transaction,
                "SELECT coalesce(max(sequence),0) FROM log_chunks WHERE attempt_id=$1",
                params![attempt],
            )
            .await
            .map_err(internal)?;
            if input.sequence != previous as u64 + 1 {
                return Err(Status::failed_precondition(format!(
                    "expected log sequence {}",
                    previous + 1
                )));
            }
            super::models::LogChunk::create()
                .attempt_id(attempt)
                .sequence(input.sequence as i64)
                .step(input.step)
                .object_key(key)
                .sha256(hash)
                .size(size)
                .exec(&mut transaction)
                .await
                .map_err(internal)?;
        }
        transaction.commit().await.map_err(internal)?;
        Ok(())
    }

    pub(crate) async fn prepare_upload(
        &self,
        node: Uuid,
        attempt: Uuid,
        input: ArtifactDeclaration<'_>,
    ) -> Result<PreparedUpload, Status> {
        let id = Uuid::new_v4();
        let key = format!("uploads/{id}");
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await.map_err(internal)?;
        sql::execute(&mut transaction, "INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,$3,$4,$5,$6,now()+interval '15 minutes') ON CONFLICT(attempt_id,path) DO NOTHING", params![id, attempt, input.path, key, input.size as i64, input.sha256]).await.map_err(internal)?;
        let row=sql::record(&mut transaction, "SELECT u.*,EXISTS(SELECT 1 FROM artifacts a WHERE a.id=u.id) AS completed FROM uploads u WHERE attempt_id=$1 AND path=$2", params![attempt, input.path], &[("id", Type::Uuid),("attempt_id", Type::Uuid),("path", Type::String),("object_key", Type::String),("expected_size", Type::I64),("expected_sha256", Type::String),("expires_at", Type::Timestamp),("completed", Type::Bool)]).await.map_err(internal)?;
        if row.decode::<i64>("expected_size").map_err(internal)? != input.size as i64
            || row.decode::<String>("expected_sha256").map_err(internal)? != input.sha256
        {
            return Err(Status::already_exists(
                "artifact path already declared with different content",
            ));
        }
        lock_artifact_attempt(&mut transaction, node, attempt, input.lease_token).await?;
        transaction.commit().await.map_err(internal)?;
        Ok(PreparedUpload {
            id: row.decode("id").map_err(internal)?,
            object_key: row.decode("object_key").map_err(internal)?,
            completed: row.decode("completed").map_err(internal)?,
        })
    }

    pub(crate) async fn complete_upload<F, Fut>(
        &self,
        node: Uuid,
        attempt: Uuid,
        id: Uuid,
        token: &str,
        promote: F,
    ) -> Result<CompletedArtifact, Status>
    where
        F: FnOnce(UploadVerification) -> Fut,
        Fut: std::future::Future<Output = Result<(), Status>>,
    {
        let row = sql::optional_record(
            &mut self.pool.clone(),
            "SELECT * FROM uploads WHERE id=$1 AND attempt_id=$2",
            params![id, attempt],
            &[
                ("id", Type::Uuid),
                ("attempt_id", Type::Uuid),
                ("path", Type::String),
                ("object_key", Type::String),
                ("expected_size", Type::I64),
                ("expected_sha256", Type::String),
                ("expires_at", Type::Timestamp),
            ],
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("upload not found"))?;
        let exists: bool = sql::scalar(
            &mut self.pool.clone(),
            "SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1)",
            params![id],
        )
        .await
        .map_err(internal)?;
        let final_key = format!("artifacts/{id}");
        if !exists {
            promote(UploadVerification {
                object_key: row.decode("object_key").map_err(internal)?,
                final_key: final_key.clone(),
                expected_size: row.decode("expected_size").map_err(internal)?,
                expected_sha256: row.decode("expected_sha256").map_err(internal)?,
            })
            .await?;
        }
        let mut db = self.pool.clone();
        let mut transaction = db.transaction().await.map_err(internal)?;
        sql::execute(
            &mut transaction,
            "SELECT id FROM uploads WHERE id=$1 AND attempt_id=$2 FOR UPDATE",
            params![id, attempt],
        )
        .await
        .map_err(internal)?;
        lock_artifact_attempt(&mut transaction, node, attempt, token).await?;
        if !exists {
            sql::execute(&mut transaction, "INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(id) DO NOTHING", params![id, attempt, row.decode::<String>("path").map_err(internal)?, final_key, row.decode::<i64>("expected_size").map_err(internal)?, row.decode::<String>("expected_sha256").map_err(internal)?]).await.map_err(internal)?;
        }
        transaction.commit().await.map_err(internal)?;
        Ok(CompletedArtifact {
            id,
            path: row.decode("path").map_err(internal)?,
            sha256: row.decode("expected_sha256").map_err(internal)?,
            size: row.decode::<i64>("expected_size").map_err(internal)? as u64,
        })
    }

    pub(crate) async fn record_heartbeat(
        &self,
        node: Uuid,
        session: Uuid,
        payload: &Value,
        update: SessionUpdate,
    ) -> Result<(), Status> {
        let (new_owner, certificate_hash) = match update {
            SessionUpdate::Current => (None, None),
            SessionUpdate::Register {
                owner,
                certificate_hash,
            } => (Some(owner), Some(certificate_hash)),
        };
        let ordinary_health = crate::grpc::ordinary_health(payload);
        // Health is a replaceable snapshot: a database crash may discard its latest
        // samples. Avoid a WAL flush for every idle heartbeat. The materialized CTE
        // confines this policy to this implicit transaction; session registration
        // and the separate lease/command transactions retain the configured policy.
        let updated=sql::execute(&mut self.pool.clone(), "WITH durability AS MATERIALIZED (SELECT set_config('synchronous_commit',CASE WHEN $8::uuid IS NULL THEN 'off' ELSE current_setting('synchronous_commit') END,true)) UPDATE nodes SET active_session=$2,connection_owner=coalesce($8,connection_owner),last_seen=now(),capabilities=CASE WHEN startup_id IS NULL THEN coalesce($3,capabilities) ELSE capabilities END,diagnostics=$4,emissary_ota_status=coalesce($5,emissary_ota_status),emissary_current_version=CASE WHEN $5::jsonb IS NULL THEN emissary_current_version ELSE $6 END,emissary_launcher_version=CASE WHEN $5::jsonb IS NULL THEN emissary_launcher_version ELSE $7 END FROM durability WHERE id=$1 AND (($8::uuid IS NULL AND active_session=$2) OR ($8::uuid IS NOT NULL AND certificate_hash=$9 AND startup_id IS NULL)) AND NOT revoked AND certificate_expires_at>clock_timestamp()", params![node, session, payload.get("capabilities").cloned(), payload.get("diagnostics").cloned().unwrap_or(json!({})), ordinary_health, payload["diagnostics"]["emissary_version"].as_str().unwrap_or(""), ordinary_health.and_then(|status|status["launcher_version"].as_str()), new_owner, certificate_hash]).await.map_err(internal)?;
        if updated == 0 {
            return Err(if new_owner.is_some() {
                Status::permission_denied("node authorization expired")
            } else {
                Status::unauthenticated("node session revoked or superseded")
            });
        }
        Ok(())
    }

    pub(crate) async fn renew_attempt(
        &self,
        node: Uuid,
        session: Uuid,
        id: Uuid,
        token: &str,
    ) -> Result<bool, Status> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await.map_err(internal)?;
        sql::execute(
            &mut transaction,
            "SELECT id FROM attempts WHERE id=$1 AND node_id=$2 FOR UPDATE",
            params![id, node],
        )
        .await
        .map_err(internal)?;
        lock_authorized_node(&mut transaction, node, session).await?;
        let row=sql::optional_record(&mut transaction, "UPDATE attempts a SET lease_expires_at=clock_timestamp()+interval '90 seconds' FROM jobs j,builds b,nodes n WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND j.id=a.job_id AND b.id=j.build_id AND NOT b.cancel_requested AND n.id=a.node_id AND n.active_session=$4 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() RETURNING a.status", params![id, node, token, session], &[("status", Type::String)]).await.map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(row.is_some())
    }

    pub(crate) async fn submit_execution_event(
        &self,
        node: Uuid,
        session: Uuid,
        id: Uuid,
        event: ExecutionEvent<'_>,
    ) -> Result<EventReceipt, Status> {
        let mut transaction_db = self.pool.clone();
        let mut transaction = transaction_db.transaction().await.map_err(internal)?;
        let row=sql::optional_record(&mut transaction, "SELECT a.*,j.target_config FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.lease_expires_at>now() FOR UPDATE OF a,j", params![id, node, event.lease_token], &[("id", Type::Uuid),("job_id", Type::Uuid),("node_id", Type::Uuid),("lease_token", Type::String),("lease_expires_at", Type::Timestamp),("status", Type::String),("command_id", Type::Uuid),("last_sequence", Type::I64),("error", Type::String),("started_at", Type::Timestamp),("finished_at", Type::Timestamp),("created_at", Type::Timestamp),("plan_revision_id", Type::Uuid),("source_snapshot_id", Type::Uuid),("failure_kind", Type::String),("target_config", Type::String)]).await.map_err(internal)?
        .ok_or_else(|| Status::failed_precondition("attempt lease expired or unknown"))?;
        lock_authorized_node(&mut transaction, node, session).await?;
        // Cancellation can commit while either row lock waits. Read it with the
        // current lease clock after both waits, rather than using the old snapshot.
        let current = sql::record(&mut transaction, "SELECT a.lease_expires_at>clock_timestamp() AS lease_valid,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1", params![id], &[("lease_valid", Type::Bool),("cancel_requested", Type::Bool)]).await.map_err(internal)?;
        if !current.decode::<bool>("lease_valid").map_err(internal)? {
            return Err(Status::failed_precondition(
                "attempt lease expired while waiting for its lock",
            ));
        }
        let canceled: bool = current.decode("cancel_requested").map_err(internal)?;
        let status: String = row.decode("status").map_err(internal)?;
        if event.kind == "accept" {
            if !["leased", "running"].contains(&status.as_str()) || canceled {
                return Err(Status::failed_precondition("attempt cannot start"));
            }
            sql::execute(&mut transaction, "UPDATE attempts SET status='running',started_at=coalesce(started_at,now()),lease_expires_at=now()+interval '90 seconds' WHERE id=$1", params![id]).await.map_err(internal)?;
            sql::execute(
                &mut transaction,
                "UPDATE jobs SET status='running' WHERE id=$1",
                params![row.decode::<Uuid>("job_id").map_err(internal)?],
            )
            .await
            .map_err(internal)?;
            transaction.commit().await.map_err(internal)?;
            return Ok(EventReceipt::Start);
        }
        let previous: i64 = row.decode("last_sequence").map_err(internal)?;
        if event.sequence <= previous as u64 {
            return Ok(EventReceipt::Ack(previous as u64));
        }
        if status != "running" || event.sequence != previous as u64 + 1 {
            return Err(Status::failed_precondition(
                "task event out of order or terminal",
            ));
        }
        let payload: Value = serde_json::from_str(event.payload_json)
            .map_err(|_| Status::invalid_argument("invalid event payload"))?;
        let target: crate::recipe::Target =
            serde_json::from_value(row.decode("target_config").map_err(internal)?)
                .map_err(internal)?;
        match event.kind {
            "source_cache" => {
                if payload.get("hit").and_then(Value::as_bool).is_none()
                    || payload.get("path").and_then(Value::as_str).is_none()
                    || payload
                        .get("commit")
                        .and_then(Value::as_str)
                        .is_none_or(|value| !crate::source_cache::valid_commit(value))
                {
                    return Err(Status::invalid_argument("invalid source cache event"));
                }
                let updated=sql::execute(&mut transaction, "UPDATE source_repositories r SET emissary_cache_hits=emissary_cache_hits+CASE WHEN $2 THEN 1 ELSE 0 END,emissary_cache_misses=emissary_cache_misses+CASE WHEN $2 THEN 0 ELSE 1 END WHERE r.id IN (SELECT (entry->>'repository_id')::uuid FROM source_snapshots s CROSS JOIN LATERAL jsonb_array_elements(s.manifest->'repositories') entry JOIN attempts a ON a.source_snapshot_id=s.id WHERE a.id=$1 AND entry->>'path'=$3 AND entry->>'commit'=$4)", params![id, payload["hit"].as_bool().unwrap(), payload["path"].as_str().unwrap(), payload["commit"].as_str().unwrap()]).await.map_err(internal)?;
                if updated != 1 {
                    return Err(Status::invalid_argument(
                        "source cache event does not match attempt snapshot",
                    ));
                }
            }
            "step_started" => {
                let index = payload
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| Status::invalid_argument("step index required"))?
                    as usize;
                let step = target
                    .steps
                    .get(index)
                    .ok_or_else(|| Status::invalid_argument("unknown step"))?;
                let complete: i64 = sql::scalar(
                    &mut transaction,
                    "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
                    params![id],
                )
                .await
                .map_err(internal)?;
                if complete != index as i64 {
                    return Err(Status::failed_precondition("previous steps not complete"));
                }
                super::models::StepRun::create()
                    .attempt_id(id)
                    .step_index(index as i32)
                    .name(&step.name)
                    .phase(&step.phase)
                    .status("running")
                    .exec(&mut transaction)
                    .await
                    .map_err(internal)?;
            }
            "step_finished" => {
                let index = payload
                    .get("index")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Status::invalid_argument("step index required"))?;
                let exit = payload
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let changed=sql::execute(&mut transaction, "UPDATE step_runs SET status=$3,exit_code=$4,finished_at=now() WHERE attempt_id=$1 AND step_index=$2 AND status='running'", params![id, index as i32, if exit==0 {"succeeded"} else {"failed"}, exit as i32]).await.map_err(internal)?;
                if changed != 1 {
                    return Err(Status::failed_precondition("step not running"));
                }
            }
            "finished" => {
                let requested = payload
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if !["succeeded", "failed", "canceled"].contains(&requested) {
                    return Err(Status::invalid_argument("invalid result status"));
                }
                let final_status = if canceled { "canceled" } else { requested };
                if final_status == "succeeded" {
                    let complete: i64 = sql::scalar(
                        &mut transaction,
                        "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
                        params![id],
                    )
                    .await
                    .map_err(internal)?;
                    let artifacts: i64 = sql::scalar(
                        &mut transaction,
                        "SELECT count(*) FROM artifacts WHERE attempt_id=$1",
                        params![id],
                    )
                    .await
                    .map_err(internal)?;
                    if complete != target.steps.len() as i64 || artifacts == 0 {
                        return Err(Status::failed_precondition(
                            "steps or verified artifacts missing",
                        ));
                    }
                }
                let error = payload.get("error").and_then(Value::as_str).unwrap_or("");
                let failure = payload
                    .get("failure_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("execution");
                if ![
                    "execution",
                    "source",
                    "authorization",
                    "artifact_transport",
                    "emissary",
                ]
                .contains(&failure)
                {
                    return Err(Status::invalid_argument("invalid failure kind"));
                }
                sql::execute(&mut transaction, "UPDATE attempts SET status=$2,error=$3,failure_kind=$4,finished_at=now() WHERE id=$1", params![id, final_status, error, failure])
                .await
                .map_err(internal)?;
                sql::execute(
                    &mut transaction,
                    "UPDATE jobs SET status=$2 WHERE id=$1",
                    params![
                        row.decode::<Uuid>("job_id").map_err(internal)?,
                        final_status
                    ],
                )
                .await
                .map_err(internal)?;
            }
            _ => return Err(Status::invalid_argument("unknown task event")),
        }
        super::models::AttemptEvent::create()
            .attempt_id(id)
            .sequence(event.sequence as i64)
            .kind(event.kind)
            .payload(toasty::Json((payload).clone()))
            .exec(&mut transaction)
            .await
            .map_err(internal)?;
        sql::execute(
            &mut transaction,
            "UPDATE attempts SET last_sequence=$2 WHERE id=$1",
            params![id, event.sequence as i64],
        )
        .await
        .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(EventReceipt::Ack(event.sequence))
    }
}
