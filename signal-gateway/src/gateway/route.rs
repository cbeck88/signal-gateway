//! Route configuration for log message handling.
//!
//! Routes define how log messages are processed based on filters, severity levels,
//! and destination overrides.

use super::LimiterSet;
use crate::{
    limiter_sequence::Limit,
    log_message::{Level, LogFilter},
};
use serde::Deserialize;
use std::time::Duration;

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

    /// Filter to match log messages for this route.
    /// If all filter fields are empty, the route matches all messages.
    #[serde(flatten)]
    pub filter: LogFilter,

    /// Optional destination override for alerts from this route.
    /// If not specified, alerts go to the default admin destination.
    #[serde(default)]
    pub destination: Option<Destination>,

    /// Rate limits applied per-origin for messages matching this route.
    /// Each limit specifies a filter and threshold for suppressing repeated alerts.
    #[serde(default, alias = "limit")]
    pub limits: Vec<Limit>,

    /// Global rate limits applied across all origins for this route.
    /// Each limit specifies a filter and threshold for suppressing repeated alerts.
    #[serde(default, alias = "global_limit")]
    pub global_limits: Vec<Limit>,
}

fn default_alert_level() -> Level {
    Level::ERROR
}

impl Route {
    /// Create a limiter set from this route's limit configurations.
    pub fn make_limiter_set(&self, max_origin_age: Duration) -> LimiterSet {
        let limits = self.limits.clone();
        let global_limiters = self.global_limits.iter().collect();
        LimiterSet::new(
            move || limits.iter().collect(),
            global_limiters,
            max_origin_age,
        )
    }
}

impl Default for Route {
    fn default() -> Self {
        Self {
            alert_level: default_alert_level(),
            filter: LogFilter::default(),
            destination: None,
            limits: Vec::new(),
            global_limits: Vec::new(),
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
