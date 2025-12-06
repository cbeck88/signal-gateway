//! Rate limiting for log alerts.

use crate::log_message::LogMessage;
use serde::Deserialize;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

/// Represents a rate threshold, expressed as a string in the format:
///
/// * `>= 2 / 10s` - burst detection: alert when rate >= 2 per 10s
/// * `> 1 / 10s` - burst detection: alert when rate > 1 per 10s (same as >= 2)
/// * `< 3 / 5m` - suppression: alert when rate < 3 per 5m
/// * `<= 2 / 5m` - suppression: alert when rate <= 2 per 5m (same as < 3)
///
/// The comparator must be specified (>=, >, <, or <=).
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct RateThreshold {
    /// Number of events for the threshold comparison.
    pub times: usize,
    /// Time window for counting events.
    pub duration: Duration,
    /// If true, alert when rate >= times/duration (burst detection).
    /// If false, alert when rate < times/duration (suppression).
    pub comparator_is_ge: bool,
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
            return Err("missing comparator (>=, >, <, or <=)".into());
        };

        let num = num.trim();
        let mut times: usize = num
            .parse()
            .map_err(|err| format!("invalid number {num}: {err}"))?;

        // Normalize to either >= (comparator_is_ge=true) or < (comparator_is_ge=false)
        let comparator_is_ge = match comparator.trim() {
            ">=" | "=>" => true,
            ">" => {
                // > N is equivalent to >= N+1
                times += 1;
                true
            }
            "<" => false,
            "<=" | "=<" => {
                // <= N is equivalent to < N+1
                times += 1;
                false
            }
            "" => return Err("missing comparator (>=, >, <, or <=)".into()),
            _ => return Err(format!("unexpected comparator: {comparator}")),
        };

        if times == 0 {
            return Err("invalid threshold: times must be > 0".into());
        }

        Ok(RateThreshold {
            times,
            duration,
            comparator_is_ge,
        })
    }
}

impl TryFrom<String> for RateThreshold {
    type Error = <RateThreshold as FromStr>::Err;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        RateThreshold::from_str(&s)
    }
}

/// Maximum entries in a source-location rate limiter before triggering cleanup.
const SOURCE_LOCATION_MAX_ENTRIES: usize = 2000;

/// A rate limiter that can be either a multi-rate limiter or a source-location limiter.
#[derive(Debug)]
pub enum Limiter {
    /// Counts events regardless of source location.
    Multi(MultiRateLimiter),
    /// Tracks events independently per source location (file:line).
    SourceLocation(SourceLocationRateLimiter),
}

impl Limiter {
    /// Evaluate whether an event should pass the rate limit.
    ///
    /// Returns `true` if the event should be allowed (not rate-limited),
    /// `false` if it should be suppressed.
    pub fn evaluate(&mut self, log_msg: &LogMessage, ts_sec: i64) -> bool {
        match self {
            Limiter::Multi(limiter) => limiter.evaluate(ts_sec),
            Limiter::SourceLocation(limiter) => {
                let file = log_msg.file.as_deref().unwrap_or("?");
                let line = log_msg.line.as_deref().unwrap_or("?");
                limiter.evaluate(file, line, ts_sec)
            }
        }
    }

    /// Create a multi-rate limiter from a threshold.
    pub fn multi(threshold: RateThreshold) -> Self {
        Limiter::Multi(MultiRateLimiter::from(threshold))
    }

    /// Create a source-location limiter from a threshold.
    ///
    /// Each source location (file:line) gets its own rate limiter with the full threshold.
    pub fn source_location(threshold: RateThreshold) -> Self {
        Limiter::SourceLocation(SourceLocationRateLimiter::new(
            threshold,
            SOURCE_LOCATION_MAX_ENTRIES,
        ))
    }
}

/// A rate limiter containing a single counter, and a minimum time window for the next event to pass
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct SimpleRateLimiter {
    last_timestamp: AtomicI64,
    window: i64,
}

#[allow(dead_code)]
impl SimpleRateLimiter {
    pub fn new(window: Duration) -> Self {
        Self {
            last_timestamp: Default::default(),
            window: window.as_secs().try_into().unwrap(),
        }
    }

    /// Check if a particular new timestamp passes the limit. This also updates the last-known timestamp.
    pub fn evaluate(&self, ts_sec: i64) -> bool {
        let last_ts = self.last_timestamp.load(Ordering::SeqCst);
        let rate_limited = ts_sec - last_ts < self.window;
        if !rate_limited && ts_sec > last_ts {
            // If this is called concurrently, guarantee that we keep going
            // until the max value is stored at self.last_timestamp,
            // so self.last_timestamp is "eventually" only monotonically increasing.
            store_max(ts_sec, &self.last_timestamp);
        }
        !rate_limited
    }
}

#[allow(dead_code)]
fn store_max(val: i64, at: &AtomicI64) {
    let prev = at.swap(val, Ordering::SeqCst);
    if prev > val {
        store_max(prev, at)
    }
}

/// A rate limiter that tracks alerts per source location (file:line).
///
/// This allows different error locations to alert independently, preventing one noisy
/// error from suppressing alerts from completely different code paths. Each location
/// gets its own `MultiRateLimiter` with the full threshold.
#[derive(Debug)]
pub struct SourceLocationRateLimiter {
    /// Maps (file, line) -> rate limiter for that location
    limiters: HashMap<(String, String), MultiRateLimiter>,
    /// Threshold for creating new limiters
    threshold: RateThreshold,
    /// Maximum entries before triggering cleanup
    max_entries: usize,
}

impl SourceLocationRateLimiter {
    pub fn new(threshold: RateThreshold, max_entries: usize) -> Self {
        Self {
            limiters: HashMap::new(),
            threshold,
            max_entries,
        }
    }

    /// Check if an error from this source location should trigger an alert.
    ///
    /// Returns true if the alert should fire (not rate-limited), false if suppressed.
    pub fn evaluate(&mut self, file: &str, line: &str, ts_sec: i64) -> bool {
        let key = (file.to_owned(), line.to_owned());

        let limiter = self
            .limiters
            .entry(key)
            .or_insert_with(|| MultiRateLimiter::from(self.threshold));

        let result = limiter.evaluate(ts_sec);

        // Clean up if we've exceeded max entries
        if self.limiters.len() > self.max_entries {
            self.cleanup(ts_sec);
        }

        result
    }

    /// Remove entries where all timestamps are older than the window
    fn cleanup(&mut self, now: i64) {
        let window = self.threshold.duration.as_secs() as i64;
        self.limiters.retain(|_, limiter| {
            // Keep if any timestamp is recent enough
            limiter.timestamps.iter().any(|&ts| now - ts < window)
        });
    }
}

/// Implements rate-limiting criteria such as 'at least n in the last w seconds'
#[derive(Debug)]
pub struct MultiRateLimiter {
    /// Records the last n events
    timestamps: Vec<i64>,
    /// Invariant: Always points to the oldest of the last n timestamps in the buffer
    idx: usize,
    /// The length of the window (in seconds)
    window: i64,
    /// If true, returns true when rate >= threshold (burst detection).
    /// If false, returns true when rate < threshold (suppression).
    comparator_is_ge: bool,
}

impl MultiRateLimiter {
    pub fn new(num: usize, window: Duration, comparator_is_ge: bool) -> Self {
        Self {
            idx: 0,
            timestamps: vec![Default::default(); num],
            window: window.as_secs().try_into().unwrap(),
            comparator_is_ge,
        }
    }

    /// Check if a particular new timestamp passes the limit. This also updates the last-known timestamp.
    ///
    /// Note: Assumes that new_timestamp is monotonically increasing, otherwise it might not work right.
    pub fn evaluate(&mut self, new_timestamp: i64) -> bool {
        let oldest = self.timestamps[self.idx];
        if oldest >= new_timestamp {
            // Duplicate timestamp - for burst detection return current state,
            // for suppression return inverted
            return !self.comparator_is_ge;
        }
        self.timestamps[self.idx] = new_timestamp;
        self.idx += 1;
        self.idx %= self.timestamps.len();
        let next_oldest = self.timestamps[self.idx];
        // If the next oldest is within 'window' of the new timestamp,
        // then all of the most recent n are within the window.
        let threshold_met = next_oldest + self.window >= new_timestamp;

        if self.comparator_is_ge {
            // Burst detection: alert when rate >= threshold
            threshold_met
        } else {
            // Suppression: alert when rate < threshold
            !threshold_met
        }
    }
}

impl From<RateThreshold> for MultiRateLimiter {
    fn from(src: RateThreshold) -> Self {
        Self::new(src.times, src.duration, src.comparator_is_ge)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn parse_rate_threshold() {
        // >= N: burst detection, alert when rate >= N
        let threshold = RateThreshold::from_str(">= 1 / 10s").unwrap();
        assert_eq!(threshold.times, 1);
        assert_eq!(threshold.duration, Duration::from_secs(10));
        assert!(threshold.comparator_is_ge);

        let threshold = RateThreshold::from_str(">=2 / 5m").unwrap();
        assert_eq!(threshold.times, 2);
        assert_eq!(threshold.duration, Duration::from_secs(300));
        assert!(threshold.comparator_is_ge);

        // > N: normalizes to >= N+1
        let threshold = RateThreshold::from_str("> 3 / 10m").unwrap();
        assert_eq!(threshold.times, 4);
        assert_eq!(threshold.duration, Duration::from_secs(600));
        assert!(threshold.comparator_is_ge);

        // < N: suppression, alert when rate < N
        let threshold = RateThreshold::from_str("< 5 / 1h").unwrap();
        assert_eq!(threshold.times, 5);
        assert_eq!(threshold.duration, Duration::from_secs(3600));
        assert!(!threshold.comparator_is_ge);

        // <= N: normalizes to < N+1
        let threshold = RateThreshold::from_str("<= 2 / 30s").unwrap();
        assert_eq!(threshold.times, 3);
        assert_eq!(threshold.duration, Duration::from_secs(30));
        assert!(!threshold.comparator_is_ge);

        // Missing comparator should error
        assert!(RateThreshold::from_str("1 / 10s").is_err());
        assert!(RateThreshold::from_str("5/10s").is_err());
    }

    #[test]
    fn source_location_rate_limiter_basic() {
        // Threshold: >= 1 event per 600 seconds per location (burst detection)
        // Returns true when there's at least 1 event in the window
        let threshold = RateThreshold {
            times: 1,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let mut limiter = SourceLocationRateLimiter::new(threshold, 100);

        // First alert from location A should pass (1 event >= 1)
        assert!(limiter.evaluate("file_a.rs", "10", 1000));

        // Second alert still passes (still >= 1 event in window)
        assert!(limiter.evaluate("file_a.rs", "10", 1100));

        // Alert from different location should also pass (independent)
        assert!(limiter.evaluate("file_b.rs", "20", 1100));

        // Same location after window passes should still pass
        assert!(limiter.evaluate("file_a.rs", "10", 1700));
    }

    #[test]
    fn source_location_rate_limiter_with_count() {
        // Threshold: >= 3 events per 600 seconds per location (burst detection)
        // Returns true when there are at least 3 events in the window
        let threshold = RateThreshold {
            times: 3,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let mut limiter = SourceLocationRateLimiter::new(threshold, 100);

        // First two alerts don't trigger (need 3 in window)
        assert!(!limiter.evaluate("file.rs", "10", 1000));
        assert!(!limiter.evaluate("file.rs", "10", 1100));

        // Third alert triggers (now have 3 in window)
        assert!(limiter.evaluate("file.rs", "10", 1200));

        // Fourth continues to trigger (still >= 3 in window)
        assert!(limiter.evaluate("file.rs", "10", 1300));

        // Different location is independent - starts fresh
        assert!(!limiter.evaluate("file.rs", "20", 1000));
        assert!(!limiter.evaluate("file.rs", "20", 1100));
        assert!(limiter.evaluate("file.rs", "20", 1200));
    }

    #[test]
    fn source_location_rate_limiter_different_lines_same_file() {
        // Threshold: >= 2 events per 600 seconds per location (burst detection)
        let threshold = RateThreshold {
            times: 2,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let mut limiter = SourceLocationRateLimiter::new(threshold, 100);

        // First event at each location doesn't trigger (need 2)
        assert!(!limiter.evaluate("file.rs", "10", 1000));
        assert!(!limiter.evaluate("file.rs", "20", 1000));
        assert!(!limiter.evaluate("file.rs", "30", 1000));

        // Second event at each location triggers
        assert!(limiter.evaluate("file.rs", "10", 1100));
        assert!(limiter.evaluate("file.rs", "20", 1100));
        assert!(limiter.evaluate("file.rs", "30", 1100));
    }

    #[test]
    fn source_location_rate_limiter_suppression() {
        // Threshold: < 3 events per 600 seconds per location (suppression)
        // Returns true when there are fewer than 3 events in the window
        let threshold = RateThreshold {
            times: 3,
            duration: Duration::from_secs(600),
            comparator_is_ge: false,
        };
        let mut limiter = SourceLocationRateLimiter::new(threshold, 100);

        // First two alerts pass (< 3 events in window)
        assert!(limiter.evaluate("file.rs", "10", 1000));
        assert!(limiter.evaluate("file.rs", "10", 1100));

        // Third alert suppressed (now have 3 in window, not < 3)
        assert!(!limiter.evaluate("file.rs", "10", 1200));

        // Fourth also suppressed (still >= 3 in window)
        assert!(!limiter.evaluate("file.rs", "10", 1300));

        // After window expires, first event passes again
        assert!(limiter.evaluate("file.rs", "10", 2000));
    }

    #[test]
    fn source_location_rate_limiter_cleanup() {
        // Use small max_entries to trigger cleanup
        let threshold = RateThreshold {
            times: 1,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let mut limiter = SourceLocationRateLimiter::new(threshold, 3);

        // Fill up the limiter
        assert!(limiter.evaluate("file1.rs", "1", 1000));
        assert!(limiter.evaluate("file2.rs", "2", 1000));
        assert!(limiter.evaluate("file3.rs", "3", 1000));
        assert_eq!(limiter.limiters.len(), 3);

        // Add one more, triggering cleanup - but all are fresh so none removed
        assert!(limiter.evaluate("file4.rs", "4", 1000));
        // Still have 4 after cleanup since none are old enough
        assert_eq!(limiter.limiters.len(), 4);

        // Now add with a timestamp far in the future - old entries should be cleaned
        assert!(limiter.evaluate("file5.rs", "5", 2000));
        // Should have cleaned up entries from timestamp 1000 (older than 600 sec window)
        assert_eq!(limiter.limiters.len(), 1);
    }
}
