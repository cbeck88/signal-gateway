//! JSON listener for receiving log messages in a logstash-compatible format.
//!
//! This module accepts JSON log messages over UDP and TCP, converting them to LogMessage.
//! It's designed to be flexible and accept various common formats.
//!
//! TCP connections use a relaxed JSON Lines format that allows newlines within
//! JSON objects (standard JSON Lines requires each value on a single line).

use chrono::{DateTime, TimeZone, Utc};
use conf::Conf;
use serde::Deserialize;
use signal_gateway::{Gateway, Level, LogMessage};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    io::BufReader,
    net::{TcpListener, TcpStream, UdpSocket},
};
use tracing::{error, info, trace};

mod json_lines;
use json_lines::read_json_lines_value;

/// Configuration for the JSON log message listener.
///
/// Listens for JSON log messages on both UDP and TCP using the same address.
///
/// See [`JsonLogMessage`] for the JSON schema.
#[derive(Clone, Conf, Debug)]
pub struct JsonConfig {
    /// Socket address to listen for JSON log messages.
    /// Both UDP and TCP listeners are started on this address.
    ///
    /// - **UDP**: Each datagram should contain a single JSON object.
    /// - **TCP**: Uses a relaxed JSON Lines format where each JSON object is
    ///   separated by newlines.
    ///
    /// See [`JsonLogMessage`] for the JSON schema. It is roughly compatible
    /// with logstash, graylog, etc.
    ///
    /// * "message", "Message", "msg" for the log mesage string itself
    /// * "timestamp", "@timestamp", "time" for the timestamp, which can be a numeric unix typestamp,
    ///   or a string in RFC3339 form
    /// * "level", "severity" for the log level string
    /// * "host" or "hostname" for the originating host
    /// * "app" or "appname" or "application" or "service" for the originating program
    /// * "file" or "filename" or "source_file" for the source file that wrote the log line
    /// * "line" or "lineno" for the source file line number that wrote the log line
    /// * "module" or "module_path" or "logger" or "logger_name" for the source module that wrote the log line
    #[conf(long, env)]
    pub listen_addr: SocketAddr,
}

impl JsonConfig {
    /// Bind UDP and TCP sockets and start background tasks to handle incoming JSON log messages.
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
        info!("Listening for JSON UDP on {}", self.listen_addr);

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

                let Ok(json_msg) = serde_json::from_str::<JsonLogMessage>(text)
                    .inspect_err(|err| error!("UDP packet was not valid JSON: {err}:\n{text}"))
                else {
                    continue;
                };

                let log_msg = json_msg.into_log_message();
                gateway.handle_log_message(log_msg).await;
            }
        }))
    }

    async fn start_tcp_task(
        &self,
        gateway: Arc<Gateway>,
    ) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let tcp_listener = TcpListener::bind(self.listen_addr).await?;
        info!("Listening for JSON TCP on {}", self.listen_addr);

        Ok(tokio::task::spawn(async move {
            loop {
                let Ok((stream, addr)) = tcp_listener
                    .accept()
                    .await
                    .inspect_err(|err| error!("Error accepting JSON TCP connection: {err}"))
                else {
                    continue;
                };

                trace!("Accepted JSON TCP connection from {addr}");

                let gateway = gateway.clone();

                // Spawn a task for each connection
                tokio::spawn(async move {
                    if let Err(err) = handle_tcp_connection(stream, &gateway).await {
                        error!("JSON TCP connection from {addr} error: {err}");
                    } else {
                        trace!("JSON TCP connection from {addr} closed");
                    }
                });
            }
        }))
    }
}

/// Handle a single TCP connection using relaxed JSON Lines framing.
async fn handle_tcp_connection(stream: TcpStream, gateway: &Gateway) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    loop {
        let Some(msg_bytes) = read_json_lines_value(&mut reader).await? else {
            return Ok(()); // Clean EOF
        };

        let text = std::str::from_utf8(&msg_bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let json_msg: JsonLogMessage = serde_json::from_str(text)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let log_msg = json_msg.into_log_message();
        gateway.handle_log_message(log_msg).await;
    }
}

/// A flexible JSON log message format compatible with logstash and similar systems.
///
/// Supports various field names and formats commonly used in logging systems.
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct JsonLogMessage {
    /// The log message text - accepts "message" or "msg"
    #[serde(alias = "msg")]
    pub message: String,

    /// Log level - accepts various formats (error, ERROR, err, etc.)
    #[serde(
        default,
        alias = "severity",
        deserialize_with = "deserialize_opt_level"
    )]
    pub level: Option<Level>,

    /// Timestamp - accepts Unix epoch seconds (int or string) or RFC3339 string
    #[serde(
        default,
        alias = "time",
        alias = "@timestamp",
        deserialize_with = "deserialize_timestamp"
    )]
    pub timestamp: Option<DateTime<Utc>>,

    /// Hostname
    #[serde(alias = "host")]
    pub hostname: Option<String>,

    /// Application name
    #[serde(alias = "app", alias = "application", alias = "service")]
    pub appname: Option<String>,

    /// Module path
    #[serde(alias = "module", alias = "logger", alias = "logger_name")]
    pub module_path: Option<String>,

    /// Source file
    #[serde(alias = "filename", alias = "source_file")]
    pub file: Option<String>,

    /// Line number - accepts integer or string
    #[serde(default, alias = "lineno", deserialize_with = "deserialize_line")]
    pub line: Option<String>,
}

impl JsonLogMessage {
    /// Convert to a LogMessage
    ///
    /// TODO: Allow this to take configuration options to customize how fields are mapped
    pub fn into_log_message(self) -> LogMessage {
        let level = self.level.unwrap_or(Level::ERROR);
        let mut builder = LogMessage::builder(level, self.message);

        if let Some(ts) = self.timestamp {
            builder = builder.timestamp(ts.timestamp());
            builder = builder.timestamp_nanos(ts.timestamp_subsec_nanos());
        }
        if let Some(hostname) = self.hostname {
            builder = builder.hostname(hostname);
        }
        if let Some(appname) = self.appname {
            builder = builder.appname(appname);
        }
        if let Some(module_path) = self.module_path {
            builder = builder.module_path(module_path);
        }
        if let Some(file) = self.file {
            builder = builder.file(file);
        }
        if let Some(line) = self.line {
            builder = builder.line(line);
        }

        builder.build()
    }
}

/// Deserialize an optional log level, returning None for unknown values.
fn deserialize_opt_level<'de, D>(deserializer: D) -> Result<Option<Level>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|s| Level::from_str(&s)))
}

/// Deserialize a timestamp from Unix epoch (int or string) or RFC3339 string
fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TimestampValue {
        Integer(i64),
        Float(f64),
        String(String),
    }

    let opt: Option<TimestampValue> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(TimestampValue::Integer(secs)) => Ok(Utc.timestamp_opt(secs, 0).single()),
        Some(TimestampValue::Float(secs)) => {
            let whole_secs = secs.trunc() as i64;
            let nanos = ((secs.fract()) * 1_000_000_000.0) as u32;
            Ok(Utc.timestamp_opt(whole_secs, nanos).single())
        }
        Some(TimestampValue::String(s)) => {
            // Try parsing as integer first (Unix timestamp as string)
            if let Ok(secs) = s.parse::<i64>() {
                return Ok(Utc.timestamp_opt(secs, 0).single());
            }
            // Try parsing as float (Unix timestamp with fractional seconds)
            if let Ok(secs) = s.parse::<f64>() {
                let whole_secs = secs.trunc() as i64;
                let nanos = ((secs.fract()) * 1_000_000_000.0) as u32;
                return Ok(Utc.timestamp_opt(whole_secs, nanos).single());
            }
            // Try parsing as RFC3339
            DateTime::parse_from_rfc3339(&s)
                .map(|dt| Some(dt.with_timezone(&Utc)))
                .map_err(|e| D::Error::custom(format!("invalid timestamp: {e}")))
        }
    }
}

/// Deserialize a line number from integer or string
fn deserialize_line<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum LineValue {
        Integer(u64),
        String(String),
    }

    let opt: Option<LineValue> = Option::deserialize(deserializer)?;
    Ok(match opt {
        None => None,
        Some(LineValue::Integer(n)) => Some(n.to_string()),
        Some(LineValue::String(s)) => Some(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_message_with_message_field() {
        let json = r#"{"message": "Hello, world!"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message, "Hello, world!");
        let log_msg = msg.into_log_message();
        assert_eq!(&*log_msg.msg, "Hello, world!");
        assert_eq!(log_msg.level, Level::ERROR); // default
    }

    #[test]
    fn test_basic_message_with_msg_field() {
        let json = r#"{"msg": "Hello from msg!"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message, "Hello from msg!");
    }

    #[test]
    fn test_level_variations() {
        // Lowercase
        let json = r#"{"message": "test", "level": "error"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::ERROR));

        // Uppercase
        let json = r#"{"message": "test", "level": "ERROR"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::ERROR));

        // Short form
        let json = r#"{"message": "test", "level": "err"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::ERROR));

        // Warning variations
        let json = r#"{"message": "test", "level": "warn"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::WARNING));

        let json = r#"{"message": "test", "level": "warning"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::WARNING));

        // Fatal -> Critical
        let json = r#"{"message": "test", "level": "fatal"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::CRITICAL));

        // Severity alias
        let json = r#"{"message": "test", "severity": "error"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, Some(Level::ERROR));
    }

    #[test]
    fn test_timestamp_as_integer() {
        let json = r#"{"message": "test", "timestamp": 1733500000}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let ts = msg.timestamp.unwrap();
        assert_eq!(ts.timestamp(), 1733500000);
    }

    #[test]
    fn test_timestamp_as_float() {
        let json = r#"{"message": "test", "timestamp": 1733500000.123456}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let ts = msg.timestamp.unwrap();
        assert_eq!(ts.timestamp(), 1733500000);
        assert!(ts.timestamp_subsec_nanos() > 123000000);
        assert!(ts.timestamp_subsec_nanos() < 124000000);
    }

    #[test]
    fn test_timestamp_as_string_integer() {
        let json = r#"{"message": "test", "timestamp": "1733500000"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let ts = msg.timestamp.unwrap();
        assert_eq!(ts.timestamp(), 1733500000);
    }

    #[test]
    fn test_timestamp_as_rfc3339() {
        let json = r#"{"message": "test", "timestamp": "2024-12-06T12:00:00Z"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let ts = msg.timestamp.unwrap();
        assert_eq!(ts.timestamp(), 1733486400);
    }

    #[test]
    fn test_timestamp_as_rfc3339_with_offset() {
        let json = r#"{"message": "test", "timestamp": "2024-12-06T12:00:00+05:30"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let ts = msg.timestamp.unwrap();
        // 12:00 +05:30 = 06:30 UTC
        assert_eq!(ts.timestamp(), 1733486400 - 5 * 3600 - 30 * 60);
    }

    #[test]
    fn test_timestamp_aliases() {
        // @timestamp (logstash style)
        let json = r#"{"message": "test", "@timestamp": "2024-12-06T12:00:00Z"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert!(msg.timestamp.is_some());

        // time
        let json = r#"{"message": "test", "time": 1733500000}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert!(msg.timestamp.is_some());
    }

    #[test]
    fn test_line_as_integer() {
        let json = r#"{"message": "test", "line": 42}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.line, Some("42".to_string()));
    }

    #[test]
    fn test_line_as_string() {
        let json = r#"{"message": "test", "line": "42"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.line, Some("42".to_string()));
    }

    #[test]
    fn test_line_aliases() {
        let json = r#"{"message": "test", "lineno": 123}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.line, Some("123".to_string()));
    }

    #[test]
    fn test_hostname_aliases() {
        let json = r#"{"message": "test", "host": "myserver"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.hostname, Some("myserver".to_string()));

        let json = r#"{"message": "test", "hostname": "myserver2"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.hostname, Some("myserver2".to_string()));
    }

    #[test]
    fn test_appname_aliases() {
        let json = r#"{"message": "test", "app": "myapp"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.appname, Some("myapp".to_string()));

        let json = r#"{"message": "test", "application": "myapp2"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.appname, Some("myapp2".to_string()));

        let json = r#"{"message": "test", "service": "myservice"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.appname, Some("myservice".to_string()));
    }

    #[test]
    fn test_module_path_aliases() {
        let json = r#"{"message": "test", "module": "mymodule"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.module_path, Some("mymodule".to_string()));

        let json = r#"{"message": "test", "logger": "mylogger"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.module_path, Some("mylogger".to_string()));

        let json = r#"{"message": "test", "logger_name": "com.example.MyClass"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.module_path, Some("com.example.MyClass".to_string()));
    }

    #[test]
    fn test_file_aliases() {
        let json = r#"{"message": "test", "file": "main.rs"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.file, Some("main.rs".to_string()));

        let json = r#"{"message": "test", "filename": "app.py"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.file, Some("app.py".to_string()));

        let json = r#"{"message": "test", "source_file": "Handler.java"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.file, Some("Handler.java".to_string()));
    }

    #[test]
    fn test_full_logstash_style_message() {
        let json = r#"{
            "@timestamp": "2024-12-06T12:00:00Z",
            "message": "User logged in",
            "level": "info",
            "host": "web-01",
            "service": "auth-service",
            "logger_name": "com.example.auth.LoginHandler",
            "filename": "LoginHandler.java",
            "lineno": 123
        }"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let log_msg = msg.into_log_message();

        assert_eq!(&*log_msg.msg, "User logged in");
        assert_eq!(log_msg.level, Level::INFO);
        assert_eq!(log_msg.hostname.as_deref(), Some("web-01"));
        assert_eq!(log_msg.appname.as_deref(), Some("auth-service"));
        assert_eq!(
            log_msg.module_path.as_deref(),
            Some("com.example.auth.LoginHandler")
        );
        assert_eq!(log_msg.file.as_deref(), Some("LoginHandler.java"));
        assert_eq!(log_msg.line.as_deref(), Some("123"));
        assert!(log_msg.timestamp.is_some());
    }

    #[test]
    fn test_minimal_message() {
        let json = r#"{"msg": "simple log"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        let log_msg = msg.into_log_message();

        assert_eq!(&*log_msg.msg, "simple log");
        assert_eq!(log_msg.level, Level::ERROR);
        assert!(log_msg.hostname.is_none());
        assert!(log_msg.appname.is_none());
        assert!(log_msg.timestamp.is_none());
    }

    #[test]
    fn test_unknown_level_defaults_to_none() {
        let json = r#"{"message": "test", "level": "unknown_level"}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.level, None);
        // Should default to ERROR when converted
        let log_msg = msg.into_log_message();
        assert_eq!(log_msg.level, Level::ERROR);
    }

    #[test]
    fn test_extra_fields_are_ignored() {
        let json =
            r#"{"message": "test", "extra_field": "ignored", "nested": {"also": "ignored"}}"#;
        let msg: JsonLogMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.message, "test");
    }

    // Tests for TCP stream parsing (read_json_lines_value + JSON parsing)

    use tokio::io::AsyncRead;

    /// Test helper that mimics handle_tcp_connection's parsing logic
    async fn parse_next_message<R: AsyncRead + Unpin>(
        reader: &mut BufReader<R>,
    ) -> std::io::Result<Option<LogMessage>> {
        let Some(msg_bytes) = read_json_lines_value(reader).await? else {
            return Ok(None);
        };

        let text = std::str::from_utf8(&msg_bytes)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        let json_msg: JsonLogMessage = serde_json::from_str(text)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

        Ok(Some(json_msg.into_log_message()))
    }

    #[tokio::test]
    async fn test_tcp_valid_message() {
        let data: &[u8] = b"{\"message\": \"hello\"}\n";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert_eq!(&*msg.msg, "hello");
    }

    #[tokio::test]
    async fn test_tcp_multiple_messages() {
        let data: &[u8] = b"{\"message\": \"first\"}\n{\"message\": \"second\"}\n";
        let mut reader = BufReader::new(data);

        let msg1 = parse_next_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(&*msg1.msg, "first");

        let msg2 = parse_next_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(&*msg2.msg, "second");

        let msg3 = parse_next_message(&mut reader).await.unwrap();
        assert!(msg3.is_none()); // EOF
    }

    #[tokio::test]
    async fn test_tcp_eof_returns_none() {
        let data: &[u8] = b"";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_tcp_invalid_utf8_returns_error() {
        // Invalid UTF-8 bytes inside a "JSON" structure
        let data: &[u8] = b"{\"\xff\xfe\": \"bad\"}\n";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_tcp_invalid_json_returns_error() {
        let data: &[u8] = b"{not valid json}\n";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_tcp_missing_required_field_returns_error() {
        // Valid JSON but missing required "message" field
        let data: &[u8] = b"{\"level\": \"info\"}\n";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_tcp_error_stops_processing() {
        // First message valid, second invalid - error should stop further processing
        let data: &[u8] = b"{\"message\": \"ok\"}\n{invalid}\n{\"message\": \"never reached\"}\n";
        let mut reader = BufReader::new(data);

        // First message succeeds
        let msg1 = parse_next_message(&mut reader).await.unwrap();
        assert!(msg1.is_some());

        // Second message fails with error
        let result = parse_next_message(&mut reader).await;
        assert!(result.is_err());
        // After error, caller should close connection - no third read attempted
    }

    #[tokio::test]
    async fn test_tcp_multiline_json() {
        let data: &[u8] = b"{\n  \"message\": \"hello\",\n  \"level\": \"error\"\n}\n";
        let mut reader = BufReader::new(data);
        let result = parse_next_message(&mut reader).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert_eq!(&*msg.msg, "hello");
        assert_eq!(msg.level, Level::ERROR);
    }
}
