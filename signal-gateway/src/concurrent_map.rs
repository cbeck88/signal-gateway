//! A concurrent map with read-preferring access pattern.
//!
//! This module provides a concurrent hash map that optimizes for the common case
//! where values already exist, using a read lock first before falling back to a
//! write lock for insertions.

use std::collections::HashMap;
use std::hash::Hash;
use tokio::sync::RwLock;

/// A concurrent hash map that uses read-preferring locking.
///
/// When accessing a value, it first tries to acquire a read lock. If the key
/// exists, it uses the value immediately. If the key doesn't exist, it upgrades
/// to a write lock and inserts a new value.
#[derive(Debug)]
pub struct ConcurrentMap<K, V> {
    inner: RwLock<HashMap<K, V>>,
}

impl<K, V> ConcurrentMap<K, V>
where
    K: Eq + Hash + Clone,
{
    /// Create a new empty concurrent map.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Get or insert a value, then access it.
    ///
    /// This method uses a read-preferring pattern:
    /// 1. First acquires a read lock and looks for the key
    /// 2. If found, calls `access` with a reference to the value
    /// 3. If not found, acquires a write lock, inserts using `create`, then calls `access`
    ///
    /// The lock is held while `access` runs, so `access` can safely use the reference.
    /// For async operations on the value, consider having `access` return a future
    /// that owns any data it needs.
    pub async fn get_or_insert_with<R, F, A>(&self, key: K, create: F, access: A) -> R
    where
        F: FnOnce() -> V,
        A: FnOnce(&V) -> R,
    {
        // Try to get existing value with read lock first
        {
            let guard = self.inner.read().await;
            if let Some(value) = guard.get(&key) {
                return access(value);
            }
        }

        // Value doesn't exist, need to create with write lock
        let mut guard = self.inner.write().await;
        // Use entry API - handles the race where another task inserted while we waited
        let value = guard.entry(key).or_insert_with(create);
        access(value)
    }

    /// Access all entries in the map with a read lock.
    ///
    /// Acquires a read lock and calls `access` with a reference to the underlying HashMap.
    /// The lock is held while `access` runs.
    pub async fn read_all<R, A>(&self, access: A) -> R
    where
        A: FnOnce(&HashMap<K, V>) -> R,
    {
        let guard = self.inner.read().await;
        access(&guard)
    }
}

impl<K, V> Default for ConcurrentMap<K, V>
where
    K: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get_or_insert_new_key() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        let result = map
            .get_or_insert_with("key1".to_string(), || 42, |v| *v)
            .await;

        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_get_or_insert_existing_key() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        // Insert first time
        map.get_or_insert_with("key1".to_string(), || 42, |_| ())
            .await;

        // Access again - should get existing value, not call create
        let mut create_called = false;
        let result = map
            .get_or_insert_with(
                "key1".to_string(),
                || {
                    create_called = true;
                    100
                },
                |v| *v,
            )
            .await;

        assert_eq!(result, 42);
        assert!(!create_called);
    }

    #[tokio::test]
    async fn test_get_or_insert_multiple_keys() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        map.get_or_insert_with("a".to_string(), || 1, |_| ()).await;
        map.get_or_insert_with("b".to_string(), || 2, |_| ()).await;
        map.get_or_insert_with("c".to_string(), || 3, |_| ()).await;

        let a = map
            .get_or_insert_with("a".to_string(), || 0, |v| *v)
            .await;
        let b = map
            .get_or_insert_with("b".to_string(), || 0, |v| *v)
            .await;
        let c = map
            .get_or_insert_with("c".to_string(), || 0, |v| *v)
            .await;

        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }
}
