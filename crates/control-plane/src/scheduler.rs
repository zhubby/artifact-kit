use crate::{
    protocol::{Assignment, Command, GitCredential, Step},
    recipe::{Recipe, Target},
    source,
    state::AppState,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub fn start(state: AppState) {
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
    state
        .store
        .resolve_build(|input| async move {
            if input.goal.is_some() {
                let model = input.model_config_snapshot.ok_or_else(|| {
                    anyhow::anyhow!(
                        "configure an enabled platform model profile before creating this build"
                    )
                })?;
                let bindings = serde_json::from_value(
                    input
                        .planning_context
                        .get("source_credentials")
                        .cloned()
                        .unwrap_or(json!([])),
                )?;
                let snapshot = crate::source_cache::capture(
                    state,
                    input.project_id,
                    &input.repository_url,
                    &input.requested_ref,
                    input.credential_id,
                    bindings,
                )
                .await?;
                return Ok(crate::store::ResolvedSource::Planned {
                    commit: snapshot.commit,
                    snapshot_id: snapshot.id,
                    model,
                });
            }
            let auth = source::credential(state, input.credential_id).await?;
            let (commit, yaml) =
                source::resolve(state, &input.repository_url, &input.requested_ref, auth).await?;
            let recipe = Recipe::parse(&yaml)?;
            recipe.environment(&input.parameters)?;
            ensure!(
                input
                    .selected_targets
                    .iter()
                    .all(|name| recipe.targets.contains_key(name)),
                "unknown selected target"
            );
            let targets: Vec<_> = recipe
                .targets
                .iter()
                .filter(|(name, _)| {
                    input.selected_targets.is_empty() || input.selected_targets.contains(name)
                })
                .map(|(name, target)| (name.clone(), target.clone()))
                .collect();
            ensure!(!targets.is_empty(), "no selected targets");
            for (_, target) in &targets {
                ensure!(
                    target
                        .signing_profiles
                        .iter()
                        .all(|profile| input.signing_profiles.contains(profile)),
                    "project is not authorized for signing profile"
                );
            }
            ensure!(
                input.release_version.is_none() || recipe.release.enabled,
                "release is not enabled by repository configuration"
            );
            Ok(crate::store::ResolvedSource::Recipe {
                commit,
                yaml,
                recipe,
                targets,
            })
        })
        .await
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
    crate::planning::reap(state).await?;
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
        let canceled = state.store.canceled_attempts(node_id).await?;
        for attempt in canceled {
            let command = Command {
                command_id: Uuid::new_v4().to_string(),
                kind: "cancel".into(),
                attempt_id: attempt.id.to_string(),
                lease_token: attempt.lease_token,
                ..Default::default()
            };
            let _ = connection.sender.try_send(Ok(command));
        }
    }
    refresh_builds(state).await?;
    queue_revisions(state).await?;
    let pending = state.store.pending_publications().await?;
    for id in pending {
        let _ = publish_build(state, id).await;
    }
    Ok(())
}

pub async fn expire(state: &AppState) -> Result<()> {
    state.store.expire().await
}

async fn assign(state: &AppState, node_id: Uuid, session: Uuid) -> Result<Option<Command>> {
    let Some(claim) = state.store.claim_job(node_id, session).await? else {
        return Ok(None);
    };
    let assignment = match assignment(state, claim.job, claim.attempt).await {
        Ok(assignment) => assignment,
        Err(error) => {
            state
                .store
                .assignment_failed(claim.job, claim.attempt)
                .await?;
            return Err(error);
        }
    };
    Ok(Some(Command {
        command_id: claim.command_id.to_string(),
        kind: "assign".into(),
        attempt_id: claim.attempt.to_string(),
        lease_token: claim.lease_token,
        lease_seconds: 90,
        assignment: Some(assignment),
        ..Default::default()
    }))
}

async fn assignment(state: &AppState, job: Uuid, attempt: Uuid) -> Result<Assignment> {
    let context = state.store.assignment_context(job).await?;
    let target: Target = serde_json::from_value(context.target_config)?;
    let plan_id: Option<Uuid> = context.plan_revision_id;
    let plan = context
        .plan
        .map(crate::execution_plan::ExecutionPlan::parse)
        .transpose()?;
    let recipe: Recipe = if let Some(plan) = &plan {
        plan.recipe.clone()
    } else {
        serde_json::from_value(context.config_snapshot)?
    };
    let mut environment = recipe.environment(&context.parameters)?;
    environment.insert("ARTIFACTORY_COMMIT_SHA".into(), context.commit_sha.clone());
    environment.insert("ARTIFACTORY_TARGET".into(), context.target.clone());
    environment.insert("ARTIFACTORY_BUILD_ID".into(), context.id.to_string());
    let source_descriptor = if let Some(id) = context.source_snapshot_id {
        Some(crate::source_cache::descriptor(state, id, attempt).await?)
    } else {
        None
    };
    let credential = source::credential(
        state,
        if source_descriptor.is_none() {
            context.credential_id
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
        build_id: context.id.to_string(),
        target: context.target,
        repository_url: context.repository_url,
        commit_sha: context.commit_sha,
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
    state.store.refresh_builds().await
}

pub async fn queue_revisions(state: &AppState) -> Result<()> {
    state.store.queue_revisions().await
}

pub async fn publish_build(state: &AppState, id: Uuid) -> Result<Uuid> {
    let result = state.store.publish_transaction(id).await;
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
            state
                .store
                .publication_failed(id, &error.to_string())
                .await?;
        }
    }
    result
}
