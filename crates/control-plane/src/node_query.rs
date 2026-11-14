//! Project-scoped node discovery and bounded bulk resource reads for planning and admission.
use crate::state::AppState;
use anyhow::{Result, ensure};
use artifactory_telemetry_model::nodes::{MAX_QUERY_BYTES, ResourceIdentity, Resources};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Query {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub metadata: std::collections::BTreeMap<String, Value>,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    pub limit: Option<u32>,
    pub cursor: Option<Uuid>,
    pub node_id: Option<Uuid>,
}
impl Query {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.limit.is_none_or(|n| (1..=100).contains(&n)),
            "node page limit must be 1 to 100"
        );
        ensure!(
            self.os
                .as_ref()
                .is_none_or(|s| ["linux", "darwin", "windows"].contains(&s.as_str()))
                && self
                    .arch
                    .as_ref()
                    .is_none_or(|s| ["amd64", "arm64"].contains(&s.as_str())),
            "invalid platform"
        );
        ensure!(
            self.metadata.len() <= 32
                && self.capabilities.len() <= 32
                && self.tools.len() <= 32
                && serde_json::to_vec(&self.metadata)?.len() <= 8192,
            "oversized node filter"
        );
        Ok(())
    }
}

pub(crate) async fn resources(
    state: &AppState,
    identities: &[ResourceIdentity],
) -> Result<HashMap<Uuid, Resources>> {
    ensure!(identities.len() <= 512, "resource batch exceeds 512 nodes");
    if identities.is_empty() {
        return Ok(HashMap::new());
    }
    let Some(cfg) = &state.settings.telemetry else {
        return Ok(HashMap::new());
    };
    let client = state
        .coordination
        .telemetry_client
        .get_or_try_init(|| async {
            let ca = tokio::fs::read(state.settings.pki_dir.join("ca.pem")).await?;
            Ok::<_, anyhow::Error>(
                reqwest::Client::builder()
                    .add_root_certificate(reqwest::Certificate::from_pem(&ca)?)
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(std::time::Duration::from_millis(400))
                    .build()?,
            )
        })
        .await?;
    let response = client
        .post(format!(
            "{}/v1/resources",
            cfg.query_url.trim_end_matches('/')
        ))
        .bearer_auth(&cfg.token)
        .json(identities)
        .timeout(std::time::Duration::from_millis(750))
        .send()
        .await?
        .error_for_status()?;
    let rows: Vec<Value> =
        artifactory_runtime_protocol::service::decode(response, 512 * 1024).await?;
    let mut result = HashMap::new();
    for mut row in rows {
        let id: Uuid = serde_json::from_value(row["node_id"].take())?;
        row.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid resource object"))?
            .remove("node_id");
        let resources: Resources = serde_json::from_value(row)?;
        if identities
            .iter()
            .any(|identity| identity.node_id == id && identity.startup_id == resources.startup_id)
        {
            result.insert(id, resources);
        }
    }
    Ok(result)
}

pub(crate) async fn tool(state: &AppState, build: Uuid, name: &str, args: &Value) -> Result<Value> {
    if name == "summarize_nodes" || name == "node_capabilities" {
        return Ok(
            json!({"nodes":state.store.planning_nodes(build).await?,"queried_at":chrono::Utc::now()}),
        );
    }
    let mut query: Query = serde_json::from_value(args.clone())?;
    query.validate()?;
    if name == "get_node" {
        ensure!(query.node_id.is_some(), "node_id is required");
        query.limit = Some(1);
    }
    let nodes = state.store.query_nodes(build, &query, name).await?;
    let identities: Vec<_> = nodes
        .iter()
        .filter_map(|n| {
            Some(ResourceIdentity {
                node_id: serde_json::from_value(n["node_id"].clone()).ok()?,
                startup_id: serde_json::from_value(n["startup_id"].clone()).ok()?,
            })
        })
        .collect();
    let resources = resources(state, &identities).await.unwrap_or_default();
    let now = chrono::Utc::now();
    let mut page = json!({"nodes":[],"queried_at":now,"next_cursor":null,"truncated":false});
    let mut size = serde_json::to_vec(&page)?.len() + 128;
    let mut last = None;
    for mut node in nodes
        .iter()
        .take(query.limit.unwrap_or(50) as usize)
        .cloned()
    {
        let id: Uuid = serde_json::from_value(node["node_id"].clone())?;
        let resource = resources.get(&id);
        node["resources"] = json!(resource);
        node["resources_fresh"] = json!(resource.is_some_and(|r| r.fresh(r.startup_id, now)));
        let bytes = serde_json::to_vec(&node)?.len() + 1;
        if size + bytes > MAX_QUERY_BYTES {
            page["truncated"] = json!(true);
            break;
        }
        size += bytes;
        page["nodes"]
            .as_array_mut()
            .expect("page nodes is an array")
            .push(node);
        last = Some(id);
    }
    if nodes.len()
        > page["nodes"]
            .as_array()
            .expect("page nodes is an array")
            .len()
    {
        page["next_cursor"] = json!(last);
        page["truncated"] = json!(true);
    }
    ensure!(
        serde_json::to_vec(&page)?.len() <= MAX_QUERY_BYTES,
        "node response budget exceeded"
    );
    Ok(page)
}
