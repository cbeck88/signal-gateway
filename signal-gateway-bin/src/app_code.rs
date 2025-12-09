//! Extended configuration for application source code browsing.

use serde::Deserialize;
use signal_gateway_app_code::{AppCode, AppCodeConfig, ShaCallback};
use std::sync::Arc;
use tracing::warn;
use url::Url;

/// Extended configuration for AppCode with HTTP-based SHA fetching.
#[derive(Clone, Debug, Deserialize)]
pub struct AppCodeConfigExt {
    /// The base AppCode configuration.
    #[serde(flatten)]
    pub config: AppCodeConfig,
    /// URL to GET the current deployed version SHA.
    pub version_sha_http_get: Url,
}

impl AppCodeConfigExt {
    /// Convert to an AppCode instance with HTTP-based SHA callback.
    pub fn into_app_code(self) -> Result<AppCode, std::io::Error> {
        let url = self.version_sha_http_get.clone();
        let client = reqwest::Client::new();

        let sha_callback: ShaCallback = Arc::new(move || {
            let url = url.clone();
            let client = client.clone();
            Box::pin(async move {
                let response = client
                    .get(url.as_str())
                    .send()
                    .await
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                        Box::new(std::io::Error::other(format!(
                            "HTTP request to {url} failed: {e}"
                        )))
                    })?;

                if !response.status().is_success() {
                    return Err(Box::new(std::io::Error::other(format!(
                        "HTTP request to {url} returned {}",
                        response.status()
                    ))) as Box<dyn std::error::Error + Send + Sync>);
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

        AppCode::new(self.config, sha_callback)
    }
}
