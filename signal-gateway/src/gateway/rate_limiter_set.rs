//! Rate limiter set for managing per-route rate limiting.

use crate::{
    log_message::{LogMessage, Origin},
    rate_limiter::Limiter,
};
use std::collections::HashMap;

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
    /// Factory to create limiters for new origins.
    make_limiters: Box<dyn Fn() -> Vec<Limiter> + Send + Sync>,
    /// Per-origin rate limiters, keyed by origin. Lazily created.
    limiters: HashMap<Origin, Vec<Limiter>>,
    /// Global rate limiters (shared across all origins).
    global_limiters: Vec<Limiter>,
}

impl std::fmt::Debug for LimiterSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LimiterSet")
            .field("limiters", &self.limiters)
            .field("global_limiters", &self.global_limiters)
            .finish_non_exhaustive()
    }
}

impl LimiterSet {
    /// Create a new limiter set with factories for per-origin and global limiters.
    pub fn new(
        make_limiters: impl Fn() -> Vec<Limiter> + Send + Sync + 'static,
        global_limiters: Vec<Limiter>,
    ) -> Self {
        Self {
            make_limiters: Box::new(make_limiters),
            limiters: HashMap::new(),
            global_limiters,
        }
    }

    /// Evaluate whether an event should pass all rate limits.
    ///
    /// Returns [`LimitResult::Passed`] if the event passes all limits.
    /// Returns [`LimitResult::Limiter(i)`] if blocked by per-origin limiter at index `i`.
    /// Returns [`LimitResult::GlobalLimiter(i)`] if blocked by global limiter at index `i`.
    pub fn evaluate(&mut self, log_msg: &LogMessage, origin: &Origin, ts_sec: i64) -> LimitResult {
        // Get or create limiters for this origin
        let origin_limiters = self
            .limiters
            .entry(origin.clone())
            .or_insert_with(&self.make_limiters);

        for (i, limiter) in origin_limiters.iter_mut().enumerate() {
            if !limiter.evaluate(log_msg, ts_sec) {
                return LimitResult::Limiter(i);
            }
        }

        for (i, limiter) in self.global_limiters.iter_mut().enumerate() {
            if !limiter.evaluate(log_msg, ts_sec) {
                return LimitResult::GlobalLimiter(i);
            }
        }

        LimitResult::Passed
    }
}
