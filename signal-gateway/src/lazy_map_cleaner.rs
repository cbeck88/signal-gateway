use crate::concurrent_map::LazyMap;
use std::sync::Mutex;

/// Trait for types that expose a timestamp in seconds.
/// For containers, this should be the oldest timestamp in the container.
pub trait TsSecs {
    fn ts_secs(&self) -> i64;
}

#[derive(Debug, Default)]
struct CleanerState {
    last_cleanup_size: usize,
    last_checkup: i64,
}

/// A policy object that can cleanup entries from a LazyMap, if the value type has timestamps.
/// The lazy map is cleaned whenever it doubles in size, or whenever enough time has passed
/// since the last cleanup. This can prevent lazy maps from growing without bound.
pub struct LazyMapCleaner {
    max_age: i64,
    min_checkup_period: i64,
    state: Mutex<CleanerState>,
}

impl LazyMapCleaner {
    /// Create a new lazy map cleaner, with given max age of items (in seconds)
    pub fn new(max_age: i64) -> Self {
        Self {
            max_age,
            min_checkup_period: 3600,
            state: Default::default(),
        }
    }

    /// Set the min checkup period (in seconds)
    /// Defaults to 1 hour if unset
    #[allow(dead_code)]
    pub fn with_min_checkup_period(self, min_checkup_period: i64) -> Self {
        Self {
            max_age: self.max_age,
            min_checkup_period,
            state: self.state,
        }
    }

    /// Maybe clean a lazy map, if we have met conditions to do so
    pub fn maybe_clean<K, V>(&self, now: i64, map: &LazyMap<K, V>)
    where
        K: Eq + std::hash::Hash + Clone,
        V: TsSecs,
    {
        // Only lock if we can do so without contention, if someone else is checking to clean,
        // we don't have to do so ourselves.
        if let Ok(mut lk) = self.state.try_lock() {
            // If the last cleanup size is 0, we'll still treat it as 1 and only look to clean if we reach 2.
            if map.len() >= lk.last_cleanup_size.max(1) * 2
                || lk.last_checkup + self.min_checkup_period < now
            {
                let cutoff = now - self.max_age;
                let new_cleanup_size = map.retain(|_key, val| val.ts_secs() >= cutoff);
                lk.last_cleanup_size = new_cleanup_size;
                lk.last_checkup = now;
            }
        }
    }
}
