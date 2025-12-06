//! Admin netcat TCP client for forwarding messages to a TCP server
//!
//! This module handles admin messages not handled by the gateway by opening a TCP connection,
//! writing the message terminated with CRLF, and reading the response until CRLF.

use async_trait::async_trait;
use conf::Conf;
use signal_gateway::{
    AdminMessage, AdminMessageResponse, Context, MessageHandler, MessageHandlerResult,
};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

/// Configuration for the admin netcat TCP client
#[derive(Clone, Conf, Debug)]
pub struct AdminNetcatConfig {
    /// TCP address to forward admin commands to
    #[conf(long, env)]
    pub tcp_addr: String,
    /// Timeout for connecting, writing, and reading
    #[conf(long, env, default_value = "5s", value_parser = conf_extra::parse_duration)]
    pub timeout: Duration,
}

impl AdminNetcatConfig {
    /// Create a message handler from this config.
    ///
    /// The returned handler opens a TCP connection to the configured address,
    /// writes the message terminated with CRLF, and reads the response until CRLF.
    pub fn into_handler(self) -> Box<dyn MessageHandler> {
        Box::new(AdminNetcatHandler { config: self })
    }
}

/// Message handler that forwards messages to a TCP server.
struct AdminNetcatHandler {
    config: AdminNetcatConfig,
}

#[async_trait]
impl MessageHandler for AdminNetcatHandler {
    async fn handle_verified_signal_message(
        &self,
        msg: AdminMessage,
        _context: &dyn Context,
    ) -> MessageHandlerResult {
        // Connect to server
        let mut stream = timeout(
            self.config.timeout,
            TcpStream::connect(&self.config.tcp_addr),
        )
        .await
        .map_err(|_| (504u16, "connecting: timeout".into()))?
        .map_err(|err| (502u16, format!("connecting: {err}").into()))?;

        // Write message with CRLF terminator
        let message = format!("{}\r\n", msg.message);
        timeout(self.config.timeout, stream.write_all(message.as_bytes()))
            .await
            .map_err(|_| (504u16, "writing: timeout".into()))?
            .map_err(|err| (502u16, format!("writing: {err}").into()))?;

        // Read response until CR
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        timeout(self.config.timeout, reader.read_until(b'\r', &mut buf))
            .await
            .map_err(|_| (504u16, "reading: timeout".into()))?
            .map_err(|err| (502u16, format!("reading: {err}").into()))?;

        // Convert to string and trim the trailing CR
        let text = std::str::from_utf8(&buf)
            .map_err(|err| (502u16, format!("utf8: {err}").into()))?
            .trim_end_matches(['\r', '\n'])
            .to_owned();

        Ok(AdminMessageResponse::new(text))
    }
}
