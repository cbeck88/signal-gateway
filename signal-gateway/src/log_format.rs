//! Log message formatting configuration.
//!
//! This module provides configuration and formatting for log messages,
//! controlling how they are rendered for alerts.

use crate::human_duration::HumanTMinus;
use crate::log_message::LogMessage;
use chrono::TimeDelta;
use conf::Conf;
use tracing::error;

/// Configuration for log message formatting.
#[derive(Clone, Conf, Debug, Default)]
pub struct LogFormatConfig {
    /// Include the module path in formatted output.
    #[conf(long, env)]
    pub format_module: bool,
    /// Include the source file and line number in formatted output.
    #[conf(long, env)]
    pub format_source_location: bool,
}

impl LogFormatConfig {
    /// Format a log message into a Writer, followed by \n.
    ///
    /// The `now` parameter is the current timestamp in seconds, used to
    /// calculate relative timestamps (e.g., "T-10s").
    pub fn write_log_msg(&self, mut writer: impl std::fmt::Write, log_msg: &LogMessage, now: i64) {
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

        if self.format_module
            && let Some(module) = log_msg.module_path.as_ref()
        {
            metadata_parts.push(module.to_string());
        }

        if self.format_source_location {
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
