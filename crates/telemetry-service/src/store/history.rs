//! Bounded historical reads through the independent telemetry read pool.
use super::TelemetryStore;
use crate::query::{Range, Resolution};
use artifactory_database::{params, sql};
use artifactory_runtime_protocol::error::{ApiError, Result};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;
#[derive(Serialize)]
pub struct HistoryPage {
    pub samples: Vec<Value>,
    pub truncated: bool,
}
#[derive(Serialize)]
pub struct LatestTelemetry {
    pub host: Option<Value>,
    pub inventory: Option<Value>,
}
async fn bounded(
    statement: &str,
    parameters: sql::Parameters,
    pool: &toasty::Db,
    limit: usize,
) -> Result<HistoryPage> {
    // A server cursor holds one repeatable-read snapshot while each fetch buffers
    // at most 128 rows. Dropping the transaction also closes the cursor.
    let mut db = pool.clone();
    let mut tx = db
        .transaction_builder()
        .read_only(true)
        .isolation(artifactory_database::IsolationLevel::RepeatableRead)
        .begin()
        .await?;
    sql::execute(
        &mut tx,
        &format!("DECLARE telemetry_history NO SCROLL CURSOR FOR {statement}"),
        parameters,
    )
    .await?;
    let mut samples = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    'pages: loop {
        let page: Vec<Value> = sql::scalars(
            &mut tx,
            "FETCH FORWARD 128 FROM telemetry_history",
            params![],
        )
        .await?;
        let done = page.len() < 128;
        for value in page {
            bytes += serde_json::to_vec(&value)?.len();
            if bytes > 2 * 1024 * 1024 || samples.len() >= limit {
                truncated = true;
                break 'pages;
            }
            samples.push(value);
        }
        if done {
            break;
        }
    }
    tx.commit().await?;
    Ok(HistoryPage { samples, truncated })
}

impl TelemetryStore {
    pub async fn latest(&self, id: Uuid) -> Result<LatestTelemetry> {
        let host:Option<Value>=sql::optional_scalar(&mut self.read.clone(), "SELECT to_jsonb(h) || jsonb_build_object('fresh', sampled_at BETWEEN now()-interval '90 seconds' AND now()+interval '30 seconds' AND received_at>now()-interval '90 seconds') FROM latest_hosts h WHERE node_id=$1", params![id]).await?;
        let inventory:Option<Value>=sql::optional_scalar(&mut self.read.clone(), "SELECT jsonb_build_object('sampled_at',i.sampled_at,'digest',i.digest,'inventory',v.inventory) FROM latest_inventories i JOIN inventory_versions v USING(node_id,digest) WHERE i.node_id=$1", params![id]).await?;
        Ok(LatestTelemetry { host, inventory })
    }

    pub async fn inventory(&self, id: Uuid) -> Result<HistoryPage> {
        bounded("SELECT jsonb_build_object('sampled_at',o.sampled_at,'digest',o.digest,'inventory',v.inventory) FROM inventory_observations o JOIN inventory_versions v USING(node_id,digest) WHERE o.node_id=$1 ORDER BY o.sampled_at DESC LIMIT 101", params![id], &self.read, 100).await
    }

    pub async fn history(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
        let (table, column, projection) = match r.resolution {
            Resolution::Raw => ("host_samples", "sampled_at", "to_jsonb(t)-'batch_id'"),
            Resolution::Minute => (
                "host_minutes",
                "bucket",
                "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'memory_avg',memory_sum/NULLIF(memory_samples,0))",
            ),
            Resolution::Hour => (
                "host_hours",
                "bucket",
                "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'memory_avg',memory_sum/NULLIF(memory_samples,0))",
            ),
        };
        let sql = format!(
            "SELECT {projection} FROM {table} t WHERE node_id=$1 AND {column}>=$2 AND {column}<$3 ORDER BY {column} LIMIT 6001"
        );
        bounded(&sql, params![id, r.from, r.to], &self.read, 6000).await
    }

    pub async fn build_history(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
        let (table, column, projection) = match r.resolution {
            Resolution::Raw => ("build_samples", "sampled_at", "to_jsonb(t)-'batch_id'"),
            Resolution::Minute => (
                "build_minutes",
                "bucket",
                "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'rss_avg',rss_sum/NULLIF(rss_samples,0))",
            ),
            Resolution::Hour => (
                "build_hours",
                "bucket",
                "to_jsonb(t) || jsonb_build_object('cpu_avg',cpu_sum/NULLIF(cpu_samples,0),'rss_avg',rss_sum/NULLIF(rss_samples,0))",
            ),
        };
        let sql = format!(
            "SELECT {projection} FROM {table} t WHERE attempt_id=$1 AND {column}>=$2 AND {column}<$3 ORDER BY {column} LIMIT 6001"
        );
        bounded(&sql, params![id, r.from, r.to], &self.read, 6000).await
    }

    pub async fn processes(&self, id: Uuid, r: &Range) -> Result<HistoryPage> {
        if !matches!(r.resolution, Resolution::Raw) {
            return Err(ApiError::BadRequest(
                "individual process history requires raw resolution".into(),
            ));
        }
        bounded("SELECT to_jsonb(p)-'batch_id' FROM process_samples p WHERE attempt_id=$1 AND sampled_at>=$2 AND sampled_at<$3 ORDER BY sampled_at,pid LIMIT 6001", params![id,r.from,r.to], &self.read,6000).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifactory_database::{PoolSettings, connect, with_options};
    use std::time::Duration;

    fn expect_history(result: Result<HistoryPage>) -> HistoryPage {
        result.unwrap_or_else(|error| match error {
            ApiError::Internal(error) => panic!("{error:#}"),
            _ => panic!("history query failed"),
        })
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL"]
    async fn history_limits_and_snapshot_survive_writes_between_pages() {
        let url = std::env::var("ARTIFACTORY_TEST_DATABASE_URL").unwrap();
        let admin = connect(&url, Default::default(), PoolSettings::new(2))
            .await
            .unwrap();
        let schema = format!("history_{}", Uuid::new_v4().simple());
        sql::execute(
            &mut admin.clone(),
            &format!("CREATE SCHEMA {schema}"),
            params![],
        )
        .await
        .unwrap();
        let db = connect(
            &with_options(&url, &[("search_path", &schema)]).unwrap(),
            Default::default(),
            PoolSettings::new(2),
        )
        .await
        .unwrap();
        sql::execute(
            &mut db.clone(),
            "CREATE TABLE samples(id integer PRIMARY KEY,value text NOT NULL)",
            params![],
        )
        .await
        .unwrap();
        sql::execute(
            &mut db.clone(),
            "INSERT INTO samples SELECT i,'original' FROM generate_series(1,256) i",
            params![],
        )
        .await
        .unwrap();

        let key = i64::from(Uuid::new_v4().as_u128() as i32);
        let mut writer_db = db.clone();
        let mut writer = writer_db.transaction().await.unwrap();
        sql::execute(
            &mut writer,
            "SELECT pg_advisory_xact_lock($1)",
            params![key],
        )
        .await
        .unwrap();
        let reader = db.clone();
        let pending = tokio::spawn(async move {
            expect_history(bounded("SELECT jsonb_build_object('id',id,'value',value,'guard',CASE WHEN id=129 THEN pg_advisory_xact_lock($1) IS NULL ELSE false END) FROM (SELECT * FROM samples ORDER BY id OFFSET 0) s", params![key], &reader, 6000).await)
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = sql::scalar(&mut admin.clone(), "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event='advisory' AND query LIKE 'FETCH FORWARD 128%')", params![]).await.unwrap();
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        sql::execute(
            &mut writer,
            "UPDATE samples SET value='changed' WHERE id>=200",
            params![],
        )
        .await
        .unwrap();
        sql::execute(
            &mut writer,
            "INSERT INTO samples VALUES(257,'new')",
            params![],
        )
        .await
        .unwrap();
        writer.commit().await.unwrap();
        let page = pending.await.unwrap();
        assert_eq!(page.samples.len(), 256);
        assert!(!page.truncated);
        assert!(
            page.samples
                .iter()
                .all(|sample| sample["value"] == "original")
        );

        for (count, truncated) in [(6000, false), (6001, true)] {
            let page = expect_history(
                bounded(
                    "SELECT to_jsonb(i) FROM generate_series(1,$1::integer) i",
                    params![count],
                    &db,
                    6000,
                )
                .await,
            );
            assert_eq!(page.samples.len(), 6000);
            assert_eq!(page.truncated, truncated);
        }
        let page = expect_history(
            bounded(
                "SELECT to_jsonb(repeat('x',65536)) FROM generate_series(1,6001)",
                params![],
                &db,
                6000,
            )
            .await,
        );
        assert!(page.truncated);
        assert_eq!(page.samples.len(), 31);
        assert!(
            page.samples
                .iter()
                .map(|v| serde_json::to_vec(v).unwrap().len())
                .sum::<usize>()
                <= 2 * 1024 * 1024
        );
        sql::execute(
            &mut admin.clone(),
            &format!("DROP SCHEMA {schema} CASCADE"),
            params![],
        )
        .await
        .unwrap();
    }
}
