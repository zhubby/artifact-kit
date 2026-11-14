//! Discovery always intersects current project permissions with the frozen build scope.
use super::ControlStore;
use crate::node_query::Query;
use anyhow::Result;
use artifactory_database::{params, sql};
use serde_json::{Value, json};
use uuid::Uuid;

impl ControlStore {
    pub(crate) async fn query_nodes(
        &self,
        build: Uuid,
        q: &Query,
        kind: &str,
    ) -> Result<Vec<Value>> {
        let mut filter = q.metadata.clone();
        for tool in &q.tools {
            filter.insert(format!("tools.{tool}"), json!({"available":true}));
        }
        Ok(sql::scalars(&mut self.pool.clone(),"SELECT jsonb_build_object('node_id',n.id,'name',n.name,'pool_id',n.pool_id,'os',n.os,'arch',n.arch,'labels',n.labels,'capabilities',n.capabilities,'signing_profiles',(SELECT coalesce(jsonb_agg(s),'[]') FROM unnest(n.signing_profiles) s WHERE s=ANY(p.signing_profiles) AND b.planning_context->'signing_profiles' ? s),'startup_id',n.startup_id,'metadata_digest',n.metadata_digest,'metadata_received_at',n.metadata_received_at,'last_seen',n.last_seen,'online',coalesce(n.last_seen>clock_timestamp()-interval '30 seconds' AND n.active_session IS NOT NULL AND n.connection_owner IS NOT NULL AND n.certificate_expires_at>clock_timestamp(),false),'draining',n.draining OR n.ota_draining,'slots',n.slots,'active_tasks',(SELECT count(*) FROM attempts a WHERE a.node_id=n.id AND a.status IN ('leased','running') AND a.lease_expires_at>clock_timestamp())) || CASE WHEN $9='get_node' THEN jsonb_build_object('metadata',n.metadata) ELSE '{}'::jsonb END FROM builds b JOIN projects p ON p.id=b.project_id JOIN nodes n ON n.pool_id=ANY(p.pool_ids) AND b.planning_context->'pool_ids' ? n.pool_id::text WHERE b.id=$1 AND NOT n.revoked AND ($2::text IS NULL OR n.os=$2) AND ($3::text IS NULL OR n.arch=$3) AND n.metadata @> $4 AND n.capabilities @> $5 AND ($6::uuid IS NULL OR n.id>$6) AND ($7::uuid IS NULL OR n.id=$7) ORDER BY n.id LIMIT $8",params![build,&q.os,&q.arch,json!(filter),json!(q.capabilities),q.cursor,q.node_id,i64::from(q.limit.unwrap_or(50))+1,kind]).await?)
    }
}
