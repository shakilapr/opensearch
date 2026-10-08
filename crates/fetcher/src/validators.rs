//! Conditional-refresh validators (spec §8; crawler ARCHITECTURE.md §18).
//! OWNER: fetcher agent.
//!
//! A [`Validators`] value is the set of cache validators we know for a resource
//! (HTTP `ETag` and/or `Last-Modified`), together with the time we last fetched
//! or revalidated it. They are stored per **canonical URL** by a
//! [`ValidatorStore`].
//!
//! The default [`MemoryValidators`] impl is in-memory and hermetic, so the
//! conditional-refresh path is testable without a database. A caller that wants
//! durability can supply its own metadata-backed impl later; the fetcher only
//! depends on the trait.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cache validators for one resource, as returned by a server and used to
/// build `If-None-Match` / `If-Modified-Since` on a later request.
///
/// `fetched_at` is Unix epoch seconds: when the representation was fetched (200)
/// or the last time it was revalidated (304).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Validators {
    /// Value of the `ETag` header, if the server sent one.
    pub etag: Option<String>,
    /// Value of the `Last-Modified` header, if the server sent one.
    pub last_modified: Option<String>,
    /// Unix epoch seconds of the fetch/revalidation that produced this value.
    pub fetched_at: i64,
}

impl Validators {
    /// Build validators from a response, stamping `fetched_at` with the current
    /// wall-clock time (Unix epoch seconds).
    pub fn new(etag: Option<String>, last_modified: Option<String>) -> Self {
        Self {
            etag,
            last_modified,
            fetched_at: now_epoch_secs(),
        }
    }

    /// Build validators with an explicit `fetched_at` (useful for tests and for
    /// metadata-backed stores that preserve the original fetch time).
    pub fn with_fetched_at(
        etag: Option<String>,
        last_modified: Option<String>,
        fetched_at: i64,
    ) -> Self {
        Self {
            etag,
            last_modified,
            fetched_at,
        }
    }

    /// Whether there is anything worth sending as a conditional request.
    pub fn has_any(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }
}

/// Store of [`Validators`] keyed by canonical URL.
///
/// `Send + Sync` so it can be shared across the async fetch workers. `get` and
/// `put` are intentionally synchronous and infallible: a store miss or a failed
/// persistence is not a fetch error (the fetch simply proceeds unconditionally).
pub trait ValidatorStore: Send + Sync {
    /// Validators previously stored for `url`, if any.
    fn get(&self, url: &str) -> Option<Validators>;
    /// Store (or replace) the validators for `url`.
    fn put(&self, url: &str, validators: Validators);
}

/// In-memory [`ValidatorStore`]. The default; hermetic and used by tests.
#[derive(Debug, Default)]
pub struct MemoryValidators {
    inner: Mutex<HashMap<String, Validators>>,
}

impl MemoryValidators {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries (test/introspection helper).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("validators mutex poisoned").len()
    }

    /// Whether the store is empty (test/introspection helper).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ValidatorStore for MemoryValidators {
    fn get(&self, url: &str) -> Option<Validators> {
        self.inner
            .lock()
            .expect("validators mutex poisoned")
            .get(url)
            .cloned()
    }

    fn put(&self, url: &str, validators: Validators) {
        self.inner
            .lock()
            .expect("validators mutex poisoned")
            .insert(url.to_string(), validators);
    }
}

/// Current wall-clock time as Unix epoch seconds (0 if the clock is before the
/// epoch, which cannot happen in practice).
fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryValidators::new();
        assert!(store.is_empty());
        assert!(store.get("http://a/").is_none());

        let v = Validators::new(Some("\"v1\"".into()), None);
        assert!(v.has_any());
        store.put("http://a/", v.clone());

        assert_eq!(store.get("http://a/"), Some(v));
        assert_eq!(store.len(), 1);
        assert!(store.get("http://b/").is_none());
    }

    #[test]
    fn empty_validators_have_nothing_to_send() {
        let v = Validators::with_fetched_at(None, None, 123);
        assert!(!v.has_any());
        assert_eq!(v.fetched_at, 123);
    }

    #[test]
    fn new_stamps_current_time() {
        let v = Validators::new(None, Some("Wed, 21 Oct 2015 07:28:00 GMT".into()));
        assert!(v.fetched_at > 0);
    }
}
