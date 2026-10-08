//! Query analysis + execution (spec §7).
//!
//! The API never talks to Tantivy directly. It depends only on the small
//! [`Searcher`] trait defined here plus the frozen `search-core` contract, so
//! the crate builds and is fully testable with a stub while the real indexer
//! lands in parallel. `src/indexer_adapter.rs` wires the real implementation
//! behind the `real-indexer` cargo feature.

use search_core::contract::{Signal, TimeInterval};
use search_core::{
    AppError, AppResult, IndexManifest, Intent, Provenance, QueryAnalysis, SearchFilters,
    SearchRequest, SearchResponse, SearchResult,
};
use std::time::Instant;

/// Largest `limit` the API will serve. Requests above this are clamped.
pub const MAX_LIMIT: usize = 100;
/// `limit` when the caller omits it (matches the serde default in core).
pub const DEFAULT_LIMIT: usize = 10;

/// Bumped when the plan/ranking behaviour changes; part of the reproducibility
/// tuple returned in every [`SearchResponse`].
pub const QUERY_PLAN_VERSION: &str = "stage0-plan-v2";
pub const RANKING_POLICY_VERSION: &str = "stage0-lexical-v1";
pub const GRAPH_VERSION: &str = "stage0-graph-v0";

/// One candidate document returned by a [`Searcher`].
///
/// This deliberately mirrors the stored fields of a [`SearchResult`] without
/// pulling the real indexer (or Tantivy) into the API's dependency graph: the
/// indexer adapter hydrates one of these per hit and `execute` finishes the job
/// (filter/dedup/sort/snippet/signals/response assembly).
///
/// `doc_type`/`language` are optional extras used only for API-side filter
/// enforcement. A [`Searcher`] that can already push those filters into its own
/// index may leave them `None`; `execute` then defers to the searcher for those
/// two facets (it never drops a hit whose facet is unknown). They are not part
/// of the wire `SearchResult`.
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
    /// Filter facet; `None` = unknown (searcher is expected to have pushed the
    /// `doc_type` filter down instead).
    pub doc_type: Option<String>,
    /// Filter facet; `None` = unknown (see `doc_type`).
    pub language: Option<String>,
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
    /// Return up to `limit` candidates for `q`.
    ///
    /// `filters` are the effective [`SearchFilters`] for this request (request
    /// filters merged with any extracted from the query text). An implementation
    /// SHOULD push down what its index supports and MAY ignore the rest —
    /// `execute` re-applies the facets it can verify on the returned hits.
    fn search(
        &self,
        q: &str,
        limit: usize,
        filters: Option<&SearchFilters>,
    ) -> AppResult<Vec<IndexHit>>;

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

    /// Degradations the searcher knows about for the current index state, e.g.
    /// a lane being unavailable. Empty by default (used verbatim by `execute`).
    fn degradations(&self) -> Vec<String> {
        Vec::new()
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
/// hydrate/filter/sort/trim them, and assemble the frozen response.
pub fn execute(searcher: &dyn Searcher, req: &SearchRequest) -> AppResult<SearchResponse> {
    let started = Instant::now();

    let query = req.query.trim();
    if query.is_empty() {
        return Err(AppError::Invalid("query must not be empty".into()));
    }
    let limit = normalize_limit(req.limit)?;

    // Analyze the text and fold `site:`/`after:`/`before:` operators into
    // filters; explicit request filters win over the derived ones.
    let (analysis, derived) = analyze_with_filters(query);
    let filters = merge_filters(req.filters.as_ref(), &derived);

    // Ask the indexer for enough candidates to satisfy offset + page.
    let want = limit.saturating_add(req.offset);
    let mut hits = searcher.search(query, want, filters.as_ref())?;

    // Authoritative facet enforcement for what the API can verify. Facets the
    // searcher already pushed down (and left unknown here) are not re-checked.
    if let Some(f) = &filters {
        hits.retain(|h| passes_filters(h, f));
    }

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

    // Deadline: if the whole procedure ran past the caller's budget, report it
    // as a gateway timeout rather than a truncated-but-silent page.
    if let Some(deadline_ms) = req.deadline_ms {
        if started.elapsed().as_millis() as u64 >= deadline_ms {
            return Err(AppError::Deadline);
        }
    }

    let manifest = searcher.manifest()?;

    // Degradations: searcher-reported ones plus "no published manifest", which
    // means the response cannot be reproduced from a published generation.
    let mut degradations = searcher.degradations();
    if manifest.generation == 0 && !degradations.iter().any(|d| d == "index_unpublished") {
        degradations.push("index_unpublished".to_string());
    }

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
        complete: degradations.is_empty(),
        degradations,
    })
}

/// Whether a hydrated hit satisfies every facet the API can verify itself.
///
/// A facet that is `None` on the hit is treated as "unknown, already handled by
/// the searcher" and does not exclude the hit; a present facet must match.
/// `published_at` with an active bound must be known (never synthesized from
/// `fetched_at`, spec §6).
fn passes_filters(h: &IndexHit, f: &SearchFilters) -> bool {
    if let (Some(want), Some(have)) = (&f.doc_type, &h.doc_type) {
        if !want.eq_ignore_ascii_case(have) {
            return false;
        }
    }
    if let (Some(want), Some(have)) = (&f.language, &h.language) {
        if !want.eq_ignore_ascii_case(have) {
            return false;
        }
    }
    if let Some(want) = &f.source_id {
        match &h.source_id {
            Some(have) if have == want => {}
            _ => return false,
        }
    }
    if f.after.is_some() || f.before.is_some() {
        match h.published_at {
            Some(p) => {
                if let Some(after) = f.after {
                    if p < after {
                        return false;
                    }
                }
                if let Some(before) = f.before {
                    if p > before {
                        return false;
                    }
                }
            }
            // A bounded temporal filter cannot be satisfied by an unknown date.
            None => return false,
        }
    }
    true
}

/// Merge explicit request filters (they win) with filters extracted from the
/// query text. Returns `None` when neither side constrains anything.
fn merge_filters(req: Option<&SearchFilters>, derived: &SearchFilters) -> Option<SearchFilters> {
    let derived_empty = derived.doc_type.is_none()
        && derived.source_id.is_none()
        && derived.language.is_none()
        && derived.after.is_none()
        && derived.before.is_none();
    if req.is_none() && derived_empty {
        return None;
    }
    let r = req.cloned().unwrap_or_default();
    Some(SearchFilters {
        doc_type: r.doc_type.or_else(|| derived.doc_type.clone()),
        source_id: r.source_id.or_else(|| derived.source_id.clone()),
        language: r.language.or_else(|| derived.language.clone()),
        after: r.after.or(derived.after),
        before: r.before.or(derived.before),
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
pub fn analyze(query: &str) -> QueryAnalysis {
    analyze_with_filters(query).0
}

/// Analyze a query *and* extract the structured operator filters embedded in it
/// (`site:host`, `after:YYYY-MM-DD`, `before:YYYY-MM-DD`). Deterministic and
/// dependency-free; no model is used.
pub fn analyze_with_filters(query: &str) -> (QueryAnalysis, SearchFilters) {
    let normalized_text = query.split_whitespace().collect::<Vec<_>>().join(" ");

    let exact_phrases = extract_quoted_phrases(&normalized_text);
    let lower = normalized_text.to_lowercase();

    // --- structured operators + negation -------------------------------
    let mut derived = SearchFilters::default();
    let mut negated: Vec<String> = Vec::new();
    let mut operator_tokens: Vec<String> = Vec::new();

    for raw in normalized_text.split_whitespace() {
        let bare = raw.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
        let lower_tok = bare.to_lowercase();
        if let Some(rest) = lower_tok.strip_prefix("site:") {
            let host = rest.trim_matches(|c: char| c == '"' || c == '/');
            if !host.is_empty() {
                derived.source_id = Some(host.to_string());
                operator_tokens.push(bare.to_lowercase());
            }
            continue;
        }
        if let Some(rest) = lower_tok.strip_prefix("after:") {
            if let Some(ts) = parse_date(rest) {
                derived.after = Some(ts);
                operator_tokens.push(bare.to_lowercase());
            }
            continue;
        }
        if let Some(rest) = lower_tok.strip_prefix("before:") {
            if let Some(ts) = parse_date(rest) {
                derived.before = Some(ts);
                operator_tokens.push(bare.to_lowercase());
            }
            continue;
        }
        if let Some(rest) = bare.strip_prefix('-') {
            let term = rest.to_lowercase();
            let clean: String = term
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !clean.is_empty() && clean == term {
                negated.push(clean.clone());
                operator_tokens.push(format!("-{clean}"));
            }
        }
    }

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
        .filter(|t| !negated.contains(t))
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

    // Explicit after:/before: win; otherwise fall back to relative expressions.
    let time_interval = if derived.after.is_some() || derived.before.is_some() {
        let now = now_unix_s();
        Some(TimeInterval {
            start: derived.after.unwrap_or(0),
            end: derived.before.unwrap_or(now),
        })
    } else {
        detect_time_interval(&normalized_text)
    };

    let preferred_types = detect_preferred_types(&lower);

    let soft: &[&str] = &[
        "best", "fastest", "cheap", "cheapest", "latest", "recent", "free",
    ];
    let soft_preferences: Vec<String> = soft
        .iter()
        .filter(|&&w| lower.split_whitespace().any(|t| t == w))
        .map(|w| w.to_string())
        .collect();

    // Hard predicates: explicit negations, recorded so the UI can show them.
    let hard_predicates: Vec<String> = negated.iter().map(|n| format!("not:{n}")).collect();

    // Complexity 0..=4: length, phrases, entities, negation, and connectors.
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
    if !hard_predicates.is_empty() {
        complexity += 1;
    }
    if lower.contains(" vs ") || lower.contains(" versus ") || lower.contains(" compare ") {
        complexity = complexity.max(3);
    }
    complexity = complexity.min(4);

    let analysis = QueryAnalysis {
        original: query.to_string(),
        normalized_text,
        language: None,
        exact_phrases,
        entities,
        entity_hypotheses: Vec::new(),
        concepts,
        hard_predicates,
        soft_preferences,
        time_interval,
        preferred_types,
        intent,
        complexity,
    };
    (analysis, derived)
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
        || lower.contains(" site:")
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

fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM:SS` into Unix
/// seconds (UTC, midnight for date-only forms). Returns `None` if unparseable.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let date = s.split('T').next().unwrap_or(s);
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = match it.next() {
        Some(x) => x.parse().ok()?,
        None => 1,
    };
    let d: i64 = match it.next() {
        Some(x) => x.parse().ok()?,
        None => 1,
    };
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil` algorithm). Deterministic, no dependencies.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Detect simple relative time expressions relative to now. Stage 0 subset:
/// `today`, `yesterday`, and `last N (day|week|month|year)s`.
fn detect_time_interval(text: &str) -> Option<TimeInterval> {
    let lower = text.to_lowercase();
    let now = now_unix_s();
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
    use std::sync::Mutex;

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

    #[test]
    fn extracts_operator_filters_and_negation() {
        let (a, f) = analyze_with_filters("rust async site:doc.rust-lang.org after:2024-01-01 -python");
        assert_eq!(f.source_id.as_deref(), Some("doc.rust-lang.org"));
        assert_eq!(f.after, parse_date("2024-01-01"));
        assert!(a.hard_predicates.contains(&"not:python".to_string()));
        assert!(!a.concepts.contains(&"python".to_string()), "negated term is not a concept");
        assert!(a.time_interval.is_some());
    }

    #[test]
    fn parses_dates_deterministically() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1970-01-02"), Some(86_400));
        assert_eq!(parse_date("2024-01-01"), Some(1_704_067_200));
        assert_eq!(parse_date("2024"), Some(days_from_civil(2024, 1, 1) * 86_400));
        assert_eq!(parse_date("not-a-date"), None);
        assert_eq!(parse_date("2024-13-01"), None);
    }

    // ---- execute() behaviour with a stub Searcher ------------------------

    #[derive(Default)]
    struct Stub {
        hits: Vec<IndexHit>,
        manifest: Option<IndexManifest>,
        seen_filter_source: Mutex<Option<String>>,
    }

    impl Searcher for Stub {
        fn search(
            &self,
            _q: &str,
            limit: usize,
            filters: Option<&SearchFilters>,
        ) -> AppResult<Vec<IndexHit>> {
            *self.seen_filter_source.lock().unwrap() =
                filters.and_then(|f| f.source_id.clone());
            Ok(self.hits.iter().take(limit).cloned().collect())
        }
        fn manifest(&self) -> AppResult<IndexManifest> {
            Ok(self.manifest.clone().unwrap_or_else(|| IndexManifest {
                manifest_id: "m-1".into(),
                generation: 1,
                doc_count: 0,
                base_segments: 0,
                watermark: 5,
                embedding_model_ids: vec![],
                metadata_version: 1,
                created_at: 0,
                published_at: None,
            }))
        }
    }

    fn hit(id: &str, score: f32, source: &str, published: i64) -> IndexHit {
        IndexHit {
            doc_id: id.into(),
            version: 1,
            resource_id: format!("res-{id}"),
            work_id: None,
            content_id: None,
            url: format!("https://example.test/{id}"),
            title: format!("title {id}"),
            snippet: format!("snippet {id}"),
            source_id: Some(source.into()),
            doc_type: Some("html".into()),
            language: Some("en".into()),
            published_at: Some(published),
            modified_at: None,
            fetched_at: published,
            lane: "lexical_base".into(),
            provenance: Provenance::Snapshot,
            score,
            signals: vec![],
        }
    }

    fn req(query: &str, limit: usize, filters: Option<SearchFilters>) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            limit,
            offset: 0,
            timezone: "UTC".into(),
            deadline_ms: None,
            filters,
        }
    }

    #[test]
    fn execute_orders_by_score_then_doc_id() {
        let stub = Stub {
            hits: vec![hit("b", 1.0, "s1", 100), hit("a", 1.0, "s1", 100), hit("c", 9.0, "s1", 100)],
            ..Default::default()
        };
        let resp = execute(&stub, &req("rust", 10, None)).unwrap();
        let ids: Vec<&str> = resp.results.iter().map(|r| r.doc_id.as_str()).collect();
        // c has the top score; a and b tie on score so doc_id orders them.
        assert_eq!(ids, vec!["c", "a", "b"]);
    }

    #[test]
    fn execute_applies_filters() {
        let stub = Stub {
            hits: vec![
                hit("a", 3.0, "news", 1_000),
                hit("b", 2.0, "blog", 2_000),
                hit("c", 1.0, "news", 3_000),
            ],
            ..Default::default()
        };
        let filters = SearchFilters {
            source_id: Some("news".into()),
            after: Some(1_500),
            ..Default::default()
        };
        let resp = execute(&stub, &req("rust", 10, Some(filters.clone()))).unwrap();
        let ids: Vec<&str> = resp.results.iter().map(|r| r.doc_id.as_str()).collect();
        // "b" is the wrong source, "a" is before the lower bound.
        assert_eq!(ids, vec!["c"]);
        // The effective filters reached the searcher.
        assert_eq!(stub.seen_filter_source.lock().unwrap().as_deref(), Some("news"));

        // doc_type/language facets are enforced when the hit is known.
        let f2 = SearchFilters { doc_type: Some("pdf".into()), ..Default::default() };
        assert!(execute(&stub, &req("rust", 10, Some(f2))).unwrap().results.is_empty());
    }

    #[test]
    fn execute_snapshot_id_is_stable_for_identical_requests() {
        let stub = Stub {
            hits: vec![hit("a", 1.0, "s1", 100)],
            ..Default::default()
        };
        let a = execute(&stub, &req("rust", 10, None)).unwrap();
        let b = execute(&stub, &req("rust", 10, None)).unwrap();
        assert_eq!(a.snapshot_id, b.snapshot_id);
        assert!(!a.snapshot_id.is_empty());
        assert!(a.complete, "no degradations for a published manifest");
        assert!(a.degradations.is_empty());
    }

    #[test]
    fn execute_reports_unpublished_index_as_degradation() {
        let stub = Stub {
            hits: vec![],
            manifest: Some(IndexManifest {
                manifest_id: "unpublished".into(),
                generation: 0,
                doc_count: 0,
                base_segments: 0,
                watermark: 0,
                embedding_model_ids: vec![],
                metadata_version: 0,
                created_at: 0,
                published_at: None,
            }),
            ..Default::default()
        };
        let resp = execute(&stub, &req("rust", 10, None)).unwrap();
        assert!(!resp.complete);
        assert_eq!(resp.degradations, vec!["index_unpublished".to_string()]);
    }
}
