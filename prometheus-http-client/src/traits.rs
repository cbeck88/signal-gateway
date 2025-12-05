//! Trait for Prometheus API requests

use crate::{Error, PromResponse};
use reqwest::{Client, Url};
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

/// Trait for types that can be sent as Prometheus API requests
#[async_trait::async_trait]
pub trait PromRequest: Serialize {
    /// The API path for this request type (e.g., "/api/v1/query")
    const PATH: &str;
    /// The output type returned by this request
    type Output<KV: Clone + Debug + DeserializeOwned>: Clone + Debug + DeserializeOwned;

    /// Send the request to the given prometheus host URL
    async fn send<KV>(&self, host: &str) -> Result<Self::Output<KV>, Error>
    where
        KV: Clone + Debug + DeserializeOwned,
    {
        self.send_with_client(&Client::new(), host).await
    }

    /// Send the request using the provided reqwest client
    async fn send_with_client<KV>(
        &self,
        client: &Client,
        host: &str,
    ) -> Result<Self::Output<KV>, Error>
    where
        KV: Clone + Debug + DeserializeOwned,
    {
        let url = Url::parse(host)?.join(Self::PATH)?;

        let resp: PromResponse<Self::Output<KV>> =
            client.get(url).query(&self).send().await?.json().await?;

        let data = resp.into_result()?;
        Ok(data)
    }
}
