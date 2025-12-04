use crate::{Error, PromResponse};
use reqwest::{Client, Url};
use serde::{Serialize, de::DeserializeOwned};
use std::fmt::Debug;

#[async_trait::async_trait]
pub trait PromRequest: Serialize {
    const PATH: &str;
    type Output<KV: Clone + Debug + DeserializeOwned>: Clone + Debug + DeserializeOwned;

    async fn send<KV>(&self, host: &str) -> Result<Self::Output<KV>, Error>
    where
        KV: Clone + Debug + DeserializeOwned,
    {
        self.send_with_client(&Client::new(), host).await
    }

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
