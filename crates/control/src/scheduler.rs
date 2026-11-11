use crate::{
    crypto,
    protocol::{Assignment, Command, GitCredential, Step},
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
    let resolver = state.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = resolve_next(&resolver).await {
                tracing::warn!(%error,"source resolution failed");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    tokio::spawn(async move {
        loop {
            if let Err(error) = tick(&state).await {
                tracing::error!(%error,"scheduler tick failed");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn resolve_next(state: &AppState) -> Result<()> {
    let Some(row) = sqlx::query("SELECT b.id,b.requested_ref,b.parameters,b.selected_targets,b.release_version,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id,p.signing_profiles FROM builds b JOIN projects p ON p.id=b.project_id WHERE b.status='resolving' AND NOT b.cancel_requested ORDER BY b.created_at LIMIT 1").fetch_optional(&state.pool).await? else { return Ok(()); };
    let id: Uuid = row.get("id");
    let result = async {
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
        sqlx::query("UPDATE builds SET status='failed',error=$2,finished_at=now() WHERE id=$1 AND NOT cancel_requested")
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

pub async fn tick(state: &AppState) -> Result<()> {
    expire(state).await?;
    let connections = state.connections.read().await.clone();
    for (node_id, connection) in connections {
        if let Some(command) = crate::ota::next(state, node_id, connection.session).await? {
            let _ = connection.sender.try_send(Ok(command));
        }
        if let Some(command) = assign(state, node_id, connection.session).await? {
            let _ =
                tokio::time::timeout(Duration::from_secs(2), connection.sender.send(Ok(command)))
                    .await;
        }
        let canceled=sqlx::query("SELECT a.id,a.lease_token FROM attempts a JOIN jobs j ON j.id=a.job_id JOIN builds b ON b.id=j.build_id WHERE a.node_id=$1 AND a.status IN ('leased','running') AND b.cancel_requested")
            .bind(node_id).fetch_all(&state.pool).await?;
        for attempt in canceled {
            let command = Command {
                command_id: Uuid::new_v4().to_string(),
                kind: "cancel".into(),
                attempt_id: attempt.get::<Uuid, _>("id").to_string(),
                lease_token: attempt.get("lease_token"),
                ..Default::default()
            };
            let _ = connection.sender.try_send(Ok(command));
        }
    }
    refresh_builds(state).await?;
    let pending:Vec<Uuid>=sqlx::query_scalar("SELECT id FROM builds WHERE status='succeeded' AND publish_status='pending' AND NOT cancel_requested LIMIT 10").fetch_all(&state.pool).await?;
    for id in pending {
        let _ = publish_build(state, id).await;
    }
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

async fn assign(state: &AppState, node_id: Uuid, session: Uuid) -> Result<Option<Command>> {
    let mut transaction = state.pool.begin().await?;
    let Some(node)=sqlx::query("SELECT * FROM nodes WHERE id=$1 AND active_session=$2 AND NOT revoked AND NOT draining AND NOT ota_draining AND certificate_expires_at>clock_timestamp() AND last_seen>now()-interval '30 seconds' FOR UPDATE SKIP LOCKED")
        .bind(node_id).bind(session).fetch_optional(&mut *transaction).await? else { return Ok(None); };
    let active:i64=sqlx::query_scalar("SELECT count(*) FROM attempts WHERE node_id=$1 AND status IN ('leased','running') AND lease_expires_at>now()")
        .bind(node_id).fetch_one(&mut *transaction).await?;
    if active >= node.get::<i32, _>("slots") as i64 {
        return Ok(None);
    }
    let candidates=sqlx::query("SELECT j.id,j.target_config,p.signing_profiles AS permitted_signing FROM jobs j JOIN builds b ON b.id=j.build_id JOIN projects p ON p.id=b.project_id WHERE j.status='queued' AND NOT b.cancel_requested AND $1=ANY(p.pool_ids) ORDER BY j.created_at FOR UPDATE OF j SKIP LOCKED LIMIT 128")
        .bind(node.get::<Uuid,_>("pool_id")).fetch_all(&mut *transaction).await?;
    for candidate in candidates {
        let target: Target = serde_json::from_value(candidate.get("target_config"))?;
        let permitted: Vec<String> = candidate.get("permitted_signing");
        if !target
            .signing_profiles
            .iter()
            .all(|profile| permitted.contains(profile))
        {
            continue;
        }
        if !matches(
            &target,
            &node.get::<String, _>("os"),
            &node.get::<String, _>("arch"),
            &node.get("labels"),
            &node.get("capabilities"),
            &node.get::<Vec<String>, _>("signing_profiles"),
        ) {
            continue;
        }
        let job: Uuid = candidate.get("id");
        let attempt = Uuid::new_v4();
        let command_id = Uuid::new_v4();
        let lease_token = crypto::token();
        sqlx::query("INSERT INTO attempts(id,job_id,node_id,lease_token,lease_expires_at,status,command_id) VALUES($1,$2,$3,$4,now()+interval '90 seconds','leased',$5)")
            .bind(attempt).bind(job).bind(node_id).bind(&lease_token).bind(command_id).execute(&mut *transaction).await?;
        sqlx::query("UPDATE jobs SET status='leased' WHERE id=$1")
            .bind(job)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        let assignment = assignment(state, job).await?;
        return Ok(Some(Command {
            command_id: command_id.to_string(),
            kind: "assign".into(),
            attempt_id: attempt.to_string(),
            lease_token,
            lease_seconds: 90,
            assignment: Some(assignment),
            ..Default::default()
        }));
    }
    Ok(None)
}

async fn assignment(state: &AppState, job: Uuid) -> Result<Assignment> {
    let row=sqlx::query("SELECT j.target,j.target_config,b.id,b.commit_sha,b.config_snapshot,b.parameters,b.source_repository_url AS repository_url,b.source_credential_id AS credential_id FROM jobs j JOIN builds b ON b.id=j.build_id WHERE j.id=$1")
        .bind(job).fetch_one(&state.pool).await?;
    let target: Target = serde_json::from_value(row.get("target_config"))?;
    let recipe: Recipe = serde_json::from_value(row.get("config_snapshot"))?;
    let mut environment = recipe.environment(&row.get::<Value, _>("parameters"))?;
    environment.insert("AK_COMMIT_SHA".into(), row.get("commit_sha"));
    environment.insert("AK_TARGET".into(), row.get("target"));
    environment.insert("AK_BUILD_ID".into(), row.get::<Uuid, _>("id").to_string());
    let credential = source::credential(state, row.get("credential_id"))
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
    })
}

pub async fn refresh_builds(state: &AppState) -> Result<()> {
    sqlx::query("WITH summaries AS (SELECT b.id, CASE WHEN b.cancel_requested THEN CASE WHEN bool_or(j.status IN ('leased','running')) THEN 'canceling' ELSE 'canceled' END WHEN bool_or(j.status='running') THEN 'running' WHEN bool_or(j.status IN ('queued','leased')) THEN 'queued' WHEN bool_or(j.status='failed') THEN 'failed' WHEN bool_or(j.status='lost') THEN 'lost' WHEN bool_or(j.status='canceled') THEN 'canceled' ELSE 'succeeded' END AS status FROM builds b JOIN jobs j ON j.build_id=b.id WHERE b.status<>'resolving' GROUP BY b.id) UPDATE builds b SET status=s.status,finished_at=CASE WHEN s.status IN ('succeeded','failed','lost','canceled') THEN coalesce(b.finished_at,now()) ELSE NULL END FROM summaries s WHERE b.id=s.id")
        .execute(&state.pool).await?;
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
    let artifacts:Vec<Uuid>=sqlx::query_scalar("SELECT ar.id FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id JOIN jobs j ON j.id=a.job_id WHERE j.build_id=$1 AND j.status='succeeded' AND a.status='succeeded'").bind(id).fetch_all(&mut *transaction).await?;
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE build_id=$1")
        .bind(id)
        .fetch_one(&mut *transaction)
        .await?;
    let complete:i64=sqlx::query_scalar("SELECT count(DISTINCT j.id) FROM jobs j JOIN attempts a ON a.job_id=j.id AND a.status='succeeded' JOIN artifacts ar ON ar.attempt_id=a.id WHERE j.build_id=$1 AND j.status='succeeded'").bind(id).fetch_one(&mut *transaction).await?;
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
