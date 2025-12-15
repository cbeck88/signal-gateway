//! Rate limiter set for managing per-route rate limiting.

use crate::{
    concurrent_map::LazyMap,
    limiter_sequence::LimiterSequence,
    log_message::{LogMessage, Origin},
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

/// A set of limiter sequences for a route, containing both per-origin and global limiters.
pub struct LimiterSet {
    /// Per-origin rate limiters, keyed by origin. Lazily created.
    limiters: LazyMap<Origin, LimiterSequence>,
    /// Global rate limiters (shared across all origins).
    global_limiters: LimiterSequence,
}

impl LimiterSet {
    /// Create a new limiter set with factories for per-origin and global limiters.
    pub fn new(
        make_limiters: impl Fn() -> LimiterSequence + Send + Sync + 'static,
        global_limiters: LimiterSequence,
    ) -> Self {
        Self {
            limiters: LazyMap::new(move |_key| make_limiters()),
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
    pub fn evaluate(&self, log_msg: &LogMessage, origin: &Origin) -> LimitResult {
        // Check per-origin limiters
        let origin_result = self.limiters.get(origin, |origin_limiters| {
            origin_limiters
                .evaluate(log_msg)
                .err()
                .map(LimitResult::Limiter)
        });

        if let Some(result) = origin_result {
            return result;
        }

        // Check global limiters
        if let Err(i) = self.global_limiters.evaluate(log_msg) {
            return LimitResult::GlobalLimiter(i);
        }

        LimitResult::Passed
    }
}
