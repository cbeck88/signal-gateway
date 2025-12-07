use super::{
    LimitResult, Limiter, LimiterSet, SignalAlertMessage, Summary, evaluate_limiter_sequence,
    log_buffer::LogBuffer,
    route::{Destination, Limit, Route},
};
use crate::{
    claude::{Tool, ToolExecutor},
    concurrent_map::LazyMap,
    log_format::LogFormatConfig,
    log_message::{LogFilter, LogMessage, Origin},
};
use async_trait::async_trait;
use chrono::Utc;
use conf::Conf;
use std::fmt;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info};

/// Reason why an alert was suppressed by rate limiting.
#[derive(Debug)]
pub enum SuppressionReason {
    /// No route's filter matched the message.
    NoRoutes,
    /// Suppressed by route limiters. Contains the index and result for each
    /// route whose filter matched but whose limiter blocked the message.
    Routes(Vec<(usize, LimitResult)>),
    /// Suppressed by an overall limiter at the given index.
    OverallLimiter(usize),
}

impl fmt::Display for SuppressionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuppressionReason::NoRoutes => write!(f, "no-routes"),
            SuppressionReason::Routes(failures) => {
                write!(f, "routes[")?;
                for (i, (idx, result)) in failures.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{idx}:{result:?}")?;
                }
                write!(f, "]")
            }
            SuppressionReason::OverallLimiter(idx) => {
                write!(f, "overall[{idx}]")
            }
        }
    }
}

/// Config options related to the log handler, and what log messages it chooses to alert on.
#[derive(Clone, Conf, Debug)]
#[conf(serde)]
pub struct LogHandlerConfig {
    /// Number of recent log messages to buffer per origin
    #[conf(long, env, default_value = "64")]
    pub log_buffer_size: usize,
    /// Debug logging level for suppressed messages.
    /// 0 = no logging, 1 = log only overall limiter, 2 = log routes + overall, 3 = log all.
    #[conf(long, env, default_value = "0")]
    pub debug_suppressions: u16,
    /// Log message formatting options.
    #[conf(flatten)]
    pub log_format: LogFormatConfig,
    /// Routes for matching and rate-limiting log messages.
    #[conf(long, env, value_parser = serde_json::from_str, default_value = "[]", serde(alias = "route"))]
    pub routes: Vec<Route>,
    /// Overall rate limits applied after route checks pass.
    #[conf(long, env, value_parser = serde_json::from_str, default_value = "[]", serde(alias = "overall_limit"))]
    pub overall_limits: Vec<Limit>,
}

/// The log handler takes log messages and decides what to do with them.
///
/// 1. Store them in a small circular buffer (per origin)
/// 2. If it is an error, and meets other criteria, trigger an alert,
///    i.e. send a message to admins containing this log and other recent logs.
/// 3. The maximum rate of alerts can also be configured.
///
/// Additionally, the log handler can format the buffer of recent logs into a string,
/// if requested.
pub struct LogHandler {
    config: LogHandlerConfig,
    signal_alert_mq_tx: UnboundedSender<SignalAlertMessage>,
    /// Log buffers keyed by origin (app + host). Lazily created.
    log_buffers: LazyMap<Origin, LogBuffer>,
    /// Routes with their associated limiter sets.
    routes: Vec<(Route, LimiterSet)>,
    /// Overall rate limiters applied after route checks pass.
    /// Each entry is a (filter, limiter) pair.
    overall_limits: Vec<(LogFilter, Limiter)>,
}

impl LogHandler {
    /// Initialize a new log handler
    pub fn new(
        config: LogHandlerConfig,
        signal_alert_mq_tx: UnboundedSender<SignalAlertMessage>,
    ) -> Self {
        let routes = config
            .routes
            .iter()
            .map(|route| (route.clone(), route.make_limiter_set()))
            .collect();

        let overall_limits = config
            .overall_limits
            .iter()
            .map(|limit| limit.make_limiter())
            .collect();

        let buffer_size = config.log_buffer_size;

        Self {
            config,
            signal_alert_mq_tx,
            log_buffers: LazyMap::new(move || LogBuffer::new(buffer_size)),
            routes,
            overall_limits,
        }
    }

    /// Format recent logs into a string for all origins, optionally filtered.
    ///
    /// If `filter` is provided, only origins matching the filter are included.
    pub async fn format_logs(&self, filter: Option<&str>) -> String {
        self.log_buffers.with_read_lock(|buffers| {
            if buffers.is_empty() {
                return "No log sources registered yet".to_string();
            }

            let mut text = String::with_capacity(4096);
            let now = Utc::now();

            for (origin, buffer) in buffers.iter() {
                // Apply filter if present
                if filter.is_some_and(|f| !origin.matches_filter(f)) {
                    continue;
                }

                use std::fmt::Write;
                writeln!(&mut text, "=== [{origin}] ===").unwrap();
                buffer.with_iter(|iter| {
                    writeln!(&mut text, "{} log messages (newest first):", iter.len()).unwrap();
                    // Guess at how much to reserve
                    text.reserve(iter.len() * 128);
                    for log_msg in iter {
                        self.config
                            .log_format
                            .write_log_msg(&mut text, log_msg, now);
                    }
                });
                text.push('\n');
            }

            if text.is_empty() {
                "No matching log sources".to_string()
            } else {
                text
            }
        })
    }

    /// Consume a new log message from the given origin
    pub async fn handle_log_message(&self, log_msg: LogMessage, origin: Origin) {
        let rate_limit_result = self.check_rate_limiters(&log_msg, &origin).await;

        if let Err(reason) = &rate_limit_result {
            let should_log = match self.config.debug_suppressions {
                0 => false,
                1 => matches!(reason, SuppressionReason::OverallLimiter(_)),
                2 => !matches!(reason, SuppressionReason::NoRoutes),
                _ => true,
            };
            if should_log {
                let sev = log_msg.level.to_str();
                info!("Suppressed {sev} ({reason}):\n{}", log_msg.msg);
            }
        }

        // Get or create the buffer for this origin, then record the message
        let formatted_text = self.log_buffers.get(&origin, |buffer| {
            if rate_limit_result.is_err() {
                buffer.push_back(log_msg);
                None
            } else {
                // Guess at capacity, it will be faster to use too much memory than too little
                // signal-cli JVM is a hog anyways.
                let mut text = String::with_capacity(4096);
                let mut first_msg_len = 0;
                let mut is_first = true;
                let now = Utc::now();

                buffer.push_back_and_drain(log_msg, |log_msg| {
                    self.config
                        .log_format
                        .write_log_msg(&mut text, log_msg, now);
                    if is_first {
                        first_msg_len = text.len();
                        is_first = false;
                    }
                });

                Some((text, first_msg_len))
            }
        });

        // Send alert if we have formatted text
        if let Some((text, first_msg_len)) = formatted_text {
            let destination_override = rate_limit_result.ok().flatten();
            if let Err(_err) = self.signal_alert_mq_tx.send(SignalAlertMessage {
                origin: Some(origin),
                text,
                attachment_paths: Default::default(),
                summary: Summary::Prefix(first_msg_len),
                destination_override,
            }) {
                error!("Could not send alert message, queue is closed");
            }
        }
    }

    /// Check if a log message passes all rate limiters.
    ///
    /// Tests the message against each route's filter in succession (no early return).
    /// For routes where the filter matches, evaluates the limiter set.
    ///
    /// Returns:
    /// - `Ok(Some(destination))` if passed and a route specified a destination override
    /// - `Ok(None)` if passed with no destination override
    /// - `Err(SuppressionReason::Routes(...))` if no route's limiter passed
    /// - `Err(SuppressionReason::Overall(...))` if routes passed but overall limiter failed
    async fn check_rate_limiters(
        &self,
        log_msg: &LogMessage,
        origin: &Origin,
    ) -> Result<Option<Destination>, SuppressionReason> {
        let mut route_failures: Vec<(usize, LimitResult)> = Vec::new();
        let mut first_passed_destination: Option<Option<Destination>> = None;

        // Test against each route's filter and limiter
        for (idx, (route, limiter_set)) in self.routes.iter().enumerate() {
            // Check if message level meets route's alert threshold
            if log_msg.level > route.alert_level {
                continue;
            }

            // Check if message matches route's filter
            if !route.filter.matches(log_msg) {
                continue;
            }

            // Filter matched, evaluate the limiter set
            let result = limiter_set.evaluate(log_msg, origin);

            match result {
                LimitResult::Passed => {
                    // Remember the first route that passed
                    if first_passed_destination.is_none() {
                        first_passed_destination = Some(route.destination.clone());
                    }
                }
                _ => {
                    // Record the failure
                    route_failures.push((idx, result));
                }
            }
        }

        // If no route passed, return the appropriate error
        let first_destination = match first_passed_destination {
            Some(dest) => dest,
            None if route_failures.is_empty() => return Err(SuppressionReason::NoRoutes),
            None => return Err(SuppressionReason::Routes(route_failures)),
        };

        // At least one route passed, now check overall limits
        if let Err(i) = evaluate_limiter_sequence(&self.overall_limits, log_msg) {
            return Err(SuppressionReason::OverallLimiter(i));
        }

        // All checks passed
        Ok(first_destination)
    }
}

fn logs_tool() -> Tool {
    Tool {
        name: "logs",
        description: "Get recent log messages from monitored applications. Returns buffered log entries, optionally filtered by application name or hostname.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Optional filter string. If it contains '@', format is 'app@host' where both parts are substring matches. Otherwise, matches either app or host containing the string."
                }
            },
            "required": []
        }),
    }
}

#[async_trait]
impl ToolExecutor for LogHandler {
    fn tools(&self) -> Vec<Tool> {
        vec![logs_tool()]
    }

    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<String, String> {
        match name {
            "logs" => {
                let filter = input.get("filter").and_then(|v| v.as_str());
                Ok(self.format_logs(filter).await)
            }
            _ => Err(format!("unknown tool: {name}")),
        }
    }
}
