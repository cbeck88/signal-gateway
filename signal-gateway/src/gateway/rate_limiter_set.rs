//! Rate limiter set for managing per-route rate limiting.

use crate::{
    concurrent_map::LazyMap,
    log_message::{LogFilter, LogMessage, Origin},
    rate_limiter::Limiter,
};

/// Result of evaluating a limiter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitResult {
    /// The event passed all limits (not rate-limited).
    Passed,
    /// The event was blocked by a per-origin limiter at the given index.
    Limiter(usize),
    /// The event was blocked by a global limiter at the given index.
    GlobalLimiter(usize),
    /// The event was blocked by an overall limiter at the given index.
    OverallLimiter(usize),
}

/// A set of limiters for a route, containing both per-origin and global limiters.
/// Each limiter is paired with a filter that must match before the limiter is evaluated.
pub struct LimiterSet {
    /// Per-origin rate limiters, keyed by origin. Lazily created.
    /// Each entry is a (filter, limiter) pair.
    limiters: LazyMap<Origin, Vec<(LogFilter, Limiter)>>,
    /// Global rate limiters (shared across all origins).
    /// Each entry is a (filter, limiter) pair.
    global_limiters: Vec<(LogFilter, Limiter)>,
}

impl LimiterSet {
    /// Create a new limiter set with factories for per-origin and global limiters.
    pub fn new(
        make_limiters: impl Fn() -> Vec<(LogFilter, Limiter)> + Send + Sync + 'static,
        global_limiters: Vec<(LogFilter, Limiter)>,
    ) -> Self {
        Self {
            limiters: LazyMap::new(make_limiters),
            global_limiters,
        }
    }

    /// Evaluate whether an event should pass all rate limits.
    ///
    /// For each limiter, first checks if the message matches the filter.
    /// If it matches, evaluates the rate limiter.
    /// If it doesn't match, the limiter is skipped (passes).
    ///
    /// Returns [`LimitResult::Passed`] if the event passes all limits.
    /// Returns [`LimitResult::Limiter(i)`] if blocked by per-origin limiter at index `i`.
    /// Returns [`LimitResult::GlobalLimiter(i)`] if blocked by global limiter at index `i`.
    pub fn evaluate(&self, log_msg: &LogMessage, origin: &Origin, ts_sec: i64) -> LimitResult {
        // Check per-origin limiters
        let origin_result = self.limiters.get(origin, |origin_limiters| {
            for (i, (filter, limiter)) in origin_limiters.iter().enumerate() {
                // Only evaluate the limiter if the message matches the filter
                if filter.matches(log_msg) && !limiter.evaluate(log_msg, ts_sec) {
                    return Some(LimitResult::Limiter(i));
                }
            }
            None
        });

        if let Some(result) = origin_result {
            return result;
        }

        // Check global limiters
        for (i, (filter, limiter)) in self.global_limiters.iter().enumerate() {
            // Only evaluate the limiter if the message matches the filter
            if filter.matches(log_msg) && !limiter.evaluate(log_msg, ts_sec) {
                return LimitResult::GlobalLimiter(i);
            }
        }

        LimitResult::Passed
    }
}
