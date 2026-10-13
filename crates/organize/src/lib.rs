//! Page organizer (plan: turn raw scraped web text into normalized dataset records).
//!
//! Uses a cheap OpenAI-chat-compatible model to extract structured metadata from each scraped
//! page, with:
//!
//! * an **on-disk response cache** keyed on `(base_url, model, prompt)` so the same completion
//!   is never paid for twice (writes are atomic; see [`client`]/[`cache`]);
//! * **single-flight** per cache key, so concurrent identical prompts call the transport once;
//! * **bounded parallelism** via a semaphore, so `organize_all` never exceeds the configured
//!   concurrency of in-flight transport calls.
//!
//! The credential comes from `SEARCH_LLM_API_KEY` (else `KITANI_API_KEY`) at runtime and is never
//! hardcoded. Tests are hermetic via the injectable [`Transport`] — no network, no real key, no
//! sleeps.
//!
//! The output [`Record`] is **exactly** the dataset crate's pack record type (re-exported, never
//! duplicated).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

mod cache;
mod client;
mod config;
mod error;
mod prompt;
mod queryplan;
mod transport;

pub use client::{CacheStats, LlmClient};
pub use config::{
    fallback_api_key, fallback_base_url, fallback_model, llm_user_agent, model_chain, LlmConfig,
    DEFAULT_BASE_URL, DEFAULT_ESCALATE_MODEL, DEFAULT_FALLBACK_BASE_URL, DEFAULT_FALLBACK_MODEL,
    DEFAULT_LLM_USER_AGENT, DEFAULT_MODEL, DEFAULT_TIMEOUT_S,
};
pub use error::OrganizeError;
pub use prompt::{build_prompt, parse_response, Organized};
pub use queryplan::{build_query_prompt, parse_plan, QueryPlan};
pub use transport::{ChainEntry, ChainTransport, FallbackTransport, Transport};

/// Re-exported verbatim: the organizer emits the dataset pack record, not a look-alike.
pub use search_dataset::Record;

/// Raw scraped page, as produced by the crawler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPage {
    /// Absolute URL of the page.
    pub url: String,
    /// Crawler/source label, copied onto the record's `source`.
    pub source: String,
    /// Page title from the crawler, used only when the model title is absent.
    pub title: Option<String>,
    /// Raw page text.
    pub text: String,
    /// Unix epoch seconds, when the crawler parsed a date.
    pub published_at: Option<i64>,
}

/// Stable record id: the SHA-256 of the page URL, hex-encoded. Deterministic from the URL alone.
pub fn id_from_url(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    hex::encode(h.finalize())
}

/// Organize one page into zero or more dataset [`Record`]s.
///
/// The primary model is asked first. On any parse/validation failure (or a primary transport
/// error) it escalates **exactly once** to the thinking model; if that also fails, an error is
/// returned. Nothing is ever fabricated: the returned record's `url` is the page URL and its
/// `text` is non-empty.
pub async fn organize_page(
    client: &LlmClient,
    page: &RawPage,
) -> Result<Vec<Record>, OrganizeError> {
    let prompt = build_prompt(page);

    let (organized, model_used) = match client.complete(&prompt).await {
        Ok(raw) => match parse_response(&raw) {
            Ok(o) => (o, client.model().to_string()),
            Err(_) => escalate(client, &prompt).await?,
        },
        Err(_) => escalate(client, &prompt).await?,
    };

    Ok(vec![build_record(page, organized, &model_used)?])
}

/// The single escalation attempt: ask the thinking model and parse its answer.
async fn escalate(
    client: &LlmClient,
    prompt: &str,
) -> Result<(Organized, String), OrganizeError> {
    let raw = client.complete_escalated(prompt).await?;
    let organized = parse_response(&raw)?;
    Ok((organized, client.escalate_model().to_string()))
}

/// Turn parsed model fields plus the source page into a dataset [`Record`].
fn build_record(
    page: &RawPage,
    organized: Organized,
    model_used: &str,
) -> Result<Record, OrganizeError> {
    // `text` stays the source text unless the model returned an explicit cleaned version.
    let text = organized
        .clean_text
        .clone()
        .unwrap_or_else(|| page.text.clone());
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(OrganizeError::InvalidPage(format!(
            "no usable text for {}",
            page.url
        )));
    }

    let mut record = Record::new(id_from_url(&page.url), page.source.clone());
    record.url = Some(page.url.clone());
    record.title = organized
        .title
        .clone()
        .or_else(|| page.title.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    record.text = text;
    record.published_at = page.published_at;

    let extras = &mut record.extras;
    extras.insert("summary".to_string(), organized.summary.clone());
    extras.insert("topics".to_string(), organized.topics.join(","));
    extras.insert("doc_type".to_string(), organized.doc_type.clone());
    if let Some(lang) = &organized.language {
        extras.insert("language".to_string(), lang.clone());
    }
    extras.insert("key_entities".to_string(), organized.key_entities.join(","));
    if let Some(hint) = &organized.id_hint {
        extras.insert("id_hint".to_string(), hint.clone());
    }
    extras.insert("organizer_model".to_string(), model_used.to_string());

    Ok(record)
}

/// A page that could not be organized, with the reason — reported by [`organize_all_lenient`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFailure {
    pub url: String,
    pub error: String,
}

/// Organize many pages, keeping every success and *reporting* failures instead of aborting.
///
/// This is the batch path for real exports over the live web: one flaky page (a transport blip,
/// a model that will not emit usable JSON) must not throw away the work already paid for. The
/// returned records preserve input order; `failures` lists the pages that could not be organized
/// after the escalation attempt.
pub async fn organize_all_lenient(
    client: &LlmClient,
    pages: Vec<RawPage>,
) -> (Vec<Record>, Vec<PageFailure>) {
    let n = pages.len();
    let mut set = tokio::task::JoinSet::new();
    for (idx, page) in pages.into_iter().enumerate() {
        let worker = client.clone();
        let url = page.url.clone();
        set.spawn(async move {
            let result = organize_page(&worker, &page).await;
            (idx, url, result)
        });
    }

    let mut slots: Vec<Option<Result<Vec<Record>, OrganizeError>>> = (0..n).map(|_| None).collect();
    let mut failures = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, _url, Ok(records))) => slots[idx] = Some(Ok(records)),
            Ok((_, url, Err(e))) => failures.push(PageFailure {
                url,
                error: e.to_string(),
            }),
            Err(e) => failures.push(PageFailure {
                url: String::new(),
                error: format!("worker task failed: {e}"),
            }),
        }
    }

    let mut out = Vec::with_capacity(n);
    for slot in slots.into_iter().flatten() {
        out.extend(slot.unwrap_or_default());
    }
    (out, failures)
}

/// Organize many pages with bounded parallelism (the client's configured concurrency).
///
/// The returned vector preserves the input order and is stable. Any page that fails to organize
/// (even after escalation) fails the whole call; for batch exports where partial results are
/// preferable, use [`organize_all_lenient`].
pub async fn organize_all(
    client: &LlmClient,
    pages: Vec<RawPage>,
) -> Result<Vec<Record>, OrganizeError> {
    let (records, failures) = organize_all_lenient(client, pages).await;
    match failures.into_iter().next() {
        Some(f) => Err(OrganizeError::InvalidPage(format!(
            "{}: {}",
            f.url, f.error
        ))),
        None => Ok(records),
    }
}

/// The cache file path for a given `(base_url, model, prompt)` tuple (thin re-export so callers
/// can inspect cache contents without reaching into private modules).
pub fn cache_entry_path(cache_dir: &std::path::Path, base_url: &str, model: &str, prompt: &str) -> PathBuf {
    let hash = cache::cache_key_hash(base_url, model, prompt);
    cache::entry_path(cache_dir, &hash)
}

/// Convenience helper for tests/tools: the current cache stats of `client` serialized as JSON.
pub fn cache_stats_json(stats: CacheStats) -> serde_json::Value {
    serde_json::json!({
        "hits": stats.hits,
        "misses": stats.misses,
        "entries": stats.entries,
    })
}

// Keep the unused-import checker honest: `transport` also re-exports the real HTTP client only
// through `LlmClient::new`, so nothing else needs it here.
#[allow(dead_code)]
fn _assert_transport_object_safe(_t: Arc<dyn Transport>) {}
