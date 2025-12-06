//! Log message formatting configuration.
//!
//! This module provides configuration and formatting for log messages,
//! controlling how they are rendered for alerts.

use crate::log_message::LogMessage;
use chrono::{DateTime, TimeDelta, Utc};
use conf::Conf;
use std::fmt::Write;
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
    /// The `now` parameter is the current time, used to calculate relative
    /// timestamps (e.g., "T-10s").
    pub fn write_log_msg(&self, mut writer: impl std::fmt::Write, log_msg: &LogMessage, now: DateTime<Utc>) {
        let sev = log_msg.level.to_str();
        let msg = &log_msg.msg;

        // Format: "ERROR     T-10s [foo bar.rs:42]: message"
        // Pad severity to 5 chars (left-aligned), time to 8 chars (right-aligned)
        if self.write_log_msg_inner(&mut writer, log_msg, now, sev, msg).is_err() {
            error!("Couldn't write log message: {sev}: {msg}");
        }
    }

    fn write_log_msg_inner(
        &self,
        writer: &mut impl std::fmt::Write,
        log_msg: &LogMessage,
        now: DateTime<Utc>,
        sev: &str,
        msg: &str,
    ) -> std::fmt::Result {
        // Write severity (5 chars left-aligned)
        write!(writer, "{:<5} ", sev)?;

        // Write timestamp (8 chars right-aligned)
        let ts = log_msg
            .timestamp
            .and_then(|ts| DateTime::from_timestamp(ts, log_msg.timestamp_nanos));
        if let Some(ts) = ts {
            write_t_minus(writer, now - ts, 8)?;
        } else {
            write!(writer, "{:>8}", "T-?")?;
        }

        // Write metadata if configured
        let has_module = self.format_module && log_msg.module_path.is_some();
        let has_location = self.format_source_location;

        if has_module || has_location {
            write!(writer, " [")?;

            if let Some(module) = log_msg.module_path.as_ref().filter(|_| self.format_module) {
                write!(writer, "{module}")?;
                if has_location {
                    write!(writer, " ")?;
                }
            }

            if self.format_source_location {
                if let Some(file) = log_msg.file.as_ref() {
                    // Strip /home/{username}/ prefix if present
                    let trimmed = strip_prefix_and_one_slash(file, "/home/");
                    // Strip .cargo/registry/src/{hash}/ if present
                    let trimmed = strip_prefix_and_one_slash(trimmed, ".cargo/registry/src/");
                    write!(writer, "{trimmed}:")?;
                    if let Some(line) = log_msg.line.as_ref() {
                        write!(writer, "{line}")?;
                    } else {
                        write!(writer, "?")?;
                    }
                } else {
                    write!(writer, "?")?;
                }
            }

            write!(writer, "]")?;
        }

        writeln!(writer, ": {msg}")
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

/// Write a relative timestamp like "T-30s", "T-1m30s", "T+5s".
///
/// For durations < 1 second, shows milliseconds.
/// For durations < 1 minute, shows hundredths of a second.
/// For durations < 10 minutes, shows tenths of a second.
/// For durations > 1 day, shows days and omits seconds.
/// For durations > 1 hour, precision is reduced to minutes.
///
/// Output is right-aligned to `align` characters (0 for no alignment).
fn write_t_minus(writer: &mut impl Write, delta: TimeDelta, align: usize) -> std::fmt::Result {
    let total_secs = delta.num_seconds();
    let (prefix, total_secs, nanos) = if total_secs >= 0 {
        ("T-", total_secs as u64, delta.subsec_nanos() as u32)
    } else {
        // For negative deltas, subsec_nanos is also negative
        ("T+", (-total_secs) as u64, (-delta.subsec_nanos()) as u32)
    };

    // For durations < 1 second, show milliseconds
    if total_secs == 0 {
        let ms = nanos / 1_000_000;
        let width = 2 + digit_count(ms as u64) + 2; // "T-" + digits + "ms"
        for _ in width..align {
            write!(writer, " ")?;
        }
        return write!(writer, "{prefix}{ms}ms");
    }

    // For durations < 1 minute, show hundredths; < 10 minutes, show tenths
    let decimal_places = if total_secs < 60 { 2 } else if total_secs < 600 { 1 } else { 0 };
    let frac = match decimal_places {
        2 => nanos / 10_000_000,  // hundredths
        1 => nanos / 100_000_000, // tenths
        _ => 0,
    };

    // For durations > 1 day, omit seconds
    // For durations > 1 hour, reduce precision to minutes
    let show_secs = total_secs <= 86400;
    let total_secs = if total_secs > 3600 {
        total_secs - (total_secs % 60)
    } else {
        total_secs
    };

    // Format duration compactly (no spaces): "1d2h30m" or "1h2m30s"
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    // Calculate width for right-alignment
    let width = 2 // "T-" or "T+"
        + if days > 0 { digit_count(days) + 1 } else { 0 }
        + if hours > 0 || days > 0 { digit_count(hours) + 1 } else { 0 }
        + if mins > 0 || hours > 0 || days > 0 { digit_count(mins) + 1 } else { 0 }
        + if show_secs && (secs > 0 || frac > 0 || (days == 0 && hours == 0 && mins == 0)) {
            digit_count(secs) + decimal_places + 1 + if decimal_places > 0 { 1 } else { 0 }
            // e.g. "5s" = 1+1, "5.3s" = 1+1+1+1, "5.32s" = 1+2+1+1
        } else {
            0
        };

    // Right-align to `align` chars
    for _ in width..align {
        write!(writer, " ")?;
    }

    write!(writer, "{prefix}")?;
    if days > 0 {
        write!(writer, "{days}d")?;
    }
    if hours > 0 || days > 0 {
        write!(writer, "{hours}h")?;
    }
    if mins > 0 || hours > 0 || days > 0 {
        write!(writer, "{mins}m")?;
    }
    if show_secs && (secs > 0 || frac > 0 || (days == 0 && hours == 0 && mins == 0)) {
        match decimal_places {
            2 => write!(writer, "{secs}.{frac:02}s")?,
            1 => write!(writer, "{secs}.{frac}s")?,
            _ => write!(writer, "{secs}s")?,
        }
    }
    Ok(())
}

fn digit_count(n: u64) -> usize {
    if n == 0 { 1 } else { (n.ilog10() + 1) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt_delta(secs: i64, nanos: u32) -> String {
        let mut buf = String::new();
        let delta = TimeDelta::new(secs, nanos).unwrap();
        write_t_minus(&mut buf, delta, 8).unwrap();
        buf
    }

    fn fmt_neg_delta(secs: i64, nanos: u32) -> String {
        let mut buf = String::new();
        let delta = -TimeDelta::new(secs, nanos).unwrap();
        write_t_minus(&mut buf, delta, 8).unwrap();
        buf
    }

    #[test]
    fn test_zero_duration() {
        assert_eq!(fmt_delta(0, 0), "   T-0ms");
    }

    #[test]
    fn test_sub_second_shows_milliseconds() {
        assert_eq!(fmt_delta(0, 500_000_000), " T-500ms");
        assert_eq!(fmt_delta(0, 123_000_000), " T-123ms");
        assert_eq!(fmt_delta(0, 10_000_000), "  T-10ms");
        assert_eq!(fmt_delta(0, 1_000_000), "   T-1ms");
        assert_eq!(fmt_delta(0, 999_000_000), " T-999ms"); // just under 1s
    }

    #[test]
    fn test_under_one_minute_shows_hundredths() {
        // 1 second boundary: >= 1s shows seconds with hundredths
        assert_eq!(fmt_delta(1, 0), " T-1.00s");
        assert_eq!(fmt_delta(1, 500_000_000), " T-1.50s"); // 1500ms = 1.50s
        assert_eq!(fmt_delta(5, 320_000_000), " T-5.32s");
        assert_eq!(fmt_delta(30, 0), "T-30.00s");
        assert_eq!(fmt_delta(59, 990_000_000), "T-59.99s");
    }

    #[test]
    fn test_one_to_ten_minutes_shows_tenths() {
        // 0 seconds not shown when minutes present and no fractional
        assert_eq!(fmt_delta(60, 0), "    T-1m");
        // But 0.Xs shown when there's a fractional part
        assert_eq!(fmt_delta(60, 500_000_000), "T-1m0.5s");
        assert_eq!(fmt_delta(90, 300_000_000), "T-1m30.3s");
        assert_eq!(fmt_delta(5 * 60 + 30, 500_000_000), "T-5m30.5s");
        assert_eq!(fmt_delta(9 * 60 + 59, 900_000_000), "T-9m59.9s");
    }

    #[test]
    fn test_ten_minutes_to_one_hour_no_decimal() {
        // 0 seconds not shown when minutes present
        assert_eq!(fmt_delta(10 * 60, 0), "   T-10m");
        assert_eq!(fmt_delta(10 * 60 + 30, 500_000_000), "T-10m30s");
        assert_eq!(fmt_delta(30 * 60, 0), "   T-30m");
        assert_eq!(fmt_delta(59 * 60 + 59, 0), "T-59m59s");
    }

    #[test]
    fn test_one_hour_to_one_day_no_seconds() {
        // > 1 hour: precision reduced to minutes, no seconds shown
        assert_eq!(fmt_delta(3600, 0), "  T-1h0m");
        assert_eq!(fmt_delta(3600 + 30 * 60, 0), " T-1h30m");
        assert_eq!(fmt_delta(3600 + 30 * 60 + 45, 0), " T-1h30m"); // 45s truncated
        assert_eq!(fmt_delta(2 * 3600, 0), "  T-2h0m");
        assert_eq!(fmt_delta(23 * 3600 + 59 * 60, 0), "T-23h59m");
    }

    #[test]
    fn test_one_day_shows_days() {
        assert_eq!(fmt_delta(86400, 0), "T-1d0h0m");
        assert_eq!(fmt_delta(86400 + 3600, 0), "T-1d1h0m");
        assert_eq!(fmt_delta(86400 + 2 * 3600 + 30 * 60, 0), "T-1d2h30m");
    }

    #[test]
    fn test_multi_day() {
        assert_eq!(fmt_delta(2 * 86400, 0), "T-2d0h0m");
        assert_eq!(fmt_delta(7 * 86400, 0), "T-7d0h0m");
        assert_eq!(fmt_delta(30 * 86400 + 12 * 3600, 0), "T-30d12h0m");
    }

    #[test]
    fn test_negative_duration_future() {
        // Negative delta means event is in the future (T+)
        assert_eq!(fmt_neg_delta(5, 320_000_000), " T+5.32s");
        assert_eq!(fmt_neg_delta(90, 500_000_000), "T+1m30.5s");
        assert_eq!(fmt_neg_delta(3600, 0), "  T+1h0m");
    }

    #[test]
    fn test_right_alignment() {
        // All outputs should be exactly 8 chars (right-aligned)
        let cases = [
            (0, 0),
            (5, 0),
            (30, 0),
            (60, 0),
            (600, 0),
            (3600, 0),
            (86400, 0),
        ];
        for (secs, nanos) in cases {
            let result = fmt_delta(secs, nanos);
            assert!(
                result.len() >= 8,
                "Expected at least 8 chars for {secs}s, got {} chars: '{result}'",
                result.len()
            );
        }
    }

    #[test]
    fn test_long_durations_exceed_8_chars() {
        // Very long durations may exceed 8 chars
        let result = fmt_delta(100 * 86400, 0);
        assert!(result.len() > 8);
        assert!(result.starts_with("T-100d"));
    }

    #[test]
    fn test_no_alignment() {
        let mut buf = String::new();
        let delta = TimeDelta::new(5, 320_000_000).unwrap();
        write_t_minus(&mut buf, delta, 0).unwrap();
        assert_eq!(buf, "T-5.32s"); // No padding
    }
}
