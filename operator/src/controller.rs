use crate::{api::*, leadership::Leadership};
use anyhow::{Result, ensure};
use artifactory_runtime_protocol::{Execution, service::Client as ServiceClient};
use futures::StreamExt;
use k8s_openapi::api::{
    apps::v1::StatefulSet,
    batch::v1::Job,
    core::v1::{Pod, Service},
};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{
        DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions, PropagationPolicy,
    },
    runtime::{Controller, controller::Action, watcher},
};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
const MANAGED_SNAPSHOT: &str = "artifactory.dev/managed-snapshot";
const HISTORY_SECONDS: i64 = 86400;
#[derive(Clone)]
pub struct Context {
    pub client: Client,
    pub namespace: String,
    pub runtime: ServiceClient,
    pub leader: Leadership,
}
pub fn job(run: &AgentRun, pool: &AgentPool) -> Result<Job> {
    let owner = run
        .controller_owner_ref(&())
        .ok_or_else(|| anyhow::anyhow!("run UID required"))?;
    let name = format!("pi-{}-g{}", run.spec.run_id, run.spec.generation);
    let mut value = json!({"apiVersion":"batch/v1","kind":"Job","metadata":{"name":name,"namespace":run.namespace(),"ownerReferences":[owner],"labels":{"artifactory.dev/run":run.spec.run_id}},"spec":{"parallelism":1,"completions":1,"backoffLimit":0,"activeDeadlineSeconds":3900,"template":{"metadata":{"labels":{"app.kubernetes.io/name":"artifactory-pi","artifactory.dev/run":run.spec.run_id}},"spec":{"serviceAccountName":"artifactory-pi","automountServiceAccountToken":false,"restartPolicy":"Never","securityContext":{"runAsNonRoot":true,"runAsUser":10001,"runAsGroup":10001,"fsGroup":10001,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"pi","image":pool.spec.image,"imagePullPolicy":pool.spec.image_pull_policy,"command":["node","dist/job-main.js"],"securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},"env":[{"name":"ARTIFACTORY_RUN_ID","value":run.spec.run_id},{"name":"ARTIFACTORY_RUN_GENERATION","value":run.spec.generation.to_string()},{"name":"ARTIFACTORY_RUNTIME_URL","value":pool.spec.runtime_url},{"name":"POD_NAME","valueFrom":{"fieldRef":{"fieldPath":"metadata.name"}}},{"name":"POD_UID","valueFrom":{"fieldRef":{"fieldPath":"metadata.uid"}}},{"name":"POD_NAMESPACE","valueFrom":{"fieldRef":{"fieldPath":"metadata.namespace"}}},{"name":"ARTIFACTORY_POD_TOKEN","value":"/var/run/artifactory/token"}],"resources":{"requests":{"cpu":pool.spec.cpu,"memory":pool.spec.memory},"limits":{"cpu":"2","memory":"2Gi"}},"volumeMounts":[{"name":"identity","mountPath":"/var/run/artifactory","readOnly":true},{"name":"tmp","mountPath":"/tmp"}]}],"volumes":[{"name":"identity","projected":{"sources":[{"serviceAccountToken":{"audience":"artifactory-runtime","expirationSeconds":600,"path":"token"}}]}},{"name":"tmp","emptyDir":{}}]}}}});
    if !pool.spec.tls_secret_name.is_empty() {
        let pod = &mut value["spec"]["template"]["spec"];
        pod["containers"][0]["env"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"NODE_EXTRA_CA_CERTS","value":"/run/service-ca/ca.crt"}));
        pod["containers"][0]["volumeMounts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"ca","mountPath":"/run/service-ca","readOnly":true}));
        pod["volumes"].as_array_mut().unwrap().push(json!({"name":"ca","secret":{"secretName":pool.spec.tls_secret_name,"items":[{"key":"ca.crt","path":"ca.crt"}]}}));
    }
    Ok(serde_json::from_value(value)?)
}
async fn create_job(ctx: &Context, run: &AgentRun, pool: &AgentPool) -> Result<Job> {
    let desired = job(run, pool)?;
    let api: Api<Job> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let name = desired.name_any();
    if let Some(existing) = api.get_opt(&name).await? {
        ensure!(
            existing
                .owner_references()
                .iter()
                .any(|o| Some(&o.uid) == run.metadata.uid.as_ref()),
            "Job owned by another run UID"
        );
        return Ok(existing);
    }
    ctx.leader.ensure().await?;
    match api.create(&PostParams::default(), &desired).await {
        Ok(job) => Ok(job),
        Err(error) => {
            if let Some(job) = api.get_opt(&name).await? {
                ensure!(
                    job.owner_references()
                        .iter()
                        .any(|o| Some(&o.uid) == run.metadata.uid.as_ref()),
                    "Job ownership mismatch"
                );
                Ok(job)
            } else {
                Err(error.into())
            }
        }
    }
}
fn job_delete_params(run: &AgentRun, job: &Job) -> Result<DeleteParams> {
    ensure!(
        job.owner_references()
            .iter()
            .any(|owner| Some(&owner.uid) == run.metadata.uid.as_ref()),
        "Job owned by another run UID"
    );
    let uid = job
        .uid()
        .ok_or_else(|| anyhow::anyhow!("Job UID required"))?;
    Ok(DeleteParams {
        propagation_policy: Some(PropagationPolicy::Background),
        preconditions: Some(Preconditions {
            uid: Some(uid),
            resource_version: None,
        }),
        ..Default::default()
    })
}
async fn cleanup_run(ctx: &Context, run: &AgentRun, phase: &str) -> Result<Action> {
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let name = format!("pi-{}-g{}", run.spec.run_id, run.spec.generation);
    let job = jobs.get_opt(&name).await?;
    if let Some(job) = &job {
        let deletion = job_delete_params(run, job)?;
        if job.metadata.deletion_timestamp.is_none() {
            ctx.leader.ensure().await?;
            match jobs.delete(&name, &deletion).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    // Background deletion can acknowledge the Job before its Pods disappear.
    // Observe by Job name even after GC has removed its UID; never delete Pods by label.
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let pod = pods
        .list(&ListParams::default().labels(&format!("job-name={name}")))
        .await?
        .into_iter()
        .find(|pod| {
            pod.owner_references()
                .iter()
                .any(|owner| owner.kind == "Job" && owner.name == name)
        });
    let pending = job.is_some() || pod.is_some();
    let pod_uid = if pending {
        pod.and_then(|p| p.uid())
            .or_else(|| run.status.as_ref().and_then(|s| s.pod_uid.clone()))
    } else {
        None
    };
    update(
        ctx,
        run,
        Status {
            phase: phase.into(),
            message: pending.then(|| "CleanupPending: waiting for Job/Pod deletion".into()),
            pod_uid,
            observed_generation: run.metadata.generation.unwrap_or(1),
        },
    )
    .await?;
    Ok(Action::requeue(Duration::from_secs(if pending {
        5
    } else {
        60
    })))
}
pub async fn run_reconcile(run: Arc<AgentRun>, ctx: Arc<Context>) -> Result<Action, Error> {
    ctx.leader.ensure().await?;
    let status: Value = crate::client::call("get", &json!({"run_id":run.spec.run_id})).await?;
    let generation = status["lifecycle"]["generation"].as_i64().unwrap_or(0);
    let state = status["status"].as_str().unwrap_or("unknown");
    let name = format!("pi-{}-g{}", run.spec.run_id, run.spec.generation);
    if generation != run.spec.generation
        || ["canceled", "failed", "succeeded", "needs_input"].contains(&state)
        || run.metadata.deletion_timestamp.is_some()
    {
        let phase = if generation != run.spec.generation {
            "Superseded"
        } else {
            state
        };
        return Ok(cleanup_run(&ctx, &run, phase).await?);
    }
    let pools: Api<AgentPool> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let pool = pools.get(&run.spec.pool).await?;
    let job = create_job(&ctx, &run, &pool).await?;
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let pods = pods
        .list(&ListParams::default().labels(&format!("job-name={name}")))
        .await?;
    let mut uid = None;
    let mut message = None;
    if let Some(pod) = pods.items.iter().find(|p| {
        p.owner_references()
            .iter()
            .any(|owner| Some(&owner.uid) == job.metadata.uid.as_ref())
    }) {
        let pod_uid = pod
            .uid()
            .ok_or_else(|| anyhow::anyhow!("Pod UID missing"))?;
        let execution = Execution {
            run_id: run.spec.run_id.parse()?,
            generation: run.spec.generation,
            uid: pod_uid.clone(),
            name: pod.name_any(),
            namespace: ctx.namespace.clone(),
        };
        ctx.leader.ensure().await?;
        let _: Value = crate::client::call(
            "register",
            &serde_json::to_value(execution).map_err(anyhow::Error::from)?,
        )
        .await?;
        uid = Some(pod_uid);
        message = pod.status.as_ref().and_then(|status| {
            status
                .container_statuses
                .as_ref()
                .and_then(|containers| {
                    containers.iter().find_map(|c| {
                        c.state.as_ref().and_then(|state| {
                            state
                                .waiting
                                .as_ref()
                                .map(|w| w.reason.clone().unwrap_or_default())
                                .or_else(|| {
                                    state
                                        .terminated
                                        .as_ref()
                                        .filter(|t| t.exit_code != 0)
                                        .map(|t| format!("Pi exited with code {}", t.exit_code))
                                })
                        })
                    })
                })
                .or_else(|| {
                    status.conditions.as_ref().and_then(|conditions| {
                        conditions
                            .iter()
                            .find(|c| c.type_ == "PodScheduled" && c.status == "False")
                            .and_then(|c| c.reason.clone())
                    })
                })
        });
    }
    update(
        &ctx,
        &run,
        Status {
            phase: if uid.is_some() {
                "Running"
            } else {
                "Provisioning"
            }
            .into(),
            message,
            pod_uid: uid,
            observed_generation: run.metadata.generation.unwrap_or(1),
        },
    )
    .await?;
    Ok(Action::requeue(Duration::from_secs(5)))
}
async fn update(ctx: &Context, run: &AgentRun, status: Status) -> Result<()> {
    if run.status.as_ref() == Some(&status) {
        return Ok(());
    }
    ctx.leader.ensure().await?;
    let api: Api<AgentRun> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    api.patch_status(
        &run.name_any(),
        &PatchParams::default(),
        &Patch::Merge(
            json!({"metadata":{"resourceVersion":run.resource_version()},"status":status}),
        ),
    )
    .await?;
    Ok(())
}
#[derive(Debug)]
pub struct Error(anyhow::Error);
impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl From<kube::Error> for Error {
    fn from(e: kube::Error) -> Self {
        Self(e.into())
    }
}
impl From<uuid::Error> for Error {
    fn from(e: uuid::Error) -> Self {
        Self(e.into())
    }
}
impl std::error::Error for Error {}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
fn policy(_object: Arc<AgentRun>, error: &Error, _ctx: Arc<Context>) -> Action {
    tracing::warn!(%error,"run reconcile failed");
    Action::requeue(Duration::from_secs(5))
}
pub async fn controller(ctx: Arc<Context>) {
    let runs: Api<AgentRun> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let jobs: Api<Job> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    Controller::new(runs, watcher::Config::default())
        .owns(jobs, watcher::Config::default())
        .with_config(kube::runtime::controller::Config::default().concurrency(8))
        .run(run_reconcile, policy, ctx)
        .for_each(|_| async {})
        .await;
}
pub async fn dispatch(ctx: &Context) -> Result<()> {
    ctx.leader.ensure().await?;
    let pools: Api<AgentPool> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    let runs: Api<AgentRun> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    for pool in pools.list(&ListParams::default()).await? {
        let value:Value=ctx.runtime.post("/v1/queue",&json!({"pool":pool.name_any(),"limit":pool.spec.max_active,"per_project":pool.spec.max_per_project})).await?;
        let status = Status {
            phase: if pool.spec.max_active == 0 {
                "Paused"
            } else {
                "Ready"
            }
            .into(),
            message: Some(format!(
                "active={}, queued={}",
                value["active"], value["queued"]
            )),
            pod_uid: None,
            observed_generation: pool.metadata.generation.unwrap_or(1),
        };
        if pool.status.as_ref() != Some(&status) {
            ctx.leader.ensure().await?;
            pools.patch_status(&pool.name_any(),&PatchParams::default(),&Patch::Merge(json!({"metadata":{"resourceVersion":pool.resource_version()},"status":status}))).await?;
        }
        for descriptor in value["runs"].as_array().into_iter().flatten() {
            let snapshots: Api<SourceSnapshot> =
                Api::namespaced(ctx.client.clone(), &ctx.namespace);
            let snapshot_id = descriptor["snapshot_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("snapshot ID missing"))?;
            let snapshot_name = format!("snapshot-{snapshot_id}");
            if snapshots.get_opt(&snapshot_name).await?.is_none() {
                let mut snapshot = SourceSnapshot::new(
                    &snapshot_name,
                    SourceSnapshotSpec {
                        snapshot_id: snapshot_id.into(),
                        digest: descriptor["snapshot_digest"]
                            .as_str()
                            .unwrap_or_default()
                            .into(),
                        source_pool: pool.spec.source_pool.clone(),
                        source_url: descriptor["source_url"]
                            .as_str()
                            .ok_or_else(|| anyhow::anyhow!("source URL missing"))?
                            .into(),
                    },
                );
                snapshot.metadata.labels = Some([(MANAGED_SNAPSHOT.into(), "true".into())].into());
                ctx.leader.ensure().await?;
                match snapshots.create(&PostParams::default(), &snapshot).await {
                    Ok(_) => {}
                    Err(kube::Error::Api(e)) if e.code == 409 => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let run = descriptor["run_id"].as_str().unwrap_or_default();
            let generation = descriptor["generation"].as_i64().unwrap_or(1);
            let name = format!("run-{run}-g{generation}");
            if runs.get_opt(&name).await?.is_some() {
                continue;
            }
            let resource = AgentRun::new(
                &name,
                AgentRunSpec {
                    run_id: run.into(),
                    generation,
                    pool: pool.name_any(),
                    snapshot_id: descriptor["snapshot_id"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    snapshot_digest: descriptor["snapshot_digest"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                },
            );
            ctx.leader.ensure().await?;
            match runs.create(&PostParams::default(), &resource).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 409 => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    let known_runs = runs.list(&ListParams::default()).await?;
    let protected_snapshots: HashSet<String> = known_runs
        .iter()
        .map(|r| r.spec.snapshot_id.clone())
        .collect();
    for run in known_runs {
        if run_reclaimable(&run, chrono::Utc::now().timestamp()) {
            ctx.leader.ensure().await?;
            runs.delete(&run.name_any(), &DeleteParams::background())
                .await?;
        }
    }
    source_pools(ctx, &protected_snapshots).await
}
fn run_reclaimable(run: &AgentRun, now: i64) -> bool {
    run.status.as_ref().is_some_and(|s| {
        s.message.is_none()
            && s.pod_uid.is_none()
            && matches!(
                s.phase.as_str(),
                "Superseded" | "canceled" | "failed" | "succeeded" | "needs_input"
            )
    }) && run
        .metadata
        .creation_timestamp
        .as_ref()
        .is_some_and(|t| t.0.as_second() < now - HISTORY_SECONDS)
}
fn snapshot_reclaimable(snapshot: &SourceSnapshot, protected: &HashSet<String>, now: i64) -> bool {
    snapshot
        .labels()
        .get(MANAGED_SNAPSHOT)
        .is_some_and(|value| value == "true")
        && !protected.contains(&snapshot.spec.snapshot_id)
        && snapshot
            .metadata
            .creation_timestamp
            .as_ref()
            .is_some_and(|t| t.0.as_second() < now - HISTORY_SECONDS)
}
async fn source_pools(ctx: &Context, protected_snapshots: &HashSet<String>) -> Result<()> {
    let api: Api<SourcePool> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    for pool in api.list(&ListParams::default()).await? {
        let name = pool.name_any();
        let spec = &pool.spec;
        let public_url = spec.download_url(&name, &ctx.namespace)?;
        let service: Service = serde_json::from_value(
            json!({"apiVersion":"v1","kind":"Service","metadata":{"name":name},"spec":{"clusterIP":"None","selector":{"artifactory.dev/source-pool":name},"ports":[{"port":8090,"targetPort":8090}]}}),
        )?;
        let services: Api<Service> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
        ctx.leader.ensure().await?;
        let current = services.get_opt(&name).await?;
        if !current.as_ref().is_some_and(|c| {
            contains(
                &serde_json::to_value(c).unwrap(),
                &serde_json::to_value(&service).unwrap(),
            )
        }) {
            services
                .patch(
                    &name,
                    &PatchParams::apply("artifactory-operator"),
                    &Patch::Apply(&service),
                )
                .await?;
        }
        let mut sts_value = json!({"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":name},"spec":{"serviceName":name,"replicas":spec.shards,"persistentVolumeClaimRetentionPolicy":{"whenDeleted":"Retain","whenScaled":"Retain"},"selector":{"matchLabels":{"artifactory.dev/source-pool":name}},"template":{"metadata":{"labels":{"artifactory.dev/source-pool":name,"app.kubernetes.io/name":"artifactory-source"}},"spec":{"securityContext":{"fsGroup":10001,"runAsUser":10001,"runAsNonRoot":true},"containers":[{"name":"source","image":spec.image,"imagePullPolicy":spec.image_pull_policy,"args":["source"],"ports":[{"containerPort":8090}],"envFrom":[{"secretRef":{"name":spec.secret_name}}],"env":[{"name":"POD_NAME","valueFrom":{"fieldRef":{"fieldPath":"metadata.name"}}},{"name":"ARTIFACTORY_SOURCE_PUBLIC_URL","value":format!("http://$(POD_NAME).{name}.{}.svc:8090",ctx.namespace)},{"name":"ARTIFACTORY_RUNTIME_URL","value":spec.runtime_url},{"name":"ARTIFACTORY_CONTROL_URL","value":spec.control_url},{"name":"ARTIFACTORY_SOURCE_DATA_DIR","value":"/var/lib/artifactory-source"}],"resources":{"requests":{"cpu":spec.cpu,"memory":spec.memory}},"readinessProbe":{"httpGet":{"path":"/healthz","port":8090},"initialDelaySeconds":3},"volumeMounts":[{"name":"cache","mountPath":"/var/lib/artifactory-source"}]}] }},"volumeClaimTemplates":[{"metadata":{"name":"cache"},"spec":{"accessModes":["ReadWriteOnce"],"storageClassName":spec.storage_class,"resources":{"requests":{"storage":spec.storage_size}}}}]}});
        let pod = &mut sts_value["spec"]["template"]["spec"];
        pod["containers"][0]["env"][1]["value"] = json!(public_url);
        if !spec.tls_secret_name.is_empty() {
            pod["containers"][0]["env"].as_array_mut().unwrap().extend([
                json!({"name":"ARTIFACTORY_SERVICE_CA","value":"/run/service-tls/ca.crt"}),
                json!({"name":"ARTIFACTORY_SERVICE_TLS_CERT","value":"/run/service-tls/tls.crt"}),
                json!({"name":"ARTIFACTORY_SERVICE_TLS_KEY","value":"/run/service-tls/tls.key"}),
            ]);
            pod["containers"][0]["volumeMounts"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"tls","mountPath":"/run/service-tls","readOnly":true}));
            pod["volumes"] = json!([{"name":"tls","secret":{"secretName":spec.tls_secret_name}}]);
            pod["containers"][0]["readinessProbe"]["httpGet"]["scheme"] = json!("HTTPS");
        }
        let sts: StatefulSet = serde_json::from_value(sts_value)?;
        let sets: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
        ctx.leader.ensure().await?;
        let current = sets.get_opt(&name).await?;
        if !current.as_ref().is_some_and(|c| {
            contains(
                &serde_json::to_value(c).unwrap(),
                &serde_json::to_value(&sts).unwrap(),
            )
        }) {
            sets.patch(
                &name,
                &PatchParams::apply("artifactory-operator"),
                &Patch::Apply(&sts),
            )
            .await?;
        }
        let phase = if current
            .as_ref()
            .and_then(|s| s.status.as_ref())
            .and_then(|s| s.ready_replicas)
            .unwrap_or(0)
            >= spec.shards
        {
            "Ready"
        } else {
            "Provisioning"
        };
        if pool.status.as_ref().is_none_or(|s| {
            s.phase != phase || s.observed_generation != pool.metadata.generation.unwrap_or(1)
        }) {
            ctx.leader.ensure().await?;
            api.patch_status(&name,&PatchParams::default(),&Patch::Merge(json!({"status":{"phase":phase,"observedGeneration":pool.metadata.generation.unwrap_or(1)}}))).await?;
        }
    }
    let snapshots: Api<SourceSnapshot> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
    for snapshot in snapshots.list(&ListParams::default()).await? {
        if snapshot_reclaimable(
            &snapshot,
            protected_snapshots,
            chrono::Utc::now().timestamp(),
        ) {
            let uid = snapshot
                .uid()
                .ok_or_else(|| anyhow::anyhow!("snapshot CRD UID required"))?;
            ctx.leader.ensure().await?;
            snapshots
                .delete(
                    &snapshot.name_any(),
                    &DeleteParams {
                        propagation_policy: Some(PropagationPolicy::Background),
                        preconditions: Some(Preconditions {
                            uid: Some(uid),
                            resource_version: None,
                        }),
                        ..Default::default()
                    },
                )
                .await?;
            continue;
        }
        if snapshot.status.as_ref().is_some_and(|s| s.phase == "Ready") {
            continue;
        }
        let source = ServiceClient::new(
            snapshot.spec.source_url.clone(),
            artifactory_runtime_protocol::service::required("ARTIFACTORY_SERVICE_TOKEN")?,
        )?;
        let metadata: Value = source
            .post(
                "/v1/metadata",
                &json!({"snapshot_id":snapshot.spec.snapshot_id}),
            )
            .await?;
        ensure!(
            metadata["digest"] == snapshot.spec.digest,
            "snapshot digest mismatch"
        );
        ctx.leader.ensure().await?;
        snapshots.patch_status(&snapshot.name_any(),&PatchParams::default(),&Patch::Merge(json!({"status":{"phase":"Ready","observedGeneration":snapshot.metadata.generation.unwrap_or(1)}}))).await?;
    }
    Ok(())
}

fn contains(actual: &Value, desired: &Value) -> bool {
    match desired {
        Value::Object(map) => map.iter().all(|(key, value)| {
            value.is_null() || actual.get(key).is_some_and(|a| contains(a, value))
        }),
        Value::Array(items) => actual.as_array().is_some_and(|a| {
            a.len() == items.len() && a.iter().zip(items).all(|(a, d)| contains(a, d))
        }),
        _ => actual == desired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn status_transitions_clear_stale_fields_and_steady_reconcile_does_not_write() {
        use axum::{
            Json, Router,
            extract::State,
            routing::{get, patch},
        };
        use std::sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        let mut run = AgentRun::new(
            "run-test",
            AgentRunSpec {
                run_id: "run".into(),
                generation: 1,
                pool: "pool".into(),
                snapshot_id: "snapshot".into(),
                snapshot_digest: "a".repeat(64),
            },
        );
        run.metadata.namespace = Some("test".into());
        run.metadata.resource_version = Some("1".into());
        run.status = Some(Status {
            phase: "Provisioning".into(),
            message: Some("ContainerCreating".into()),
            pod_uid: Some("pod-a".into()),
            observed_generation: 1,
        });
        let stored = Arc::new(Mutex::new(serde_json::to_value(&run).unwrap()));
        let writes = Arc::new(AtomicUsize::new(0));
        let state = (stored.clone(), writes.clone());
        let app = Router::new()
            .route(
                "/apis/coordination.k8s.io/v1/namespaces/test/leases/operator",
                get(|| async {
                    Json(json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease",
                    "metadata":{"name":"operator"},"spec":{"holderIdentity":"leader",
                    "renewTime":chrono::Utc::now().to_rfc3339(),"leaseDurationSeconds":15}}))
                }),
            )
            .route(
                "/apis/agents.artifactory.dev/v1alpha1/namespaces/test/agentruns/run-test/status",
                patch(
                    |State((stored, writes)): State<(Arc<Mutex<Value>>, Arc<AtomicUsize>)>,
                     Json(patch): Json<Value>| async move {
                        let mut value = stored.lock().unwrap();
                        // RFC 7396: absent properties are retained; explicit null deletes them.
                        for (key, field) in patch["status"].as_object().unwrap() {
                            if field.is_null() {
                                value["status"].as_object_mut().unwrap().remove(key);
                            } else {
                                value["status"][key] = field.clone();
                            }
                        }
                        writes.fetch_add(1, Ordering::SeqCst);
                        Json(value.clone())
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(kube::Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = Context {
            client: client.clone(),
            namespace: "test".into(),
            runtime: ServiceClient::new("https://runtime.invalid".into(), "x".repeat(64)).unwrap(),
            leader: Leadership {
                api: Api::namespaced(client, "test"),
                identity: Arc::new("leader".into()),
                name: "operator".into(),
            },
        };
        let running = Status {
            phase: "Running".into(),
            message: None,
            pod_uid: Some("pod-a".into()),
            observed_generation: 1,
        };
        update(&ctx, &run, running.clone()).await.unwrap();
        let current: AgentRun = serde_json::from_value(stored.lock().unwrap().clone()).unwrap();
        assert_eq!(
            current.status.as_ref().unwrap().message,
            None,
            "a ready Pod must clear its previous waiting reason"
        );
        update(&ctx, &current, running).await.unwrap();
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "steady reconcile must not patch status"
        );
        let terminal = Status {
            phase: "succeeded".into(),
            message: None,
            pod_uid: None,
            observed_generation: 1,
        };
        update(&ctx, &current, terminal.clone()).await.unwrap();
        let current: AgentRun = serde_json::from_value(stored.lock().unwrap().clone()).unwrap();
        assert_eq!(
            current.status.as_ref().unwrap().pod_uid,
            None,
            "terminal status must clear the old Pod identity"
        );
        update(&ctx, &current, terminal).await.unwrap();
        assert_eq!(
            writes.load(Ordering::SeqCst),
            2,
            "only the two state transitions may write"
        );
        server.abort();
    }

    #[test]
    fn job_cleanup_cascades_pods_and_fences_replaced_resource_uids() {
        let mut run = AgentRun::new(
            "run-test",
            AgentRunSpec {
                run_id: "run".into(),
                generation: 1,
                pool: "pool".into(),
                snapshot_id: "snapshot".into(),
                snapshot_digest: "a".repeat(64),
            },
        );
        run.metadata.uid = Some("original-run".into());
        let mut job: Job = serde_json::from_value(json!({
            "metadata":{"uid":"original-job","ownerReferences":[{
                "apiVersion":"agents.artifactory.dev/v1alpha1", "kind":"AgentRun",
                "name":"run-test", "uid":"original-run", "controller":true
            }]}
        }))
        .unwrap();
        let deletion = serde_json::to_value(job_delete_params(&run, &job).unwrap()).unwrap();
        assert_eq!(deletion["propagationPolicy"], "Background");
        assert_eq!(deletion["preconditions"]["uid"], "original-job");
        run.metadata.uid = Some("replacement-run".into());
        assert!(job_delete_params(&run, &job).is_err());
        run.metadata.uid = Some("original-run".into());
        job.metadata.uid = None;
        assert!(job_delete_params(&run, &job).is_err());
    }

    #[tokio::test]
    async fn cleanup_keeps_identity_until_pods_disappear_and_retries_api_failures() {
        use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
        use std::sync::Mutex;

        struct Stored {
            run: Value,
            job: Option<Value>,
            pods: Vec<Value>,
            fail_list: bool,
            deletions: Vec<Value>,
            writes: usize,
        }
        let mut run = AgentRun::new(
            "run-test",
            AgentRunSpec {
                run_id: "run".into(),
                generation: 1,
                pool: "pool".into(),
                snapshot_id: "snapshot".into(),
                snapshot_digest: "a".repeat(64),
            },
        );
        run.metadata.uid = Some("run-a".into());
        run.metadata.resource_version = Some("1".into());
        run.status = Some(Status {
            phase: "Running".into(),
            message: None,
            pod_uid: Some("pod-a".into()),
            observed_generation: 1,
        });
        let stored = Arc::new(Mutex::new(Stored {
            run: serde_json::to_value(&run).unwrap(),
            job: Some(json!({"apiVersion":"batch/v1","kind":"Job", "metadata":{
                "name":"pi-run-g1","uid":"job-a","ownerReferences":[{
                    "apiVersion":"agents.artifactory.dev/v1alpha1","kind":"AgentRun",
                    "name":"run-test","uid":"run-a","controller":true}]}})),
            pods: vec![json!({"metadata":{"name":"pi-run-g1-test","uid":"pod-a",
                "deletionTimestamp":"2026-11-12T00:00:00Z", "ownerReferences":[{
                    "apiVersion":"batch/v1","kind":"Job","name":"pi-run-g1",
                    "uid":"job-a","controller":true}]},"status":{"phase":"Running"}})],
            fail_list: false,
            deletions: Vec::new(),
            writes: 0,
        }));
        let app = Router::new()
            .route("/apis/coordination.k8s.io/v1/namespaces/test/leases/operator", get(|| async {
                Json(json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease",
                    "metadata":{"name":"operator"},"spec":{"holderIdentity":"leader",
                    "renewTime":chrono::Utc::now().to_rfc3339(),"leaseDurationSeconds":15}}))
            }))
            .route("/apis/batch/v1/namespaces/test/jobs/pi-run-g1", get(
                |State(stored): State<Arc<Mutex<Stored>>>| async move {
                    let value = stored.lock().unwrap().job.clone();
                    match value {
                        Some(job) => (StatusCode::OK, Json(job)),
                        None => (StatusCode::NOT_FOUND, Json(json!({"kind":"Status",
                            "apiVersion":"v1","status":"Failure","reason":"NotFound",
                            "message":"job gone","code":404}))),
                    }
                }).delete(|State(stored): State<Arc<Mutex<Stored>>>, Json(params): Json<Value>| async move {
                    let mut value = stored.lock().unwrap();
                    value.job = None;
                    value.deletions.push(params);
                    Json(json!({"apiVersion":"v1","kind":"Status","status":"Success"}))
                }))
            .route("/api/v1/namespaces/test/pods", get(
                |State(stored): State<Arc<Mutex<Stored>>>| async move {
                    let value = stored.lock().unwrap();
                    if value.fail_list {
                        (StatusCode::FORBIDDEN, Json(json!({"kind":"Status",
                            "apiVersion":"v1","status":"Failure","code":403,
                            "reason":"Forbidden","message":"injected loss of API access"})))
                    } else {
                        (StatusCode::OK, Json(json!({"apiVersion":"v1","kind":"PodList",
                            "metadata":{},"items":value.pods})))
                    }
                }))
            .route("/apis/agents.artifactory.dev/v1alpha1/namespaces/test/agentruns/run-test/status",
                axum::routing::patch(|State(stored): State<Arc<Mutex<Stored>>>, Json(patch): Json<Value>| async move {
                    let mut value = stored.lock().unwrap();
                    for (key, field) in patch["status"].as_object().unwrap() {
                        if field.is_null() {
                            value.run["status"].as_object_mut().unwrap().remove(key);
                        } else {
                            value.run["status"][key] = field.clone();
                        }
                    }
                    value.writes += 1;
                    Json(value.run.clone())
                }))
            .with_state(stored.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::try_from(kube::Config::new(
            format!("http://{}", listener.local_addr().unwrap())
                .parse()
                .unwrap(),
        ))
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ctx = Context {
            client: client.clone(),
            namespace: "test".into(),
            runtime: ServiceClient::new("https://runtime.invalid".into(), "x".repeat(64)).unwrap(),
            leader: Leadership {
                api: Api::namespaced(client, "test"),
                identity: Arc::new("leader".into()),
                name: "operator".into(),
            },
        };
        let action = cleanup_run(&ctx, &run, "canceled").await.unwrap();
        let current: AgentRun = serde_json::from_value(stored.lock().unwrap().run.clone()).unwrap();
        let status = current.status.as_ref().unwrap();
        assert_eq!(status.phase, "canceled");
        assert_eq!(
            status.pod_uid.as_deref(),
            Some("pod-a"),
            "Job deletion ACK must not discard an existing Pod identity"
        );
        assert!(
            status
                .message
                .as_deref()
                .unwrap()
                .starts_with("CleanupPending:")
        );
        assert_eq!(action, Action::requeue(Duration::from_secs(5)));
        assert_eq!(
            stored.lock().unwrap().deletions[0]["preconditions"]["uid"],
            "job-a"
        );
        cleanup_run(&ctx, &current, "canceled").await.unwrap();
        assert_eq!(
            stored.lock().unwrap().writes,
            1,
            "a deleted Job with a lingering Pod must remain pending without status churn"
        );
        stored.lock().unwrap().fail_list = true;
        assert!(
            cleanup_run(&ctx, &current, "canceled").await.is_err(),
            "an API failure must retry, never declare cleanup complete"
        );
        assert_eq!(stored.lock().unwrap().writes, 1);
        {
            let mut value = stored.lock().unwrap();
            value.fail_list = false;
            value.pods.clear();
        }
        assert_eq!(
            cleanup_run(&ctx, &current, "canceled").await.unwrap(),
            Action::requeue(Duration::from_secs(60))
        );
        let current: AgentRun = serde_json::from_value(stored.lock().unwrap().run.clone()).unwrap();
        assert!(current.status.as_ref().unwrap().pod_uid.is_none());
        assert!(current.status.as_ref().unwrap().message.is_none());
        cleanup_run(&ctx, &current, "canceled").await.unwrap();
        assert_eq!(stored.lock().unwrap().writes, 2);
        server.abort();
    }

    #[test]
    fn history_retention_does_not_remove_pending_cleanup() {
        use k8s_openapi::{apimachinery::pkg::apis::meta::v1::Time, jiff::Timestamp};
        let now = chrono::Utc::now().timestamp();
        let mut run = AgentRun::new(
            "run-test",
            AgentRunSpec {
                run_id: "run".into(),
                generation: 1,
                pool: "pool".into(),
                snapshot_id: "snapshot".into(),
                snapshot_digest: "a".repeat(64),
            },
        );
        run.metadata.creation_timestamp = Some(Time(
            Timestamp::from_second(now - HISTORY_SECONDS - 1).unwrap(),
        ));
        run.status = Some(Status {
            phase: "canceled".into(),
            message: Some("CleanupPending: waiting for Pod deletion".into()),
            pod_uid: Some("pod-a".into()),
            observed_generation: 1,
        });
        assert!(
            !run_reclaimable(&run, now),
            "history TTL must retain unfinished cleanup"
        );
        run.status.as_mut().unwrap().pod_uid = None;
        assert!(
            !run_reclaimable(&run, now),
            "a pending Job still protects the run"
        );
        run.status.as_mut().unwrap().message = None;
        assert!(run_reclaimable(&run, now));
        run.status.as_mut().unwrap().phase = "Running".into();
        assert!(!run_reclaimable(&run, now));
    }

    #[test]
    fn snapshot_metadata_cleanup_preserves_referenced_recent_and_user_managed_resources() {
        use k8s_openapi::{apimachinery::pkg::apis::meta::v1::Time, jiff::Timestamp};
        let now = chrono::Utc::now().timestamp();
        let mut snapshot = SourceSnapshot::new(
            "snapshot",
            SourceSnapshotSpec {
                snapshot_id: "fixed-snapshot".into(),
                digest: "a".repeat(64),
                source_pool: "sources".into(),
                source_url: "https://source".into(),
            },
        );
        snapshot.metadata.creation_timestamp = Some(Time(
            Timestamp::from_second(now - HISTORY_SECONDS - 1).unwrap(),
        ));
        let mut protected = HashSet::new();
        assert!(
            !snapshot_reclaimable(&snapshot, &protected, now),
            "never collect user-declared CRDs"
        );
        snapshot.metadata.labels = Some([(MANAGED_SNAPSHOT.into(), "true".into())].into());
        protected.insert(snapshot.spec.snapshot_id.clone());
        assert!(
            !snapshot_reclaimable(&snapshot, &protected, now),
            "run references retain their metadata"
        );
        protected.clear();
        assert!(snapshot_reclaimable(&snapshot, &protected, now));
        snapshot.metadata.creation_timestamp = Some(Time(Timestamp::from_second(now).unwrap()));
        assert!(
            !snapshot_reclaimable(&snapshot, &protected, now),
            "preserve the diagnostic retention window"
        );
    }
}
