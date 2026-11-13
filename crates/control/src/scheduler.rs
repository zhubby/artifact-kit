use crate::{
    crypto,
    protocol::{Assignment, GitCredential, Step},
    recipe::{Recipe, Target},
    source,
    state::AppState,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

pub fn start(state: AppState) {
    let notifications = state.clone();
    tokio::spawn(async move {
        loop {
            if notifications.coordination.stop.is_cancelled() {
                break;
            }
            let result = async {
                let mut listener = sqlx::postgres::PgListener::connect_with(&notifications.pool).await?;
                listener.listen("artifactory_work").await?;
                loop {
                    tokio::select! {
                        _ = notifications.coordination.stop.cancelled() => return Ok::<(), sqlx::Error>(()),
                        message = listener.recv() => {
                            message?;
                            notifications.coordination.wake.notify_one();
                            notifications.coordination.commands.notify_one();
                        }
                    }
                }
            }.await;
            if let Err(error) = result {
                tracing::warn!(%error,"control notifications reconnecting; reconciliation remains active");
            }
            tokio::select! { _=notifications.coordination.stop.cancelled()=>break, _=tokio::time::sleep(Duration::from_secs(1))=>{} }
        }
    });
    if std::env::var("ARTIFACTORY_RUNTIME_URL").is_ok() {
        let bridge = state.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = crate::runtime_bridge::deliver_intents(&bridge).await {
                    tracing::warn!(%error,"runtime intent delivery deferred");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        for _ in 0..8 {
            let resolver = state.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(error) = crate::runtime_bridge::resolve_remote_next(&resolver).await
                    {
                        tracing::warn!(%error,"remote source resolution deferred");
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
    }
    if state.settings.local_source_url.is_none() {
        start_source(state.clone());
    }
    // Source restoration can take minutes. Keep assignment preparation apart
    // from cancellation, lease expiry and scheduling, with one bounded batch.
    let assignments = state.clone();
    tokio::spawn(async move {
        loop {
            if !assignments.coordination.is_draining()
                && let Err(error) = crate::dispatch::deliver_assignments(&assignments).await
            {
                tracing::warn!(%error,"assignment delivery deferred");
            }
            tokio::select! {
                _ = assignments.coordination.stop.cancelled() => break,
                _ = assignments.coordination.commands.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
        }
    });
    // Connection/work wakeups should only accelerate job admission and command
    // delivery. Global scans run on a fixed cadence and cannot stall that path.
    let maintenance = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = maintenance.coordination.stop.cancelled() => break,
                _ = interval.tick() => {},
            }
            if !maintenance.coordination.is_draining()
                && let Err(error) = crate::dispatch::deliver_ota(&maintenance).await
            {
                tracing::warn!(%error,"OTA delivery deferred");
            }
            if let Err(error) = maintain(&maintenance).await {
                tracing::error!(%error,"scheduler maintenance failed");
            }
        }
    });
    tokio::spawn(async move {
        loop {
            if let Err(error) = dispatch_work(&state).await {
                tracing::error!(%error,"scheduler tick failed");
            }
            tokio::select! {
                _ = state.coordination.stop.cancelled() => break,
                _ = state.coordination.wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
        }
    });
}

/// Run the singleton legacy source cache and resolver on its retained volume.
pub fn start_source(state: AppState) {
    let cache = state.clone();
    tokio::spawn(async move {
        let quota = std::env::var("ARTIFACTORY_SOURCE_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(20 * 1024 * 1024 * 1024);
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Err(error) = crate::source_cache::collect(&cache, quota).await {
                tracing::warn!(%error,"source cache collection failed");
            }
        }
    });
    let resolver = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = resolve_next(&resolver).await {
                tracing::warn!(%error,"source resolution failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn resolve_next(state: &AppState) -> Result<()> {
    let mut resolver_lock = state.pool.begin().await?;
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:source-resolver',2))",
    )
    .fetch_one(&mut *resolver_lock)
    .await?;
    if !acquired {
        return Ok(());
    }
    let Some(row) = sqlx::query("SELECT b.id,b.project_id,b.requested_ref,b.parameters,b.selected_targets,b.release_version,b.goal,b.planning_context,b.model_config_snapshot,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id,p.signing_profiles FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.status='resolving' AND b.planner_backend='poll' AND NOT b.cancel_requested ORDER BY b.created_at LIMIT 1").fetch_optional(&state.pool).await? else { return Ok(()); };
    let id: Uuid = row.get("id");
    let result = async {
        if row.get::<Option<String>,_>("goal").is_some() {
            let model=row.get::<Option<Value>,_>("model_config_snapshot").ok_or_else(||anyhow::anyhow!("configure an enabled platform model profile before creating this build"))?;
            let context:Value=row.get("planning_context");
            let bindings=serde_json::from_value(context.get("source_credentials").cloned().unwrap_or(json!([])))?;
            let snapshot=crate::source_cache::capture(state,row.get("project_id"),&row.get::<String,_>("repository_url"),&row.get::<String,_>("requested_ref"),row.get("credential_id"),bindings).await?;
            let mut tx=state.pool.begin().await?;
            let status:String=sqlx::query_scalar("SELECT status FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *tx).await?;
            if status!="resolving" {return Ok(());}
            sqlx::query("UPDATE builds SET commit_sha=$2,source_snapshot_id=$3,status='planning' WHERE id=$1").bind(id).bind(snapshot.commit).bind(snapshot.id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config) VALUES($1,$2,'initial',$3)").bind(Uuid::new_v4()).bind(id).bind(model).execute(&mut *tx).await?;
            tx.commit().await?;
            return Ok(());
        }
        let auth=source::credential(state,row.get("credential_id")).await?;
        let (commit,yaml)=source::resolve(state,&row.get::<String,_>("repository_url"),&row.get::<String,_>("requested_ref"),auth).await?;
        let recipe=Recipe::parse(&yaml)?;
        recipe.environment(&row.get::<Value,_>("parameters"))?;
        let selected:Vec<String>=row.get("selected_targets");
        ensure!(selected.iter().all(|name| recipe.targets.contains_key(name)),"unknown selected target");
        let permitted:Vec<String>=row.get("signing_profiles");
        let targets:Vec<_>=recipe.targets.iter().filter(|(name,_)| selected.is_empty() || selected.contains(name)).collect();
        ensure!(!targets.is_empty(),"no selected targets");
        for (_,target) in &targets { ensure!(target.signing_profiles.iter().all(|profile| permitted.contains(profile)),"project is not authorized for signing profile"); }
        let release:Option<String>=row.get("release_version");
        ensure!(release.is_none() || recipe.release.enabled,"release is not enabled by repository configuration");
        let mut transaction=state.pool.begin().await?;
        let canceled:bool=sqlx::query_scalar("SELECT cancel_requested FROM builds WHERE id=$1 FOR UPDATE").bind(id).fetch_one(&mut *transaction).await?;
        if canceled { return Ok(()); }
        for (name,target) in targets {
            sqlx::query("INSERT INTO jobs(id,build_id,target,target_config) VALUES($1,$2,$3,$4) ON CONFLICT(build_id,target) DO NOTHING")
                .bind(Uuid::new_v4()).bind(id).bind(name).bind(serde_json::to_value(target)?).execute(&mut *transaction).await?;
        }
        sqlx::query("UPDATE builds SET commit_sha=$2,config_yaml=$3,config_snapshot=$4,status='queued' WHERE id=$1")
            .bind(id).bind(commit).bind(yaml).bind(serde_json::to_value(recipe)?).execute(&mut *transaction).await?;
        transaction.commit().await?;
        Ok::<(),anyhow::Error>(())
    }.await;
    if let Err(error) = result {
        sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1 AND status='resolving' AND NOT cancel_requested")
            .bind(id).bind(error.to_string()).execute(&state.pool).await?;
    }
    Ok(())
}

pub fn matches(
    target: &Target,
    os: &str,
    arch: &str,
    labels: &Value,
    capabilities: &Value,
    signing: &[String],
) -> bool {
    if target.os != os || target.arch != arch {
        return false;
    }
    let Some(capabilities) = capabilities.as_array() else {
        return false;
    };
    let has = |name: &str| {
        capabilities
            .iter()
            .any(|entry| entry.as_str() == Some(name))
    };
    has("git")
        && target.capabilities.iter().all(|name| has(name))
        && target.steps.iter().all(|step| {
            has(step.shell.as_deref().unwrap_or(if os == "windows" {
                "powershell"
            } else {
                "bash"
            }))
        })
        && target
            .labels
            .iter()
            .all(|(key, value)| labels.get(key).and_then(Value::as_str) == Some(value))
        && target
            .signing_profiles
            .iter()
            .all(|name| signing.contains(name))
}

/// Reconcile job admission, command cancellation, OTA and global maintenance.
///
/// # Errors
/// Returns database or preparation failures; durable work remains retryable.
pub async fn tick(state: &AppState) -> Result<()> {
    if !state.coordination.is_draining() {
        crate::dispatch::deliver_ota(state).await?;
    }
    dispatch_work(state).await?;
    maintain(state).await
}

async fn dispatch_work(state: &AppState) -> Result<()> {
    if !state.coordination.is_draining() {
        schedule(state).await?;
        crate::dispatch::deliver_cancellations(state).await?;
    }
    Ok(())
}

async fn maintain(state: &AppState) -> Result<()> {
    // Only one replica performs housekeeping at a time. This transaction holds
    // the advisory lock, while each operation retains its existing short commits.
    let mut guard = state.pool.begin().await?;
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtextextended('artifactory:maintenance',12))",
    )
    .fetch_one(&mut *guard)
    .await?;
    if !acquired {
        return Ok(());
    }
    crate::planning::reap(state).await?;
    expire(state).await?;
    refresh_builds(state).await?;
    queue_revisions(state).await?;
    sqlx::query("DELETE FROM device_commands WHERE id IN (SELECT id FROM device_commands WHERE acknowledged_at<now()-interval '7 days' ORDER BY acknowledged_at LIMIT 1000)")
        .execute(&state.pool).await?;
    let pending: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM builds WHERE status='succeeded' AND publish_status='pending' AND NOT cancel_requested LIMIT 10")
        .fetch_all(&state.pool).await?;
    for id in pending {
        let _ = publish_build(state, id).await;
    }
    guard.commit().await?;
    Ok(())
}

pub async fn expire(state: &AppState) -> Result<()> {
    let mut transaction = state.pool.begin().await?;
    let rows=sqlx::query("SELECT a.id,a.job_id,a.status,b.cancel_requested FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.status IN ('leased','running') AND a.lease_expires_at<=now() FOR UPDATE OF a,j SKIP LOCKED")
        .fetch_all(&mut *transaction).await?;
    for row in rows {
        let started = row.get::<String, _>("status") == "running";
        let canceled: bool = row.get("cancel_requested");
        sqlx::query("UPDATE attempts SET status=$2,error='execution lease expired',finished_at=now() WHERE id=$1")
            .bind(row.get::<Uuid,_>("id")).bind(if canceled {"canceled"} else {"lost"}).execute(&mut *transaction).await?;
        sqlx::query("UPDATE jobs SET status=$2 WHERE id=$1")
            .bind(row.get::<Uuid, _>("job_id"))
            .bind(if canceled {
                "canceled"
            } else if started {
                "lost"
            } else {
                "queued"
            })
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Match queued jobs to available nodes, without visiting idle connections.
///
/// # Errors
/// Returns database or invalid persisted target errors.
pub async fn schedule(state: &AppState) -> Result<()> {
    for _ in 0..state.settings.control.scheduler_batch {
        if state.coordination.is_draining() {
            break;
        }
        let mut tx = state.pool.begin().await?;
        let candidate = sqlx::query("SELECT j.id,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.goal,b.planning_context,p.pool_ids,p.signing_profiles AS permitted_signing FROM jobs j JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE j.status='queued' AND j.dispatch_after<=clock_timestamp() AND b.status IN ('queued','running') AND NOT b.cancel_requested ORDER BY j.dispatch_after,j.created_at,j.id LIMIT 1 FOR UPDATE OF b,j SKIP LOCKED")
            .fetch_optional(&mut *tx).await?;
        let Some(candidate) = candidate else {
            break;
        };
        let job: Uuid = candidate.get("id");
        let target: Target = serde_json::from_value(candidate.get("target_config"))?;
        let permitted: Vec<String> = candidate.get("permitted_signing");
        let mut pools: Vec<Uuid> = candidate.get("pool_ids");
        let mut capabilities = target.capabilities.clone();
        capabilities.push("git".into());
        capabilities.extend(target.steps.iter().map(|s| {
            s.shell.clone().unwrap_or_else(|| {
                if target.os == "windows" {
                    "powershell".into()
                } else {
                    "bash".into()
                }
            })
        }));
        if candidate.get::<Option<String>, _>("goal").is_some() {
            capabilities.extend(["source-cache-v1".into(), "external-build-plan-v1".into()]);
            let context: Value = candidate.get("planning_context");
            let allowed: Vec<Uuid> =
                serde_json::from_value(context.get("pool_ids").cloned().unwrap_or(json!([])))?;
            pools.retain(|pool| allowed.contains(pool));
        }
        let node = if target
            .signing_profiles
            .iter()
            .all(|profile| permitted.contains(profile))
        {
            sqlx::query("SELECT n.id FROM nodes n WHERE n.pool_id=ANY($1) AND n.os=$2 AND n.arch=$3 AND n.labels @> $4 AND n.capabilities @> $5 AND $6::text[] <@ n.signing_profiles AND n.active_session IS NOT NULL AND n.connection_owner IS NOT NULL AND NOT n.revoked AND NOT n.draining AND NOT n.ota_draining AND n.certificate_expires_at>clock_timestamp() AND n.last_seen>clock_timestamp()-interval '30 seconds' AND n.slots>(SELECT count(*) FROM attempts a WHERE a.node_id=n.id AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp()) ORDER BY n.id LIMIT 1 FOR UPDATE OF n SKIP LOCKED")
                .bind(&pools).bind(&target.os).bind(&target.arch).bind(serde_json::to_value(&target.labels)?).bind(json!(capabilities)).bind(&target.signing_profiles)
                .fetch_optional(&mut *tx).await?
        } else {
            None
        };
        let node = if let Some(node) = node {
            // Use a new READ COMMITTED snapshot after acquiring the node lock.
            // Another assignment may have committed after the selection began.
            let valid: bool = sqlx::query_scalar("SELECT active_session IS NOT NULL AND connection_owner IS NOT NULL AND NOT revoked AND NOT draining AND NOT ota_draining AND certificate_expires_at>clock_timestamp() AND last_seen>clock_timestamp()-interval '30 seconds' AND slots>(SELECT count(*) FROM attempts WHERE node_id=$1 AND status IN ('leased','running') AND lease_expires_at>clock_timestamp()) FROM nodes WHERE id=$1")
                .bind(node.get::<Uuid,_>("id")).fetch_one(&mut *tx).await?;
            valid.then_some(node)
        } else {
            None
        };
        if let Some(node) = node {
            // The node row serializes slots and OTA admission across replicas.
            sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id,plan_revision_id,source_snapshot_id) VALUES($1,$2,$3,$4,clock_timestamp()+interval '90 seconds','leased',$5,$6,$7)")
                .bind(Uuid::new_v4()).bind(job).bind(node.get::<Uuid,_>("id")).bind(crypto::token()).bind(Uuid::new_v4()).bind(candidate.get::<Option<Uuid>,_>("plan_revision_id")).bind(candidate.get::<Option<Uuid>,_>("source_snapshot_id"))
                .execute(&mut *tx).await?;
            sqlx::query("UPDATE jobs SET status='leased' WHERE id=$1")
                .bind(job)
                .execute(&mut *tx)
                .await?;
        } else {
            // Rotate incompatible/saturated work so the oldest platform cannot
            // starve later jobs. Periodic reconciliation bounds lost wakeups.
            sqlx::query(
                "UPDATE jobs SET dispatch_after=clock_timestamp()+interval '5 seconds' WHERE id=$1",
            )
            .bind(job)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

pub(crate) async fn assignment(state: &AppState, job: Uuid, attempt: Uuid) -> Result<Assignment> {
    let row=sqlx::query("SELECT j.target,j.target_config,j.plan_revision_id,b.source_snapshot_id,b.id,b.commit_sha,b.config_snapshot,b.parameters,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1")
        .bind(job).fetch_one(&state.pool).await?;
    let target: Target = serde_json::from_value(row.get("target_config"))?;
    let plan_id: Option<Uuid> = row.get("plan_revision_id");
    let plan = if let Some(id) = plan_id {
        Some(crate::execution_plan::ExecutionPlan::parse(
            sqlx::query_scalar("SELECT contents FROM plan_revisions WHERE id=$1")
                .bind(id)
                .fetch_one(&state.pool)
                .await?,
        )?)
    } else {
        None
    };
    let recipe: Recipe = if let Some(plan) = &plan {
        plan.recipe.clone()
    } else {
        serde_json::from_value(row.get("config_snapshot"))?
    };
    let mut environment = recipe.environment(&row.get::<Value, _>("parameters"))?;
    environment.insert("ARTIFACTORY_COMMIT_SHA".into(), row.get("commit_sha"));
    environment.insert("ARTIFACTORY_TARGET".into(), row.get("target"));
    environment.insert(
        "ARTIFACTORY_BUILD_ID".into(),
        row.get::<Uuid, _>("id").to_string(),
    );
    let source_descriptor = if let Some(id) = row.get::<Option<Uuid>, _>("source_snapshot_id") {
        Some(crate::source_cache::descriptor(state, id, attempt).await?)
    } else {
        None
    };
    let credential = source::credential(
        state,
        if source_descriptor.is_none() {
            row.get("credential_id")
        } else {
            None
        },
    )
    .await?
    .map(|auth| GitCredential {
        kind: auth.kind,
        username: auth.username,
        secret: auth.secret,
        known_hosts: auth.known_hosts,
    });
    Ok(Assignment {
        job_id: job.to_string(),
        build_id: row.get::<Uuid, _>("id").to_string(),
        target: row.get("target"),
        repository_url: row.get("repository_url"),
        commit_sha: row.get("commit_sha"),
        steps: target
            .steps
            .into_iter()
            .map(|step| Step {
                name: step.name,
                phase: step.phase,
                shell: step.shell.unwrap_or_else(|| {
                    if target.os == "windows" {
                        "powershell"
                    } else {
                        "bash"
                    }
                    .into()
                }),
                run: step.run,
                timeout_seconds: step.timeout_seconds,
            })
            .collect(),
        artifacts: target.artifacts,
        environment: environment.into_iter().collect(),
        signing_profiles: target.signing_profiles,
        git_credential: credential,
        timeout_seconds: target.timeout_seconds,
        source: source_descriptor,
        plan_revision_id: plan_id.map(|id| id.to_string()).unwrap_or_default(),
        scripts: plan.map(|plan| plan.scripts()).unwrap_or_default(),
    })
}

pub async fn refresh_builds(state: &AppState) -> Result<()> {
    sqlx::query("WITH summaries AS (SELECT b.id, CASE WHEN b.cancel_requested THEN CASE WHEN bool_or(j.status IN ('leased','running')) THEN 'canceling' ELSE 'canceled' END WHEN bool_or(j.status='running') THEN 'running' WHEN bool_or(j.status IN ('queued','leased')) THEN 'queued' WHEN bool_or(j.status='failed') THEN 'failed' WHEN bool_or(j.status='lost') THEN 'lost' WHEN bool_or(j.status='canceled') THEN 'canceled' ELSE 'succeeded' END AS status FROM builds b JOIN jobs j ON j.build_id=b.id WHERE b.status IN ('queued','running','canceling') GROUP BY b.id) UPDATE builds b SET status=s.status,finished_at=CASE WHEN s.status IN ('succeeded','failed','lost','canceled') THEN coalesce(b.finished_at,now()) ELSE NULL END FROM summaries s WHERE b.id=s.id AND b.status IS DISTINCT FROM s.status")
        .execute(&state.pool).await?;
    Ok(())
}

pub async fn queue_revisions(state: &AppState) -> Result<()> {
    let mut tx = state.pool.begin().await?;
    let rows=sqlx::query("SELECT id,model_config_snapshot FROM builds b WHERE goal IS NOT NULL AND current_plan_id IS NOT NULL AND status='failed' AND NOT cancel_requested AND revision_count<2 AND (SELECT state FROM planning_runs WHERE build_id=b.id ORDER BY created_at DESC LIMIT 1)='succeeded' AND EXISTS(SELECT 1 FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=b.id AND j.status='failed' AND a.failure_kind='execution') ORDER BY created_at FOR UPDATE SKIP LOCKED LIMIT 10")
        .fetch_all(&mut *tx).await?;
    for row in rows {
        let id: Uuid = row.get("id");
        let failed:Vec<String>=sqlx::query_scalar("SELECT j.target FROM jobs j JOIN LATERAL (SELECT failure_kind FROM attempts WHERE job_id=j.id ORDER BY created_at DESC LIMIT 1) a ON true WHERE j.build_id=$1 AND j.status='failed' AND a.failure_kind='execution'").bind(id).fetch_all(&mut *tx).await?;
        sqlx::query("UPDATE builds SET revision_count=revision_count+1,status='replanning',finished_at=NULL WHERE id=$1").bind(id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO planning_runs(id,build_id,purpose,model_config,failed_targets) VALUES($1,$2,'revision',$3,$4)").bind(Uuid::new_v4()).bind(id).bind(row.get::<Value,_>("model_config_snapshot")).bind(failed).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn publish_build(state: &AppState, id: Uuid) -> Result<Uuid> {
    let result = publish_transaction(state, id).await;
    match &result {
        Ok(release) => {
            state
                .audit(
                    "publisher",
                    "release_published",
                    release,
                    json!({"build_id":id}),
                )
                .await?;
        }
        Err(error) => {
            sqlx::query("UPDATE builds SET publish_status='failed',publish_error=$2 WHERE id=$1 AND publish_status<>'succeeded'").bind(id).bind(error.to_string()).execute(&state.pool).await?;
            sqlx::query(
                "INSERT INTO publish_attempts(id,build_id,status,error) VALUES($1,$2,'failed',$3)",
            )
            .bind(Uuid::new_v4())
            .bind(id)
            .bind(error.to_string())
            .execute(&state.pool)
            .await?;
        }
    }
    result
}

async fn publish_transaction(state: &AppState, id: Uuid) -> Result<Uuid> {
    let mut transaction = state.pool.begin().await?;
    let build = sqlx::query("SELECT * FROM builds WHERE id=$1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    ensure!(
        build.get::<String, _>("status") == "succeeded"
            && !build.get::<bool, _>("cancel_requested"),
        "only a successful build can be published"
    );
    let version: Option<String> = build.get("release_version");
    let version = version.ok_or_else(|| anyhow::anyhow!("release version is required"))?;
    let recipe: Recipe = serde_json::from_value(build.get("config_snapshot"))?;
    ensure!(
        recipe.release.enabled,
        "publishing is disabled by repository configuration"
    );
    let key = format!("build:{id}");
    let project: Uuid = build.get("project_id");
    if let Some(existing) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM releases WHERE project_id=$1 AND idempotency_key=$2",
    )
    .bind(project)
    .bind(&key)
    .fetch_optional(&mut *transaction)
    .await?
    {
        return Ok(existing);
    }
    let artifacts:Vec<Uuid>=sqlx::query_scalar("SELECT ar.id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND j.status='succeeded' AND a.status='succeeded' AND a.plan_revision_id IS NOT DISTINCT FROM j.plan_revision_id AND a.source_snapshot_id IS NOT DISTINCT FROM (SELECT source_snapshot_id FROM builds WHERE id=$1) AND a.id=(SELECT id FROM attempts WHERE job_id=j.id ORDER BY created_at DESC,id DESC LIMIT 1)").bind(id).fetch_all(&mut *transaction).await?;
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE build_id=$1")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    let complete:i64=sqlx::query_scalar("SELECT count(DISTINCT j.id) FROM jobs j JOIN attempts a ON a.job_id=j.id AND a.status='succeeded' JOIN artifacts ar ON ar.attempt_id=a.id WHERE j.build_id=$1 AND j.status='succeeded' AND ar.id=ANY($2)").bind(id).bind(&artifacts).fetch_one(&mut *transaction).await?;
    ensure!(
        jobs > 0 && jobs == complete,
        "every target must have verified artifacts"
    );
    let release = Uuid::new_v4();
    sqlx::query("INSERT INTO releases(id,project_id,build_id,version,idempotency_key,commit_sha) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(release).bind(project).bind(id).bind(version).bind(key).bind(build.get::<String,_>("commit_sha")).execute(&mut *transaction).await.context("release version already exists or publication failed")?;
    for artifact in artifacts {
        sqlx::query("INSERT INTO release_artifacts VALUES($1,$2)")
            .bind(release)
            .bind(artifact)
            .execute(&mut *transaction)
            .await?;
    }
    sqlx::query("UPDATE builds SET publish_status='succeeded',publish_error=NULL WHERE id=$1")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    sqlx::query("INSERT INTO publish_attempts(id,build_id,status) VALUES($1,$2,'succeeded')")
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(release)
}

use anyhow::Context;
