//! Startup metadata is durable and cannot be replaced by a retired runtime or connection.
use super::ControlStore;
use artifactory_database::{params, sql};
use artifactory_telemetry_model::nodes::MetadataSnapshot;
use serde_json::{Value, json};
use tonic::Status;
use uuid::Uuid;

impl ControlStore {
    pub(crate) async fn register_metadata_session(
        &self,
        node: Uuid,
        session: Uuid,
        owner: Uuid,
        certificate: &str,
        snapshot: &MetadataSnapshot,
    ) -> Result<(), Status> {
        snapshot
            .validate()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let digest = snapshot.digest().map_err(crate::grpc::internal)?;
        let mut db = self.pool.clone();
        let mut tx = db.transaction().await.map_err(crate::grpc::internal)?;
        let current: Option<Option<Uuid>> = sql::optional_scalar(&mut tx,"SELECT startup_id FROM nodes WHERE id=$1 AND certificate_hash=$2 AND NOT revoked AND certificate_expires_at>clock_timestamp() FOR UPDATE", params![node,certificate]).await.map_err(crate::grpc::internal)?;
        let current =
            current.ok_or_else(|| Status::permission_denied("node authorization expired"))?;
        let known: Option<String> = sql::optional_scalar(
            &mut tx,
            "SELECT digest FROM node_startups WHERE node_id=$1 AND startup_id=$2",
            params![node, snapshot.startup_id],
        )
        .await
        .map_err(crate::grpc::internal)?;
        if let Some(known) = known {
            if current != Some(snapshot.startup_id) || known != digest {
                return Err(Status::failed_precondition(
                    "startup retired or immutable metadata changed",
                ));
            }
            // Reconnects change ownership only. Avoid rewriting the potentially
            // TOASTed metadata on every fleet reconnect.
            sql::execute(
                &mut tx,
                "UPDATE nodes SET active_session=$2,connection_owner=$3,last_seen=clock_timestamp() WHERE id=$1",
                params![node, session, owner],
            )
            .await
            .map_err(crate::grpc::internal)?;
        } else {
            sql::execute(
                &mut tx,
                "INSERT INTO node_startups(node_id,startup_id,digest) VALUES($1,$2,$3)",
                params![node, snapshot.startup_id, &digest],
            )
            .await
            .map_err(crate::grpc::internal)?;
            sql::execute(&mut tx,"UPDATE nodes SET active_session=$2,connection_owner=$3,last_seen=clock_timestamp(),startup_id=$4,metadata=$5,metadata_digest=$6,metadata_received_at=clock_timestamp(),capabilities=$7 WHERE id=$1",params![node,session,owner,snapshot.startup_id,json!(snapshot.metadata),digest,json!(snapshot.capabilities)]).await.map_err(crate::grpc::internal)?;
        }
        tx.commit().await.map_err(crate::grpc::internal)?;
        Ok(())
    }

    pub(crate) async fn retry_metadata(
        &self,
        node: Uuid,
        session: Uuid,
        snapshot: &MetadataSnapshot,
    ) -> Result<(), Status> {
        snapshot
            .validate()
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let valid:bool=sql::scalar(&mut self.pool.clone(),"SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND active_session=$2 AND startup_id=$3 AND metadata_digest=$4 AND NOT revoked AND certificate_expires_at>clock_timestamp())",params![node,session,snapshot.startup_id,snapshot.digest().map_err(crate::grpc::internal)?]).await.map_err(crate::grpc::internal)?;
        if !valid {
            return Err(Status::failed_precondition(
                "metadata startup or connection superseded",
            ));
        }
        Ok(())
    }

    pub async fn node_metadata(&self, node: Uuid) -> anyhow::Result<Option<Value>> {
        Ok(sql::optional_scalar(&mut self.pool.clone(),"SELECT jsonb_build_object('node_id',id,'startup_id',startup_id,'digest',metadata_digest,'received_at',metadata_received_at,'metadata',metadata) FROM nodes WHERE id=$1",params![node]).await?)
    }
}
