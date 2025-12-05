use super::circular_buffer::CircularBuffer;
use super::{AdminMessage, MultiRateLimiter, RateThreshold, SourceLocationRateLimiter};
use crate::{
    human_duration::HumanTMinus,
    log_message::{Level, LogFilter, LogMessage, Origin},
};
use chrono::{TimeDelta, Utc};
use conf::Conf;
use serde::Deserialize;
use std::{fmt, time::Duration};
use tokio::sync::{Mutex, mpsc::UnboundedSender};
use tracing::{error, info, warn};

/// Reason why an alert was suppressed
enum SuppressionReason {
    /// Suppressed by a configured alert rule (with 0-based rule index)
    Rule(usize),
    /// Suppressed by the source-location rate limiter
    SourceLocation { file: Box<str>, line: Box<str> },
}

impl fmt::Display for SuppressionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuppressionReason::Rule(idx) => write!(f, "rule[{idx}]"),
            SuppressionReason::SourceLocation { file, line } => {
                write!(f, "source-location({file}:{line})")
            }
        }
    }
}

/// Config options related to the log handler, and what log messages it chooses to alert on.
#[derive(Clone, Conf, Debug)]
pub struct LogHandlerConfig {
    #[conf(long, env, value_parser = serde_json::from_str)]
    pub alert_rate_limits: Vec<AlertRule>,
    #[conf(long, env, default_value = "10m", value_parser = conf_extra::parse_duration)]
    pub overall_alert_limit: Duration,
    #[conf(long, env)]
    pub format_module: bool,
    #[conf(long, env)]
    pub format_source_location: bool,
    /// Number of recent log messages to buffer per origin
    #[conf(long, env, default_value = "64")]
    pub log_buffer_size: usize,
}

/// Specifies both a rate limiting threshold, and criteria for the threshold to apply
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRule {
    #[serde(flatten)]
    pub filter: LogFilter,
    pub threshold: RateThreshold,
}

/// The log handler takes log messages from a single origin and decides what
/// to do with them.
///
/// 1. Store them in a small circular buffer
/// 2. If it is an error, and meets other criteria, trigger an alert,
///    i.e. send a message to admins containing this log and other recent logs.
/// 3. The maximum rate of alerts can also be configured.
///
/// Additionally, the log handler can format the buffer of recent logs into a string,
/// if requested.
///
/// Each origin (app + host pair) gets its own LogHandler instance, managed by the Gateway.
#[derive(Debug)]
pub struct LogHandler {
    config: LogHandlerConfig,
    admin_mq_tx: UnboundedSender<AdminMessage>,
    log_buffer: Mutex<CircularBuffer<LogMessage>>,
    rate_limiters: Vec<(AlertRule, Mutex<MultiRateLimiter>)>,
    /// Rate limiter keyed by source location (file:line), so different error locations
    /// can alert independently without suppressing each other.
    overall_limiter: Mutex<SourceLocationRateLimiter>,
    /// True if any configured rule uses the module structured data field.
    any_rule_uses_module: bool,
    /// True if any configured rule uses the file structured data field.
    any_rule_uses_file: bool,
    /// True if any configured rule uses the line structured data field.
    any_rule_uses_line: bool,
}

/// Maximum entries in the source-location rate limiter before triggering cleanup
const OVERALL_LIMITER_MAX_ENTRIES: usize = 2000;

impl LogHandler {
    /// Initialize a new log handler
    pub fn new(config: LogHandlerConfig, admin_mq_tx: UnboundedSender<AdminMessage>) -> Self {
        let any_rule_uses_module = config
            .alert_rate_limits
            .iter()
            .any(|r| r.filter.uses_module());
        let any_rule_uses_file = config
            .alert_rate_limits
            .iter()
            .any(|r| r.filter.uses_file());
        let any_rule_uses_line = config
            .alert_rate_limits
            .iter()
            .any(|r| r.filter.uses_line());
        let rate_limiters = config
            .alert_rate_limits
            .iter()
            .map(|rule| {
                (
                    rule.clone(),
                    Mutex::new(MultiRateLimiter::from(rule.threshold)),
                )
            })
            .collect::<Vec<_>>();
        let overall_limiter = Mutex::new(SourceLocationRateLimiter::new(
            config.overall_alert_limit,
            OVERALL_LIMITER_MAX_ENTRIES,
        ));

        let log_buffer = Mutex::new(CircularBuffer::new(config.log_buffer_size));

        Self {
            config,
            admin_mq_tx,
            log_buffer,
            rate_limiters,
            overall_limiter,
            any_rule_uses_module,
            any_rule_uses_file,
            any_rule_uses_line,
        }
    }

    /// Format recent logs into a string
    pub async fn format_logs(&self) -> String {
        let lk = self.log_buffer.lock().await;

        let mut text = format!("{} log messages (newest first):\n", lk.len());

        // Calculate now once for consistent relative timestamps
        let now = Utc::now().timestamp();

        // Collect and reverse to show newest first
        let messages: Vec<_> = lk.iter().collect();
        for log_msg in messages.into_iter().rev() {
            self.write_log_msg(&mut text, log_msg, now);
        }
        text
    }

    /// Consume a new log message from the given origin
    pub async fn handle_log_message(&self, mut log_msg: LogMessage, origin: Origin) {
        let suppression_reason = self.check_suppression(&mut log_msg).await;
        if let Some(reason) = &suppression_reason
            && log_msg.level <= Level::ERROR
        {
            let sev = log_msg.level.to_str();
            info!("Suppressed {sev} ({reason}):\n{}", log_msg.msg);
        }

        // Record this new message.
        // Then, if we should alert now, also format the whole buffer to a string,
        // and then release the lock.
        let formatted_text = {
            let mut lk = self.log_buffer.lock().await;
            lk.push_back(log_msg);
            if suppression_reason.is_some() {
                return;
            }

            let mut text = String::default();

            // Calculate now once for consistent relative timestamps
            let now = Utc::now().timestamp();

            // Iterate in reverse (newest first) without copying
            for log_msg in lk.iter().rev() {
                self.write_log_msg(&mut text, log_msg, now);
            }
            lk.clear();

            text
        };

        if let Err(_err) = self.admin_mq_tx.send(AdminMessage {
            origin: Some(origin),
            text: formatted_text,
            attachment_paths: Default::default(),
            summary: None,
        }) {
            error!("Could not send alert message, queue is closed");
        }
    }

    // Format a log message into a Writer, followed by \n, and using any config options to do so
    fn write_log_msg(&self, mut writer: impl std::fmt::Write, log_msg: &LogMessage, now: i64) {
        let sev = log_msg.level.to_str();
        let msg = &log_msg.msg;

        // Format relative timestamp if available
        let time_str = if let Some(ts) = log_msg.timestamp {
            let diff_secs = now.saturating_sub(ts);
            HumanTMinus(TimeDelta::seconds(diff_secs)).to_string()
        } else {
            "T-?".to_owned()
        };

        // Extract metadata from structured data if configured
        let mut metadata_parts = Vec::new();

        if self.config.format_module
            && let Some(module) = log_msg.module_path.as_ref()
        {
            metadata_parts.push(module.to_string());
        }

        if self.config.format_source_location {
            let file_opt = log_msg.file.as_ref();
            let line_opt = log_msg.line.as_ref();

            let location = if let Some(file) = file_opt {
                // Strip /home/{username}/ prefix if present
                let trimmed_file = strip_prefix_and_one_slash(file, "/home/");
                // Strip .cargo/registry/src/{hash}/ if present
                let trimmed_file = strip_prefix_and_one_slash(trimmed_file, ".cargo/registry/src/");

                if let Some(line) = line_opt {
                    format!("{trimmed_file}:{line}")
                } else {
                    format!("{trimmed_file}:?")
                }
            } else {
                // No file present, use "?" even if line is present
                "?".to_owned()
            };

            metadata_parts.push(location);
        }

        // Format: "ERROR     T-10s [foo bar.rs:42]: message"
        // Pad severity to 5 chars (left-aligned), time to 8 chars (right-aligned)
        let result = if metadata_parts.is_empty() {
            writeln!(writer, "{:<5} {:>8}: {}", sev, time_str, msg)
        } else {
            let metadata = metadata_parts.join(" ");
            writeln!(writer, "{:<5} {:>8} [{}]: {}", sev, time_str, metadata, msg)
        };

        if let Err(err) = result {
            error!("Couldn't write log message ({err}): {sev}: {msg}");
        }
    }

    /// Check if an alert should be suppressed for a given error message.
    ///
    /// Returns `None` if the alert should fire, or `Some(reason)` if suppressed.
    async fn check_suppression(&self, log_msg: &mut LogMessage) -> Option<SuppressionReason> {
        let ts_sec = *log_msg
            .timestamp
            .get_or_insert_with(|| Utc::now().timestamp());

        let high_severity = log_msg.level <= Level::ERROR;
        if !high_severity {
            // Low severity messages are always "suppressed" (not alerted on)
            // but we don't need to log a reason for this
            return Some(SuppressionReason::Rule(usize::MAX));
        }

        // Warn if rules expect structured data but the message doesn't have it
        let has_module = log_msg.module_path.is_some();
        let has_file = log_msg.file.is_some();
        let has_line = log_msg.line.is_some();
        if (self.any_rule_uses_module && !has_module)
            || (self.any_rule_uses_file && !has_file)
            || (self.any_rule_uses_line && !has_line)
        {
            warn!(
                "Log message missing source location data, filtering rules may not work: {log_msg:#?}"
            );
        }

        // Check each configured rule - track which rule suppressed the alert
        // Note: we check all rules even if one already suppressed, to update all rate limiters
        let mut suppressed_by_rule: Option<usize> = None;
        for (idx, (rule, limiter)) in self.rate_limiters.iter().enumerate() {
            if rule.filter.matches(log_msg) && !limiter.lock().await.evaluate(ts_sec) {
                suppressed_by_rule.get_or_insert(idx);
            }
        }
        if let Some(idx) = suppressed_by_rule {
            return Some(SuppressionReason::Rule(idx));
        }

        // Extract source location for per-location rate limiting
        let file = log_msg.file.as_deref().unwrap_or("?");
        let line = log_msg.line.as_deref().unwrap_or("?");

        if !self
            .overall_limiter
            .lock()
            .await
            .evaluate(file, line, ts_sec)
        {
            return Some(SuppressionReason::SourceLocation {
                file: file.into(),
                line: line.into(),
            });
        }

        None // Alert should fire
    }
}

// Strip a prefix, then find the first remaining slash and skip up to that as well.
fn strip_prefix_and_one_slash<'a>(target: &'a str, prefix: &str) -> &'a str {
    let Some(target) = target.strip_prefix(prefix) else {
        return target;
    };

    if let Some((_, after)) = target.split_once('/') {
        after
    } else {
        target
    }
}
