//! Route configuration for log message handling.
//!
//! Routes define how log messages are processed based on filters, severity levels,
//! and destination overrides.

use crate::log_message::{Level, LogFilter};
use serde::Deserialize;
use std::{str::FromStr, time::Duration};

/// Represents a rate threshold, expressed as a string in the format:
///
/// * `1 / 10s`
/// * `2 / 5m`
/// * `3 / 1h`
/// * `> 1 / 10s`
/// * `>= 2 / 10s`
///
/// When the comparator is omitted, it is treated as `>=`
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct RateThreshold {
    /// Number of events required to trigger the threshold.
    pub times: usize,
    /// Time window for counting events.
    pub duration: Duration,
}

impl FromStr for RateThreshold {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((first, second)) = s.trim().split_once('/') else {
            return Err("missing '/' character in rate threshold".into());
        };

        let duration = conf_extra::parse_duration(second.trim())?;

        let first = first.trim();
        let maybe_mid = first.as_bytes().iter().position(|b| b.is_ascii_digit());
        let (comparator, num) = if let Some(mid) = maybe_mid {
            first.split_at(mid)
        } else {
            ("", first)
        };

        let is_greater_equal = match comparator.trim() {
            ">" => false,
            ">=" | "=>" | "" => true,
            _ => return Err(format!("Unexpected comparator format: {comparator}")),
        };

        let num = num.trim();
        let mut times: usize = num
            .parse()
            .map_err(|err| format!("invalid number {num}: {err}"))?;

        if !is_greater_equal {
            times += 1;
        }

        if times == 0 {
            return Err("Invalid threshold, times must be > 0".into());
        }

        Ok(RateThreshold { times, duration })
    }
}

impl TryFrom<String> for RateThreshold {
    type Error = <RateThreshold as FromStr>::Err;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RateThreshold::from_str(&s)
    }
}

/// A rate limit rule for suppressing repeated alerts.
///
/// Combines a filter to match specific log messages with a rate threshold.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    /// Filter criteria for messages this limit applies to.
    #[serde(flatten)]
    pub filter: LogFilter,
    /// Rate threshold for suppressing alerts.
    pub threshold: RateThreshold,
    /// If true, rate limit independently per source location (file:line).
    /// If false (default), count all matching events together.
    #[serde(default)]
    pub by_source_location: bool,
}

impl Limit {
    /// Create the appropriate limiter for this limit configuration.
    pub fn make_limiter(&self) -> super::rate_limiter::Limiter {
        if self.by_source_location {
            super::rate_limiter::Limiter::source_location(self.threshold)
        } else {
            super::rate_limiter::Limiter::multi(self.threshold)
        }
    }
}

/// A route configuration for processing log messages.
///
/// Routes match incoming log messages based on an optional filter, then apply
/// the configured alert level threshold. Each route can optionally override
/// the default destination and define rate limits.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Minimum severity level for messages to trigger an alert.
    /// Messages at this level or higher (lower numeric value) will alert.
    /// Default: ERROR
    #[serde(default = "default_alert_level")]
    pub alert_level: Level,

    /// Optional filter to match log messages for this route.
    /// If not specified, the route matches all messages.
    #[serde(default)]
    pub filter: Option<LogFilter>,

    /// Optional destination override for alerts from this route.
    /// If not specified, alerts go to the default admin destination.
    #[serde(default)]
    pub destination: Option<Destination>,

    /// Rate limits applied per-origin for messages matching this route.
    /// Each limit specifies a filter and threshold for suppressing repeated alerts.
    #[serde(default)]
    pub limit: Vec<Limit>,

    /// Global rate limits applied across all origins for this route.
    /// Each limit specifies a filter and threshold for suppressing repeated alerts.
    #[serde(default)]
    pub global_limit: Vec<Limit>,
}

fn default_alert_level() -> Level {
    Level::ERROR
}

impl Route {
    /// Create a limiter set from this route's limit configurations.
    pub fn make_limiter_set(&self) -> super::rate_limiter::LimiterSet {
        super::rate_limiter::LimiterSet::new(self.limit.clone(), self.global_limit.clone())
    }
}

impl Default for Route {
    fn default() -> Self {
        Self {
            alert_level: default_alert_level(),
            filter: None,
            destination: None,
            limit: Vec::new(),
            global_limit: Vec::new(),
        }
    }
}

/// Destination override for alert messages.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// Send to specific recipient UUIDs.
    Recipients(Vec<String>),
    /// Send to a Signal group by group ID.
    Group(String),
}
