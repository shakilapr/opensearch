//! Source registry (plan Stage 1, crawler §3).
//!
//! Captures what we know about a source: topics, entities, URL patterns, authority, observed
//! change rate, JS probability, and fetch statistics. Pure types + logic; persistence lives
//! in `search-metadata`. Interfaces frozen in `docs/STAGE1_SPEC.md`.
//!
//! ## Interface note: `observe_fetch` signature
//!
//! The Stage-1 spec sketches `observe_fetch(&mut self, ok: bool)`. A change rate cannot be a
//! "smoothed ratio of changed observations" without observing *whether* a resource changed, so
//! the signature was widened to `observe_fetch(&mut self, ok: bool, changed: bool)`. Semantics:
//! every call counts as a fetch; a `!ok` call increments `error_count` and does **not** update
//! `change_rate` (a failed fetch tells us nothing about change). A successful call folds
//! `changed` into the exponential moving average `change_rate`.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Smoothing factor for the change-rate exponential moving average.
const CHANGE_RATE_ALPHA: f32 = 0.2;
/// Smoothing factor for the JS-probability exponential moving average.
const JS_PROB_ALPHA: f32 = 0.2;
/// Pseudo-count weight of the authority prior (higher = trust the seed more).
const AUTHORITY_PRIOR_WEIGHT: f32 = 4.0;
/// Laplace epsilon keeping the authority prior strictly inside `(0, 1)`.
const AUTHORITY_PRIOR_EPSILON: f32 = 0.5;

/// What we know about one source (site / logical collection).
///
/// Fields are exactly the frozen Stage-1 surface and the `source_profile` table columns in
/// `0003_stage1.sql`. `authority`/`change_rate`/`js_probability` are the *current estimates*
/// (seeds initially, updated from observations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceProfile {
    pub source_id: String,
    pub domain: String,
    pub topics: Vec<String>,
    pub entities: Vec<String>,
    pub url_patterns: Vec<String>,
    pub authority: f32,
    pub change_rate: f32,
    pub js_probability: f32,
    pub fetch_count: u64,
    pub error_count: u64,
}

impl SourceProfile {
    /// New profile with sensible defaults: authority seeded from the domain hint (TLD / known
    /// domain), everything else neutral (change_rate 0.0, js_probability 0.0, counts 0).
    pub fn new(source_id: impl Into<String>, domain: impl Into<String>) -> Self {
        let domain = domain.into();
        let authority = authority_seed(&domain);
        Self {
            source_id: source_id.into(),
            domain,
            topics: Vec::new(),
            entities: Vec::new(),
            url_patterns: Vec::new(),
            authority,
            change_rate: 0.0,
            js_probability: 0.0,
            fetch_count: 0,
            error_count: 0,
        }
    }

    /// Register/derive a profile from a bare domain string. Normalizes the domain to lowercase
    /// (trimmed), uses it as both `domain` and `source_id`, and seeds `authority` from the
    /// TLD / known-domain hint.
    pub fn from_domain(domain: impl Into<String>) -> Self {
        let domain = domain.into().trim().to_ascii_lowercase();
        let mut profile = Self::new(domain.clone(), domain);
        profile.source_id = profile.domain.clone();
        profile
    }

    /// Record one fetch observation.
    ///
    /// * every call increments `fetch_count`;
    /// * `!ok` increments `error_count` and leaves `change_rate` untouched;
    /// * `ok` folds `changed` into the change-rate EMA (first success sets it directly).
    pub fn observe_fetch(&mut self, ok: bool, changed: bool) {
        // Successful observations seen *before* this call.
        let prior_successes = self.fetch_count.saturating_sub(self.error_count);
        self.fetch_count = self.fetch_count.saturating_add(1);

        if !ok {
            self.error_count = self.error_count.saturating_add(1);
            return;
        }

        let x = if changed { 1.0 } else { 0.0 };
        if prior_successes == 0 {
            // Warm-up: the first successful observation defines the rate.
            self.change_rate = x;
        } else {
            self.change_rate += CHANGE_RATE_ALPHA * (x - self.change_rate);
        }
        self.change_rate = self.change_rate.clamp(0.0, 1.0);
    }

    /// Record whether a fetch required JS rendering, folding it into `js_probability` (EMA).
    /// Call it right after the corresponding [`observe_fetch`](Self::observe_fetch); the first
    /// observation (while `fetch_count <= 1`) sets the value directly, later ones smooth.
    /// Optional helper; not part of the frozen surface but useful for the SPA/stub detector.
    pub fn observe_js(&mut self, required_js: bool) {
        let x = if required_js { 1.0 } else { 0.0 };
        if self.fetch_count <= 1 {
            self.js_probability = x;
        } else {
            self.js_probability += JS_PROB_ALPHA * (x - self.js_probability);
        }
        self.js_probability = self.js_probability.clamp(0.0, 1.0);
    }

    /// True when the URL matches the first applicable pattern.
    ///
    /// Patterns support `*` (any sequence, including empty) and `?` (single char) as an
    /// anchored glob; a pattern with no wildcard is treated as a bare prefix. Patterns are
    /// evaluated in order and the first match wins.
    pub fn matches_url(&self, url: &str) -> bool {
        self.url_patterns
            .iter()
            .any(|pattern| pattern_matches(pattern, url))
    }

    /// Smoothed authority estimate, strictly inside `(0, 1)`.
    ///
    /// Beta-style posterior blending the seed `authority` (weight [`AUTHORITY_PRIOR_WEIGHT`])
    /// with observed successful/errored fetches, plus a Laplace epsilon so the result is never
    /// exactly 0 or 1. Monotonically increasing in `authority` for fixed fetch statistics.
    pub fn authority_prior(&self) -> f32 {
        let successes = self.fetch_count.saturating_sub(self.error_count) as f32;
        let errors = self.error_count as f32;
        let a = self.authority.clamp(0.0, 1.0) * AUTHORITY_PRIOR_WEIGHT + successes
            + AUTHORITY_PRIOR_EPSILON;
        let b = (1.0 - self.authority.clamp(0.0, 1.0)) * AUTHORITY_PRIOR_WEIGHT + errors
            + AUTHORITY_PRIOR_EPSILON;
        (a / (a + b)).clamp(f32::EPSILON, 1.0 - f32::EPSILON)
    }
}

impl Default for SourceProfile {
    fn default() -> Self {
        Self::new(String::new(), String::new())
    }
}

/// Match a single pattern against a URL. Wildcards → anchored glob; otherwise prefix.
fn pattern_matches(pattern: &str, url: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        glob_match(pattern, url)
    } else {
        !pattern.is_empty() && url.starts_with(pattern)
    }
}

/// Anchored glob match supporting `*` (any run) and `?` (one char), the classic
/// two-pointer-with-backtracking algorithm (linear on typical inputs).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_t = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '*' || p[pi] == '?' || p[pi] == t[ti]) {
            if p[pi] == '*' {
                star = Some(pi);
                star_t = ti;
                pi += 1;
                continue;
            }
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Trivial authority seed from the TLD / a few well-known high-authority domains.
fn authority_seed(domain: &str) -> f32 {
    let d = domain.trim().to_ascii_lowercase();
    // Strip a leading scheme/host noise if someone passed a URL-ish string.
    let host = d
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();

    // Explicit known-domain hints win over TLD rules.
    for known in [
        "arxiv.org",
        "wikipedia.org",
        "pubmed.ncbi.nlm.nih.gov",
        "ncbi.nlm.nih.gov",
        "nature.com",
        "science.org",
        "acm.org",
        "ieee.org",
    ] {
        if host == known || host.ends_with(&format!(".{known}")) {
            return 0.85;
        }
    }

    let tld = host.rsplit('.').next().unwrap_or("");
    match tld {
        "gov" | "edu" => 0.75,
        "org" => 0.6,
        _ => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_sensible_defaults() {
        let p = SourceProfile::new("src_1", "example.com");
        assert_eq!(p.source_id, "src_1");
        assert_eq!(p.domain, "example.com");
        assert!(p.topics.is_empty() && p.entities.is_empty() && p.url_patterns.is_empty());
        assert_eq!(p.change_rate, 0.0);
        assert_eq!(p.js_probability, 0.0);
        assert_eq!(p.fetch_count, 0);
        assert_eq!(p.error_count, 0);
        assert_eq!(p.authority, 0.5); // default TLD seed
    }

    #[test]
    fn from_domain_normalizes_and_seeds_authority() {
        let p = SourceProfile::from_domain("  ArXiv.ORG ");
        assert_eq!(p.domain, "arxiv.org");
        assert_eq!(p.source_id, "arxiv.org");
        assert_eq!(p.authority, 0.85);

        assert_eq!(SourceProfile::from_domain("nasa.gov").authority, 0.75);
        assert_eq!(SourceProfile::from_domain("mit.edu").authority, 0.75);
        assert_eq!(SourceProfile::from_domain("example.com").authority, 0.5);
    }

    #[test]
    fn observe_fetch_stats_and_change_rate() {
        let mut p = SourceProfile::new("s", "example.com");

        // First successful, changed → rate jumps to 1.0.
        p.observe_fetch(true, true);
        assert_eq!(p.fetch_count, 1);
        assert_eq!(p.error_count, 0);
        assert_eq!(p.change_rate, 1.0);

        // Then unchanged → EMA pulls down by alpha 0.2.
        p.observe_fetch(true, false);
        assert_eq!(p.fetch_count, 2);
        assert_eq!(p.error_count, 0);
        assert!((p.change_rate - 0.8).abs() < 1e-6);

        // Then changed → EMA back up.
        p.observe_fetch(true, true);
        assert!((p.change_rate - 0.84).abs() < 1e-6);

        // A failed fetch counts + errors but must NOT move change_rate.
        let before = p.change_rate;
        p.observe_fetch(false, true);
        assert_eq!(p.fetch_count, 4);
        assert_eq!(p.error_count, 1);
        assert_eq!(p.change_rate, before);
    }

    #[test]
    fn first_success_after_failures_sets_rate() {
        let mut p = SourceProfile::new("s", "example.com");
        p.observe_fetch(false, false);
        assert_eq!(p.change_rate, 0.0);
        p.observe_fetch(true, true); // first *successful* observation
        assert_eq!(p.change_rate, 1.0);
    }

    #[test]
    fn observe_js_is_smoothed() {
        let mut p = SourceProfile::new("s", "example.com");
        p.observe_fetch(true, false); // first fetch (count = 1)
        p.observe_js(true); // warm-up → set directly
        assert_eq!(p.js_probability, 1.0);
        p.observe_fetch(true, false); // count = 2
        p.observe_js(false); // EMA down by alpha 0.2
        assert!((p.js_probability - 0.8).abs() < 1e-6);
    }

    #[test]
    fn matches_url_glob_prefix_and_first_match() {
        let mut p = SourceProfile::new("s", "example.com");
        p.url_patterns = vec![
            "https://example.com/docs/*".into(),
            "https://blog.example.com".into(),
        ];
        assert!(p.matches_url("https://example.com/docs/guide/intro"));
        assert!(p.matches_url("https://blog.example.com/post/1")); // bare prefix
        assert!(!p.matches_url("https://example.com/news"));
        assert!(!p.matches_url("https://other.com/docs/x"));

        // `*` in the middle.
        let mut q = SourceProfile::new("s", "example.com");
        q.url_patterns = vec!["*/blog/*".into()];
        assert!(q.matches_url("https://example.com/blog/2026/x"));
        assert!(!q.matches_url("https://example.com/news/x"));

        // First-matching pattern: the leading pattern decides.
        let mut r = SourceProfile::new("s", "example.com");
        r.url_patterns = vec!["https://example.com/".into(), "https://example.com/news".into()];
        assert!(r.matches_url("https://example.com/news")); // matches via first (prefix)

        // No patterns → never matches.
        let empty = SourceProfile::new("s", "example.com");
        assert!(!empty.matches_url("https://example.com/"));
    }

    #[test]
    fn authority_prior_bounds_and_monotonicity() {
        let mut low = SourceProfile::new("l", "example.com");
        low.authority = 0.1;
        let mut mid = SourceProfile::new("m", "example.com");
        mid.authority = 0.5;
        let mut high = SourceProfile::new("h", "example.com");
        high.authority = 0.9;

        // Strictly inside (0, 1).
        for a in [0.0f32, 0.1, 0.5, 0.9, 1.0] {
            let mut p = SourceProfile::new("x", "example.com");
            p.authority = a;
            let prior = p.authority_prior();
            assert!(prior > 0.0 && prior < 1.0, "prior {prior} out of (0,1) for a={a}");
        }

        // Monotonic in authority.
        assert!(low.authority_prior() < mid.authority_prior());
        assert!(mid.authority_prior() < high.authority_prior());

        // A spotless success record raises the prior; a failed one lowers it.
        let baseline = mid.authority_prior();
        for _ in 0..10 {
            mid.observe_fetch(true, false);
        }
        assert!(mid.authority_prior() > baseline);

        let mut flaky = SourceProfile::new("f", "example.com");
        flaky.authority = 0.5;
        for _ in 0..10 {
            flaky.observe_fetch(false, false);
        }
        assert!(flaky.authority_prior() < baseline);
    }

    #[test]
    fn serde_round_trip() {
        let mut p = SourceProfile::new("src", "arxiv.org");
        p.topics = vec!["llm".into(), "search".into()];
        p.entities = vec!["Transformer".into()];
        p.url_patterns = vec!["https://arxiv.org/abs/*".into()];
        p.observe_fetch(true, true);
        p.observe_fetch(false, false);

        let json = serde_json::to_string(&p).unwrap();
        let back: SourceProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }
}
