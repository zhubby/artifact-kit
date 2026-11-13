use anyhow::{Result, ensure};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::{
    Api,
    api::{ObjectMeta, PostParams},
};
use std::sync::Arc;
#[derive(Clone)]
pub struct Leadership {
    pub api: Api<Lease>,
    pub identity: Arc<String>,
    pub name: String,
}
impl Leadership {
    pub async fn renew(&self) -> Result<bool> {
        let now = Timestamp::now();
        let previous = self.api.get_opt(&self.name).await?;
        let mut lease = previous.clone().unwrap_or_else(|| Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                ..Default::default()
            },
            ..Default::default()
        });
        let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
        if spec.holder_identity.as_deref() != Some(self.identity.as_str())
            && spec.renew_time.as_ref().is_some_and(|t| {
                t.0 + SignedDuration::from_secs(i64::from(
                    spec.lease_duration_seconds.unwrap_or(15),
                )) > now
            })
        {
            return Ok(false);
        }
        spec.holder_identity = Some(self.identity.to_string());
        spec.lease_duration_seconds = Some(15);
        spec.renew_time = Some(MicroTime(now));
        let result = if previous.is_some() {
            self.api
                .replace(&self.name, &PostParams::default(), &lease)
                .await
        } else {
            self.api.create(&PostParams::default(), &lease).await
        };
        match result {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn ensure(&self) -> Result<()> {
        let lease = self.api.get(&self.name).await?;
        let spec = lease.spec.unwrap_or_default();
        ensure!(
            spec.holder_identity.as_deref() == Some(self.identity.as_str())
                && spec
                    .renew_time
                    .is_some_and(|time| time.0 + SignedDuration::from_secs(10) > Timestamp::now()),
            "operator leadership lost"
        );
        Ok(())
    }
}
