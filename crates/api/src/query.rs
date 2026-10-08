//! Query analysis + execution (spec §7).
//!
//! The API never talks to Tantivy directly. It depends only on the small
//! [`Searcher`] trait defined here plus the frozen `search-core` contract, so
//! the crate builds and is fully testable with a stub while the real indexer
//! lands in parallel. `src/indexer_adapter.rs` wires the real implementation
//! behind the `real-indexer` cargo feature.

use search_core::contract::{Signal, TimeInterval};
use search_core::{
    AppError, AppResult, IndexManifest, Intent, Provenance, QueryAnalysis, SearchRequest,
    SearchResponse, SearchResult,
};
use std::time::Instant;

/// Largest `limit` the API will serve. Requests above this are clamped.
pub const MAX_LIMIT: usize = 100;
/// `limit` when the caller omits it (matches the serde default in core).
pub const DEFAULT_LIMIT: usize = 10;

/// Bumped when the plan/ranking behaviour changes; part of the reproducibility
/// tuple returned in every [`SearchResponse`].
pub const QUERY_PLAN_VERSION: &str = "stage0-plan-v1";
pub const RANKING_POLICY_VERSION: &str = "stage0-lexical-v1";
pub const GRAPH_VERSION: &str = "stage0-graph-v0";

/// One candidate document returned by a [`Searcher`].
///
/// This deliberately mirrors the stored fields of a [`SearchResult`] without
/// pulling the real indexer (or Tantivy) into the API's dependency graph: the
/// indexer adapter hydrates one of these per hit and `execute` finishes the job
/// (dedup/sort/snippet/signals/response assembly).
#[derive(Debug, Clone)]
pub struct IndexHit {
    pub doc_id: String,
    pub version: u64,
    pub resource_id: String,
    pub work_id: Option<String>,
    pub content_id: Option<String>,
    /// Canonical URL of the document.
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub source_id: Option<String>,
    pub published_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub fetched_at: i64,
    /// `"lexical_base"` | `"lexical_overlay"`.
    pub lane: String,
    pub provenance: Provenance,
    pub score: f32,
    pub signals: Vec<Signal>,
}

impl From<IndexHit> for SearchResult {
    fn from(h: IndexHit) -> Self {
        let mut signals = h.signals;
        if signals.is_empty() {
            // Every result must carry at least one Signal so "Why this result?"
            // works from Stage 0 (spec §7).
            signals.push(Signal {
                name: "lexical_score".to_string(),
                value: h.score,
                note: format!("bm25 lexical score in lane {}", h.lane),
            });
        }
        SearchResult {
            doc_id: h.doc_id,
            version: h.version,
            resource_id: h.resource_id,
            work_id: h.work_id,
            content_id: h.content_id,
            canonical_url: h.url,
            title: h.title,
            snippet: h.snippet,
            source_id: h.source_id,
            published_at: h.published_at,
            modified_at: h.modified_at,
            fetched_at: h.fetched_at,
            lane: h.lane,
            provenance: h.provenance,
            score: h.score,
            signals,
        }
    }
}

/// The one thing the API needs from retrieval. Kept intentionally tiny so it is
/// trivial to stub in tests and to implement/adapt over the real Tantivy base +
/// overlay indexer.
pub trait Searcher: Send + Sync {
    /// Return up to `limit` candidates for `q`, ranked by the implementation.
    fn search(&self, q: &str, limit: usize) -> AppResult<Vec<IndexHit>>;

    /// The manifest describing the index snapshot these answers come from.
    ///
    /// Defaulted so a stub stays a stub; real implementations override this to
    /// report the published generation and overlay watermark, which are echoed
    /// in `/health` and in every [`SearchResponse`] for reproducibility.
    fn manifest(&self) -> AppResult<IndexManifest> {
        Ok(IndexManifest {
            manifest_id: "unpublished".to_string(),
            generation: 0,
            doc_count: 0,
            base_segments: 0,
            watermark: 0,
            embedding_model_ids: Vec::new(),
            metadata_version: 0,
            created_at: 0,
            published_at: None,
        })
    }
}

/// Validate + normalize the numeric bounds of a request.
///
/// `limit` outside `1..=MAX_LIMIT` is an error only at the zero end (a request
/// for "no results" is meaningless); an oversized limit is clamped rather than
/// rejected so a client asking for 500 still gets the best 100.
pub fn normalize_limit(limit: usize) -> AppResult<usize> {
    if limit == 0 {
        return Err(AppError::Invalid("limit must be >= 1".into()));
    }
    Ok(limit.min(MAX_LIMIT))
}

/// Run a search: analyze the query, pull candidates from the [`Searcher`],
/// hydrate/sort/trim them, and assemble the frozen response.
pub fn execute(searcher: &dyn Searcher, req: &SearchRequest) -> AppResult<SearchResponse> {
    let started = Instant::now();

    let query = req.query.trim();
    if query.is_empty() {
        return Err(AppError::Invalid("query must not be empty".into()));
    }
    let limit = normalize_limit(req.limit)?;
    let analysis = analyze(query);

    // Ask the indexer for enough candidates to satisfy offset + page.
    let want = limit.saturating_add(req.offset);
    let mut hits = searcher.search(query, want)?;

    // Rank: score desc; tie-break on doc_id for a stable, reproducible order.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    let results: Vec<SearchResult> = hits
        .into_iter()
        .skip(req.offset)
        .take(limit)
        .map(SearchResult::from)
        .collect();

    let manifest = searcher.manifest()?;
    let snapshot_id = snapshot_id(
        &manifest.manifest_id,
        manifest.watermark,
        GRAPH_VERSION,
        RANKING_POLICY_VERSION,
        QUERY_PLAN_VERSION,
    );

    Ok(SearchResponse {
        query_analysis: analysis,
        results,
        timing_ms: started.elapsed().as_millis() as u64,
        index_manifest_id: manifest.manifest_id,
        overlay_watermark: manifest.watermark,
        graph_version: GRAPH_VERSION.to_string(),
        ranking_policy_version: RANKING_POLICY_VERSION.to_string(),
        query_plan_version: QUERY_PLAN_VERSION.to_string(),
        snapshot_id,
        complete: true,
        degradations: Vec::new(),
    })
}

/// Deterministic FNV-1a 64-bit hash over the reproducibility tuple.
fn snapshot_id(
    manifest_id: &str,
    watermark: u64,
    graph_version: &str,
    ranking_policy_version: &str,
    query_plan_version: &str,
) -> String {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            acc ^= *b as u64;
            acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    mix(manifest_id.as_bytes());
    mix(&watermark.to_le_bytes());
    mix(graph_version.as_bytes());
    mix(ranking_policy_version.as_bytes());
    mix(query_plan_version.as_bytes());
    format!("{:016x}", acc)
}

/// Heuristic query analysis for Stage 0 (spec §7 `QueryAnalysis`).
///
/// Extracts quoted exact phrases, classifies intent, and gives a rough
/// complexity score. Language/entity detection is intentionally shallow here;
/// later stages swap in classifiers without changing the response shape.
pub fn analyze(query: &str) -> QueryAnalysis {
    let normalized_text = query.split_whitespace().collect::<Vec<_>>().join(" ");

    let exact_phrases = extract_quoted_phrases(&normalized_text);
    let lower = normalized_text.to_lowercase();

    let terms: Vec<String> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    let stop: &[&str] = &[
        "the", "a", "an", "of", "to", "in", "on", "for", "and", "or", "is", "are", "was", "were",
        "be", "with", "at", "by", "from", "as", "it", "this", "that", "how", "what", "who",
        "when", "where", "why", "which",
    ];
    let concepts: Vec<String> = terms
        .iter()
        .filter(|t| t.len() > 1 && !stop.contains(&t.as_str()))
        .cloned()
        .collect();

    // Entity heuristic: capitalized tokens (not sentence-initial) that are not
    // stopwords. Deliberately conservative — Stage 0 keeps hypotheses empty.
    let mut entities = Vec::new();
    for (i, raw) in normalized_text.split_whitespace().enumerate() {
        let bare = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if bare.is_empty() {
            continue;
        }
        let mut chars = bare.chars();
        let first_upper = chars.next().map(|c| c.is_uppercase()).unwrap_or(false);
        if first_upper && i > 0 && !stop.contains(&bare.to_lowercase().as_str()) {
            let e = bare.to_string();
            if !entities.contains(&e) {
                entities.push(e);
            }
        }
    }

    let intent = classify_intent(&normalized_text, &terms);
    let time_interval = detect_time_interval(&normalized_text);
    let preferred_types = detect_preferred_types(&lower);

    let soft: &[&str] = &["best", "fastest", "cheap", "cheapest", "latest", "recent", "free"];
    let soft_preferences: Vec<String> = soft
        .iter()
        .filter(|&&w| lower.split_whitespace().any(|t| t == w))
        .map(|w| w.to_string())
        .collect();

    // Complexity 0..=4: length, phrases, entities, and explicit connectors.
    let mut complexity: u8 = 0;
    if terms.len() > 3 {
        complexity += 1;
    }
    if terms.len() > 8 {
        complexity += 1;
    }
    if !exact_phrases.is_empty() {
        complexity += 1;
    }
    if !entities.is_empty() {
        complexity += 1;
    }
    if lower.contains(" vs ") || lower.contains(" versus ") || lower.contains(" compare ") {
        complexity = complexity.max(3);
    }
    complexity = complexity.min(4);

    QueryAnalysis {
        original: query.to_string(),
        normalized_text,
        language: None,
        exact_phrases,
        entities,
        entity_hypotheses: Vec::new(),
        concepts,
        hard_predicates: Vec::new(),
        soft_preferences,
        time_interval,
        preferred_types,
        intent,
        complexity,
    }
}

/// Pull every `"quoted phrase"` out of the query, in order, de-duplicated.
fn extract_quoted_phrases(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    for ch in text.chars() {
        match ch {
            '"' => match current.take() {
                Some(p) => {
                    let p = p.trim().to_string();
                    if !p.is_empty() && !out.contains(&p) {
                        out.push(p);
                    }
                }
                None => current = Some(String::new()),
            },
            _ => {
                if let Some(buf) = current.as_mut() {
                    buf.push(ch);
                }
            }
        }
    }
    out
}

fn classify_intent(text: &str, terms: &[String]) -> Intent {
    let lower = text.to_lowercase();
    let has = |w: &str| lower.split_whitespace().any(|t| t == w);

    // A single capitalized bare token ("Wikipedia") reads as a navigational reach.
    let single_capitalized = terms.len() == 1
        && text
            .split_whitespace()
            .next()
            .and_then(|w| w.chars().next())
            .map(|c| c.is_uppercase())
            .unwrap_or(false);

    // Navigational: looks like a URL/host or a "go to X" reach.
    let looks_like_host = lower.contains(".com")
        || lower.contains(".org")
        || lower.contains(".net")
        || lower.contains("http://")
        || lower.contains("https://");
    if looks_like_host
        || has("login")
        || has("download")
        || lower.starts_with("site:")
        || single_capitalized
    {
        return Intent::Navigational;
    }

    // Current: recency-wanting.
    if has("latest") || has("news") || has("today") || has("breaking") || has("current") {
        return Intent::Current;
    }

    // Research: explicit study/analysis framing.
    if lower.contains(" vs ")
        || lower.contains(" versus ")
        || has("compare")
        || has("analysis")
        || has("study")
        || has("paper")
        || has("research")
        || has("survey")
    {
        return Intent::Research;
    }

    Intent::Factual
}

/// Detect simple relative time expressions relative to now. Stage 0 subset:
/// `today`, `yesterday`, and `last N (day|week|month|year)s`.
fn detect_time_interval(text: &str) -> Option<TimeInterval> {
    let lower = text.to_lowercase();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    const DAY: i64 = 86_400;

    let words: Vec<&str> = lower.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        // First alphabetic run of the token: "today's" -> "today".
        let w = w.split(|c: char| !c.is_alphabetic()).next().unwrap_or("");
        if w == "today" {
            return Some(TimeInterval {
                start: now - DAY,
                end: now,
            });
        }
        if w == "yesterday" {
            return Some(TimeInterval {
                start: now - 2 * DAY,
                end: now - DAY,
            });
        }
        if w == "last" {
            if let Some(n) = words.get(i + 1).and_then(|s| s.parse::<i64>().ok()) {
                if let Some(unit) = words.get(i + 2) {
                    let unit = unit.trim_end_matches('s');
                    let span = match unit {
                        "day" => n * DAY,
                        "week" => n * 7 * DAY,
                        "month" => n * 30 * DAY,
                        "year" => n * 365 * DAY,
                        _ => continue,
                    };
                    return Some(TimeInterval {
                        start: now - span,
                        end: now,
                    });
                }
            }
        }
    }
    None
}

fn detect_preferred_types(lower: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (needle, ty) in [
        ("pdf", "pdf"),
        ("video", "video"),
        ("image", "image"),
        ("news", "news"),
        ("paper", "paper"),
        ("dataset", "dataset"),
    ] {
        if lower.split_whitespace().any(|t| t == needle) {
            out.push(ty.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_exact_phrases_in_quotes() {
        let a = analyze("best \"rust async runtime\" for \"tokio\" users");
        assert_eq!(
            a.exact_phrases,
            vec!["rust async runtime".to_string(), "tokio".to_string()]
        );
        // Unbalanced/empty quotes contribute nothing.
        let b = analyze("a lone \" phrase");
        assert!(b.exact_phrases.is_empty());
    }

    #[test]
    fn classifies_intent() {
        assert_eq!(analyze("what is rust").intent, Intent::Factual);
        assert_eq!(
            analyze("https://example.com/login").intent,
            Intent::Navigational
        );
        assert_eq!(analyze("latest rust news").intent, Intent::Current);
        assert_eq!(analyze("tokio vs async-std analysis").intent, Intent::Research);
        // Single capitalized token: treated as a navigational reach.
        assert_eq!(analyze("Wikipedia").intent, Intent::Navigational);
    }

    #[test]
    fn normalized_and_complexity() {
        let a = analyze("  rust   async   runtime  ");
        assert_eq!(a.normalized_text, "rust async runtime");
        assert!(a.complexity <= 4);
        assert!(a.concepts.contains(&"rust".to_string()));
        let complex = analyze("compare \"exact phrase\" tokio vs async just for fun really");
        assert!(complex.complexity >= 3);
    }

    #[test]
    fn detects_time_interval_and_types() {
        assert!(analyze("news from last 7 days").time_interval.is_some());
        assert!(analyze("today's weather").time_interval.is_some());
        assert!(analyze("anything at all").time_interval.is_none());
        assert_eq!(analyze("rust pdf guide").preferred_types, vec!["pdf"]);
    }

    #[test]
    fn limit_validation() {
        assert_eq!(normalize_limit(0).unwrap_err().to_string(), "invalid input: limit must be >= 1");
        assert_eq!(normalize_limit(10).unwrap(), 10);
        assert_eq!(normalize_limit(500).unwrap(), MAX_LIMIT);
    }
}
