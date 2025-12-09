//! Gateway for bridging alerts and logs with Signal messenger.

#[cfg(unix)]
use crate::signal_jsonrpc::connect_ipc;
use crate::{
    alertmanager::AlertPost,
    claude::{ClaudeAgent, ClaudeConfig, SentBy, Tool, ToolExecutor, ToolResult},
    log_message::{LogMessage, Origin},
    message_handler::{AdminMessage, AdminMessageResponse, Context, MessageHandlerResult},
    prometheus::{Prometheus, PrometheusConfig},
    signal_jsonrpc::{
        Envelope, MessageTarget, RpcClient, RpcClientError, SignalMessage, connect_tcp,
    },
};
use async_trait::async_trait;
use chrono::Utc;
use conf::{Conf, Subcommands};
use futures_util::FutureExt;
use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use prometheus_http_client::{AlertStatus, ExtractLabels};
use std::{
    fmt::Write, net::SocketAddr, path::PathBuf, sync::Arc, sync::Mutex, sync::OnceLock, sync::Weak,
    time::Duration,
};
use tokio::{
    join,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tokio_util::{bytes::Buf, sync::CancellationToken};
use tracing::{debug, error, info, warn};

mod signal_trust_set;
pub use signal_trust_set::SignalTrustSet;

mod command_router;
pub use command_router::{CommandRouter, CommandRouterBuilder, Handling};

mod log_buffer;
mod log_handler;
use log_handler::{LogHandler, LogHandlerConfig};

mod rate_limiter_set;
pub use rate_limiter_set::{LimitResult, LimiterSet};

mod route;
pub use route::{Destination, Limit, Route, evaluate_limiter_sequence};

pub use crate::rate_limiter::{Limiter, RateThreshold};

use std::error::Error;

/// An admin message response with its source attribution.
///
/// This wraps an [`AdminMessageResponse`] with information about who generated
/// the response (e.g., System, Claude, etc.) for proper message history tracking.
pub struct SourcedAdminMessageResponse {
    /// The response content.
    pub resp: AdminMessageResponse,
    /// Who generated this response.
    pub source: SentBy,
}

/// Result type for sourced admin message responses.
type SourcedMessageResult =
    Result<SourcedAdminMessageResponse, (u16, Box<dyn Error + Send + Sync>)>;

/// Configuration for the gateway.
#[derive(Conf, Debug)]
#[conf(serde)]
#[cfg_attr(unix, conf(one_of_fields(signal_cli_tcp_addr, signal_cli_socket_path)))]
#[cfg_attr(not(unix), conf(one_of_fields(signal_cli_tcp_addr)))]
pub struct GatewayConfig {
    /// TCP address of signal-cli JSON-RPC server.
    #[conf(long, env)]
    pub signal_cli_tcp_addr: Option<SocketAddr>,
    /// Unix socket path of signal-cli JSON-RPC server.
    #[cfg(unix)]
    #[conf(long, env)]
    pub signal_cli_socket_path: Option<PathBuf>,
    /// The phone number or UUID of the Signal account to use.
    #[conf(long, env)]
    pub signal_account: String,
    /// Delay before retrying connection to signal-cli after an error.
    #[conf(long, env, default_value = "5s", value_parser = conf_extra::parse_duration, serde(use_value_parser))]
    pub signal_cli_retry_delay: Duration,
    /// Signal admin UUIDs mapped to their safety numbers (can be empty).
    /// Accepts either a map `{"uuid1": ["12345..."], "uuid2": []}` or a list `["uuid1", "uuid2"]`.
    #[conf(long, env, value_parser = serde_json::from_str)]
    pub signal_admins: SignalTrustSet,
    /// If set, alerts are sent to this group instead of individual admins.
    #[conf(long, env)]
    pub alert_group_id: Option<String>,
    /// Prometheus server configuration for querying metrics.
    #[conf(flatten)]
    pub prometheus: Option<PrometheusConfig>,
    /// Log handler configuration for processing log messages.
    #[conf(flatten, prefix)]
    pub log_handler: LogHandlerConfig,
    /// Claude API configuration for AI-powered responses.
    #[conf(flatten, prefix)]
    pub claude: Option<ClaudeConfig>,
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
    /// Stop current Claude request
    #[conf(name = "claude-stop", alias = "CLAUDE-STOP")]
    ClaudeStop,
    /// Compact Claude's message history
    #[conf(name = "claude-compact", alias = "CLAUDE-COMPACT")]
    ClaudeCompact,
    /// Log Claude's message buffer for debugging
    #[conf(name = "claude-debug", alias = "CLAUDE-DEBUG")]
    ClaudeDebug,
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
    let args = std::iter::once("signal-gateway")
        .chain(s.split_whitespace())
        .collect::<Vec<_>>();

    GatewayCommandWrapper::try_parse_from::<&str, &str, &str>(args, vec![])
        .map(|wrapper| wrapper.command)
        .map_err(|e| e.to_string())
}

/// Summary for logging an alert message.
#[derive(Clone, Debug)]
enum Summary {
    /// Use a prefix of the message text (capped at 512 chars).
    Prefix(usize),
    /// Use an owned summary string.
    Owned(Box<str>),
}

impl Default for Summary {
    fn default() -> Self {
        Summary::Prefix(512)
    }
}

/// A message queued to be sent to all admins.
/// This is generally an alert message, which may have attached images.
#[derive(Clone, Debug, Default)]
struct SignalAlertMessage {
    /// The origin of the message (app + host), if from syslog
    origin: Option<Origin>,
    text: String,
    attachment_paths: Vec<PathBuf>,
    /// Short summary for logging (e.g., alert names for prometheus).
    summary: Summary,
    /// Optional destination override from route configuration.
    /// If present, overrides the default alert destination.
    destination_override: Option<Destination>,
}

impl SignalAlertMessage {
    /// Get the summary string for logging.
    fn get_summary(&self) -> &str {
        match &self.summary {
            Summary::Owned(s) => s,
            Summary::Prefix(n) => {
                let len = self.text.len().min(*n).min(512);
                &self.text[..len]
            }
        }
    }
}

/// The gateway manages sending messages to signal-cli and receiving messages from signal-cli.
/// It maintains a queue of messages to be sent to all admins, generated by alerts etc.
/// It also subscribes to messages received from signal and processes them one-by-one,
/// calling a user-provided handler for non-command messages.
///
/// This is the only task that communicates directly with signal-cli, and by design it linearizes
/// all interaction, which prevents any possible races.
///
/// The gateway also maintains a buffer of at most 64 syslog messages that it has received.
/// If an error occurs, that message and the previous messages in the buffer are sent to signal admins,
/// and then the buffer is purged. This serves as a minimal log-aggregation and alerting system.
pub struct Gateway {
    config: GatewayConfig,
    signal_alert_mq_tx: UnboundedSender<SignalAlertMessage>,
    /// Alert receiver, wrapped in Option so it can be taken by run().
    /// This ensures run() can only be called once.
    signal_alert_mq_rx: Mutex<Option<UnboundedReceiver<SignalAlertMessage>>>,
    token: CancellationToken,
    prometheus: Option<Prometheus>,
    /// Log handler for processing log messages from all origins.
    log_handler: LogHandler,
    /// Command router for dispatching admin messages.
    command_router: CommandRouter,
    /// Claude API client for AI-powered responses.
    /// Initialized after Arc creation so it can hold a weak reference back to Gateway.
    claude: OnceLock<Box<ClaudeAgent>>,
}

impl Gateway {
    /// Create a new gateway with the given configuration.
    pub async fn new(
        config: GatewayConfig,
        token: CancellationToken,
        command_router: CommandRouter,
    ) -> Arc<Self> {
        let (signal_alert_mq_tx, signal_alert_mq_rx) = unbounded_channel();

        let prometheus = config
            .prometheus
            .as_ref()
            .map(|pc| Prometheus::new(pc.clone()))
            .transpose()
            .expect("Invalid prometheus config");

        let log_handler = LogHandler::new(config.log_handler.clone(), signal_alert_mq_tx.clone());

        let claude_config = config.claude.clone();

        let gateway = Arc::new(Self {
            config,
            signal_alert_mq_tx,
            signal_alert_mq_rx: Mutex::new(Some(signal_alert_mq_rx)),
            token,
            prometheus,
            log_handler,
            command_router,
            claude: OnceLock::new(),
        });

        // Initialize Claude with a weak reference back to the gateway
        if let Some(cc) = claude_config {
            let claude = ClaudeAgent::new(cc, Arc::downgrade(&gateway) as Weak<dyn ToolExecutor>)
                .expect("Invalid claude config");
            gateway
                .claude
                .set(Box::new(claude))
                .unwrap_or_else(|_| panic!("claude OnceLock was already set"));
        }

        gateway
    }

    /// Run the gateway main loop, reconnecting to signal-cli on errors.
    ///
    /// # Panics
    /// Panics if `Gateway::run` is called more than once on a given `Gateway`.
    pub async fn run(&self) {
        let mut alert_rx = self
            .signal_alert_mq_rx
            .lock()
            .unwrap()
            .take()
            .expect("Gateway::run can only be called once");

        loop {
            if self.token.is_cancelled() {
                return;
            }

            if let Err(err) = self.connect_and_run(&mut alert_rx).await {
                error!("Error with signal-cli, reconnecting: {err}");
                tokio::time::sleep(self.config.signal_cli_retry_delay).await;
            }
        }
    }

    /// Connect to signal-cli and run the main loop.
    async fn connect_and_run(
        &self,
        alert_rx: &mut UnboundedReceiver<SignalAlertMessage>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(unix)]
        if let Some(path) = &self.config.signal_cli_socket_path {
            info!(
                "Connecting to signal-cli via unix socket: {}",
                path.display()
            );
            let client = connect_ipc(path).await?;
            return Ok(self.do_run(&client, alert_rx).await?);
        }

        if let Some(addr) = &self.config.signal_cli_tcp_addr {
            info!("Connecting to signal-cli via TCP: {addr}");
            let client = connect_tcp(addr).await?;
            return Ok(self.do_run(&client, alert_rx).await?);
        }

        // This shouldn't happen due to one_of_fields validation
        unreachable!("one_of_fields should ensure exactly one transport is configured")
    }

    async fn do_run(
        &self,
        signal_cli: &impl RpcClient,
        alert_rx: &mut UnboundedReceiver<SignalAlertMessage>,
    ) -> Result<(), RpcClientError> {
        // Retry trust update until it succeeds
        loop {
            match self
                .config
                .signal_admins
                .update_trust(signal_cli, &self.config.signal_account)
                .await
            {
                Ok(()) => break,
                Err(err) => {
                    error!("Trust update failed: {err}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }

        let mut signal_rx = signal_cli
            .subscribe_receive(Some(self.config.signal_account.clone()))
            .await?;

        loop {
            tokio::select! {
                _ = self.token.cancelled() => {
                    info!("Stop requested");
                    return Ok(());
                },
                outbound_admin_msg = alert_rx.recv() => {
                    if let Some(msg) = outbound_admin_msg {
                        info!("Sending alert: {}", msg.get_summary());
                        // Prepend origin line if present
                        let message = if let Some(origin) = &msg.origin {
                            format!("[{origin}]\n{}", msg.text)
                        } else {
                            msg.text
                        };
                        let attachments = msg.attachment_paths.into_iter().map(|p| p.to_str().unwrap().to_owned()).collect();
                        // Use destination override if present, otherwise use configured default
                        let target = match msg.destination_override {
                            Some(Destination::Group(group_id)) => MessageTarget::Group(group_id),
                            Some(Destination::Recipients(recipients)) => MessageTarget::Recipients(recipients),
                            None => {
                                // Send to group if configured, otherwise to individual admins
                                if let Some(group_id) = &self.config.alert_group_id {
                                    MessageTarget::Group(group_id.clone())
                                } else {
                                    MessageTarget::Recipients(self.config.signal_admins.uuids().map(str::to_owned).collect())
                                }
                            }
                        };
                        SignalMessage {
                            sender: self.config.signal_account.clone(),
                            target,
                            message: message.clone(),
                            attachments,
                        }.send(signal_cli).await?;

                        if let Some(claude) = self.claude.get() {
                            claude.record_message(SentBy::System, &message, Utc::now().timestamp_millis() as u64);
                        }
                    } else {
                        warn!("alert_rx is closed, halting service");
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
                            let Some(data_message) = &msg.envelope.data_message else {
                                debug!("Ignoring message which was not a data message: {msg:?}");
                                continue;
                            };

                            // Determine if this message came from a group
                            let from_group = data_message.group_info.as_ref().map(|g| g.group_id.clone());

                            // Check if sender is an admin
                            if !self.config.signal_admins.is_trusted(&msg.envelope) {
                                warn!("Ignoring message from non-admin: {msg:?}");
                                continue;
                            }

                            let (result, _) = join!(
                                self.handle_signal_admin_message(&msg.envelope),
                                msg.envelope.send_read_receipt(signal_cli, &self.config.signal_account).map(|r| {
                                    if let Err(err) = r {
                                        warn!("Couldn't send read receipt: {err}");
                                    }
                                })
                            );

                            // If no route matched, don't send a response
                            let Some(result) = result else {
                                continue;
                            };

                            let sourced = result.unwrap_or_else(|(code, msg)| {
                                let text = format!("{code}: {msg}");
                                error!("Message handler error: {text}");
                                SourcedAdminMessageResponse {
                                    resp: AdminMessageResponse::new(text),
                                    source: SentBy::System,
                                }
                            });

                            let attachments = sourced.resp.attachments.iter().map(|p| p.to_str().expect("attachments must have utf8 paths").to_owned()).collect();

                            // Reply to group if message came from a group, otherwise reply to sender
                            let target = if let Some(group_id) = from_group {
                                MessageTarget::Group(group_id)
                            } else {
                                MessageTarget::Recipients(vec![msg.envelope.source_uuid.clone()])
                            };

                            SignalMessage {
                                sender: self.config.signal_account.clone(),
                                target,
                                message: sourced.resp.text.clone(),
                                attachments,
                            }.send(signal_cli).await?;

                            if let Some(claude) = self.claude.get() {
                                claude.record_message(sourced.source, &sourced.resp.text, Utc::now().timestamp_millis() as u64);
                            }
                        }
                    }
                }
            }
        }
    }

    // Returns None if no route matched (don't send a response)
    // Returns Some(Err) in case of a handler error
    // Returns Some(Ok) when success or error text is generated
    async fn handle_signal_admin_message(&self, msg: &Envelope) -> Option<SourcedMessageResult> {
        let data = msg.data_message.as_ref().unwrap();

        let (stripped_message, handling) = self.command_router.route(&data.message)?;

        match handling {
            Handling::Help => {
                // Record as system command
                if let Some(claude) = self.claude.get() {
                    claude.record_message(SentBy::UserToSystem, &data.message, data.timestamp);
                }
                Some(Ok(SourcedAdminMessageResponse {
                    resp: AdminMessageResponse::new(self.command_router.help()),
                    source: SentBy::System,
                }))
            }
            Handling::GatewayCommand => {
                // Parse the command using conf (stripped message has "/" prefix removed)
                let cmd = match parse_gateway_command(stripped_message) {
                    Ok(cmd) => cmd,
                    Err(err) => return Some(Err((400u16, err.into()))),
                };

                // Record this as a system command in Claude's history
                if let Some(claude) = self.claude.get() {
                    claude.record_message(SentBy::UserToSystem, &data.message, data.timestamp);
                }

                let resp = match self.handle_gateway_command(cmd).await {
                    Ok(resp) => resp,
                    Err(err) => return Some(Err(err)),
                };
                Some(Ok(SourcedAdminMessageResponse {
                    resp,
                    source: SentBy::System,
                }))
            }
            Handling::Claude => {
                // Send directly to Claude (use stripped message)
                let Some(claude) = self.claude.get() else {
                    return Some(Err((501u16, "Claude is not configured".into())));
                };

                match claude.request(stripped_message, data.timestamp).await {
                    Ok(resp) => Some(Ok(SourcedAdminMessageResponse {
                        resp,
                        source: SentBy::Claude,
                    })),
                    Err(err) => Some(Err((500, err.to_string().into()))),
                }
            }
            Handling::Custom(handler) => {
                // Record as system message
                if let Some(claude) = self.claude.get() {
                    claude.record_message(SentBy::UserToSystem, &data.message, data.timestamp);
                }

                // Pass stripped message to custom handler
                let admin_msg = AdminMessage {
                    message: stripped_message.to_owned(),
                    timestamp: data.timestamp,
                    sender_uuid: msg.source_uuid.clone(),
                    group_id: data.group_info.as_ref().map(|g| g.group_id.clone()),
                };
                match handler
                    .handle_verified_signal_message(admin_msg, &GatewayContext)
                    .await
                {
                    Ok(resp) => Some(Ok(SourcedAdminMessageResponse {
                        resp,
                        source: SentBy::System,
                    })),
                    Err(err) => Some(Err(err)),
                }
            }
        }
    }

    /// Handle an incoming HTTP request (e.g., webhooks from Alertmanager).
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

        match req.uri().path() {
            "/" | "/health" | "/ready" => {
                if !matches!(req.method(), &Method::GET | &Method::HEAD) {
                    Ok(err_resp(
                        StatusCode::NOT_IMPLEMENTED,
                        "Use GET or HEAD with this route",
                    ))
                } else {
                    Ok(ok_resp())
                }
            }
            "/alert" => {
                if !matches!(req.method(), &Method::POST) {
                    return Ok(err_resp(
                        StatusCode::NOT_IMPLEMENTED,
                        "Use POST with this route",
                    ));
                }
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

    async fn handle_gateway_command(&self, cmd: GatewayCommand) -> MessageHandlerResult {
        match cmd {
            GatewayCommand::Log { filter } => {
                let text = self.log_handler.format_logs(filter.as_deref()).await;
                Ok(AdminMessageResponse::new(text))
            }
            GatewayCommand::Query { query } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (501u16, "prometheus was not configured".into()))?;

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

                        Ok(AdminMessageResponse::new(text))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            #[cfg(feature = "plot")]
            GatewayCommand::Plot { query, duration } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (501u16, "prometheus was not configured".into()))?;

                prometheus.purge_old_plots();
                match prometheus.create_oneoff_plot(query.clone(), duration).await {
                    Ok(filename) => Ok(AdminMessageResponse::new(query).with_attachment(filename)),
                    Err(err) => Err((500, err)),
                }
            }
            #[cfg(not(feature = "plot"))]
            GatewayCommand::Plot { .. } => {
                Err((501, "the plot feature was not enabled at build time".into()))
            }
            GatewayCommand::Series { matchers } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (501u16, "prometheus was not configured".into()))?;

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
                        Ok(AdminMessageResponse::new(text))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            GatewayCommand::Labels { matchers } => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (501u16, "prometheus was not configured".into()))?;

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
                        Ok(AdminMessageResponse::new(text))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            GatewayCommand::Alerts => {
                let prometheus = self
                    .prometheus
                    .as_ref()
                    .ok_or_else(|| (501u16, "prometheus was not configured".into()))?;

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
                        Ok(AdminMessageResponse::new(text))
                    }
                    Err(err) => Err((500, err)),
                }
            }
            GatewayCommand::ClaudeStop => {
                let claude = self
                    .claude
                    .get()
                    .ok_or_else(|| (501u16, "claude was not configured".into()))?;

                claude.request_stop();
                Ok(AdminMessageResponse::new("stop requested"))
            }
            GatewayCommand::ClaudeCompact => {
                let claude = self
                    .claude
                    .get()
                    .ok_or_else(|| (501u16, "claude was not configured".into()))?;

                claude.request_compaction();
                Ok(AdminMessageResponse::new("compaction requested"))
            }
            GatewayCommand::ClaudeDebug => {
                let claude = self
                    .claude
                    .get()
                    .ok_or_else(|| (501u16, "claude was not configured".into()))?;

                claude.request_debug();
                Ok(AdminMessageResponse::new("debug logged"))
            }
        }
    }

    async fn handle_post_alert(&self, body_bytes: &[u8]) -> Result<(), (StatusCode, &'static str)> {
        let body_text = str::from_utf8(body_bytes).map_err(|err| {
            warn!("When reading body bytes: {err}");
            (StatusCode::BAD_REQUEST, "Request body was not utf-8")
        })?;

        let alert_msg: AlertPost = serde_json::from_str(body_text).map_err(|err| {
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
                match prometheus.create_alert_plot(alert, false).await {
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
                let symbol = alert.status.symbol();
                let name = alert
                    .labels
                    .get("alertname")
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("{symbol}{name}")
            })
            .collect::<Vec<_>>()
            .join(" ");

        self.signal_alert_mq_tx
            .send(SignalAlertMessage {
                origin: None, // Prometheus alerts don't have a syslog origin
                text,
                attachment_paths,
                summary: Summary::Owned(summary.into()),
                destination_override: None,
            })
            .map_err(|_err| {
                error!("Could not send alert message, queue is closed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Can't send signal msg right now, queue is closed",
                )
            })
    }

    fn format_alert_text(&self, msg: &AlertPost) -> Result<String, String> {
        let mut text = "Alert:\n".to_owned();
        let now = Utc::now();

        for alert in msg.alerts.iter() {
            let symbol = alert.status.symbol();
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

    /// Process an incoming log message, buffering it and potentially triggering an alert.
    pub async fn handle_log_message(&self, log_msg: impl Into<LogMessage>) {
        let log_msg = log_msg.into();
        let origin = Origin::from(&log_msg);
        self.log_handler.handle_log_message(log_msg, origin).await;
    }
}

#[async_trait]
impl ToolExecutor for Gateway {
    fn tools(&self) -> Vec<Tool> {
        let mut tools = self.log_handler.tools();
        if let Some(prometheus) = &self.prometheus {
            tools.extend(prometheus.tools());
        }
        tools
    }

    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<ToolResult, String> {
        if self.log_handler.has_tool(name) {
            return self.log_handler.execute(name, input).await;
        }

        if let Some(prometheus) = &self.prometheus
            && prometheus.has_tool(name)
        {
            return prometheus.execute(name, input).await;
        }

        Err(format!("unknown tool: {name}"))
    }
}

/// Placeholder context for message handlers.
struct GatewayContext;

impl Context for GatewayContext {}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.token.cancel();
    }
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
}
