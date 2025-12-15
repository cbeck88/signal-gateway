//! Configuration for a sequence of rate limiters, each with a filter criteria.
//! When a sequence is evaluated, all rate limiters are evaluated against the message,
//! without stopping early. But if any of them blocks the message, it is blocked,
//! and the first one to do so is returned.

use crate::{
    lazy_map_cleaner::TsSecs,
    log_message::{LogFilter, LogMessage},
    rate_limiter::{Limiter, RateThreshold},
};
use serde::Deserialize;

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
    /// Returns a (filter, limiter) pair so the filter can be checked before rate limiting.
    pub fn make_limiter(&self) -> (LogFilter, Limiter) {
        let limiter = if self.by_source_location {
            Limiter::source_location(self.threshold)
        } else {
            Limiter::multi(self.threshold)
        };
        (self.filter.clone(), limiter)
    }
}

/// A sequence of limits each applied in parallel to incoming messages.
/// If any of them blocks a message, it is blocked.
pub struct LimiterSequence(Vec<(LogFilter, Limiter)>);

impl LimiterSequence {
    /// Evaluate a sequence of (filter, limiter) pairs against a log message.
    ///
    /// Returns `Ok(())` if no limiter blocks the message.
    /// Returns `Err(index)` if the limiter at `index` blocked the message.
    ///
    /// Semantic:
    /// All limiters are evaluated regardless of if an earlier limiter blocks.
    pub fn evaluate(&self, log_msg: &LogMessage) -> Result<(), usize> {
        let mut maybe_idx = None;
        for (i, (filter, limiter)) in self.0.iter().enumerate() {
            if filter.matches(log_msg) && !limiter.evaluate(log_msg) && maybe_idx.is_none() {
                maybe_idx = Some(i);
            }
        }
        match maybe_idx {
            None => Ok(()),
            Some(i) => Err(i),
        }
    }
}

impl TsSecs for LimiterSequence {
    fn ts_secs(&self) -> i64 {
        self.0.iter().map(|(_f, l)| l.ts_secs()).max().unwrap_or(0)
    }
}

impl<'a> FromIterator<&'a Limit> for LimiterSequence {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = &'a Limit>,
    {
        Self(iter.into_iter().map(Limit::make_limiter).collect())
    }
}
