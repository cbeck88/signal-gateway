//! Rate limiting for log alerts.

use crate::{concurrent_map::LazyMap, log_message::LogMessage};
use serde::Deserialize;
use std::{
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicI64, Ordering},
    },
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
    /// If true, passes threshold when rate >= times/duration (burst detection).
    /// If false, passes threshold when rate < times/duration (suppression).
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
                times += 1;
                true
            }
            "<" => false,
            "<=" | "=<" => {
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

/// An enum over rate limiter implementations
pub enum Limiter {
    /// Applies a threshold based on recent occurrences of the event.
    Multi(MultiRateLimiter),
    /// Tracks events independently per source location (file:line).
    SourceLocation(SourceLocationRateLimiter),
}

impl Limiter {
    /// Evaluate whether an event should pass the rate limit.
    ///
    /// Returns `true` if the event should be allowed (not rate-limited),
    /// `false` if it should be suppressed.
    pub fn evaluate(&self, log_msg: &LogMessage) -> bool {
        let ts_sec = log_msg.get_timestamp_or_fallback();
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
        Limiter::SourceLocation(SourceLocationRateLimiter::new(threshold))
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
            self.last_timestamp.fetch_max(ts_sec, Ordering::SeqCst);
        }
        !rate_limited
    }
}

/// A rate limiter that tracks alerts per source location (file:line).
///
/// This allows different error locations to alert independently, preventing one noisy
/// error from suppressing alerts from completely different code paths. Each location
/// gets its own `MultiRateLimiter` with the full threshold.
pub struct SourceLocationRateLimiter {
    /// Maps (file, line) -> rate limiter for that location
    limiters: LazyMap<(Box<str>, Box<str>), MultiRateLimiter>,
    /// Threshold for creating new limiters (used for cleanup window calculation)
    threshold: RateThreshold,
    /// Maximum entries before triggering cleanup
    last_cleanup_size: Mutex<usize>,
}

impl SourceLocationRateLimiter {
    pub fn new(threshold: RateThreshold) -> Self {
        Self {
            limiters: LazyMap::with_capacity(16, move |_key| MultiRateLimiter::from(threshold)),
            threshold,
            last_cleanup_size: Mutex::new(8),
        }
    }

    /// Check if an error from this source location should trigger an alert.
    ///
    /// Returns true if the alert should fire (not rate-limited), false if suppressed.
    pub fn evaluate(&self, file: &str, line: &str, ts_sec: i64) -> bool {
        let key = (
            file.to_owned().into_boxed_str(),
            line.to_owned().into_boxed_str(),
        );

        let result = self.limiters.get(&key, |limiter| limiter.evaluate(ts_sec));

        // Opportunistically check for cleanup, but not if someone else is cleaning up
        if let Ok(mut lk) = self.last_cleanup_size.try_lock() {
            // If we're twice as large as the ending count from the last time we
            // cleaned up, then let's try to clean up again.
            if self.limiters.len() >= 2 * *lk {
                *lk = self.cleanup(ts_sec);
            }
        }

        result
    }

    /// Remove entries where all timestamps are older than the window
    fn cleanup(&self, now: i64) -> usize {
        let window = self.threshold.duration.as_secs() as i64;
        let cutoff = now - window;
        self.limiters.retain(|_, limiter| {
            // Keep if any timestamp is recent enough
            limiter.get_latest() > cutoff
        })
    }
}

/// Implements rate-limiting criteria such as 'at least n in the last w seconds'
///
/// Uses a ring buffer to track the N most recent timestamps.
pub struct MultiRateLimiter {
    inner: Mutex<MultiRateLimiterInner>,
    /// The length of the window (in seconds)
    window: i64,
    /// If true, returns true when rate >= threshold (burst detection).
    /// If false, returns true when rate < threshold (suppression).
    comparator_is_ge: bool,
    /// Cache of latest timestamp recorded
    latest: AtomicI64,
}

impl MultiRateLimiter {
    pub fn new(num: usize, window: Duration, comparator_is_ge: bool) -> Self {
        Self {
            inner: Mutex::new(MultiRateLimiterInner::new(num)),
            window: window.as_secs().try_into().unwrap(),
            comparator_is_ge,
            latest: AtomicI64::new(0),
        }
    }

    /// Get the latest timestamp recorded
    pub fn get_latest(&self) -> i64 {
        self.latest.load(Ordering::SeqCst)
    }

    /// Check if a particular new timestamp passes the limit. This also updates the internal state.
    pub fn evaluate(&self, new_timestamp: i64) -> bool {
        let (earliest, latest) = self.inner.lock().unwrap().insert_and_pop(new_timestamp);
        self.latest.fetch_max(latest, Ordering::SeqCst);

        let threshold_met = earliest + self.window >= latest;

        if self.comparator_is_ge {
            // Burst detection: alert when rate >= threshold
            threshold_met
        } else {
            // Suppression: alert when rate < threshold
            !threshold_met
        }
    }
}

struct MultiRateLimiterInner {
    /// Records the last n events as a ring buffer.
    /// Initialized to 0, representing "very distant past".
    timestamps: Box<[i64]>,
    /// Points to smallest entry.
    /// Invariant: timestamps[i] <= timestamp[i + 1  % n], with the exception
    ///            that timestamps[idx - 1] may be > timestamps[idx].
    idx: usize,
}

impl MultiRateLimiterInner {
    fn new(n: usize) -> Self {
        Self {
            timestamps: vec![0i64; n].into(),
            idx: 0,
        }
    }

    // Insert a new timestamp into the buffer. Update self.idx as needed.
    // Then return the evicted timestamp (earliest), and the latest timestamp in the set.
    //
    // Note that the evicted timestamp could be the `new_timestamp` value
    // if things are badly out of order.
    fn insert_and_pop(&mut self, new_timestamp: i64) -> (i64, i64) {
        let n = self.timestamps.len();

        let evicted = self.insert_helper(new_timestamp);

        let newest = self.timestamps[self.idx];
        self.idx += 1;
        if self.idx >= n {
            self.idx -= n;
        }

        (evicted, newest)
    }

    // Insert a new timestamp into the set using an insertion sort strategy that starts
    // at idx - 1. This terminates very quickly if new_timestamp is actually the latest
    // timestamp, which should be the typical case.
    //
    // Does NOT update self.idx. However self.idx should be unconditionally incremented
    // with wraparound after this.
    // Returns the previous oldest timestamp, or new_timestamp if that is older.
    fn insert_helper(&mut self, new_timestamp: i64) -> i64 {
        let prev_oldest = self.timestamps[self.idx];
        let n = self.timestamps.len();

        // Trying to insert first at idx and comparing with idx - 1 entry, walk towards 0.
        for j in (0..self.idx).rev() {
            if self.timestamps[j] <= new_timestamp {
                self.timestamps[j + 1] = new_timestamp;
                return prev_oldest;
            } else {
                self.timestamps[j + 1] = self.timestamps[j];
            }
        }
        // Ring buffer wraparound point: conceptually when j is n-1, and only if n-1 != 0.
        if n > 1 {
            if self.timestamps[n - 1] <= new_timestamp {
                self.timestamps[0] = new_timestamp;
                return prev_oldest;
            } else {
                self.timestamps[0] = self.timestamps[n - 1];
            }
        }
        // Now walk from n-2 towards self.idx
        for j in (self.idx + 1..n - 1).rev() {
            if self.timestamps[j] <= new_timestamp {
                self.timestamps[j + 1] = new_timestamp;
                return prev_oldest;
            } else {
                self.timestamps[j + 1] = self.timestamps[j];
            }
        }
        // We've tried every position except j = self.idx, so now we figure out
        // if this is the oldest or older than the previous oldest.
        // If n == 1, this is the only block that isn't vacuous
        let f = {
            let mut f = self.idx + 1;
            if f >= n {
                f -= n;
            }
            f
        };
        if prev_oldest < new_timestamp {
            self.timestamps[f] = new_timestamp;
            prev_oldest
        } else {
            // We've now shifted the whole buffer by one and new_timestamp
            // couldn't be inserted. This was slow but usually timestamps should
            // be increasing, so this shouldn't happen often.
            self.timestamps[f] = prev_oldest;
            new_timestamp
        }
    }

    #[cfg(test)]
    fn assert_invariant(&self) {
        let n = self.timestamps.len();
        if n < 2 {
            return;
        }

        for j in 0..n {
            let inc = {
                let mut inc = j + 1;
                if inc == n {
                    inc = 0;
                }
                inc
            };
            if inc != self.idx {
                assert!(
                    self.timestamps[j] <= self.timestamps[inc],
                    "assertion failed at j = {j}, inc = {inc}\nidx = {idx}\nself.timestamps = {ts:#?}",
                    ts = self.timestamps,
                    idx = self.idx
                );
            }
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
        // With evicted-vs-latest semantic: returns true when a previous event
        // was evicted and it's still within the window of the new event.
        let threshold = RateThreshold {
            times: 1,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let limiter = SourceLocationRateLimiter::new(threshold);

        // First alert from location A: no previous event to compare, returns false
        assert!(!limiter.evaluate("file_a.rs", "10", 1000));

        // Second alert: evicted=1000, newest=1100, 1000+600>=1100 → true
        assert!(limiter.evaluate("file_a.rs", "10", 1100));

        // First alert from different location: no previous event, returns false
        assert!(!limiter.evaluate("file_b.rs", "20", 1100));

        // Same location, still within window of previous
        assert!(limiter.evaluate("file_a.rs", "10", 1700));

        // Same location, outside window of previous (1700 + 600 < 2400)
        assert!(!limiter.evaluate("file_a.rs", "10", 2400));
    }

    #[test]
    fn source_location_rate_limiter_with_count() {
        // Threshold: >= 3 events per 600 seconds per location (burst detection)
        // With evicted-vs-latest: need 4 events total, where the 1st (evicted)
        // is still within window of the 4th (newest).
        let threshold = RateThreshold {
            times: 3,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let limiter = SourceLocationRateLimiter::new(threshold);

        // First three alerts: buffer filling, evicted=0, returns false
        assert!(!limiter.evaluate("file.rs", "10", 1000));
        assert!(!limiter.evaluate("file.rs", "10", 1100));
        assert!(!limiter.evaluate("file.rs", "10", 1200));

        // Fourth alert: evicted=1000, newest=1300, 1000+600>=1300 → true
        assert!(limiter.evaluate("file.rs", "10", 1300));

        // Fifth continues to trigger (evicted=1100, 1100+600>=1400 → true)
        assert!(limiter.evaluate("file.rs", "10", 1400));

        // Different location is independent - starts fresh
        assert!(!limiter.evaluate("file.rs", "20", 1000));
        assert!(!limiter.evaluate("file.rs", "20", 1100));
        assert!(!limiter.evaluate("file.rs", "20", 1200));
        assert!(limiter.evaluate("file.rs", "20", 1300));
    }

    #[test]
    fn source_location_rate_limiter_different_lines_same_file() {
        // Threshold: >= 2 events per 600 seconds per location (burst detection)
        // With evicted-vs-latest: need 3 events, where 1st (evicted) is within window of 3rd
        let threshold = RateThreshold {
            times: 2,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let limiter = SourceLocationRateLimiter::new(threshold);

        // First two events at each location: buffer filling, evicted=0
        assert!(!limiter.evaluate("file.rs", "10", 1000));
        assert!(!limiter.evaluate("file.rs", "20", 1000));
        assert!(!limiter.evaluate("file.rs", "30", 1000));

        assert!(!limiter.evaluate("file.rs", "10", 1100));
        assert!(!limiter.evaluate("file.rs", "20", 1100));
        assert!(!limiter.evaluate("file.rs", "30", 1100));

        // Third event at each location triggers (evicted within window of newest)
        assert!(limiter.evaluate("file.rs", "10", 1200));
        assert!(limiter.evaluate("file.rs", "20", 1200));
        assert!(limiter.evaluate("file.rs", "30", 1200));
    }

    #[test]
    fn source_location_rate_limiter_suppression() {
        // Threshold: < 3 events per 600 seconds per location (suppression)
        // With evicted-vs-latest: returns true when evicted + window < latest
        // (i.e., the evicted event is too old, meaning events are spread out)
        let threshold = RateThreshold {
            times: 3,
            duration: Duration::from_secs(600),
            comparator_is_ge: false,
        };
        let limiter = SourceLocationRateLimiter::new(threshold);

        // First three alerts: evicted=0, threshold_met=false, returns true
        assert!(limiter.evaluate("file.rs", "10", 1000));
        assert!(limiter.evaluate("file.rs", "10", 1100));
        assert!(limiter.evaluate("file.rs", "10", 1200));

        // Fourth: evicted=1000, 1000+600>=1300 → true, returns false (suppressed)
        assert!(!limiter.evaluate("file.rs", "10", 1300));

        // Fifth also suppressed (evicted=1100, 1100+600>=1400 → true)
        assert!(!limiter.evaluate("file.rs", "10", 1400));

        // After gap: evicted=1200, 1200+600=1800 < 2000 → false, returns true
        assert!(limiter.evaluate("file.rs", "10", 2000));
    }

    #[test]
    fn multi_rate_limiter_inner_mostly_monotonic() {
        let mut inner = MultiRateLimiterInner::new(4);
        inner.assert_invariant();

        // Mostly increasing with a few out-of-order
        let (evicted, newest) = inner.insert_and_pop(100);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 100);

        let (evicted, newest) = inner.insert_and_pop(200);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 200);

        let (evicted, newest) = inner.insert_and_pop(150); // Out of order!
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 200); // 200 is still newest

        let (evicted, newest) = inner.insert_and_pop(300);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 300);

        // Buffer is now full: [100, 150, 200, 300]
        // Next insert evicts the oldest (100)
        let (evicted, newest) = inner.insert_and_pop(400);
        inner.assert_invariant();
        assert_eq!(evicted, 100);
        assert_eq!(newest, 400);

        // Insert slightly out of order
        let (evicted, newest) = inner.insert_and_pop(350); // Between 300 and 400
        inner.assert_invariant();
        assert_eq!(evicted, 150);
        assert_eq!(newest, 400); // 400 still newest

        let (evicted, newest) = inner.insert_and_pop(500);
        inner.assert_invariant();
        assert_eq!(evicted, 200);
        assert_eq!(newest, 500);

        let (evicted, newest) = inner.insert_and_pop(450); // Out of order
        inner.assert_invariant();
        assert_eq!(evicted, 300);
        assert_eq!(newest, 500);
    }

    #[test]
    fn multi_rate_limiter_inner_non_monotonic() {
        let mut inner = MultiRateLimiterInner::new(3);
        inner.assert_invariant();

        // Chaotic sequence: 500, 100, 300, 200, 400, 150, 600, 50
        let (evicted, newest) = inner.insert_and_pop(500);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 500);

        let (evicted, newest) = inner.insert_and_pop(100); // Much smaller
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 500);

        let (evicted, newest) = inner.insert_and_pop(300); // In between
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 500);

        // Buffer full: [100, 300, 500]
        let (evicted, newest) = inner.insert_and_pop(200); // Smaller than all but 100
        inner.assert_invariant();
        assert_eq!(evicted, 100);
        assert_eq!(newest, 500);

        // Buffer: [200, 300, 500]
        let (evicted, newest) = inner.insert_and_pop(400); // In between
        inner.assert_invariant();
        assert_eq!(evicted, 200);
        assert_eq!(newest, 500);

        // Buffer: [300, 400, 500]
        let (evicted, newest) = inner.insert_and_pop(150); // Smaller than all - gets evicted!
        inner.assert_invariant();
        assert_eq!(evicted, 150); // The new value itself is evicted
        assert_eq!(newest, 500);

        // Buffer unchanged: [300, 400, 500]
        let (evicted, newest) = inner.insert_and_pop(600); // New largest
        inner.assert_invariant();
        assert_eq!(evicted, 300);
        assert_eq!(newest, 600);

        // Buffer: [400, 500, 600]
        let (evicted, newest) = inner.insert_and_pop(50); // Way too old - gets evicted
        inner.assert_invariant();
        assert_eq!(evicted, 50);
        assert_eq!(newest, 600);
    }

    #[test]
    fn multi_rate_limiter_inner_size_one() {
        let mut inner = MultiRateLimiterInner::new(1);
        inner.assert_invariant();

        let (evicted, newest) = inner.insert_and_pop(100);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 100);

        let (evicted, newest) = inner.insert_and_pop(200);
        inner.assert_invariant();
        assert_eq!(evicted, 100);
        assert_eq!(newest, 200);

        let (evicted, newest) = inner.insert_and_pop(150); // Out of order - still evicts previous
        inner.assert_invariant();
        assert_eq!(evicted, 150); // 150 < 200, so 150 gets evicted immediately
        assert_eq!(newest, 200);

        let (evicted, newest) = inner.insert_and_pop(300);
        inner.assert_invariant();
        assert_eq!(evicted, 200);
        assert_eq!(newest, 300);
    }

    #[test]
    fn multi_rate_limiter_inner_size_two() {
        let mut inner = MultiRateLimiterInner::new(2);
        inner.assert_invariant();

        let (evicted, newest) = inner.insert_and_pop(100);
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 100);

        let (evicted, newest) = inner.insert_and_pop(50); // Smaller - but buffer not full
        inner.assert_invariant();
        assert_eq!(evicted, 0);
        assert_eq!(newest, 100); // 100 still newest

        // Buffer: [50, 100]
        let (evicted, newest) = inner.insert_and_pop(200);
        inner.assert_invariant();
        assert_eq!(evicted, 50);
        assert_eq!(newest, 200);

        // Buffer: [100, 200]
        let (evicted, newest) = inner.insert_and_pop(150); // In between
        inner.assert_invariant();
        assert_eq!(evicted, 100);
        assert_eq!(newest, 200);

        // Buffer: [150, 200]
        let (evicted, newest) = inner.insert_and_pop(75); // Too old
        inner.assert_invariant();
        assert_eq!(evicted, 75);
        assert_eq!(newest, 200);
    }

    #[test]
    fn source_location_rate_limiter_cleanup() {
        // Cleanup triggers when len >= 2 * last_cleanup_size (starts at 8, so triggers at 16)
        let threshold = RateThreshold {
            times: 1,
            duration: Duration::from_secs(600),
            comparator_is_ge: true,
        };
        let limiter = SourceLocationRateLimiter::new(threshold);

        // Add 15 stale entries at timestamp 1000
        for i in 0..15 {
            limiter.evaluate(&format!("file{}.rs", i), &i.to_string(), 1000);
        }
        assert_eq!(limiter.limiters.len(), 15);

        // Add one more fresh entry at timestamp 2000 - this triggers cleanup (16 >= 2*8)
        // cutoff = 2000 - 600 = 1400, so entries with latest <= 1400 are removed
        limiter.evaluate("fresh.rs", "1", 2000);

        // Only the fresh entry should remain
        assert_eq!(limiter.limiters.len(), 1);
    }
}
