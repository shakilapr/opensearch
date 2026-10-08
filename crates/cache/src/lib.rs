//! Caches (plan Stage 8): TTL + LRU with generation-based invalidation.
//! Interfaces frozen in `docs/STAGE8_SPEC.md`.
//!
//! [`Cache`] stores `V: Clone` values keyed by `String`. Coordination state lives behind a
//! [`Mutex`] so a `&self` cache is `Send + Sync` and shareable across threads. Two expiry
//! mechanisms run side by side:
//!
//! * **TTL** — each entry carries an absolute `expires_at` computed from the caller-supplied
//!   `now_ms`; reads at or past that instant miss and evict. `ttl_ms == 0` therefore means
//!   "never cached".
//! * **Generation** — the cache holds a monotonic generation; entries are stamped with the
//!   current one at write. [`Cache::invalidate_generation`] advances it and drops every entry
//!   stamped older, so newly written entries survive while stale ones disappear observably.
//!
//! Eviction at `max_entries` uses least-recently-used order (recency = monotonic access tick,
//! bumped on every `put`/`get` hit).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Monotonic generation counter used to bulk-invalidate stamped entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(pub u64);

/// Sizing/expiry policy for a [`Cache`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// Time-to-live in milliseconds; an entry is invalid once `now_ms >= write_ms + ttl_ms`.
    pub ttl_ms: u64,
    /// Maximum live entries; the least-recently-used entry is evicted past this.
    pub max_entries: usize,
}

/// One cached value plus its bookkeeping.
struct Entry<V> {
    value: V,
    expires_at: u64,
    generation: u64,
    last_access: u64,
}

struct Inner<V> {
    map: HashMap<String, Entry<V>>,
    /// Monotonic access clock; the `last_access` of the most recent touch is the maximum.
    clock: u64,
}

/// TTL + LRU cache with generation invalidation. Cheap to share by reference (`&Cache`) or via
/// `Arc<Cache>`; all mutation is interior behind a [`Mutex`].
pub struct Cache<V> {
    config: CacheConfig,
    generation: AtomicU64,
    inner: Mutex<Inner<V>>,
}

impl<V: Clone> Cache<V> {
    /// Build an empty cache under `config`. Starts at [`Generation`] `0`.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            config,
            generation: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                clock: 0,
            }),
        }
    }

    /// Fetch a live value. Returns `None` when the key is absent or expired (at the moment of
    /// the call an expired entry is removed). A hit refreshes the entry's LRU recency.
    pub fn get(&self, key: &str, now_ms: u64) -> Option<V> {
        let mut inner = self.lock();

        let expired = match inner.map.get(key) {
            Some(entry) => now_ms >= entry.expires_at,
            None => return None,
        };
        if expired {
            inner.map.remove(key);
            return None;
        }

        inner.clock += 1;
        let clock = inner.clock;
        let entry = inner.map.get_mut(key).expect("checked above");
        entry.last_access = clock;
        Some(entry.value.clone())
    }

    /// Insert or overwrite `key`, stamping it with the current generation and
    /// `now_ms + ttl_ms`, then evict LRU entries while over `max_entries`.
    pub fn put(&self, key: impl Into<String>, value: V, now_ms: u64) {
        let key = key.into();
        let generation = self.generation.load(Ordering::SeqCst);

        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;
        inner.map.insert(
            key,
            Entry {
                value,
                expires_at: now_ms.saturating_add(self.config.ttl_ms),
                generation,
                last_access: clock,
            },
        );

        while inner.map.len() > self.config.max_entries {
            let victim = inner
                .map
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    inner.map.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Advance the cache generation to at least `gen` and drop every entry stamped strictly
    /// older than `gen.0`. Entries written after the advance are stamped `gen` and survive
    /// subsequent invalidations at the same generation.
    pub fn invalidate_generation(&self, gen: Generation) {
        let target = gen.0;
        // Advance monotonically (never regress the generation).
        let mut current = self.generation.load(Ordering::SeqCst);
        while target > current {
            match self.generation.compare_exchange(
                current,
                target,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }

        let mut inner = self.lock();
        inner.map.retain(|_, entry| entry.generation >= target);
    }

    /// Number of stored entries (expired-but-untouched entries are still counted until read).
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, Inner<V>> {
        // Recover from a poisoned lock: cache state stays usable, no panic propagates.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn cfg(ttl_ms: u64, max_entries: usize) -> CacheConfig {
        CacheConfig { ttl_ms, max_entries }
    }

    #[test]
    fn hit_miss_and_overwrite() {
        let cache: Cache<u32> = Cache::new(cfg(1_000, 8));
        assert_eq!(cache.get("missing", 0), None);
        assert!(cache.is_empty());

        cache.put("a", 1, 0);
        assert_eq!(cache.get("a", 0), Some(1));
        assert_eq!(cache.len(), 1);

        cache.put("a", 2, 10);
        assert_eq!(cache.get("a", 10), Some(2));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn ttl_expiry_is_observable_with_explicit_now() {
        let cache: Cache<&str> = Cache::new(cfg(100, 8));
        cache.put("k", "v", 0);

        // Just before the deadline it is live.
        assert_eq!(cache.get("k", 99), Some("v"));
        // At the deadline it is expired and evicted...
        assert_eq!(cache.get("k", 100), None);
        assert!(cache.is_empty());
        // ...and stays gone afterwards.
        assert_eq!(cache.get("k", 101), None);
    }

    #[test]
    fn ttl_zero_means_never_cached() {
        let cache: Cache<u8> = Cache::new(cfg(0, 8));
        cache.put("k", 1, 0);
        assert_eq!(cache.get("k", 0), None);
    }

    #[test]
    fn lru_eviction_order() {
        let cache: Cache<u32> = Cache::new(cfg(1_000, 2));
        cache.put("a", 1, 0);
        cache.put("b", 2, 0);

        // Touch "a" so "b" becomes least-recently-used.
        assert_eq!(cache.get("a", 0), Some(1));

        // Inserting "c" exceeds capacity → evict "b", keep "a" and "c".
        cache.put("c", 3, 0);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a", 0), Some(1));
        assert_eq!(cache.get("c", 0), Some(3));
        assert_eq!(cache.get("b", 0), None);

        // A pure put-only sequence evicts in insertion order.
        let cache2: Cache<u32> = Cache::new(cfg(1_000, 2));
        cache2.put("x", 1, 0);
        cache2.put("y", 2, 0);
        cache2.put("z", 3, 0);
        assert_eq!(cache2.get("x", 0), None);
        assert_eq!(cache2.get("y", 0), Some(2));
        assert_eq!(cache2.get("z", 0), Some(3));
    }

    #[test]
    fn generation_invalidation_drops_old_and_keeps_new() {
        let cache: Cache<u32> = Cache::new(cfg(10_000, 16));

        cache.put("old_a", 1, 0);
        cache.put("old_b", 2, 0);
        assert_eq!(cache.len(), 2);

        // Advance generation → entries stamped 0 (older than 1) drop.
        cache.invalidate_generation(Generation(1));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.get("old_a", 0), None);
        assert_eq!(cache.get("old_b", 0), None);

        // New writes are stamped generation 1 and survive invalidations at the same generation.
        cache.put("new_a", 3, 0);
        cache.put("new_b", 4, 0);
        cache.invalidate_generation(Generation(1));
        assert_eq!(cache.get("new_a", 0), Some(3));
        assert_eq!(cache.get("new_b", 0), Some(4));
        assert_eq!(cache.len(), 2);

        // Advancing again drops them.
        cache.invalidate_generation(Generation(2));
        assert_eq!(cache.len(), 0);

        // The generation never regresses: an older request is a no-op.
        cache.put("gen2", 5, 0);
        cache.invalidate_generation(Generation(1));
        assert_eq!(cache.get("gen2", 0), Some(5));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn concurrent_put_and_get_do_not_panic() {
        let cache: Arc<Cache<u64>> = Arc::new(Cache::new(cfg(10_000, 64)));
        let mut handles = Vec::new();

        for t in 0..8u64 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..500u64 {
                    let key = format!("k{}", (t + i) % 32);
                    cache.put(key.clone(), t * 1_000 + i, i);
                    let _ = cache.get(&key, i);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert!(cache.len() <= 64);
        // Cache remains usable after the concurrent storm.
        cache.put("final", 7, 0);
        assert_eq!(cache.get("final", 0), Some(7));
    }
}
