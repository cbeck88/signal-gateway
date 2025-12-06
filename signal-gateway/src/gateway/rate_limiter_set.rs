//! Rate limiter set for managing per-route rate limiting.

use crate::{
    concurrent_map::LazyMap,
    log_message::{LogMessage, Origin},
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
}

/// A set of limiters for a route, containing both per-origin and global limiters.
pub struct LimiterSet {
    /// Per-origin rate limiters, keyed by origin. Lazily created.
    limiters: LazyMap<Origin, Vec<Limiter>>,
    /// Global rate limiters (shared across all origins).
    global_limiters: Vec<Limiter>,
}

impl LimiterSet {
    /// Create a new limiter set with factories for per-origin and global limiters.
    pub fn new(
        make_limiters: impl Fn() -> Vec<Limiter> + Send + Sync + 'static,
        global_limiters: Vec<Limiter>,
    ) -> Self {
        Self {
            limiters: LazyMap::new(make_limiters),
            global_limiters,
        }
    }

    /// Evaluate whether an event should pass all rate limits.
    ///
    /// Returns [`LimitResult::Passed`] if the event passes all limits.
    /// Returns [`LimitResult::Limiter(i)`] if blocked by per-origin limiter at index `i`.
    /// Returns [`LimitResult::GlobalLimiter(i)`] if blocked by global limiter at index `i`.
    pub fn evaluate(&self, log_msg: &LogMessage, origin: &Origin, ts_sec: i64) -> LimitResult {
        // Check per-origin limiters
        let origin_result = self.limiters.get(origin, |origin_limiters| {
            for (i, limiter) in origin_limiters.iter().enumerate() {
                if !limiter.evaluate(log_msg, ts_sec) {
                    return Some(LimitResult::Limiter(i));
                }
            }
            None
        });

        if let Some(result) = origin_result {
            return result;
        }

        // Check global limiters
        for (i, limiter) in self.global_limiters.iter().enumerate() {
            if !limiter.evaluate(log_msg, ts_sec) {
                return LimitResult::GlobalLimiter(i);
            }
        }

        LimitResult::Passed
    }
}
