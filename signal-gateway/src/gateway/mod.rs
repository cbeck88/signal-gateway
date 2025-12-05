use crate::{
    http::{AlertMessage, Status},
    jsonrpc::{Envelope, RpcClient, RpcClientError, SignalMessage, connect_tcp},
    prometheus::{Prometheus, PrometheusConfig},
};
use bytes::Buf;
use conf::{Conf, Subcommands};
use futures_util::FutureExt;
pub use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use prometheus_http_client::{AlertStatus, ExtractLabels};
//use jsonrpsee::async_client::{Client as JsonRpcClient, Error as JsonRpcError};
use chrono::Utc;
use std::{
    collections::HashMap, error::Error, fmt::Write, net::SocketAddr, path::PathBuf, time::Duration,
};
use syslog_rfc5424::SyslogMessage;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    join,
    net::TcpStream,
    sync::{
        Mutex, RwLock,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

mod circular_buffer;
mod log_handler;
use log_handler::{LogHandler, LogHandlerConfig};

mod rate_limiter;
use rate_limiter::{MultiRateLimiter, RateThreshold, SourceLocationRateLimiter};

#[derive(Conf, Debug)]
pub struct GatewayConfig {
    #[conf(long, env, default_value = "127.0.0.1:7583")]
    pub signal_cli_tcp_addr: SocketAddr,
    #[conf(long, env)]
    pub signal_account: String,
    #[conf(long, env)]
    pub cbmm_tcp_addr: String,
    #[conf(long, env, default_value = "5s", value_parser = conf_extra::parse_duration)]
    pub cbmm_timeout: Duration,
    #[conf(repeat, long, env)]
    pub admin_uuid: Vec<String>,
    #[conf(flatten)]
    pub prometheus: Option<PrometheusConfig>,
    #[conf(flatten)]
    pub log_handler: LogHandlerConfig,
}

/// Wrapper for parsing gateway commands
#[derive(Clone, Debug, Conf)]
struct GatewayCommandWrapper {
    #[conf(subcommands)]
    command: GatewayCommand,
}

/// Commands that can be sent to the gateway (prefixed with /)
#[allow(unused)]
#[derive(Clone, Debug, Subcommands)]
enum GatewayCommand {
    /// Show recent log messages
    #[conf(name = "log", alias = "LOG")]
    Log {
        /// Optional filter: show only origins where app or host contains this string
        #[conf(pos)]
        filter: Option<String>,
    },
    /// Query prometheus for current values
    #[conf(name = "query", alias = "QUERY")]
    Query {
        /// PromQL query expression
        #[conf(pos)]
        query: String,
    },
    /// Plot a prometheus query over time
    #[conf(name = "plot", alias = "PLOT")]
    Plot {
        /// PromQL query expression
        #[conf(pos)]
        query: String,
        /// Duration to plot (e.g., 1h, 24h)
        #[conf(long, short = 'd', default_value = "1h", value_parser = conf_extra::parse_duration)]
        duration: Duration,
    },
    /// List series matching label patterns
    #[conf(name = "series", alias = "SERIES")]
    Series {
        /// Label matchers (e.g., __name__=~".*requests.*")
        #[conf(repeat, pos)]
        matchers: Vec<String>,
    },
    /// List label names matching patterns
    #[conf(name = "labels", alias = "LABELS")]
    Labels {
        /// Label matchers
        #[conf(repeat, pos)]
        matchers: Vec<String>,
    },
    /// Show current alerts from prometheus
    #[conf(name = "alerts", alias = "ALERTS")]
    Alerts,
}

/// Parse a gateway command from a string (with or without leading /)
fn parse_gateway_command(s: &str) -> Result<GatewayCommand, String> {
    // Remove leading slash if present
    let s = s.strip_prefix('/').unwrap_or(s).trim();

    if s.is_empty() {
        return Err("Empty command".to_string());
    }

    // Parse using Conf, treating the input as command line arguments
    // Prepend a dummy program name since Conf expects argv[0]
    let args = std::iter::once("gateway")
        .chain(s.split_whitespace())
        .collect::<Vec<_>>();

    GatewayCommandWrapper::try_parse_from::<&str, &str, &str>(args, vec![])
        .map(|wrapper| wrapper.command)
        .map_err(|e| e.to_string())
}

/// Identifies the source of log messages (app name + host).
/// Used to separate log buffers and rate limiters per source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Origin {
    pub app: String,
    pub host: String,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.app, self.host)
    }
}

impl From<&SyslogMessage> for Origin {
    fn from(msg: &SyslogMessage) -> Self {
        Self {
            app: msg.appname.clone().unwrap_or_default(),
            host: msg.hostname.clone().unwrap_or_default(),
        }
    }
}

impl Origin {
    /// Check if this origin matches a filter string.
    ///
    /// If the filter contains '@', it is split on the first '@':
    /// - The part before '@' must be a substring of `app`
    /// - The part after '@' must be a substring of `host`
    ///
    /// If the filter does not contain '@', it matches if either `app` or `host`
    /// contains the filter string.
    pub fn matches_filter(&self, filter: &str) -> bool {
        if let Some((app_filter, host_filter)) = filter.split_once('@') {
            self.app.contains(app_filter) && self.host.contains(host_filter)
        } else {
            self.app.contains(filter) || self.host.contains(filter)
        }
    }
}

/// A message queued to be sent to all admins.
/// This is generally an alert message, which may have attached images.
#[derive(Clone, Debug, Default)]
struct AdminMessage {
    /// The origin of the message (app + host), if from syslog
    origin: Option<Origin>,
    text: String,
    attachment_paths: Vec<PathBuf>,
    /// Short summary for logging (e.g., alert names for prometheus).
    /// If None, the consumer will use a truncated slice of `text` for logging.
    summary: Option<String>,
}

/// The gateway manages sending messages to signal-cli and receiving messages from signal-cli.
/// It maintains a queue of messages to be sent to all admins, generated by alerts etc.
/// It also subscribes to messages received from signal and processes them one-by-one, possibly
/// making TCP request to cbmm to handle them.
///
/// This is the only task that communicates directly with signal-cli, and by design it linearizes
/// all interaction, which prevents any possible races.
///
/// The gateway also maintains a buffer of at most 64 syslog messages that it has received.
/// If an error occurs, that message and the previous messages in the buffer are sent to signal admins,
/// and then the buffer is purged. This serves as a minimal log-aggregation and alerting system.
pub struct Gateway {
    config: GatewayConfig,
    admin_mq_tx: UnboundedSender<AdminMessage>,
    admin_mq_rx: Mutex<UnboundedReceiver<AdminMessage>>,
    token: CancellationToken,
    prometheus: Option<Prometheus>,
    /// Log handlers keyed by origin (app + host). Lazily created when first message from an origin arrives.
    log_handlers: RwLock<HashMap<Origin, LogHandler>>,
}

impl Gateway {
    pub async fn new(config: GatewayConfig, token: CancellationToken) -> Self {
        let (admin_mq_tx, admin_mq_rx) = unbounded_channel();

        let prometheus = config
            .prometheus
            .as_ref()
            .map(|pc| Prometheus::new(pc.clone()))
            .transpose()
            .expect("Invalid prometheus config");

        Self {
            config,
            admin_mq_tx,
            admin_mq_rx: Mutex::new(admin_mq_rx),
            token,
            prometheus,
            log_handlers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn run(&self) {
        loop {
            if self.token.is_cancelled() {
                return;
            }
            match connect_tcp(&self.config.signal_cli_tcp_addr).await {
                Err(err) => {
                    error!(
                        "Could not connect to signal_cli @ ({}): {err}",
                        self.config.signal_cli_tcp_addr
                    );
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Ok(client) => {
                    if let Err(err) = self.do_run(&client).await {
                        error!("Error with signal cli, reconnecting: {err}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } else {
                        continue;
                    }
                }
            }
        }
    }

    async fn do_run(&self, signal_cli: &impl RpcClient) -> Result<(), RpcClientError> {
        let mut admin_mq_rx = self
            .admin_mq_rx
            .try_lock()
            .expect("Mutex should not be contended");
        let mut signal_rx = signal_cli
            .subscribe_receive(Some(self.config.signal_account.clone()))
            .await?;

        loop {
            tokio::select! {
                _ = self.token.cancelled() => {
                    info!("Stop requested");
                    return Ok(());
                },
                outbound_admin_msg = admin_mq_rx.recv() => {
                    if let Some(msg) = outbound_admin_msg {
                        // Log summary, or first 500 bytes of text if no summary provided
                        let summary = msg.summary.as_deref().unwrap_or_else(|| {
                            let len = msg.text.len().min(500);
                            &msg.text[..len]
                        });
                        info!("Sending alert: {summary}");
                        // Prepend origin line if present
                        let message = if let Some(origin) = &msg.origin {
                            format!("[{origin}]\n{}", msg.text)
                        } else {
                            msg.text
                        };
                        let attachments = msg.attachment_paths.into_iter().map(|p| p.to_str().unwrap().to_owned()).collect();
                        SignalMessage {
                            sender: self.config.signal_account.clone(),
                            recipient: self.config.admin_uuid.clone(),
                            message,
                            attachments,
                        }.send(signal_cli).await?;
                    } else {
                        warn!("admin_mq_rx is closed, halting service");
                        self.token.cancel();
                        return Ok(());
                    }
                },
                signal_msg = signal_rx.next() => {
                    match signal_msg {
                        None => {
                            info!("Signal Rx: stream closed");
                            return Ok(());
                        },
                        Some(Err(err)) => {
                            error!("Signal Rx: {err}");
                            return Err(RpcClientError::ParseError(err));
                        }
                        Some(Ok(msg)) => {
                            //info!("Signal Rx: {msg:?}");
                            if msg.envelope.data_message.is_none() {
                                debug!("Ignoring message which was not a data message: {msg:?}");
                                continue;
                            }

                            if !self.config.admin_uuid.contains(&msg.envelope.source_uuid) {
                                warn!("Ignoring message from non-admin: {msg:?}");
                                continue;
                            }

                            let (resp, _) = join!(
                                self.handle_signal_admin_message(&msg.envelope),
                                msg.envelope.send_read_receipt(signal_cli, &self.config.signal_account).map(|result| {
                                    if let Err(err) = result {
                                        warn!("Couldn't send read receipt: {err}");
                                    }
                                })
                            );

                            let (message, attachments) = resp.unwrap_or_else(
                                |(code, msg)| {
                                    let text = format!("{code}: {msg}");
                                    error!("(cbmm) {text}");
                                    (text, vec![])
                                }
                            );

                            let attachments = attachments.into_iter().map(|p| p.to_str().expect("attachments must have utf8 paths").to_owned()).collect();

                            SignalMessage {
                                sender: self.config.signal_account.clone(),
                                recipient: vec![msg.envelope.source_uuid.clone()],
                                message,
                                attachments,
                            }.send(signal_cli).await?;
                        }
                    }
                }
            }
        }
    }

    // Returns Err in case of a timeout
    // Returns Ok when success or error text is generated
    async fn handle_signal_admin_message(
        &self,
        msg: &Envelope,
    ) -> Result<(String, Vec<PathBuf>), (u16, Box<dyn Error>)> {
        let data = msg.data_message.as_ref().unwrap();

        // Admin messages starting with / are handled by gateway
        // Other messages are forwarded to cbmm
        if data.message.starts_with("/") {
            // Parse the command using conf
            let cmd = parse_gateway_command(&data.message).map_err(|err| (400, err.into()))?;

            self.handle_gateway_command(cmd).await
        } else {
            // Connect to cbmm. Note that we could use a keep-alive strategy here maybe...
            let mut cbmm_stream = timeout(
                self.config.cbmm_timeout,
                TcpStream::connect(&self.config.cbmm_tcp_addr),
            )
            .await
            .map_err(format_err("connecting", 504))?
            .map_err(format_err("connecting", 502))?;

            timeout(
                self.config.cbmm_timeout,
                cbmm_stream.write_all(data.message.as_bytes()),
            )
            .await
            .map_err(format_err("writing", 504))?
            .map_err(format_err("writing", 502))?;

            let _ = cbmm_stream.shutdown().await;

            // Wrap as BufReader so that we can use "read_until" which simplifies things
            let mut reader = BufReader::new(cbmm_stream);
            let mut buf = vec![];
            timeout(self.config.cbmm_timeout, reader.read_until(b'\r', &mut buf))
                .await
                .map_err(format_err("reading", 504))?
                .map_err(format_err("reading", 502))?;

            let s = str::from_utf8(&buf).map_err(format_err("utf8", 502))?;
            let text = s.trim().to_owned();
            Ok((text, vec![]))
        }
    }

    // Handler function that processes incoming http requests (push's from alertmanager expected)
    pub async fn handle_http_request<B>(&self, req: Request<B>) -> Result<Response<String>, String>
    where
        B: Body + Send,
        B::Data: Buf + Send,
        B::Error: std::fmt::Display,
    {
        info!(
            "Received http request: {} {} (version: {:?})",
            req.method(),
            req.uri().path(),
            req.version()
        );

        fn ok_resp() -> Response<String> {
            Response::new("OK".into())
        }
        fn err_resp(code: StatusCode, text: impl Into<String>) -> Response<String> {
            let mut resp = Response::new(text.into());
            *resp.status_mut() = code;
            resp
        }

        match (req.method(), req.uri().path()) {
            (&Method::GET, "/") => Ok(ok_resp()),
            (&Method::GET, "/health") => Ok(ok_resp()),
            (&Method::POST, "/alert") => {
                let v = req
                    .into_body()
                    .collect()
                    .await
                    .map_err(|err| format!("When reading body bytes: {err}"))?
                    .to_bytes()
                    .to_vec();

                if let Err((code, msg)) = self.handle_post_alert(&v).await {
                    Ok(err_resp(code, msg))
                } else {
                    Ok(ok_resp())
                }
            }
            _ => Ok(err_resp(
                StatusCode::NOT_FOUND,
                format!("Not found '{} {}'", req.method(), req.uri().path()),
            )),
        }
    }

    async fn handle_gateway_command(
        &self,
        cmd: GatewayCommand,
    ) -> Result<(String, Vec<PathBuf>), (u16, Box<dyn Error>)> {
        match cmd {
            GatewayCommand::Log { filter } => {
                let handlers = self.log_handlers.read().await;
                if handlers.is_empty() {
                    return Ok(("No log sources registered yet".to_string(), vec![]));
                }
                let mut text = String::new();
                for (origin, handler) in handlers.iter() {
                    // Apply filter if present
                    if let Some(ref f) = filter
                        && !origin.matches_filter(f)
                    {
                        continue;
                    }
                    writeln!(&mut text, "=== [{origin}] ===").unwrap();
                    text.push_str(&handler.format_logs().await);
                    text.push('\n');
                }
                if text.is_empty() {
                    return Ok(("No matching log sources".to_string(), vec![]));
                }
                Ok((text, vec![]))
            }
            GatewayCommand::Query { query } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (500, "prometheus was not configured".into()))?;

                match prometheus.oneoff_query(query).await {
                    Ok((
                        ExtractLabels {
                            name,
                            common_labels,
                            specific_labels,
                        },
                        ts,
                    )) => {
                        let mut text = format!("{name} {common_labels:?}\n");

                        for (mut sl, maybe_val) in specific_labels.into_iter().zip(ts.into_iter()) {
                            let name = sl.remove("__name__").unwrap_or_default();
                            let label = format!("{name} {sl:?}");

                            // TODO: Include timestamp?
                            let val = maybe_val
                                .map(|(_time, val)| val.to_string())
                                .unwrap_or_else(|| "-".to_string());
                            if let Err(err) = writeln!(&mut text, "\t{label}\t\t{val}") {
                                return Err((500, err.into()));
                            }
                        }

                        Ok((text, vec![]))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            #[cfg(feature = "plot")]
            GatewayCommand::Plot { query, duration } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (500, "prometheus was not configured".into()))?;

                prometheus.purge_old_plots();
                match prometheus.create_oneoff_plot(query.clone(), duration).await {
                    Ok(filename) => Ok((query, vec![filename])),
                    Err(err) => Err((500, err)),
                }
            }
            #[cfg(not(feature = "plot"))]
            GatewayCommand::Plot { .. } => {
                Err((500, "the plot feature was not enabled at build time".into()))
            }
            GatewayCommand::Series { matchers } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (500, "prometheus was not configured".into()))?;

                let matcher_refs: Vec<&str> = matchers.iter().map(|s| s.as_str()).collect();
                match prometheus.series(&matcher_refs).await {
                    Ok(data) => {
                        let mut text = data.iter().fold(String::default(), |mut buf, kv| {
                            writeln!(&mut buf, "{kv:?}").unwrap();
                            buf
                        });
                        if text.is_empty() {
                            text = "no matches".into();
                        }
                        Ok((text, vec![]))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            GatewayCommand::Labels { matchers } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (500, "prometheus was not configured".into()))?;

                let matcher_refs: Vec<&str> = matchers.iter().map(|s| s.as_str()).collect();
                match prometheus.labels(&matcher_refs).await {
                    Ok(data) => {
                        let mut text = data.iter().fold(String::default(), |mut buf, l| {
                            writeln!(&mut buf, "{l}").unwrap();
                            buf
                        });
                        if text.is_empty() {
                            text = "no matches".into();
                        }
                        Ok((text, vec![]))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            GatewayCommand::Alerts => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (500, "prometheus was not configured".into()))?;

                match prometheus.alerts().await {
                    Ok(data) => {
                        let now = Utc::now();
                        let mut text = data.iter().fold(String::default(), |mut buf, alert| {
                            let symbol = match alert.state {
                                AlertStatus::Pending => "🟡",
                                AlertStatus::Firing => "🔴",
                                AlertStatus::Resolved => "🟢",
                            };
                            let since = {
                                let dur = (now - alert.active_at).to_std().unwrap_or_default();
                                // reduce precision to at most seconds
                                let mut secs = dur.as_secs();
                                // if the duration is more than an hour, then reduce precision to minutes
                                if secs > 3600 {
                                    secs -= secs % 60;
                                }
                                humantime::format_duration(Duration::new(secs, 0))
                            };
                            let annotations = &alert.annotations;
                            let labels = &alert.labels;

                            writeln!(&mut buf, "{symbol} {since} {annotations:?} {labels:?}")
                                .unwrap();
                            buf
                        });
                        if text.is_empty() {
                            text = "no alerts".into();
                        }
                        Ok((text, vec![]))
                    }
                    Err(err) => Err((500, err)),
                }
            }
        }
    }

    async fn handle_post_alert(&self, body_bytes: &[u8]) -> Result<(), (StatusCode, &'static str)> {
        let body_text = str::from_utf8(body_bytes).map_err(|err| {
            warn!("When reading body bytes: {err}");
            (StatusCode::BAD_REQUEST, "Request body was not utf-8")
        })?;

        let alert_msg: AlertMessage = serde_json::from_str(body_text).map_err(|err| {
            error!("Could not parse json: {err}:\n{body_text}");
            (StatusCode::BAD_REQUEST, "Invalid Json")
        })?;

        let text = self
            .format_alert_text(&alert_msg)
            .unwrap_or_else(|err| format!("error formatting alert text: {err}:\n{alert_msg:#?}"));

        #[allow(unused_mut)]
        let mut attachment_paths = vec![];

        #[cfg(feature = "plot")]
        if let Some(prometheus) = self.prometheus.as_ref() {
            prometheus.purge_old_plots();
            for alert in alert_msg.alerts.iter() {
                match prometheus.create_alert_plot(alert).await {
                    Ok(path) => {
                        attachment_paths.push(path);
                    }
                    Err(err) => {
                        error!("Could not format plot: {err} for {alert:#?}");
                    }
                }
            }
        }

        // Build summary: status sigils followed by alert names
        let summary = alert_msg
            .alerts
            .iter()
            .map(|alert| {
                let symbol = match alert.status {
                    Status::Firing => "🔴",
                    Status::Resolved => "🟢",
                };
                let name = alert
                    .labels
                    .get("alertname")
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("{symbol}{name}")
            })
            .collect::<Vec<_>>()
            .join(" ");

        self.admin_mq_tx
            .send(AdminMessage {
                origin: None, // Prometheus alerts don't have a syslog origin
                text,
                attachment_paths,
                summary: Some(summary),
            })
            .map_err(|_err| {
                error!("Could not send alert message, queue is closed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Can't send signal msg right now, queue is closed",
                )
            })
    }

    fn format_alert_text(&self, msg: &AlertMessage) -> Result<String, String> {
        let mut text = "Alert:\n".to_owned();
        let now = Utc::now();

        for alert in msg.alerts.iter() {
            let symbol = match alert.status {
                Status::Firing => "🔴",
                Status::Resolved => "🟢",
            };
            let since = {
                let dur = (now - alert.starts_at).to_std().unwrap_or_default();
                // reduce precision to at most seconds
                let mut secs = dur.as_secs();
                // if the duration is more than an hour, then reduce precision to minutes
                if secs > 3600 {
                    secs -= secs % 60;
                }
                humantime::format_duration(Duration::new(secs, 0))
            };
            let name = alert
                .annotations
                .get("summary")
                .or_else(|| alert.labels.get("alertname"))
                .map(|s| s.as_str())
                .unwrap_or("?");

            let expr = match alert.parse_expr_from_generator_url() {
                Ok(expr) => expr,
                Err(err) => {
                    error!(
                        "Couldn't parse generator url {}: {err}",
                        alert.generator_url
                    );
                    String::default()
                }
            };

            writeln!(&mut text, "{symbol}: ({since}) '{name}' {expr}")
                .map_err(|err| format!("formatting error: {err}"))?;
        }

        Ok(text)
    }

    pub async fn handle_syslog_message(&self, syslog_msg: SyslogMessage) {
        let origin = Origin::from(&syslog_msg);

        // Try to get existing handler with read lock first
        {
            let handlers = self.log_handlers.read().await;
            if let Some(handler) = handlers.get(&origin) {
                handler.handle_syslog_message(syslog_msg, origin).await;
                return;
            }
        }

        // Handler doesn't exist, need to create one with write lock
        let mut handlers = self.log_handlers.write().await;
        // Double-check in case another task created it while we were waiting for the write lock
        let handler = handlers.entry(origin.clone()).or_insert_with(|| {
            info!("Creating new log handler for origin: {origin}");
            LogHandler::new(self.config.log_handler.clone(), self.admin_mq_tx.clone())
        });
        handler.handle_syslog_message(syslog_msg, origin).await;
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

// Returns a lambda that expresses an error as a (u16, String) with given context info
fn format_err<E: std::fmt::Display>(
    context: &'static str,
    code: u16,
) -> impl Fn(E) -> (u16, Box<dyn Error>) {
    move |err: E| -> (u16, Box<dyn Error>) { (code, format!("{context}: {err}").into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gateway_command() {
        // Test log command without filter
        let cmd = parse_gateway_command("/log").unwrap();
        assert!(matches!(cmd, GatewayCommand::Log { filter: None }));

        // Test log command with filter
        let cmd = parse_gateway_command("/log myapp").unwrap();
        if let GatewayCommand::Log { filter } = cmd {
            assert_eq!(filter, Some("myapp".to_string()));
        } else {
            panic!("Expected Log command");
        }

        // Test uppercase log command
        let cmd = parse_gateway_command("/LOG").unwrap();
        assert!(matches!(cmd, GatewayCommand::Log { filter: None }));

        // Test query command
        let cmd = parse_gateway_command("/query test_metric").unwrap();
        if let GatewayCommand::Query { query } = cmd {
            assert_eq!(query, "test_metric");
        } else {
            panic!("Expected Query command");
        }

        // Test query command (uppercase)
        let cmd = parse_gateway_command("/QUERY test_metric").unwrap();
        if let GatewayCommand::Query { query } = cmd {
            assert_eq!(query, "test_metric");
        } else {
            panic!("Expected Query command");
        }

        // Test plot command with default duration
        let cmd = parse_gateway_command("/plot my_query").unwrap();
        if let GatewayCommand::Plot { query, duration } = cmd {
            assert_eq!(query, "my_query");
            assert_eq!(duration, Duration::from_secs(60 * 60));
        } else {
            panic!("Expected Plot command");
        }

        // Test plot command with custom duration
        let cmd = parse_gateway_command("/plot my_query -d 24h").unwrap();
        if let GatewayCommand::Plot { query, duration } = cmd {
            assert_eq!(query, "my_query");
            assert_eq!(duration, Duration::from_secs(24 * 60 * 60));
        } else {
            panic!("Expected Plot command");
        }

        // Test series command with matchers
        let cmd = parse_gateway_command("/series metric1 metric2").unwrap();
        if let GatewayCommand::Series { matchers } = cmd {
            assert_eq!(matchers, vec!["metric1", "metric2"]);
        } else {
            panic!("Expected Series command");
        }

        // Test labels command
        let cmd = parse_gateway_command("/labels foo bar").unwrap();
        if let GatewayCommand::Labels { matchers } = cmd {
            assert_eq!(matchers, vec!["foo", "bar"]);
        } else {
            panic!("Expected Labels command");
        }

        // Test alerts command
        let cmd = parse_gateway_command("/alerts").unwrap();
        assert!(matches!(cmd, GatewayCommand::Alerts));

        // Test alerts command (uppercase)
        let cmd = parse_gateway_command("/ALERTS").unwrap();
        assert!(matches!(cmd, GatewayCommand::Alerts));

        // Test without leading slash
        let cmd = parse_gateway_command("log").unwrap();
        assert!(matches!(cmd, GatewayCommand::Log { filter: None }));

        // Test empty command
        assert!(parse_gateway_command("/").is_err());
        assert!(parse_gateway_command("").is_err());
    }

    #[test]
    fn test_origin_matches_filter() {
        let origin = Origin {
            app: "muad-dib".to_string(),
            host: "tokyo-server".to_string(),
        };

        // Without @: matches if app OR host contains the string
        assert!(origin.matches_filter("muad"));
        assert!(origin.matches_filter("dib"));
        assert!(origin.matches_filter("tokyo"));
        assert!(origin.matches_filter("server"));
        assert!(!origin.matches_filter("paris"));

        // With @: app must contain first part AND host must contain second part
        assert!(origin.matches_filter("muad@tokyo"));
        assert!(origin.matches_filter("dib@server"));
        assert!(origin.matches_filter("muad-dib@tokyo-server"));
        assert!(!origin.matches_filter("muad@paris"));
        assert!(!origin.matches_filter("other@tokyo"));

        // Empty parts with @
        assert!(origin.matches_filter("@tokyo")); // empty app filter matches any app
        assert!(origin.matches_filter("muad@")); // empty host filter matches any host
        assert!(origin.matches_filter("@")); // both empty, matches everything

        // Edge case: filter matches the @ in the format but origin has no @
        let origin2 = Origin {
            app: "app".to_string(),
            host: "host".to_string(),
        };
        assert!(origin2.matches_filter("app@host"));
        assert!(!origin2.matches_filter("app@other"));
    }
}
