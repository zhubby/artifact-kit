//! Persisted ranked admission and bounded, placement-only planning rounds.
use super::ControlStore;
use anyhow::{Result, ensure};
use artifactory_database::{params, sql};
use artifactory_telemetry_model::nodes::{Placement, ResourceIdentity, Resources};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use toasty::stmt::Type;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct Admission {
    pub(crate) versions: HashMap<Uuid, i64>,
    pub(crate) placements: HashMap<Uuid, Placement>,
    pub(crate) identities: Vec<ResourceIdentity>,
    pub(crate) resources: HashMap<Uuid, Resources>,
}
impl Admission {
    pub(crate) fn eligible(&self, job: Uuid, version: i64) -> Value {
        if self.versions.get(&job) != Some(&version) {
            return json!([]);
        }
        json!(self.placements.get(&job).into_iter().flat_map(|p|p.candidates.iter().filter_map(|candidate| {
            let resources=self.resources.get(&candidate.node_id)?;
            resources.satisfies(resources.startup_id,&p.resources,chrono::Utc::now()).then(||json!({"node_id":candidate.node_id,"digest":candidate.metadata_digest,"startup_id":resources.startup_id,"sampled_at":resources.sampled_at,"received_at":resources.received_at}))
        })).collect::<Vec<_>>())
    }
}

impl ControlStore {
    pub(crate) async fn placement_context(&self, run: Uuid) -> Result<Option<Value>> {
        Ok(sql::optional_scalar(&mut self.pool.clone(),"SELECT jsonb_build_object('round',r.round,'targets',r.versions,'current_placements',(SELECT coalesce(jsonb_object_agg(j.target,jsonb_build_object('placement',p.placement,'reason',p.reason)),'{}') FROM jobs j JOIN job_placements p ON p.job_id=j.id WHERE j.build_id=r.build_id AND j.status='queued')) FROM placement_rounds r WHERE r.run_id=$1",params![run]).await?)
    }

    pub(crate) async fn queue_placements(&self) -> Result<()> {
        let mut db = self.pool.clone();
        let mut tx = db.transaction().await?;
        let builds=sql::scalars::<Uuid>(&mut tx,"SELECT b.id FROM builds b WHERE b.status IN ('queued','running') AND NOT b.cancel_requested AND b.planning_context->>'placement_version'='1' AND b.model_config_snapshot IS NOT NULL AND EXISTS(SELECT 1 FROM jobs j JOIN job_placements p ON p.job_id=j.id WHERE j.build_id=b.id AND j.status='queued' AND p.waiting_since<clock_timestamp()-interval '30 seconds') AND (SELECT count(*) FROM placement_rounds r WHERE r.build_id=b.id)<2 AND NOT EXISTS(SELECT 1 FROM planning_runs r WHERE r.build_id=b.id AND r.state IN ('queued','leased')) AND NOT EXISTS(SELECT 1 FROM placement_rounds r WHERE r.build_id=b.id AND r.created_at>clock_timestamp()-interval '60 seconds') ORDER BY b.created_at LIMIT 10 FOR UPDATE OF b SKIP LOCKED",params![]).await?;
        for build in builds {
            let mut config: Value = sql::scalar(
                &mut tx,
                "SELECT model_config_snapshot FROM builds WHERE id=$1",
                params![build],
            )
            .await?;
            config["max_calls"] = json!(config["max_calls"].as_u64().unwrap_or(20).min(5));
            let timeout = config["timeout_seconds"].as_u64().unwrap_or(600).min(60);
            config["timeout_seconds"] = json!(timeout);
            let versions:Value=sql::scalar(&mut tx,"SELECT coalesce(jsonb_object_agg(j.target,jsonb_build_object('job_id',j.id,'version',p.version)),'{}') FROM jobs j JOIN job_placements p ON p.job_id=j.id WHERE j.build_id=$1 AND j.status='queued'",params![build]).await?;
            let run = Uuid::new_v4();
            // Consuming a round is durable before any worker or model is invoked.
            sql::execute(&mut tx,"INSERT INTO planning_runs(id,build_id,purpose,model_config,deadline_at) VALUES($1,$2,'placement',$3,clock_timestamp()+make_interval(secs=>$4))",params![run,build,config,timeout as f64]).await?;
            sql::execute(&mut tx,"INSERT INTO placement_rounds(build_id,round,run_id,versions) SELECT $1,coalesce(max(round),0)+1,$2,$3 FROM placement_rounds WHERE build_id=$1",params![build,run,versions]).await?;
        }
        tx.commit().await?;
        // Expired queued/provisioning Runtime work also consumes its round and cannot
        // accept late results. Do not alter the build or any running job.
        sql::execute(&mut self.pool.clone(),"UPDATE planning_runs SET state='canceled',finished_at=clock_timestamp(),error='placement deadline reached' WHERE purpose='placement' AND state IN ('queued','leased') AND deadline_at<=clock_timestamp()",params![]).await?;
        Ok(())
    }

    pub(crate) async fn placement_admission(&self, limit: i64) -> Result<Admission> {
        let rows=sql::records(&mut self.pool.clone(),"SELECT p.job_id,p.version,p.placement FROM job_placements p JOIN jobs j ON j.id=p.job_id JOIN builds b ON b.id=j.build_id WHERE j.status='queued' AND j.dispatch_after<=clock_timestamp() AND b.status IN ('queued','running') AND NOT b.cancel_requested ORDER BY j.dispatch_after,j.created_at,j.id LIMIT $1",params![limit.clamp(1,64)],&[("job_id",Type::Uuid),("version",Type::I64),("placement",Type::String)]).await?;
        let mut admission = Admission::default();
        let mut ids = Vec::new();
        for row in rows {
            let id: Uuid = row.decode("job_id")?;
            let placement: Placement = serde_json::from_value(row.decode("placement")?)?;
            ids.extend(placement.candidates.iter().map(|c| c.node_id));
            admission.versions.insert(id, row.decode("version")?);
            admission.placements.insert(id, placement);
        }
        if ids.is_empty() {
            return Ok(admission);
        }
        admission.identities=sql::scalars::<Value>(&mut self.pool.clone(),"SELECT jsonb_build_object('node_id',id,'startup_id',startup_id) FROM nodes WHERE id=ANY($1) AND startup_id IS NOT NULL",params![ids]).await?.into_iter().map(serde_json::from_value).collect::<std::result::Result<_,_>>()?;
        Ok(admission)
    }
}

pub(super) async fn persist_placements(
    tx: &mut toasty::db::Transaction<'_>,
    row: &sql::Record,
    args: &Value,
) -> Result<Value> {
    ensure!(
        row.decode::<String>("purpose")? == "placement",
        "not a placement planning run"
    );
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Proposal {
        placements: BTreeMap<String, Placement>,
    }
    let proposal: Proposal = serde_json::from_value(args.clone())?;
    ensure!(
        !proposal.placements.is_empty() && proposal.placements.len() <= 32,
        "invalid placement target count"
    );
    for placement in proposal.placements.values() {
        placement.validate()?;
    }
    let run: Uuid = row.decode("id")?;
    let versions: Value = sql::scalar(
        tx,
        "SELECT versions FROM placement_rounds WHERE run_id=$1",
        params![run],
    )
    .await?;
    let build: Uuid = row.decode("build_id")?;
    // The build row is already locked, so assignment and cancellation serialize here.
    let jobs=sql::records(tx,"SELECT j.id,j.target,p.version,p.placement FROM jobs j JOIN job_placements p ON p.job_id=j.id WHERE j.build_id=$1 AND j.status='queued' FOR UPDATE OF j,p",params![build],&[("id",Type::Uuid),("target",Type::String),("version",Type::I64),("placement",Type::String)]).await?;
    let mut updated = 0;
    for (target, placement) in proposal.placements {
        let expected = versions
            .get(&target)
            .ok_or_else(|| anyhow::anyhow!("target outside placement round"))?;
        if let Some(job) = jobs.iter().find(|job| {
            job.decode::<String>("target")
                .is_ok_and(|name| name == target)
        }) {
            let previous: Placement = serde_json::from_value(job.decode("placement")?)?;
            ensure!(
                placement.resources == previous.resources,
                "placement rounds cannot change resource requirements"
            );
            ensure!(
                expected["job_id"] == json!(job.decode::<Uuid>("id")?)
                    && expected["version"] == json!(job.decode::<i64>("version")?),
                "placement version superseded"
            );
            sql::execute(tx,"UPDATE job_placements SET placement=$2,version=version+1,waiting_since=NULL,reason=NULL,selected_node=NULL,selected_reason=NULL WHERE job_id=$1",params![job.decode::<Uuid>("id")?,serde_json::to_value(placement)?]).await?;
            sql::execute(
                tx,
                "UPDATE jobs SET dispatch_after=clock_timestamp() WHERE id=$1",
                params![job.decode::<Uuid>("id")?],
            )
            .await?;
            updated += 1;
        }
    }
    sql::execute(tx,"UPDATE planning_runs SET state='succeeded',finished_at=clock_timestamp() WHERE id=$1 AND state IN ('queued','leased')",params![run]).await?;
    Ok(json!({"accepted":true,"updated_targets":updated}))
}
