use crate::settings::Settings;
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub struct Storage {
    bucket: Bucket,
    public_bucket: Bucket,
    credentials: Credentials,
    client: reqwest::Client,
    temporary_dir: std::path::PathBuf,
}

impl Storage {
    pub fn new(settings: &Settings) -> Result<Self> {
        Ok(Self {
            bucket: Bucket::new(
                settings.s3_endpoint.parse()?,
                UrlStyle::Path,
                settings.s3_bucket.clone(),
                settings.s3_region.clone(),
            )?,
            public_bucket: Bucket::new(
                settings.s3_public_endpoint.parse()?,
                UrlStyle::Path,
                settings.s3_bucket.clone(),
                settings.s3_region.clone(),
            )?,
            credentials: Credentials::new(&settings.s3_access_key, &settings.s3_secret_key),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            temporary_dir: settings.data_dir.clone(),
        })
    }

    pub async fn initialize(&self) -> Result<()> {
        let url = self
            .bucket
            .create_bucket(&self.credentials)
            .sign(Duration::from_secs(60));
        let response = self
            .client
            .put(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("object store connection failed")?;
        ensure!(
            response.status().is_success() || response.status().as_u16() == 409,
            "object store bucket initialization failed: {}",
            response.status()
        );
        Ok(())
    }

    pub fn upload_url(&self, key: &str) -> String {
        self.public_bucket
            .put_object(Some(&self.credentials), key)
            .sign(Duration::from_secs(900))
            .to_string()
    }

    pub fn download_url(&self, key: &str) -> String {
        self.public_bucket
            .get_object(Some(&self.credentials), key)
            .sign(Duration::from_secs(60))
            .to_string()
    }

    pub async fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let url = self
            .bucket
            .put_object(Some(&self.credentials), key)
            .sign(Duration::from_secs(60));
        self.client
            .put(url)
            .body(body)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(|error| error.without_url())?;
        Ok(())
    }

    pub async fn get_log(&self, key: &str) -> Result<Vec<u8>> {
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key)
            .sign(Duration::from_secs(60));
        let mut stream = self
            .client
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(|error| error.without_url())?
            .bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
            ensure!(bytes.len() <= 65536, "oversized log object");
        }
        Ok(bytes)
    }

    pub async fn verify_and_promote(
        &self,
        key: &str,
        final_key: &str,
        expected_size: i64,
        expected_hash: &str,
    ) -> Result<()> {
        use tokio::io::{AsyncSeekExt, AsyncWriteExt};
        let url = self
            .bucket
            .get_object(Some(&self.credentials), key)
            .sign(Duration::from_secs(300));
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(|error| error.without_url())?;
        if let Some(size) = response.content_length() {
            ensure!(size == expected_size as u64, "artifact size does not match");
        }
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        let mut verified = tokio::fs::File::from_std(tempfile::tempfile_in(&self.temporary_dir)?);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            size += chunk.len() as u64;
            ensure!(
                size <= expected_size as u64,
                "artifact exceeds expected size"
            );
            digest.update(&chunk);
            verified.write_all(&chunk).await?;
        }
        ensure!(
            size == expected_size as u64 && hex::encode(digest.finalize()) == expected_hash,
            "artifact integrity verification failed"
        );
        verified.seek(std::io::SeekFrom::Start(0)).await?;
        let url = self
            .bucket
            .put_object(Some(&self.credentials), final_key)
            .sign(Duration::from_secs(300));
        self.client
            .put(url)
            .header(reqwest::header::CONTENT_LENGTH, size)
            .body(reqwest::Body::wrap_stream(
                tokio_util::io::ReaderStream::new(verified),
            ))
            .send()
            .await
            .map_err(reqwest::Error::without_url)?
            .error_for_status()
            .map_err(|error| error.without_url())?;
        Ok(())
    }
}
