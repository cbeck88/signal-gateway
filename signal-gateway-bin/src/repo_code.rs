//! Extended configuration for application source code browsing.

use serde::Deserialize;
use signal_gateway_repo_code::{RepoCode, RepoCodeConfig, ShaCallback};
use std::sync::Arc;
use tracing::warn;
use url::Url;

/// Extended configuration for RepoCode with HTTP-based SHA fetching.
#[derive(Clone, Debug, Deserialize)]
pub struct RepoCodeConfigExt {
    /// The base RepoCode configuration.
    #[serde(flatten)]
    pub config: RepoCodeConfig,
    /// URL to GET the current deployed version SHA.
    pub version_sha_http_get: Url,
}

impl RepoCodeConfigExt {
    /// Convert to an RepoCode instance with HTTP-based SHA callback.
    pub fn into_app_code(self) -> Result<RepoCode, std::io::Error> {
        let url = self.version_sha_http_get.clone();
        let client = reqwest::Client::new();

        let sha_callback: ShaCallback = Arc::new(move || {
            let url = url.clone();
            let client = client.clone();
            Box::pin(async move {
                let response = client.get(url.as_str()).send().await.map_err(
                    |e| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(std::io::Error::other(format!(
                            "HTTP request to {url} failed: {e}"
                        )))
                    },
                )?;

                if !response.status().is_success() {
                    return Err(Box::new(std::io::Error::other(format!(
                        "HTTP request to {url} returned {}",
                        response.status()
                    )))
                        as Box<dyn std::error::Error + Send + Sync>);
                }

                let mut sha = response
                    .text()
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(std::io::Error::other(format!(
                            "Failed to read response from {url}: {e}"
                        )))
                    })?
                    .trim()
                    .to_string();

                // Handle -dirty suffix
                if let Some(clean_sha) = sha.strip_suffix("-dirty") {
                    warn!(
                        "Version SHA has -dirty suffix, using clean SHA: {}",
                        clean_sha
                    );
                    sha = clean_sha.to_string();
                }

                Ok(sha)
            })
        });

        RepoCode::new(self.config, sha_callback)
    }
}
