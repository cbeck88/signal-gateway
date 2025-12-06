//! Gateway for bridging alerts and logs with Signal messenger.

#[cfg(unix)]
use crate::signal_jsonrpc::connect_ipc;
use crate::{
    alertmanager::AlertPost,
    log_message::{LogMessage, Origin},
    message_handler::{
        AdminMessage, AdminMessageResponse, Context, MessageHandler, MessageHandlerResult,
    },
    prometheus::{Prometheus, PrometheusConfig},
    signal_jsonrpc::{
        Envelope, Identity, MessageTarget, RpcClient, RpcClientError, SignalMessage, connect_tcp,
    },
};
use chrono::Utc;
use conf::{Conf, Subcommands};
use futures_util::FutureExt;
use http::{Method, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::BodyExt;
use prometheus_http_client::{AlertStatus, ExtractLabels};
use std::{collections::HashMap, fmt::Write, net::SocketAddr, path::PathBuf, time::Duration};
use tokio::{
    join,
    sync::{
        Mutex,
        mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    },
};
use tokio_util::{bytes::Buf, sync::CancellationToken};
use tracing::{debug, error, info, warn};

mod log_buffer;
mod log_handler;
use log_handler::{LogHandler, LogHandlerConfig};

mod rate_limiter_set;
pub use rate_limiter_set::{LimitResult, LimiterSet};

mod route;
pub use route::{Destination, Limit, Route};

pub use crate::rate_limiter::{Limiter, RateThreshold};

/// Configuration for the gateway.
#[derive(Conf, Debug)]
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
    /// Admin UUIDs mapped to their safety numbers (can be empty).
    /// Example: `{"uuid1": ["12345...", "67890..."], "uuid2": []}`
    #[conf(long, env, value_parser = serde_json::from_str)]
    pub admin_safety_numbers: HashMap<String, Vec<String>>,
    /// If set, alerts are sent to this group instead of individual admins.
    #[conf(long, env)]
    pub alert_group_id: Option<String>,
    /// Prometheus server configuration for querying metrics.
    #[conf(flatten)]
    pub prometheus: Option<PrometheusConfig>,
    /// Log handler configuration for processing log messages.
    #[conf(flatten)]
    pub log_handler: LogHandlerConfig,
}

impl GatewayConfig {
    /// Check if a UUID is a registered admin
    pub fn is_admin(&self, uuid: &str) -> bool {
        self.admin_safety_numbers.contains_key(uuid)
    }

    /// Get all admin UUIDs
    pub fn admin_uuids(&self) -> Vec<String> {
        self.admin_safety_numbers.keys().cloned().collect()
    }
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
    signal_alert_mq_rx: Mutex<UnboundedReceiver<SignalAlertMessage>>,
    token: CancellationToken,
    prometheus: Option<Prometheus>,
    /// Log handler for processing log messages from all origins.
    log_handler: LogHandler,
    /// Handler for admin messages that don't start with `/`
    message_handler: Option<Box<dyn MessageHandler>>,
}

impl Gateway {
    /// Create a new gateway with the given configuration.
    pub async fn new(
        config: GatewayConfig,
        token: CancellationToken,
        message_handler: Option<Box<dyn MessageHandler>>,
    ) -> Self {
        let (signal_alert_mq_tx, signal_alert_mq_rx) = unbounded_channel();

        let prometheus = config
            .prometheus
            .as_ref()
            .map(|pc| Prometheus::new(pc.clone()))
            .transpose()
            .expect("Invalid prometheus config");

        let log_handler = LogHandler::new(config.log_handler.clone(), signal_alert_mq_tx.clone());

        Self {
            config,
            signal_alert_mq_tx,
            signal_alert_mq_rx: Mutex::new(signal_alert_mq_rx),
            token,
            prometheus,
            log_handler,
            message_handler,
        }
    }

    /// Run the gateway main loop, reconnecting to signal-cli on errors.
    pub async fn run(&self) {
        loop {
            if self.token.is_cancelled() {
                return;
            }

            if let Err(err) = self.connect_and_run().await {
                error!("Error with signal-cli, reconnecting: {err}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    /// Connect to signal-cli and run the main loop.
    async fn connect_and_run(&self) -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(unix)]
        if let Some(path) = &self.config.signal_cli_socket_path {
            info!(
                "Connecting to signal-cli via unix socket: {}",
                path.display()
            );
            let client = connect_ipc(path).await?;
            return Ok(self.do_run(&client).await?);
        }

        if let Some(addr) = &self.config.signal_cli_tcp_addr {
            info!("Connecting to signal-cli via TCP: {addr}");
            let client = connect_tcp(addr).await?;
            return Ok(self.do_run(&client).await?);
        }

        // This shouldn't happen due to one_of_fields validation
        unreachable!("one_of_fields should ensure exactly one transport is configured")
    }

    /// Update trust for admins with safety numbers configured.
    ///
    /// For each admin with safety numbers:
    /// 1. Check current identities in signal-cli via listIdentities
    /// 2. If any trusted identity is NOT in our config, remove the contact entirely and re-add only configured ones
    /// 3. Otherwise, just trust any new safety numbers from config that aren't already trusted
    async fn update_trust(&self, signal_cli: &impl RpcClient) {
        for (uuid, safety_numbers) in &self.config.admin_safety_numbers {
            if safety_numbers.is_empty() {
                continue;
            }

            // Get current identities from signal-cli
            let current_identities: Vec<Identity> = match signal_cli
                .list_identities(Some(self.config.signal_account.clone()), Some(uuid.clone()))
                .await
            {
                Ok(value) => serde_json::from_value(value).unwrap_or_default(),
                Err(err) => {
                    debug!("Could not list identities for {uuid} (may not exist yet): {err}");
                    Vec::new()
                }
            };

            // Check if any trusted identity in signal-cli is NOT in our config
            let has_revoked_identity = current_identities.iter().any(|id| {
                id.trust_level.is_trusted() && !safety_numbers.contains(&id.safety_number)
            });

            if has_revoked_identity {
                // Log which identities are being revoked
                for id in &current_identities {
                    if id.trust_level.is_trusted() && !safety_numbers.contains(&id.safety_number) {
                        warn!(
                            "Revoking trust for admin {uuid}: safety number {} is trusted but not in config",
                            id.safety_number
                        );
                    }
                }

                // Remove contact to clear all existing trust
                info!("Resetting trust for admin {uuid}");
                if let Err(err) = signal_cli
                    .remove_contact(
                        Some(self.config.signal_account.clone()),
                        uuid.clone(),
                        true,  // forget - delete identity keys and sessions
                        false, // hide
                    )
                    .await
                {
                    error!("Failed to remove contact {uuid}: {err}");
                    continue;
                }

                // Re-add all configured safety numbers
                for safety_number in safety_numbers {
                    info!("Trusting safety number for admin {uuid}");
                    if let Err(err) = signal_cli
                        .trust(
                            Some(self.config.signal_account.clone()),
                            uuid.clone(),
                            false,
                            Some(safety_number.clone()),
                        )
                        .await
                    {
                        error!("Failed to trust admin {uuid}: {err}");
                    }
                }
            } else {
                // Just add any new safety numbers that aren't already trusted
                let already_trusted: Vec<_> = current_identities
                    .iter()
                    .filter(|id| id.trust_level.is_trusted())
                    .map(|id| &id.safety_number)
                    .collect();

                for safety_number in safety_numbers {
                    if !already_trusted.contains(&safety_number) {
                        info!("Trusting new safety number for admin {uuid}");
                        if let Err(err) = signal_cli
                            .trust(
                                Some(self.config.signal_account.clone()),
                                uuid.clone(),
                                false,
                                Some(safety_number.clone()),
                            )
                            .await
                        {
                            error!("Failed to trust admin {uuid}: {err}");
                        }
                    }
                }
            }
        }
    }

    async fn do_run(&self, signal_cli: &impl RpcClient) -> Result<(), RpcClientError> {
        self.update_trust(signal_cli).await;

        let mut signal_alert_mq_rx = self
            .signal_alert_mq_rx
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
                outbound_admin_msg = signal_alert_mq_rx.recv() => {
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
                                    MessageTarget::Recipients(self.config.admin_uuids())
                                }
                            }
                        };
                        SignalMessage {
                            sender: self.config.signal_account.clone(),
                            target,
                            message,
                            attachments,
                        }.send(signal_cli).await?;
                    } else {
                        warn!("signal_alert_mq_rx is closed, halting service");
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
                            if !self.config.is_admin(&msg.envelope.source_uuid) {
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

                            let resp = resp.unwrap_or_else(|(code, msg)| {
                                let text = format!("{code}: {msg}");
                                error!("Message handler error: {text}");
                                AdminMessageResponse::new(text)
                            });

                            let attachments = resp.attachments.into_iter().map(|p| p.to_str().expect("attachments must have utf8 paths").to_owned()).collect();

                            // Reply to group if message came from a group, otherwise reply to sender
                            let target = if let Some(group_id) = from_group {
                                MessageTarget::Group(group_id)
                            } else {
                                MessageTarget::Recipients(vec![msg.envelope.source_uuid.clone()])
                            };

                            SignalMessage {
                                sender: self.config.signal_account.clone(),
                                target,
                                message: resp.text,
                                attachments,
                            }.send(signal_cli).await?;
                        }
                    }
                }
            }
        }
    }

    // Returns Err in case of a timeout or handler error
    // Returns Ok when success or error text is generated
    async fn handle_signal_admin_message(&self, msg: &Envelope) -> MessageHandlerResult {
        let data = msg.data_message.as_ref().unwrap();

        // Admin messages starting with / are handled by gateway
        // Other messages are passed to the configured message handler
        if data.message.starts_with("/") {
            // Parse the command using conf
            let cmd = parse_gateway_command(&data.message).map_err(|err| (400u16, err.into()))?;

            self.handle_gateway_command(cmd).await
        } else if let Some(handler) = &self.message_handler {
            let msg = AdminMessage {
                message: data.message.clone(),
                timestamp: data.timestamp,
                sender_uuid: msg.source_uuid.clone(),
                group_id: data.group_info.as_ref().map(|g| g.group_id.clone()),
            };
            handler
                .handle_verified_signal_message(msg, &GatewayContext)
                .await
        } else {
            Err((501u16, "No message handler configured".into()))
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
