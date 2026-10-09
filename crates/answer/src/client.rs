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
use crate::transport::Transport;

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
            }),
        })
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
}
