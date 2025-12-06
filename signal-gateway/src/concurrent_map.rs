//! A concurrent map with read-preferring access pattern.
//!
//! This module provides a concurrent hash map that optimizes for the common case
//! where values already exist, using a read lock first before falling back to a
//! write lock for insertions.

use std::{borrow::Borrow, collections::HashMap, hash::Hash, sync::RwLock};

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
    /// The key is only cloned when a new value needs to be inserted.
    ///
    /// The lock is held while `access` runs.
    pub fn get_or_insert_with<Q, R, F, A>(&self, key: Q, create: F, access: A) -> R
    where
        Q: Borrow<K>,
        F: FnOnce() -> V,
        A: FnOnce(&V) -> R,
    {
        let key = key.borrow();

        // Try to get existing value with read lock first
        {
            let guard = self.inner.read().unwrap();
            if let Some(value) = guard.get(key) {
                return access(value);
            }
        }

        // Value doesn't exist, need to create with write lock
        let mut guard = self.inner.write().unwrap();
        // Use entry API - handles the race where another task inserted while we waited
        let value = guard.entry(key.clone()).or_insert_with(create);
        access(value)
    }

    /// Access all entries in the map with a read lock.
    ///
    /// Acquires a read lock and calls `access` with a reference to the underlying HashMap.
    /// The lock is held while `access` runs.
    pub fn with_read_lock<R, A>(&self, access: A) -> R
    where
        A: FnOnce(&HashMap<K, V>) -> R,
    {
        let guard = self.inner.read().unwrap();
        access(&guard)
    }

    /// Retain only entries that satisfy the predicate.
    ///
    /// Acquires a write lock and calls `retain` on the underlying HashMap.
    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        let mut guard = self.inner.write().unwrap();
        guard.retain(f);
    }

    /// Returns the number of entries in the map.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
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

/// A concurrent hash map with a default factory for creating new values.
///
/// Wraps a [`ConcurrentMap`] and stores a factory closure, so callers don't need
/// to pass the creation function on every access.
pub struct LazyMap<K, V> {
    inner: ConcurrentMap<K, V>,
    factory: Box<dyn Fn() -> V + Send + Sync>,
}

impl<K, V> LazyMap<K, V>
where
    K: Eq + Hash + Clone,
{
    /// Create a new lazy map with the given factory for creating values.
    pub fn new(factory: impl Fn() -> V + Send + Sync + 'static) -> Self {
        Self {
            inner: ConcurrentMap::new(),
            factory: Box::new(factory),
        }
    }

    /// Get a value, creating it with the factory if it doesn't exist.
    ///
    /// Uses the factory provided at construction time to create new values.
    /// The key is only cloned when a new value needs to be inserted.
    pub fn get<Q, R, A>(&self, key: Q, access: A) -> R
    where
        Q: Borrow<K>,
        A: FnOnce(&V) -> R,
    {
        self.inner.get_or_insert_with(key, &self.factory, access)
    }

    /// Access all entries in the map with a read lock.
    pub fn with_read_lock<R, A>(&self, access: A) -> R
    where
        A: FnOnce(&HashMap<K, V>) -> R,
    {
        self.inner.with_read_lock(access)
    }

    /// Retain only entries that satisfy the predicate.
    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        self.inner.retain(f);
    }

    /// Returns the number of entries in the map.
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> std::fmt::Debug for LazyMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyMap").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_or_insert_new_key() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        let result = map.get_or_insert_with("key1".to_string(), || 42, |v| *v);

        assert_eq!(result, 42);
    }

    #[test]
    fn test_get_or_insert_existing_key() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        // Insert first time
        map.get_or_insert_with("key1".to_string(), || 42, |_| ());

        // Access again - should get existing value, not call create
        let mut create_called = false;
        let result = map.get_or_insert_with(
            "key1".to_string(),
            || {
                create_called = true;
                100
            },
            |v| *v,
        );

        assert_eq!(result, 42);
        assert!(!create_called);
    }

    #[test]
    fn test_get_or_insert_multiple_keys() {
        let map: ConcurrentMap<String, i32> = ConcurrentMap::new();

        map.get_or_insert_with("a".to_string(), || 1, |_| ());
        map.get_or_insert_with("b".to_string(), || 2, |_| ());
        map.get_or_insert_with("c".to_string(), || 3, |_| ());

        let a = map.get_or_insert_with("a".to_string(), || 0, |v| *v);
        let b = map.get_or_insert_with("b".to_string(), || 0, |v| *v);
        let c = map.get_or_insert_with("c".to_string(), || 0, |v| *v);

        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }
}
