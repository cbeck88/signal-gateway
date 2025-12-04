use serde::Deserialize;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

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
    pub times: usize,
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
/// error from suppressing alerts from completely different code paths.
#[derive(Debug)]
pub struct SourceLocationRateLimiter {
    /// Maps (file, line) -> last alert timestamp
    last_timestamps: HashMap<(String, String), i64>,
    /// The rate limiting window in seconds
    window: i64,
    /// Maximum entries before triggering cleanup
    max_entries: usize,
}

impl SourceLocationRateLimiter {
    pub fn new(window: Duration, max_entries: usize) -> Self {
        Self {
            last_timestamps: HashMap::new(),
            window: window.as_secs().try_into().unwrap(),
            max_entries,
        }
    }

    /// Check if an error from this source location should trigger an alert.
    ///
    /// Returns true if the alert should fire (not rate-limited), false if suppressed.
    /// Updates the stored timestamp if the alert fires.
    pub fn evaluate(&mut self, file: &str, line: &str, ts_sec: i64) -> bool {
        let key = (file.to_owned(), line.to_owned());

        if let Some(&last_ts) = self.last_timestamps.get(&key)
            && ts_sec - last_ts < self.window
        {
            return false; // Rate limited
        }

        // Alert should fire - update timestamp
        self.last_timestamps.insert(key, ts_sec);

        // Clean up if we've exceeded max entries
        if self.last_timestamps.len() > self.max_entries {
            self.cleanup(ts_sec);
        }

        true
    }

    /// Remove entries older than the window
    fn cleanup(&mut self, now: i64) {
        self.last_timestamps
            .retain(|_, &mut ts| now - ts < self.window);
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
}

impl MultiRateLimiter {
    pub fn new(num: usize, window: Duration) -> Self {
        Self {
            idx: 0,
            timestamps: vec![Default::default(); num],
            window: window.as_secs().try_into().unwrap(),
        }
    }

    /// Check if a particular new timestamp passes the limit. This also updates the last-known timestamp.
    ///
    /// Note: Assumes that new_timestamp is monotonically increasing, otherwise it might not work right.
    pub fn evaluate(&mut self, new_timestamp: i64) -> bool {
        let oldest = self.timestamps[self.idx];
        if oldest >= new_timestamp {
            return false;
        }
        self.timestamps[self.idx] = new_timestamp;
        self.idx += 1;
        self.idx %= self.timestamps.len();
        let next_oldest = self.timestamps[self.idx];
        // If the next oldest is within 'window' of the new timestamp,
        // then all of the most recent n are. Otherwise, at most n-1 of the most recent are.
        next_oldest + self.window >= new_timestamp
    }
}

impl From<RateThreshold> for MultiRateLimiter {
    fn from(src: RateThreshold) -> Self {
        Self::new(src.times, src.duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rate_threshold() {
        let threshold = RateThreshold::from_str("1/10s").unwrap();
        assert_eq!(threshold.times, 1);
        assert_eq!(threshold.duration, Duration::from_secs(10));
        let threshold = RateThreshold::from_str("1 / 10s").unwrap();
        assert_eq!(threshold.times, 1);
        assert_eq!(threshold.duration, Duration::from_secs(10));

        let threshold = RateThreshold::from_str("2 / 5m").unwrap();
        assert_eq!(threshold.times, 2);
        assert_eq!(threshold.duration, Duration::from_secs(300));

        let threshold = RateThreshold::from_str("> 3 / 10m").unwrap();
        assert_eq!(threshold.times, 4);
        assert_eq!(threshold.duration, Duration::from_secs(600));

        let threshold = RateThreshold::from_str(">=3/10m").unwrap();
        assert_eq!(threshold.times, 3);
        assert_eq!(threshold.duration, Duration::from_secs(600));
    }

    #[test]
    fn source_location_rate_limiter_basic() {
        let mut limiter = SourceLocationRateLimiter::new(Duration::from_secs(600), 100);

        // First alert from location A should pass
        assert!(limiter.evaluate("file_a.rs", "10", 1000));

        // Second alert from same location within window should be rate limited
        assert!(!limiter.evaluate("file_a.rs", "10", 1100));

        // Alert from different location should pass (independent rate limiting)
        assert!(limiter.evaluate("file_b.rs", "20", 1100));

        // Same location after window passes should alert again
        assert!(limiter.evaluate("file_a.rs", "10", 1700)); // 1000 + 600 + 100
    }

    #[test]
    fn source_location_rate_limiter_different_lines_same_file() {
        let mut limiter = SourceLocationRateLimiter::new(Duration::from_secs(600), 100);

        // Different lines in same file should be independent
        assert!(limiter.evaluate("file.rs", "10", 1000));
        assert!(limiter.evaluate("file.rs", "20", 1000));
        assert!(limiter.evaluate("file.rs", "30", 1000));

        // Each should still be rate limited individually
        assert!(!limiter.evaluate("file.rs", "10", 1100));
        assert!(!limiter.evaluate("file.rs", "20", 1100));
    }

    #[test]
    fn source_location_rate_limiter_cleanup() {
        // Use small max_entries to trigger cleanup
        let mut limiter = SourceLocationRateLimiter::new(Duration::from_secs(600), 3);

        // Fill up the limiter
        assert!(limiter.evaluate("file1.rs", "1", 1000));
        assert!(limiter.evaluate("file2.rs", "2", 1000));
        assert!(limiter.evaluate("file3.rs", "3", 1000));
        assert_eq!(limiter.last_timestamps.len(), 3);

        // Add one more, triggering cleanup - but all are fresh so none removed
        assert!(limiter.evaluate("file4.rs", "4", 1000));
        // Still have 4 after cleanup since none are old enough
        assert_eq!(limiter.last_timestamps.len(), 4);

        // Now add with a timestamp far in the future - old entries should be cleaned
        assert!(limiter.evaluate("file5.rs", "5", 2000));
        // Should have cleaned up entries from timestamp 1000 (older than 600 sec window)
        assert_eq!(limiter.last_timestamps.len(), 1);
    }
}
