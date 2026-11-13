use kube::{CustomResource, CustomResourceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub phase: String,
    // Explicit null clears obsolete fields when status uses JSON Merge Patch.
    pub message: Option<String>,
    pub pod_uid: Option<String>,
    pub observed_generation: i64,
}
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.artifactory.dev",
    version = "v1alpha1",
    kind = "AgentPool",
    namespaced,
    status = "Status"
)]
#[serde(rename_all = "camelCase")]
pub struct AgentPoolSpec {
    #[schemars(range(min = 0, max = 1000))]
    pub max_active: u32,
    #[schemars(range(min = 1, max = 1000))]
    pub max_per_project: u32,
    pub image: String,
    #[serde(default = "pull_policy")]
    pub image_pull_policy: String,
    pub runtime_url: String,
    pub source_pool: String,
    #[serde(default = "cpu")]
    pub cpu: String,
    #[serde(default = "memory")]
    pub memory: String,
    #[serde(default)]
    pub tls_secret_name: String,
}
fn cpu() -> String {
    "250m".into()
}
fn pull_policy() -> String {
    "IfNotPresent".into()
}
fn memory() -> String {
    "512Mi".into()
}
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.artifactory.dev",
    version = "v1alpha1",
    kind = "SourcePool",
    namespaced,
    status = "Status"
)]
#[serde(rename_all = "camelCase")]
pub struct SourcePoolSpec {
    #[schemars(range(min = 1, max = 32))]
    pub shards: i32,
    pub image: String,
    #[serde(default = "pull_policy")]
    pub image_pull_policy: String,
    pub runtime_url: String,
    pub control_url: String,
    pub secret_name: String,
    pub storage_class: String,
    pub storage_size: String,
    #[serde(default = "cpu")]
    pub cpu: String,
    #[serde(default = "source_memory")]
    pub memory: String,
    #[serde(default)]
    pub tls_secret_name: String,
    /// Device-reachable download base. Supports {pod}, {pool} and {namespace}.
    /// Internal RPCs continue to use the shard's Kubernetes service address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url_template: Option<String>,
}
impl SourcePoolSpec {
    pub fn download_url(&self, pool: &str, namespace: &str) -> anyhow::Result<String> {
        let value = self.public_url_template.as_ref().map_or_else(
            || {
                format!(
                    "{}://$(POD_NAME).{pool}.{namespace}.svc:8090",
                    if self.tls_secret_name.is_empty() {
                        "http"
                    } else {
                        "https"
                    }
                )
            },
            |template| {
                template
                    .replace("{pod}", "$(POD_NAME)")
                    .replace("{pool}", pool)
                    .replace("{namespace}", namespace)
            },
        );
        let concrete = value.replace("$(POD_NAME)", &format!("{pool}-0"));
        anyhow::ensure!(
            !concrete.contains(['{', '}', '$']),
            "unsupported public URL template variable"
        );
        artifactory_runtime_protocol::service::download_origin(&concrete)?;
        Ok(value.trim_end_matches('/').to_owned())
    }
}
fn source_memory() -> String {
    "1Gi".into()
}
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.artifactory.dev",
    version = "v1alpha1",
    kind = "SourceSnapshot",
    namespaced,
    status = "Status"
)]
#[serde(rename_all = "camelCase")]
pub struct SourceSnapshotSpec {
    pub snapshot_id: String,
    pub digest: String,
    pub source_pool: String,
    pub source_url: String,
}
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "agents.artifactory.dev",
    version = "v1alpha1",
    kind = "AgentRun",
    namespaced,
    status = "Status"
)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunSpec {
    pub run_id: String,
    #[schemars(range(min = 1))]
    pub generation: i64,
    pub pool: String,
    pub snapshot_id: String,
    pub snapshot_digest: String,
}
pub fn crds()
-> Vec<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition>
{
    let mut crds = vec![
        AgentPool::crd(),
        SourcePool::crd(),
        SourceSnapshot::crd(),
        AgentRun::crd(),
    ];
    for crd in &mut crds {
        if matches!(crd.spec.names.kind.as_str(), "AgentRun" | "SourceSnapshot") {
            let mut value = serde_json::to_value(&*crd).unwrap();
            value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"] = serde_json::json!([{"rule":"self == oldSelf","message":"execution and snapshot context are immutable"}]);
            *crd = serde_json::from_value(value).unwrap();
        } else if crd.spec.names.kind == "SourcePool" {
            let mut value = serde_json::to_value(&*crd).unwrap();
            value["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["x-kubernetes-validations"] = serde_json::json!([{"rule":"self.shards >= oldSelf.shards && self.storageClass == oldSelf.storageClass && self.storageSize == oldSelf.storageSize","message":"source shard removal and PVC changes require an explicit data migration"}]);
            *crd = serde_json::from_value(value).unwrap();
        }
    }
    crds
}
