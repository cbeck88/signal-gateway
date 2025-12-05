use super::circular_buffer::CircularBuffer;
use super::{AdminMessage, MultiRateLimiter, Origin, RateThreshold, SourceLocationRateLimiter};
use crate::human_duration::HumanTMinus;
use chrono::{TimeDelta, Utc};
use conf::Conf;
use serde::Deserialize;
use std::{fmt, time::Duration};
use syslog_rfc5424::{SyslogMessage, SyslogSeverity};
use tokio::sync::{Mutex, mpsc::UnboundedSender};
use tracing::{error, info, warn};

/// Reason why an alert was suppressed
enum SuppressionReason {
    /// Suppressed by a configured alert rule (with 0-based rule index)
    Rule(usize),
    /// Suppressed by the source-location rate limiter
    SourceLocation { file: String, line: String },
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
    /// Structured data ID for tracing metadata (module, file, line) in syslog messages
    #[conf(long, env, default_value = "tracing-meta@64700")]
    pub sd_id: String,
    /// Number of recent log messages to buffer per origin
    #[conf(long, env, default_value = "64")]
    pub log_buffer_size: usize,
}

/// Specifies both a rate limiting threshold, and criteria for the threshold to apply
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertRule {
    #[serde(default)]
    pub msg_contains: String,
    #[serde(default)]
    pub module_equals: String,
    #[serde(default)]
    pub file_equals: String,
    #[serde(default)]
    pub line_equals: String,
    pub threshold: RateThreshold,
}

impl AlertRule {
    /// Check if a syslog message passes the filter defined by this rule
    fn eval_filter(&self, syslog_msg: &SyslogMessage, sd_id: &str) -> bool {
        if !self.msg_contains.is_empty() && !syslog_msg.msg.contains(&self.msg_contains) {
            return false;
        }

        if !self.module_equals.is_empty() {
            match syslog_msg.sd.find_tuple(sd_id, "module") {
                Some(module) if module == self.module_equals.as_str() => {}
                _ => return false,
            }
        }

        if !self.file_equals.is_empty() {
            match syslog_msg.sd.find_tuple(sd_id, "file") {
                Some(file) if file == self.file_equals.as_str() => {}
                _ => return false,
            }
        }

        if !self.line_equals.is_empty() {
            match syslog_msg.sd.find_tuple(sd_id, "line") {
                Some(line) if line == self.line_equals.as_str() => {}
                _ => return false,
            }
        }

        true
    }
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
    syslog_buffer: Mutex<CircularBuffer<SyslogMessage>>,
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
            .any(|r| !r.module_equals.is_empty());
        let any_rule_uses_file = config
            .alert_rate_limits
            .iter()
            .any(|r| !r.file_equals.is_empty());
        let any_rule_uses_line = config
            .alert_rate_limits
            .iter()
            .any(|r| !r.line_equals.is_empty());
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

        let syslog_buffer = Mutex::new(CircularBuffer::new(config.log_buffer_size));

        Self {
            config,
            admin_mq_tx,
            syslog_buffer,
            rate_limiters,
            overall_limiter,
            any_rule_uses_module,
            any_rule_uses_file,
            any_rule_uses_line,
        }
    }

    /// Format recent logs into a string
    pub async fn format_logs(&self) -> String {
        let lk = self.syslog_buffer.lock().await;

        let mut text = format!("{} log messages (newest first):\n", lk.len());

        // Calculate now once for consistent relative timestamps
        let now = Utc::now().timestamp();

        // Collect and reverse to show newest first
        let messages: Vec<_> = lk.iter().collect();
        for syslog_msg in messages.into_iter().rev() {
            self.write_syslog_msg(&mut text, syslog_msg, now);
        }
        text
    }

    /// Consume a new syslog message from the given origin
    pub async fn handle_syslog_message(&self, mut syslog_msg: SyslogMessage, origin: Origin) {
        let suppression_reason = self.check_suppression(&mut syslog_msg).await;
        if let Some(reason) = &suppression_reason
            && syslog_msg.severity <= SyslogSeverity::SEV_ERR
        {
            let sev = Self::severity_to_str(syslog_msg.severity);
            info!("Suppressed {sev} ({reason}):\n{}", syslog_msg.msg);
        }

        // Record this new message.
        // Then, if we should alert now, also format the whole buffer to a string,
        // and then release the lock.
        let formatted_text = {
            let mut lk = self.syslog_buffer.lock().await;
            lk.push_back(syslog_msg);
            if suppression_reason.is_some() {
                return;
            }

            let mut text = String::default();

            // Calculate now once for consistent relative timestamps
            let now = Utc::now().timestamp();

            // Iterate in reverse (newest first) without copying
            for syslog_msg in lk.iter().rev() {
                self.write_syslog_msg(&mut text, syslog_msg, now);
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

    // Convert SyslogSeverity to our own all-caps string that fits in 5 chars
    fn severity_to_str(severity: SyslogSeverity) -> &'static str {
        match severity {
            SyslogSeverity::SEV_EMERG => "EMERG",
            SyslogSeverity::SEV_ALERT => "ALERT",
            SyslogSeverity::SEV_CRIT => "CRIT",
            SyslogSeverity::SEV_ERR => "ERROR",
            SyslogSeverity::SEV_WARNING => "WARN",
            SyslogSeverity::SEV_NOTICE => "NOTE",
            SyslogSeverity::SEV_INFO => "INFO",
            SyslogSeverity::SEV_DEBUG => "DEBUG",
        }
    }

    // Format a syslog message into a Writer, followed by \n, and using any config options to do so
    fn write_syslog_msg(
        &self,
        mut writer: impl std::fmt::Write,
        syslog_msg: &SyslogMessage,
        now: i64,
    ) {
        let sev = Self::severity_to_str(syslog_msg.severity);
        let msg = &syslog_msg.msg;

        // Format relative timestamp if available
        let time_str = if let Some(ts) = syslog_msg.timestamp {
            let diff_secs = now.saturating_sub(ts);
            HumanTMinus(TimeDelta::seconds(diff_secs)).to_string()
        } else {
            "T-?".to_owned()
        };

        // Extract metadata from structured data if configured
        let sd_id = &self.config.sd_id;
        let mut metadata_parts = Vec::new();

        if self.config.format_module
            && let Some(module) = syslog_msg.sd.find_tuple(sd_id, "module")
        {
            metadata_parts.push(module.to_string());
        }

        if self.config.format_source_location {
            let file_opt = syslog_msg.sd.find_tuple(sd_id, "file");
            let line_opt = syslog_msg.sd.find_tuple(sd_id, "line");

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
            error!("Couldn't write syslog message ({err}): {sev}: {msg}");
        }
    }

    /// Check if an alert should be suppressed for a given error message.
    ///
    /// Returns `None` if the alert should fire, or `Some(reason)` if suppressed.
    async fn check_suppression(&self, syslog_msg: &mut SyslogMessage) -> Option<SuppressionReason> {
        let ts_sec = *syslog_msg
            .timestamp
            .get_or_insert_with(|| Utc::now().timestamp());

        let high_severity = syslog_msg.severity <= SyslogSeverity::SEV_ERR;
        if !high_severity {
            // Low severity messages are always "suppressed" (not alerted on)
            // but we don't need to log a reason for this
            return Some(SuppressionReason::Rule(usize::MAX));
        }

        let sd_id = &self.config.sd_id;

        // Warn if rules expect structured data but the message doesn't have it
        let has_module = syslog_msg.sd.find_tuple(sd_id, "module").is_some();
        let has_file = syslog_msg.sd.find_tuple(sd_id, "file").is_some();
        let has_line = syslog_msg.sd.find_tuple(sd_id, "line").is_some();
        if (self.any_rule_uses_module && !has_module)
            || (self.any_rule_uses_file && !has_file)
            || (self.any_rule_uses_line && !has_line)
        {
            warn!(
                "Error message missing structured data (sd_id={sd_id}), filtering rules may not work: {syslog_msg:#?}"
            );
        }

        // Check each configured rule - track which rule suppressed the alert
        // Note: we check all rules even if one already suppressed, to update all rate limiters
        let mut suppressed_by_rule: Option<usize> = None;
        for (idx, (filter, limiter)) in self.rate_limiters.iter().enumerate() {
            if filter.eval_filter(syslog_msg, sd_id) && !limiter.lock().await.evaluate(ts_sec) {
                suppressed_by_rule.get_or_insert(idx);
            }
        }
        if let Some(idx) = suppressed_by_rule {
            return Some(SuppressionReason::Rule(idx));
        }

        // Extract source location for per-location rate limiting
        let file = syslog_msg
            .sd
            .find_tuple(sd_id, "file")
            .map_or("?", |s| s.as_str());
        let line = syslog_msg
            .sd
            .find_tuple(sd_id, "line")
            .map_or("?", |s| s.as_str());

        if !self
            .overall_limiter
            .lock()
            .await
            .evaluate(file, line, ts_sec)
        {
            return Some(SuppressionReason::SourceLocation {
                file: file.to_owned(),
                line: line.to_owned(),
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
