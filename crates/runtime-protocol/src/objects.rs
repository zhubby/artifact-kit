use crate::service::{client, required};
use anyhow::{Result, ensure};
use futures::StreamExt;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::{path::Path, time::Duration};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

#[derive(Clone)]
pub struct Objects {
    bucket: Bucket,
    credentials: Credentials,
    http: reqwest::Client,
}
impl Objects {
    pub async fn initialize(&self) -> Result<()> {
        let response = self
            .http
            .put(
                self.bucket
                    .create_bucket(&self.credentials)
                    .sign(Duration::from_secs(60)),
            )
            .send()
            .await?;
        ensure!(
            response.status().is_success() || response.status().as_u16() == 409,
            "object bucket unavailable"
        );
        Ok(())
    }
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bucket: Bucket::new(
                required("ARTIFACTORY_S3_ENDPOINT")?.parse()?,
                UrlStyle::Path,
                required("ARTIFACTORY_S3_BUCKET")?,
                std::env::var("ARTIFACTORY_S3_REGION").unwrap_or("us-east-1".into()),
            )?,
            credentials: Credentials::new(
                required("ARTIFACTORY_S3_ACCESS_KEY")?,
                required("ARTIFACTORY_S3_SECRET_KEY")?,
            ),
            http: client()?,
        })
    }
    pub async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.http
            .put(
                self.bucket
                    .put_object(Some(&self.credentials), key)
                    .sign(Duration::from_secs(3600)),
            )
            .body(bytes)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        self.http
            .put(
                self.bucket
                    .put_object(Some(&self.credentials), key)
                    .sign(Duration::from_secs(3600)),
            )
            .header("content-length", size)
            .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
    pub async fn get_file(&self, key: &str, path: &Path, limit: u64) -> Result<()> {
        let response = self
            .http
            .get(
                self.bucket
                    .get_object(Some(&self.credentials), key)
                    .sign(Duration::from_secs(3600)),
            )
            .send()
            .await?
            .error_for_status()?;
        let mut stream = response.bytes_stream();
        let mut file = tokio::fs::File::create(path).await?;
        let mut size = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            size += chunk.len() as u64;
            ensure!(size <= limit, "object exceeds size limit");
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        Ok(())
    }
}
