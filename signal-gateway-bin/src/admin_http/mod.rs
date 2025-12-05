//! Admin HTTP client for forwarding messages to an HTTP server
//!
//! This module handles admin messages not handled by the gateway by making an HTTP POST request
//! with the message as the body, and returning the response body as the reply.

use async_trait::async_trait;
use conf::Conf;
use signal_gateway::{
    AdminMessageResponse, Context, MessageHandler, MessageHandlerResult, VerifiedSignalMessage,
};
use std::time::Duration;

/// Configuration for the admin HTTP client
#[derive(Clone, Conf, Debug)]
pub struct AdminHttpConfig {
    /// URL to POST admin commands to
    #[conf(long, env)]
    pub url: String,
    /// Timeout for the HTTP request
    #[conf(long, env, default_value = "5s", value_parser = conf_extra::parse_duration)]
    pub timeout: Duration,
}

impl AdminHttpConfig {
    /// Create a message handler from this config.
    ///
    /// The returned handler makes an HTTP POST request to the configured URL
    /// with the message as the body, and returns the response body.
    pub fn into_handler(self) -> Box<dyn MessageHandler> {
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .expect("Failed to build HTTP client");

        Box::new(AdminHttpHandler {
            client,
            url: self.url,
        })
    }
}

/// Message handler that forwards messages to an HTTP server.
struct AdminHttpHandler {
    client: reqwest::Client,
    url: String,
}

#[async_trait]
impl MessageHandler for AdminHttpHandler {
    async fn handle_verified_signal_message(
        &self,
        msg: VerifiedSignalMessage,
        _context: &dyn Context,
    ) -> MessageHandlerResult {
        let response = self
            .client
            .post(&self.url)
            .body(msg.message)
            .send()
            .await
            .map_err(|err| (502u16, format!("HTTP request failed: {err}").into()))?;

        let status = response.status();
        let body = response.text().await.map_err(|err| {
            (
                502u16,
                format!("Failed to read response body: {err}").into(),
            )
        })?;

        if !status.is_success() {
            return Err((status.as_u16(), body.into()));
        }

        Ok(AdminMessageResponse::new(body))
    }
}
