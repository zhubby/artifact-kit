//! Node identity, execution leases, events and upload persistence.
use super::ControlStore;
use crate::{crypto, pki};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::Value;
use sqlx::Row;
use tonic::Status;
use uuid::Uuid;
fn internal(error: impl std::fmt::Display) -> Status {
    crate::grpc::internal(error)
}
const VALID_ATTEMPT_QUERY: &str = "SELECT EXISTS(SELECT 1 FROM attempts a JOIN nodes n ON n.id=a.node_id JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status='running' AND a.lease_expires_at>clock_timestamp() AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() AND ($4 OR NOT b.cancel_requested))";

async fn valid_node<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp())")
        .bind(node).bind(session).fetch_one(executor).await.map_err(internal)?;
    if valid {
        Ok(())
    } else {
        Err(Status::unauthenticated(
            "node session revoked or superseded",
        ))
    }
}

async fn lock_authorized_node(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: Uuid,
    session: Uuid,
) -> Result<(), Status> {
    // Acquire this after the attempt lock, then validate with a fresh clock and
    // snapshot. Keep revocation/session replacement serialized with the write.
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR SHARE")
        .bind(node)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    valid_node(&mut **transaction, node, session).await
}

async fn valid_attempt<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    node: Uuid,
    attempt: &str,
    token: &str,
    allow_canceled: bool,
) -> Result<Uuid, Status> {
    let attempt =
        Uuid::parse_str(attempt).map_err(|_| Status::invalid_argument("invalid attempt ID"))?;
    let valid: bool = sqlx::query_scalar(VALID_ATTEMPT_QUERY)
        .bind(attempt)
        .bind(node)
        .bind(token)
        .bind(allow_canceled)
        .fetch_one(executor)
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
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    node: Uuid,
    attempt: Uuid,
    token: &str,
) -> Result<(), Status> {
    // Shared locks coexist with insertion's foreign-key KEY SHARE lock. Check
    // again after waiting; hold authorization stable through the short commit.
    sqlx::query("SELECT id FROM attempts WHERE id=$1 FOR SHARE")
        .bind(attempt)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR SHARE")
        .bind(node)
        .execute(&mut **transaction)
        .await
        .map_err(internal)?;
    valid_attempt(&mut **transaction, node, &attempt.to_string(), token, false).await?;
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

impl ControlStore {
    pub(crate) async fn node_identity(&self, hash: String) -> Result<Uuid, Status> {
        sqlx::query_scalar("SELECT id FROM nodes WHERE certificate_hash=$1 AND NOT revoked AND certificate_expires_at>now()")
        .bind(hash).fetch_optional(&self.pool).await.map_err(internal)?
        .ok_or_else(|| Status::unauthenticated("node certificate revoked, expired or unknown"))
    }

    pub(crate) async fn validate_node_session(
        &self,
        node: Uuid,
        session: Uuid,
    ) -> Result<(), Status> {
        valid_node(&self.pool, node, session).await
    }

    pub(crate) async fn validate_attempt(
        &self,
        node: Uuid,
        attempt: &str,
        token: &str,
        canceled: bool,
    ) -> Result<Uuid, Status> {
        valid_attempt(&self.pool, node, attempt, token, canceled).await
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
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        let token=sqlx::query("SELECT id,pool_id FROM enrollment_tokens WHERE token_hash=$1 AND used_at IS NULL AND expires_at>now() FOR UPDATE")
            .bind(crypto::hash(input.token)).fetch_optional(&mut *transaction).await.map_err(internal)?
            .ok_or_else(|| Status::unauthenticated("invalid or expired enrollment token"))?;
        let id = Uuid::new_v4();
        let certificate = issue(id).await?;
        let hash = pki::certificate_hash(&certificate).map_err(internal)?;
        sqlx::query("INSERT INTO nodes(id,name,os,arch,pool_id,certificate_hash,certificate_expires_at) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(id).bind(input.name).bind(input.os).bind(input.arch).bind(token.get::<Uuid,_>("pool_id")).bind(hash).bind(Utc::now()+ChronoDuration::days(30)).execute(&mut *transaction).await.map_err(internal)?;
        sqlx::query("UPDATE enrollment_tokens SET used_at=now() WHERE id=$1")
            .bind(token.get::<Uuid, _>("id"))
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(EnrolledNode { id, certificate })
    }

    pub(crate) async fn connect_node(&self, node: Uuid, session: Uuid) -> Result<(), Status> {
        sqlx::query("UPDATE nodes SET active_session=$2,last_seen=now() WHERE id=$1")
            .bind(node)
            .bind(session)
            .execute(&self.pool)
            .await
            .map_err(internal)?;
        Ok(())
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
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        sqlx::query("SELECT id FROM attempts WHERE id=$1 FOR UPDATE")
            .bind(attempt)
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        let valid: bool = sqlx::query_scalar(VALID_ATTEMPT_QUERY)
            .bind(attempt)
            .bind(node)
            .bind(input.token)
            .bind(true)
            .fetch_one(&mut *transaction)
            .await
            .map_err(internal)?;
        if !valid {
            return Err(Status::failed_precondition(
                "execution lease is no longer valid",
            ));
        }
        if let Some(existing) = sqlx::query_scalar::<_, String>(
            "SELECT sha256 FROM log_chunks WHERE attempt_id=$1 AND sequence=$2",
        )
        .bind(attempt)
        .bind(input.sequence as i64)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(internal)?
        {
            if existing != hash {
                return Err(Status::already_exists("log sequence content mismatch"));
            }
        } else {
            let previous: i64 = sqlx::query_scalar(
                "SELECT coalesce(max(sequence),0) FROM log_chunks WHERE attempt_id=$1",
            )
            .bind(attempt)
            .fetch_one(&mut *transaction)
            .await
            .map_err(internal)?;
            if input.sequence != previous as u64 + 1 {
                return Err(Status::failed_precondition(format!(
                    "expected log sequence {}",
                    previous + 1
                )));
            }
            sqlx::query("INSERT INTO log_chunks(attempt_id,sequence,step,object_key,sha256,size) VALUES($1,$2,$3,$4,$5,$6)")
                    .bind(attempt).bind(input.sequence as i64).bind(input.step).bind(key).bind(hash).bind(size).execute(&mut *transaction).await.map_err(internal)?;
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
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        sqlx::query("INSERT INTO uploads(id,attempt_id,path,object_key,expected_size,expected_sha256,expires_at) VALUES($1,$2,$3,$4,$5,$6,now()+interval '15 minutes') ON CONFLICT(attempt_id,path) DO NOTHING")
            .bind(id).bind(attempt).bind(input.path).bind(key).bind(input.size as i64).bind(input.sha256).execute(&mut *transaction).await.map_err(internal)?;
        let row=sqlx::query("SELECT u.*,EXISTS(SELECT 1 FROM artifacts a WHERE a.id=u.id) AS completed FROM uploads u WHERE attempt_id=$1 AND path=$2").bind(attempt).bind(input.path).fetch_one(&mut *transaction).await.map_err(internal)?;
        if row.get::<i64, _>("expected_size") != input.size as i64
            || row.get::<String, _>("expected_sha256") != input.sha256
        {
            return Err(Status::already_exists(
                "artifact path already declared with different content",
            ));
        }
        lock_artifact_attempt(&mut transaction, node, attempt, input.lease_token).await?;
        transaction.commit().await.map_err(internal)?;
        Ok(PreparedUpload {
            id: row.get("id"),
            object_key: row.get("object_key"),
            completed: row.get("completed"),
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
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        let row = sqlx::query("SELECT * FROM uploads WHERE id=$1 AND attempt_id=$2 FOR UPDATE")
            .bind(id)
            .bind(attempt)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("upload not found"))?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifacts WHERE id=$1)")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await
            .map_err(internal)?;
        let final_key = format!("artifacts/{id}");
        if !exists {
            promote(UploadVerification {
                object_key: row.get("object_key"),
                final_key: final_key.clone(),
                expected_size: row.get("expected_size"),
                expected_sha256: row.get("expected_sha256"),
            })
            .await?;
        }
        // Network verification completes before taking the attempt/node locks,
        // so a slow S3 request does not block heartbeat renewal.
        lock_artifact_attempt(&mut transaction, node, attempt, token).await?;
        if !exists {
            sqlx::query("INSERT INTO artifacts(id,attempt_id,path,object_key,size,sha256) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(id).bind(attempt).bind(row.get::<String,_>("path")).bind(final_key).bind(row.get::<i64,_>("expected_size")).bind(row.get::<String,_>("expected_sha256")).execute(&mut *transaction).await.map_err(internal)?;
        }
        transaction.commit().await.map_err(internal)?;
        Ok(CompletedArtifact {
            id,
            path: row.get("path"),
            sha256: row.get("expected_sha256"),
            size: row.get::<i64, _>("expected_size") as u64,
        })
    }

    pub(crate) async fn record_heartbeat(
        &self,
        node: Uuid,
        session: Uuid,
        capabilities: Value,
        diagnostics: Value,
    ) -> Result<(), Status> {
        sqlx::query("UPDATE nodes SET last_seen=now(),capabilities=$3,diagnostics=$4 WHERE id=$1 AND active_session=$2")
        .bind(node).bind(session).bind(capabilities).bind(diagnostics).execute(&self.pool).await.map_err(internal)?;
        Ok(())
    }

    pub(crate) async fn renew_attempt(
        &self,
        node: Uuid,
        session: Uuid,
        id: Uuid,
        token: &str,
    ) -> Result<bool, Status> {
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        sqlx::query("SELECT id FROM attempts WHERE id=$1 AND node_id=$2 FOR UPDATE")
            .bind(id)
            .bind(node)
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        lock_authorized_node(&mut transaction, node, session).await?;
        let row=sqlx::query("UPDATE attempts a SET lease_expires_at=clock_timestamp()+interval '90 seconds' FROM jobs j,builds b,nodes n WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp() AND j.id=a.job_id AND b.id=j.build_id AND NOT b.cancel_requested AND n.id=a.node_id AND n.active_session=$4 AND NOT n.revoked AND n.certificate_expires_at>clock_timestamp() RETURNING a.status")
                .bind(id).bind(node).bind(token).bind(session).fetch_optional(&mut *transaction).await.map_err(internal)?;
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
        let mut transaction = self.pool.begin().await.map_err(internal)?;
        let row=sqlx::query("SELECT a.*,j.target_config FROM attempts a JOIN jobs j ON j.id=a.job_id WHERE a.id=$1 AND a.node_id=$2 AND a.lease_token=$3 AND a.lease_expires_at>now() FOR UPDATE OF a,j")
        .bind(id).bind(node).bind(event.lease_token).fetch_optional(&mut *transaction).await.map_err(internal)?
        .ok_or_else(|| Status::failed_precondition("attempt lease expired or unknown"))?;
        lock_authorized_node(&mut transaction, node, session).await?;
        // Cancellation can commit while either row lock waits. Read it with the
        // current lease clock after both waits, rather than using the old snapshot.
        let current = sqlx::query("SELECT a.lease_expires_at>clock_timestamp() AS lease_valid,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.id=$1")
        .bind(id).fetch_one(&mut *transaction).await.map_err(internal)?;
        if !current.get::<bool, _>("lease_valid") {
            return Err(Status::failed_precondition(
                "attempt lease expired while waiting for its lock",
            ));
        }
        let canceled: bool = current.get("cancel_requested");
        let status: String = row.get("status");
        if event.kind == "accept" {
            if !["leased", "running"].contains(&status.as_str()) || canceled {
                return Err(Status::failed_precondition("attempt cannot start"));
            }
            sqlx::query("UPDATE attempts SET status='running',started_at=coalesce(started_at,now()),lease_expires_at=now()+interval '90 seconds' WHERE id=$1").bind(id).execute(&mut *transaction).await.map_err(internal)?;
            sqlx::query("UPDATE jobs SET status='running' WHERE id=$1")
                .bind(row.get::<Uuid, _>("job_id"))
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
            transaction.commit().await.map_err(internal)?;
            return Ok(EventReceipt::Start);
        }
        let previous: i64 = row.get("last_sequence");
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
            serde_json::from_value(row.get("target_config")).map_err(internal)?;
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
                let updated=sqlx::query("UPDATE source_repositories r SET emissary_cache_hits=emissary_cache_hits+CASE WHEN $2 THEN 1 ELSE 0 END,emissary_cache_misses=emissary_cache_misses+CASE WHEN $2 THEN 0 ELSE 1 END WHERE r.id IN (SELECT (entry->>'repository_id')::uuid FROM source_snapshots s CROSS JOIN LATERAL jsonb_array_elements(s.manifest->'repositories') entry JOIN attempts a ON a.source_snapshot_id=s.id WHERE a.id=$1 AND entry->>'path'=$3 AND entry->>'commit'=$4)")
                .bind(id).bind(payload["hit"].as_bool().unwrap()).bind(payload["path"].as_str().unwrap()).bind(payload["commit"].as_str().unwrap()).execute(&mut *transaction).await.map_err(internal)?;
                if updated.rows_affected() != 1 {
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
                let complete: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
                )
                .bind(id)
                .fetch_one(&mut *transaction)
                .await
                .map_err(internal)?;
                if complete != index as i64 {
                    return Err(Status::failed_precondition("previous steps not complete"));
                }
                sqlx::query("INSERT INTO step_runs(attempt_id,step_index,name,phase,status) VALUES($1,$2,$3,$4,'running')")
                .bind(id).bind(index as i32).bind(&step.name).bind(&step.phase).execute(&mut *transaction).await.map_err(internal)?;
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
                let changed=sqlx::query("UPDATE step_runs SET status=$3,exit_code=$4,finished_at=now() WHERE attempt_id=$1 AND step_index=$2 AND status='running'")
                .bind(id).bind(index as i32).bind(if exit==0 {"succeeded"} else {"failed"}).bind(exit as i32).execute(&mut *transaction).await.map_err(internal)?;
                if changed.rows_affected() != 1 {
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
                    let complete: i64 = sqlx::query_scalar(
                        "SELECT count(*) FROM step_runs WHERE attempt_id=$1 AND status='succeeded'",
                    )
                    .bind(id)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(internal)?;
                    let artifacts: i64 =
                        sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE attempt_id=$1")
                            .bind(id)
                            .fetch_one(&mut *transaction)
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
                sqlx::query("UPDATE attempts SET status=$2,error=$3,failure_kind=$4,finished_at=now() WHERE id=$1")
                .bind(id)
                .bind(final_status)
                .bind(error)
                .bind(failure)
                .execute(&mut *transaction)
                .await
                .map_err(internal)?;
                sqlx::query("UPDATE jobs SET status=$2 WHERE id=$1")
                    .bind(row.get::<Uuid, _>("job_id"))
                    .bind(final_status)
                    .execute(&mut *transaction)
                    .await
                    .map_err(internal)?;
            }
            _ => return Err(Status::invalid_argument("unknown task event")),
        }
        sqlx::query(
            "INSERT INTO attempt_events(attempt_id,sequence,kind,payload) VALUES($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(event.sequence as i64)
        .bind(event.kind)
        .bind(payload)
        .execute(&mut *transaction)
        .await
        .map_err(internal)?;
        sqlx::query("UPDATE attempts SET last_sequence=$2 WHERE id=$1")
            .bind(id)
            .bind(event.sequence as i64)
            .execute(&mut *transaction)
            .await
            .map_err(internal)?;
        transaction.commit().await.map_err(internal)?;
        Ok(EventReceipt::Ack(event.sequence))
    }
}
