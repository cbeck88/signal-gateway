//! A concurrent map with read-preferring access pattern.
//!
//! This module provides a simple concurrent hash map with limited API,
//! which fills in requested values that don't exist using a default function,
//! taking a write lock only when necessary to do so.
//!
//! This is used to categorize logs by their source, or implement special
//! rate limiters that depend on the source of the log.
//!
//! The idea is that in these use-cases, there is a relatively small set of
//! sources, and insertions occur only a few times at the beginning of the process.
//! Then almost all accesses are to existing elements, and from that point on,
//! only read locks are taken when using this API.
//!
//! The API also allows to call "retain" if the map gets too big and it needs
//! to be pruned in some manner.
//!
//! This is used instead of dash_map and once_map to avoid unnecessary complexity
//! and dependencies, and give exactly the API needed in our application.

use std::{
    borrow::Borrow,
    collections::HashMap,
    hash::Hash,
    sync::{
        RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

/// A concurrent hash map that uses read-preferring locking.
///
/// When accessing a value, it first tries to acquire a read lock. If the key
/// exists, it uses the value immediately. If the key doesn't exist, it upgrades
/// to a write lock and inserts a new value, and then accesses it.
#[derive(Debug)]
pub struct ConcurrentMap<K, V> {
    inner: RwLock<HashMap<K, V>>,
    len_cache: AtomicUsize,
}

impl<K, V> ConcurrentMap<K, V>
where
    K: Eq + Hash + Clone,
{
    /// Make a new empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize a map with a given capacity
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: RwLock::new(HashMap::with_capacity(cap)),
            len_cache: Default::default(),
        }
    }

    /// Find and access a value in the map. If it doesn't exist, create it using the create function.
    ///
    /// Note: May deadlock if access or create calls `get_or_insert_with` recursively in the same hashmap.
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
        let value = guard.entry(key.clone()).or_insert_with(|| {
            self.len_cache.fetch_add(1, Ordering::SeqCst);
            (create)()
        });
        access(value)
    }

    /// Access all entries in the map with a read lock.
    pub fn with_read_lock<R, A>(&self, access: A) -> R
    where
        A: FnOnce(&HashMap<K, V>) -> R,
    {
        let guard = self.inner.read().unwrap();
        access(&guard)
    }

    /// Retain only entries that satisfy the predicate.
    pub fn retain<F>(&self, f: F) -> usize
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        let mut guard = self.inner.write().unwrap();
        guard.retain(f);
        let len = guard.len();
        self.len_cache.store(len, Ordering::SeqCst);
        len
    }

    /// Get the number of entries in the map.
    pub fn len(&self) -> usize {
        self.len_cache.load(Ordering::SeqCst)
    }
}

impl<K, V> Default for ConcurrentMap<K, V>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self {
            inner: RwLock::new(Default::default()),
            len_cache: Default::default(),
        }
    }
}

/// A concurrent hash map with a default factory for creating new values.
///
/// Wraps a [`ConcurrentMap`] and stores a factory closure, so callers don't need
/// to pass the creation function on every access.
pub struct LazyMap<K, V> {
    inner: ConcurrentMap<K, V>,
    factory: Box<dyn Fn(&K) -> V + Send + Sync>,
}

impl<K, V> LazyMap<K, V>
where
    K: Eq + Hash + Clone,
{
    /// Create a new lazy map with the given factory for creating initial values.
    pub fn new(factory: impl Fn(&K) -> V + Send + Sync + 'static) -> Self {
        Self {
            inner: ConcurrentMap::new(),
            factory: Box::new(factory),
        }
    }

    /// Create a new lazy map with given capacity, and factory
    pub fn with_capacity(cap: usize, factory: impl Fn(&K) -> V + Send + Sync + 'static) -> Self {
        Self {
            inner: ConcurrentMap::with_capacity(cap),
            factory: Box::new(factory),
        }
    }

    /// Get a value, creating it with the factory if it doesn't exist.
    pub fn get<Q, R, A>(&self, key: Q, access: A) -> R
    where
        Q: Borrow<K>,
        A: FnOnce(&V) -> R,
    {
        let key = key.borrow();
        self.inner
            .get_or_insert_with(key, || (self.factory)(key), access)
    }

    /// Access all entries in the map with a read lock.
    pub fn with_read_lock<R, A>(&self, access: A) -> R
    where
        A: FnOnce(&HashMap<K, V>) -> R,
    {
        self.inner.with_read_lock(access)
    }

    /// Retain only entries that satisfy the predicate.
    pub fn retain<F>(&self, f: F) -> usize
    where
        F: FnMut(&K, &mut V) -> bool,
    {
        self.inner.retain(f)
    }

    /// Returns the number of entries in the map.
    pub fn len(&self) -> usize {
        self.inner.len()
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
