//! Syslog listener for receiving RFC 5424 syslog messages over UDP and TCP.
//!
//! TCP transport follows RFC 6587, supporting both octet counting and
//! non-transparent framing methods.

use conf::Conf;
use signal_gateway::{Gateway, Level, LogMessage};
use std::{net::SocketAddr, str::FromStr, sync::Arc};
use syslog_rfc5424::{SyslogMessage, SyslogSeverity};
use tokio::{
    io::{AsyncRead, AsyncReadExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
};
use tracing::{error, info, trace};

/// Configuration for the syslog listener (supports both UDP and TCP).
#[derive(Clone, Conf, Debug)]
pub struct SyslogConfig {
    /// Socket address to listen for syslog messages.
    /// Both UDP and TCP listeners are started on this address.
    /// Messages use RFC 5424 format; TCP transport follows RFC 6587.
    #[conf(long, env)]
    pub listen_addr: SocketAddr,
    /// Structured data ID for tracing metadata (module, file, line) in syslog messages.
    #[conf(long, env, default_value = "tracing-meta@64700")]
    pub sd_id: String,
    /// If true, use CR as the message delimiter for non-transparent framing instead of LF.
    /// See RFC 6587 (Syslog over TCP) section 3.4.2.
    /// Default delimiters: NUL or LF. With this flag: NUL or CR.
    #[conf(long, env)]
    pub tcp_cr_is_delimiter: bool,
}

impl SyslogConfig {
    /// Bind UDP and TCP sockets and start background tasks to handle incoming syslog messages.
    ///
    /// Returns join handles for the background tasks.
    pub async fn start_tasks(
        &self,
        gateway: Arc<Gateway>,
    ) -> std::io::Result<(tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>)> {
        let udp_handle = self.start_udp_task(gateway.clone()).await?;
        let tcp_handle = self.start_tcp_task(gateway).await?;
        Ok((udp_handle, tcp_handle))
    }

    async fn start_udp_task(
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
                    .inspect_err(|err| error!("Error receiving syslog UDP packet: {err}"))
                else {
                    continue;
                };

                let Ok(text) = std::str::from_utf8(&buf[0..len])
                    .inspect_err(|err| error!("Syslog UDP packet was not utf8: {err}"))
                else {
                    continue;
                };

                let Ok(syslog_msg) = SyslogMessage::from_str(text)
                    .inspect_err(|err| error!("Syslog UDP packet was not valid syslog: {err}:\n{text}"))
                else {
                    continue;
                };

                let log_msg = syslog_to_log_message(syslog_msg, &sd_id);
                gateway.handle_log_message(log_msg).await;
            }
        }))
    }

    async fn start_tcp_task(
        &self,
        gateway: Arc<Gateway>,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let tcp_listener = TcpListener::bind(self.listen_addr).await?;
        info!("Listening for syslog TCP on {}", self.listen_addr);

        let config = self.clone();

        Ok(tokio::task::spawn(async move {
            loop {
                let Ok((stream, addr)) = tcp_listener
                    .accept()
                    .await
                    .inspect_err(|err| error!("Error accepting syslog TCP connection: {err}"))
                else {
                    continue;
                };

                trace!("Accepted syslog TCP connection from {addr}");

                let gateway = gateway.clone();
                let config = config.clone();

                // Spawn a task for each connection
                tokio::spawn(async move {
                    if let Err(err) = handle_tcp_connection(stream, &gateway, &config).await {
                        error!("Syslog TCP connection from {addr} error: {err}");
                    } else {
                        trace!("Syslog TCP connection from {addr} closed");
                    }
                });
            }
        }))
    }
}

/// Handle a single TCP connection using RFC 6587 framing.
async fn handle_tcp_connection(
    stream: TcpStream,
    gateway: &Gateway,
    config: &SyslogConfig,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    loop {
        let Some(msg_bytes) = read_framed_syslog_bytes(&mut reader, config.tcp_cr_is_delimiter).await? else {
            return Ok(()); // Clean EOF
        };

        // Parse and process the message
        let Ok(text) = std::str::from_utf8(&msg_bytes)
            .inspect_err(|err| error!("Syslog TCP message was not utf8: {err}"))
        else {
            continue;
        };

        let Ok(syslog_msg) = SyslogMessage::from_str(text)
            .inspect_err(|err| error!("Syslog TCP message was not valid: {err}:\n{text}"))
        else {
            continue;
        };

        let log_msg = syslog_to_log_message(syslog_msg, &config.sd_id);
        gateway.handle_log_message(log_msg).await;
    }
}

/// Read a single framed syslog message using RFC 6587 framing detection.
///
/// Returns:
/// - `Ok(Some(bytes))` - A complete message was read
/// - `Ok(None)` - EOF reached (no more messages)
/// - `Err(InvalidData)` - Invalid framing (e.g., unexpected first byte)
/// - `Err(other)` - I/O error
async fn read_framed_syslog_bytes<R>(
    reader: &mut BufReader<R>,
    cr_is_delimiter: bool,
) -> std::io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    loop {
        // Read first byte to determine framing method
        let first_byte = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        };

        match first_byte {
            // Whitespace: skip and continue
            b' ' | b'\t' | b'\n' | b'\r' => continue,

            // Octet counting: starts with non-zero digit
            b'1'..=b'9' => {
                return read_octet_counted_message(reader, first_byte).await.map(Some)
            }

            // Non-transparent framing: starts with '<' (beginning of syslog PRI)
            b'<' => {
                return read_non_transparent_message(reader, first_byte, cr_is_delimiter)
                    .await
                    .map(Some)
            }

            // Invalid first byte
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid first byte 0x{:02x} ({:?})",
                        first_byte,
                        char::from(first_byte)
                    ),
                ))
            }
        }
    }
}

/// Read an octet-counted message (RFC 6587 section 3.4.1).
/// Format: MSG-LEN SP SYSLOG-MSG
async fn read_octet_counted_message<R>(
    reader: &mut BufReader<R>,
    first_digit: u8,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut len: usize = (first_digit - b'0') as usize;

    // Read remaining digits until space
    loop {
        let b = reader.read_u8().await?;
        match b {
            b'0'..=b'9' => {
                len = len
                    .checked_mul(10)
                    .and_then(|l| l.checked_add((b - b'0') as usize))
                    .ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "message length overflow")
                    })?;
            }
            b' ' => break,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("expected digit or space in octet count, got 0x{b:02x}"),
                ));
            }
        }
    }

    // Read exactly `len` bytes
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Read a non-transparent framed message (RFC 6587 section 3.4.2).
/// Delimiter is NUL or LF by default, or NUL or CR if `tcp_cr_is_delimiter` is set.
/// Returns the message buffer on EOF (connection closed) as well as on delimiter.
async fn read_non_transparent_message<R>(
    reader: &mut BufReader<R>,
    first_byte: u8,
    cr_is_delimiter: bool,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut msg = vec![first_byte];

    let line_delimiter = if cr_is_delimiter { b'\r' } else { b'\n' };

    loop {
        let b = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break, // EOF
            Err(e) => return Err(e),
        };

        if b == b'\0' || b == line_delimiter {
            break;
        }

        msg.push(b);
    }

    Ok(msg)
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

    #[tokio::test]
    async fn test_framing_eof_returns_none() {
        let data: &[u8] = b"";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_framing_octet_counting() {
        let data: &[u8] = b"5 hello";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_octet_counting_multidigit_length() {
        let msg = "a]".repeat(50); // 100 bytes
        let data = format!("100 {msg}");
        let mut reader = BufReader::new(data.as_bytes());
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(msg.as_bytes().to_vec()));
    }

    #[tokio::test]
    async fn test_framing_non_transparent_lf_delimiter() {
        let data: &[u8] = b"<14>1 test message\n";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"<14>1 test message".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_non_transparent_nul_delimiter() {
        let data: &[u8] = b"<14>1 test message\0";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"<14>1 test message".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_non_transparent_cr_delimiter() {
        let data: &[u8] = b"<14>1 test message\r";
        let mut reader = BufReader::new(data);
        // With cr_is_delimiter=true, CR is a delimiter
        let result = read_framed_syslog_bytes(&mut reader, true).await.unwrap();
        assert_eq!(result, Some(b"<14>1 test message".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_non_transparent_cr_not_delimiter_by_default() {
        let data: &[u8] = b"<14>1 test\rmessage\n";
        let mut reader = BufReader::new(data);
        // With cr_is_delimiter=false, CR is part of the message
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"<14>1 test\rmessage".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_non_transparent_eof_returns_buffer() {
        // No delimiter, just EOF - should return the accumulated buffer
        let data: &[u8] = b"<14>1 test message";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"<14>1 test message".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_whitespace_skipping() {
        let data: &[u8] = b"  \n\t\r5 hello";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, Some(b"hello".to_vec()));
    }

    #[tokio::test]
    async fn test_framing_whitespace_only_then_eof() {
        let data: &[u8] = b"  \n\t\r";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_framing_invalid_first_byte() {
        let data: &[u8] = b"xyz";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_framing_zero_not_valid_start() {
        // '0' is not a valid start for octet counting (must be 1-9)
        let data: &[u8] = b"0 hello";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_framing_multiple_messages() {
        let data: &[u8] = b"5 hello<14>1 world\n3 foo";
        let mut reader = BufReader::new(data);

        let r1 = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(r1, Some(b"hello".to_vec()));

        let r2 = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(r2, Some(b"<14>1 world".to_vec()));

        let r3 = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(r3, Some(b"foo".to_vec()));

        let r4 = read_framed_syslog_bytes(&mut reader, false).await.unwrap();
        assert_eq!(r4, None);
    }

    #[tokio::test]
    async fn test_framing_octet_counting_invalid_separator() {
        // After digits, we expect a space, not another character
        let data: &[u8] = b"5xhello";
        let mut reader = BufReader::new(data);
        let result = read_framed_syslog_bytes(&mut reader, false).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }
}
