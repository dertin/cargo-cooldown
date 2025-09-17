use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::config::Config;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionMeta {
    pub created_at: DateTime<Utc>,
    pub yanked: bool,
    #[serde(default)]
    pub num: String,
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    version: VersionMeta,
}

#[derive(Debug, Deserialize)]
struct CrateResponse {
    versions: Vec<VersionMeta>,
}

#[derive(Clone)]
pub struct RegistryClient {
    http: Client,
    base: Url,
    retries: u32,
}

impl RegistryClient {
    pub fn new(config: &Config) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("cargo-cooldown/0.1")
            .build()?;
        let base = Url::parse(&config.registry_api).context("invalid registry API URL")?;
        Ok(Self {
            http,
            base,
            retries: config.http_retries,
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: Url) -> Result<T> {
        let mut attempt = 0;
        loop {
            let response = self.http.get(url.clone()).send().await;
            match response {
                Ok(resp) => {
                    let status_resp = resp.error_for_status()?;
                    let value = status_resp.json::<T>().await?;
                    return Ok(value);
                }
                Err(err) => {
                    attempt += 1;
                    if attempt > self.retries {
                        return Err(err.into());
                    }
                    let backoff = Duration::from_millis(200 * attempt as u64);
                    sleep(backoff).await;
                }
            }
        }
    }

    pub async fn fetch_version(&self, name: &str, version: &str) -> Result<VersionMeta> {
        let url = self
            .base
            .join(&format!("crates/{}/{}", name, version))
            .with_context(|| format!("failed to build version URL for {name}:{version}"))?;
        let resp: VersionResponse = self.get_json(url).await?;
        Ok(resp.version)
    }

    pub async fn list_versions(&self, name: &str) -> Result<Vec<VersionMeta>> {
        let url = self
            .base
            .join(&format!("crates/{}", name))
            .with_context(|| format!("failed to build crate URL for {name}"))?;
        let resp: CrateResponse = self.get_json(url).await?;
        Ok(resp.versions)
    }
}
