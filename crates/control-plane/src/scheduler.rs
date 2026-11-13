use crate::{
    protocol::{Assignment, GitCredential, Step},
    recipe::{Recipe, Target},
    source,
    state::AppState,
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
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
                let mut listener = crate::store::ControlStore::listen_work(&notifications.settings.database_url).await?;
                loop {
                    tokio::select! {
                        _ = notifications.coordination.stop.cancelled() => return Ok::<(), anyhow::Error>(()),
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
    state
        .store
        .maintain(|| async {
            crate::planning::reap(state).await?;
            expire(state).await?;
            refresh_builds(state).await?;
            queue_revisions(state).await?;
            state.store.retain_commands().await?;
            let pending = state.store.pending_publications().await?;
            for id in pending {
                let _ = publish_build(state, id).await;
            }
            Ok(())
        })
        .await
}

pub async fn expire(state: &AppState) -> Result<()> {
    state.store.expire().await
}

/// Match queued jobs to available nodes in bounded short transactions.
///
/// # Errors
/// Returns database or persisted target errors.
pub async fn schedule(state: &AppState) -> Result<()> {
    for _ in 0..state.settings.control.scheduler_batch {
        if state.coordination.is_draining() || !state.store.schedule_one().await? {
            break;
        }
    }
    Ok(())
}

pub(crate) async fn assignment(state: &AppState, job: Uuid, attempt: Uuid) -> Result<Assignment> {
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
