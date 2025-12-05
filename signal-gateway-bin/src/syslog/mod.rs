//! Syslog UDP listener for receiving RFC 5424 syslog messages

use conf::Conf;
use signal_gateway::{Gateway, Level, LogMessage};
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use syslog_rfc5424::{SyslogMessage, SyslogSeverity};
use tokio::net::UdpSocket;
use tracing::{error, info};

/// Configuration for the syslog UDP listener
#[derive(Conf, Debug)]
pub struct SyslogUdpConfig {
    /// Socket to listen for UDP messages, in syslog RFC 5424 format
    #[conf(long, env)]
    pub listen_addr: SocketAddr,
    /// Structured data ID for tracing metadata (module, file, line) in syslog messages
    #[conf(long, env, default_value = "tracing-meta@64700")]
    pub sd_id: String,
}

impl SyslogUdpConfig {
    /// Bind a UDP socket and start a background task to handle incoming syslog messages.
    ///
    /// Returns a join handle for the background task.
    pub async fn start_udp_task(
        &self,
        gateway: Arc<Gateway>,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let udp_socket = UdpSocket::bind(self.listen_addr).await?;
        info!("Listening for syslog UDP on {}", self.listen_addr);

        let sd_id = self.sd_id.clone();

        Ok(tokio::task::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                let Ok((len, _addr)) = udp_socket
                    .recv_from(&mut buf)
                    .await
                    .inspect_err(|err| error!("Error receiving UDP packet: {err}"))
                else {
                    continue;
                };

                let Ok(text) = std::str::from_utf8(&buf[0..len])
                    .inspect_err(|err| error!("UDP packet was not utf8: {err}"))
                else {
                    continue;
                };

                let Ok(syslog_msg) = SyslogMessage::from_str(text)
                    .inspect_err(|err| error!("UDP packet was not valid syslog: {err}:\n{text}"))
                else {
                    continue;
                };

                let log_msg = syslog_to_log_message(syslog_msg, &sd_id);
                gateway.handle_log_message(log_msg).await;
            }
        }))
    }
}

/// Convert SyslogSeverity to our Level enum
fn severity_to_level(sev: SyslogSeverity) -> Level {
    match sev {
        SyslogSeverity::SEV_EMERG => Level::EMERGENCY,
        SyslogSeverity::SEV_ALERT => Level::ALERT,
        SyslogSeverity::SEV_CRIT => Level::CRITICAL,
        SyslogSeverity::SEV_ERR => Level::ERROR,
        SyslogSeverity::SEV_WARNING => Level::WARNING,
        SyslogSeverity::SEV_NOTICE => Level::NOTICE,
        SyslogSeverity::SEV_INFO => Level::INFO,
        SyslogSeverity::SEV_DEBUG => Level::DEBUG,
    }
}

/// Convert a SyslogMessage to a LogMessage, extracting structured data for tracing metadata
fn syslog_to_log_message(msg: SyslogMessage, sd_id: &str) -> LogMessage {
    let level = severity_to_level(msg.severity);
    let mut builder = LogMessage::builder(level, msg.msg);

    if let Some(ts) = msg.timestamp {
        builder = builder.timestamp(ts);
    }
    if let Some(nanos) = msg.timestamp_nanos {
        builder = builder.timestamp_nanos(nanos);
    }
    if let Some(hostname) = msg.hostname {
        builder = builder.hostname(hostname);
    }
    if let Some(appname) = msg.appname {
        builder = builder.appname(appname);
    }

    // Extract tracing metadata from structured data
    if let Some(sd_element) = msg.sd.find_sdid(sd_id) {
        if let Some(module) = sd_element.get("module") {
            builder = builder.module_path(module.clone());
        }
        if let Some(file) = sd_element.get("file") {
            builder = builder.file(file.clone());
        }
        if let Some(line) = sd_element.get("line") {
            builder = builder.line(line.clone());
        }
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_syslog_parsing() {
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815221698+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports",
        )
        .unwrap();
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815221698+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports",
        )
        .unwrap();
        SyslogMessage::from_str(
            "<12>1 2025-11-08T02:24:10.815+00:00 ip-172-31-5-8 app 92748 - - Dropped 3/4 reports due to staleness"
        )
        .unwrap();
    }
}
