//! The answerer client: cache + single-flight + bounded concurrency over an injected
//! [`Transport`], exposed as a synchronous [`search_engine::Answerer`].
//!
//! `Answerer::answer` is synchronous (the engine calls it on the search hot path), so the client
//! owns a small dedicated Tokio runtime and drives the async transport future to completion.
//! When `answer` is invoked from inside another Tokio runtime it is moved to a fresh OS thread
//! first, because nesting `block_on` on a runtime thread panics.
//!
//! Retries: none. A transport error is returned to the engine, which ignores it — a retry would
//! only double the hot-path latency. (The spec allows at most one; this is the safe minimum.)

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::cache::{cache_key_hash, count_entries, read_cache, write_cache_atomic};
use crate::config::AnswerConfig;
use crate::error::AnswerError;
use crate::prompt::{build_prompt, parse_chat_response, PROMPT_VERSION};
use crate::transport::{ChainTransport, Transport};

/// A snapshot of cache activity. `hits`/`misses` count `answer` calls served from, or populating,
/// the on-disk cache. `entries` is the number of `*.json` files on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: u64,
}

struct Inner {
    cfg: AnswerConfig,
    cache_dir: PathBuf,
    transport: Arc<dyn Transport>,
    sem: Arc<Semaphore>,
    key_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    rt: tokio::runtime::Runtime,
    /// The chain this client was built over, when it was: lets the attempts of the last call be read
    /// back (which models were tried, which failed, which answered).
    chain: Option<Arc<ChainTransport>>,
    /// Usage of the most recent model call. `None` until one happens; a cache hit records zeros,
    /// because no tokens were sent.
    last_usage: Mutex<Option<search_core::TokenUsage>>,
}

/// LLM answerer implementing [`search_engine::Answerer`].
///
/// Cheap to clone (`Arc` inside). Holds the on-disk cache directory, the injected transport, a
/// concurrency semaphore and the per-key single-flight locks.
#[derive(Clone)]
pub struct LlmAnswerer {
    inner: Arc<Inner>,
}

impl LlmAnswerer {
    /// Build a client with a caller-supplied transport (hermetic tests, or a custom backend).
    pub fn with_transport(cfg: AnswerConfig, transport: Arc<dyn Transport>) -> Result<Self, AnswerError> {
        Self::with_transport_and_chain(cfg, transport, None)
    }

    /// Build a client over a [`ChainTransport`], so the attempts of each call can be reported
    /// alongside the answer. `chain` must be the same object as `transport` when it is one.
    pub fn with_transport_and_chain(
        cfg: AnswerConfig,
        transport: Arc<dyn Transport>,
        chain: Option<Arc<ChainTransport>>,
    ) -> Result<Self, AnswerError> {
        std::fs::create_dir_all(&cfg.cache_dir)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| AnswerError::Config(format!("failed to build runtime: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                sem: Arc::new(Semaphore::new(cfg.concurrency.max(1))),
                key_locks: Mutex::new(HashMap::new()),
                hits: AtomicU64::new(0),
                misses: AtomicU64::new(0),
                cache_dir: cfg.cache_dir.clone(),
                cfg,
                transport,
                rt,
                chain,
                last_usage: Mutex::new(None),
            }),
        })
    }

    /// Record what the most recent call cost (see [`Inner::last_usage`]).
    fn record_usage(&self, usage: Option<search_core::TokenUsage>) {
        if let Ok(mut guard) = self.inner.last_usage.lock() {
            *guard = usage;
        }
    }

    /// The configured model id.
    pub fn model(&self) -> &str {
        &self.inner.cfg.model
    }

    /// Snapshot of cache activity.
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            hits: self.inner.hits.load(Ordering::Relaxed),
            misses: self.inner.misses.load(Ordering::Relaxed),
            entries: count_entries(&self.inner.cache_dir),
        }
    }

    /// Drive `fut` to completion from a synchronous context.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T> + Send,
        T: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Already inside a runtime: run on our own runtime from a dedicated thread so we
            // never nest `block_on` on the caller's runtime (which panics).
            std::thread::scope(|s| {
                s.spawn(|| self.inner.rt.block_on(fut))
                    .join()
                    .expect("answer runtime thread panicked")
            })
        } else {
            self.inner.rt.block_on(fut)
        }
    }

    async fn answer_inner(
        &self,
        query: &str,
        evidence: &[search_engine::evidence::EvidenceRecord],
        passages: &[String],
    ) -> Result<String, AnswerError> {
        let prompt = build_prompt(query, evidence, passages);
        let model = self.inner.cfg.model.clone();
        let hash = cache_key_hash(&model, PROMPT_VERSION, &prompt);

        // Fast path: no lock, no transport.
        if let Some(hit) = read_cache(&self.inner.cache_dir, &hash) {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            // A cache hit sent no tokens: report zeros rather than a stale figure from an earlier
            // call, so "0 in / 0 out" reads as "nothing was asked of a model".
            self.record_usage(Some(search_core::TokenUsage::default()));
            return Ok(hit);
        }

        // Single-flight: one lock per cache key so concurrent identical prompts call the
        // transport once.
        let lock = {
            let mut map = self.inner.key_locks.lock().expect("key lock map poisoned");
            map.entry(hash.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        // Double-checked: a peer may have populated the entry while we waited.
        if let Some(hit) = read_cache(&self.inner.cache_dir, &hash) {
            self.inner.hits.fetch_add(1, Ordering::Relaxed);
            self.record_usage(Some(search_core::TokenUsage::default()));
            return Ok(hit);
        }

        self.inner.misses.fetch_add(1, Ordering::Relaxed);

        // Bounded parallelism: hold a permit only across the transport call.
        let body = {
            let _permit = self
                .inner
                .sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| AnswerError::Config("concurrency semaphore closed".to_string()))?;
            self.inner.transport.chat(&model, &prompt).await?
        };

        // Record what the call cost before parsing: the provider was paid even if its body turns
        // out to be unparseable.
        let usage = serde_json::from_str::<serde_json::Value>(&body)
            .map(|v| search_core::TokenUsage::from_provider_json(&v))
            .unwrap_or_default();
        self.record_usage(Some(usage));

        let answer = parse_chat_response(&body)?;
        write_cache_atomic(
            &self.inner.cache_dir,
            &hash,
            &model,
            PROMPT_VERSION,
            &prompt,
            &answer,
        )?;
        Ok(answer)
    }
}

impl search_engine::Answerer for LlmAnswerer {
    fn answer(
        &self,
        query: &str,
        evidence: &[search_engine::evidence::EvidenceRecord],
        passages: &[String],
    ) -> Result<String, String> {
        self.block_on(self.answer_inner(query, evidence, passages))
            .map_err(|e| e.to_string())
    }

    /// What the most recent answer call cost the provider (`0 in / 0 out` on a cache hit, because
    /// nothing was sent).
    fn last_usage(&self) -> Option<search_core::TokenUsage> {
        self.inner.last_usage.lock().ok().and_then(|guard| *guard)
    }

    /// The models tried by the most recent answer call, in order: which failed and which answered.
    fn last_attempts(&self) -> Vec<search_core::LlmAttempt> {
        self.inner
            .chain
            .as_ref()
            .map(|chain| chain.last_attempts())
            .unwrap_or_default()
    }
}
