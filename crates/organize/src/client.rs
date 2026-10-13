//! The LLM client: cache + single-flight + bounded concurrency over an injected [`Transport`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::cache::{cache_key_hash, count_entries, read_cache, write_cache_atomic};
use crate::config::LlmConfig;
use crate::error::OrganizeError;
use crate::transport::{ChainEntry, ChainTransport, HttpTransport, Transport};

/// A snapshot of cache activity. `hits`/`misses` count `complete*` calls that were served from,
/// or had to populate, the on-disk cache. `entries` is the number of `*.json` files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: u64,
}

/// Organizer LLM client.
///
/// Holds the cache directory, the injected transport, a concurrency semaphore and the
/// per-key single-flight locks. Cheap to clone (all fields are `Arc`/`Clone`), which is what
/// [`crate::organize_all`] uses to fan work out across tasks.
#[derive(Clone)]
pub struct LlmClient {
    cfg: LlmConfig,
    cache_dir: PathBuf,
    transport: Arc<dyn Transport>,
    sem: Arc<Semaphore>,
    key_locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl LlmClient {
    /// Build a client with the real HTTP transport. `cache_dir` holds one file per
    /// `(base_url, model, prompt)`; `concurrency` bounds simultaneous transport calls.
    pub fn new(
        cfg: LlmConfig,
        cache_dir: PathBuf,
        concurrency: usize,
    ) -> Result<Self, OrganizeError> {
        let api_key = cfg.api_key.clone().ok_or_else(|| {
            OrganizeError::MissingEnv(
                "KITANI_API_KEY (or SEARCH_LLM_API_KEY / OPENROUTER_API_KEY)".to_string(),
            )
        })?;
        // Free models first, on the primary provider, then the fallback provider as the last
        // resort. A model that fails moves to the next entry: today a dead primary provider means
        // the planner loses the plan entirely, which shows up as zero expansion probes.
        let primary_label = crate::transport::provider_label(&cfg.base_url);
        let listed = cfg.models.clone();
        let models = if listed.is_empty() {
            vec![cfg.model.clone()]
        } else {
            listed
        };
        let mut entries: Vec<ChainEntry> = Vec::with_capacity(models.len() + 1);
        for model in models {
            let http = HttpTransport::new(&cfg, api_key.clone())?;
            entries.push(ChainEntry::new(primary_label.clone(), model, Arc::new(http)));
        }
        if let Some(fallback_key) = crate::config::fallback_api_key() {
            let mut fb = cfg.clone();
            fb.base_url = crate::config::fallback_base_url();
            fb.api_key = Some(fallback_key.clone());
            let http = HttpTransport::new(&fb, fallback_key)?;
            entries.push(ChainEntry::new(
                crate::transport::provider_label(&fb.base_url),
                crate::config::fallback_model(),
                Arc::new(http),
            ));
        }
        let transport: Arc<dyn Transport> = Arc::new(ChainTransport::new(entries));
        Self::with_transport(cfg, cache_dir, concurrency, transport)
    }

    /// Hermetic-test constructor: identical cache/single-flight/concurrency semantics, with a
    /// caller-supplied transport.
    pub fn with_transport(
        cfg: LlmConfig,
        cache_dir: PathBuf,
        concurrency: usize,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, OrganizeError> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cfg,
            cache_dir,
            transport,
            sem: Arc::new(Semaphore::new(concurrency.max(1))),
            key_locks: Arc::new(Mutex::new(HashMap::new())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The configured primary (cheap) model id.
    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// The configured escalation (thinking) model id.
    pub fn escalate_model(&self) -> &str {
        &self.cfg.escalate_model
    }

    /// Ask the primary model; a cached `(model, prompt)` response short-circuits the transport.
    pub async fn complete(&self, prompt: &str) -> Result<String, OrganizeError> {
        let model = self.cfg.model.clone();
        self.complete_with(&model, prompt).await
    }

    /// Ask the escalation model (e.g. `deepseek/deepseek-v4.1-flash-thinking`).
    pub async fn complete_escalated(&self, prompt: &str) -> Result<String, OrganizeError> {
        let model = self.cfg.escalate_model.clone();
        self.complete_with(&model, prompt).await
    }

    /// Snapshot of cache activity.
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: count_entries(&self.cache_dir),
        }
    }

    async fn complete_with(&self, model: &str, prompt: &str) -> Result<String, OrganizeError> {
        let hash = cache_key_hash(&self.cfg.base_url, model, prompt);

        // Fast path: no lock, no transport.
        if let Some(resp) = read_cache(&self.cache_dir, &hash) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(resp);
        }

        // Single-flight: one lock per cache key, so concurrent identical prompts do not all
        // call the transport. Distinct keys never contend.
        let lock = {
            let mut map = self.key_locks.lock().expect("key lock map poisoned");
            map.entry(hash.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Double-checked: a peer may have populated the entry while we waited for the lock.
        if let Some(resp) = read_cache(&self.cache_dir, &hash) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(resp);
        }

        self.misses.fetch_add(1, Ordering::Relaxed);

        // Bounded parallelism: hold a permit only across the transport call.
        let resp = {
            let _permit = self
                .sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| OrganizeError::Config("concurrency semaphore closed".to_string()))?;
            self.transport.chat(model, prompt).await?
        };

        write_cache_atomic(&self.cache_dir, &hash, model, prompt, &resp)?;
        Ok(resp)
    }
}
