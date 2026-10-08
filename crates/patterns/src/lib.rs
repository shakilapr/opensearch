//! URL pattern learning (plan Stage 5): templatization + pagination recognition.
//! Interfaces frozen in `docs/STAGE5_SPEC.md`.
//!
//! [`templatize`] normalizes a URL by replacing variable path/query runs — digit runs, UUIDs and
//! date-like runs — with stable tokens, while leaving the scheme and host untouched. Two URLs
//! that differ only in such a run (e.g. `/page/3` and `/page/4`) collapse to one template.
//! [`PatternLearner`] counts those templates and recognizes high-support pagination patterns.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Token substituted for a run of digits.
pub const TOKEN_NUMBER: &str = "{n}";
/// Token substituted for a UUID.
pub const TOKEN_UUID: &str = "{uuid}";
/// Token substituted for a date-like run (`YYYY-MM-DD`, `YYYY/MM/DD`, `YYYY-MM`).
pub const TOKEN_DATE: &str = "{date}";
/// Minimum support before a page-number template counts as paginated.
pub const PAGE_MIN_SUPPORT: u64 = 3;
/// Path segments that, immediately before a `{n}` segment, mark a page-number template.
const PAGE_SEGMENTS: &[&str] = &["page", "pages", "p", "pg", "pagination"];

/// Replace digit / UUID / date-like runs in a URL with tokens, preserving scheme + host and the
/// path shape. `/page/3` and `/page/4` both become `/page/{n}`.
pub fn templatize(url: &str) -> String {
    // Keep the authority verbatim so a host or port with digits is never templated.
    match url.find("://") {
        Some(pos) => {
            let after_scheme = pos + 3;
            let authority_end = url[after_scheme..]
                .find(['/', '?', '#'])
                .map(|p| after_scheme + p)
                .unwrap_or(url.len());
            let (authority, rest) = url.split_at(authority_end);
            let mut out = String::with_capacity(url.len() + 8);
            out.push_str(authority);
            out.push_str(&templatize_rest(rest));
            out
        }
        None => templatize_rest(url),
    }
}

/// Templatize the path/query/fragment portion of a URL.
fn templatize_rest(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = match_uuid(bytes, i) {
            out.extend_from_slice(TOKEN_UUID.as_bytes());
            i += len;
            continue;
        }
        if let Some(len) = match_date(bytes, i) {
            out.extend_from_slice(TOKEN_DATE.as_bytes());
            i += len;
            continue;
        }
        if bytes[i].is_ascii_digit() {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            out.extend_from_slice(TOKEN_NUMBER.as_bytes());
            continue;
        }
        // Copy one byte verbatim (preserves UTF-8 continuation bytes).
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("templatize preserves valid UTF-8")
}

/// Match a canonical lowercase/uppercase UUID (`8-4-4-4-12` hex). Returns its byte length.
fn match_uuid(bytes: &[u8], start: usize) -> Option<usize> {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
    if start + 36 > bytes.len() {
        return None;
    }
    let mut p = start;
    for (gi, &g) in GROUPS.iter().enumerate() {
        for _ in 0..g {
            if !bytes[p].is_ascii_hexdigit() {
                return None;
            }
            p += 1;
        }
        if gi < GROUPS.len() - 1 {
            if bytes[p] != b'-' {
                return None;
            }
            p += 1;
        }
    }
    Some(36)
}

/// Match a date-like run: exactly four digits, a `-`/`/` separator, 1-2 digit month, and an
/// optional second separator plus 1-2 digit day. Returns its byte length.
fn match_date(bytes: &[u8], start: usize) -> Option<usize> {
    let mut p = start;
    for _ in 0..4 {
        if p >= bytes.len() || !bytes[p].is_ascii_digit() {
            return None;
        }
        p += 1;
    }
    if p >= bytes.len() {
        return None;
    }
    let sep = bytes[p];
    if sep != b'-' && sep != b'/' {
        return None;
    }
    p += 1;
    let month_start = p;
    while p < bytes.len() && p - month_start < 2 && bytes[p].is_ascii_digit() {
        p += 1;
    }
    if p == month_start {
        return None;
    }
    if p < bytes.len() && bytes[p] == sep {
        let day_sep = p;
        p += 1;
        let day_start = p;
        while p < bytes.len() && p - day_start < 2 && bytes[p].is_ascii_digit() {
            p += 1;
        }
        if p == day_start {
            // `YYYY-MM` but the trailing separator is not followed by a day: keep only the
            // year-month run, backing up over the separator.
            return Some(day_sep - start);
        }
    }
    Some(p - start)
}

/// A learned URL template and its observation count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UrlPattern {
    pub template: String,
    pub support: u64,
}

/// Counts observed URL templates and recognizes pagination.
#[derive(Debug, Clone, Default)]
pub struct PatternLearner {
    counts: BTreeMap<String, u64>,
}

impl PatternLearner {
    /// A fresh learner with no observations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a URL: templatize it and increment that template's support.
    pub fn observe(&mut self, url: &str) {
        *self.counts.entry(templatize(url)).or_insert(0) += 1;
    }

    /// Learned templates, highest support first; ties broken by template text (deterministic).
    pub fn patterns(&self) -> Vec<UrlPattern> {
        let mut patterns: Vec<UrlPattern> = self
            .counts
            .iter()
            .map(|(template, &support)| UrlPattern {
                template: template.clone(),
                support,
            })
            .collect();
        patterns.sort_by(|a, b| {
            b.support
                .cmp(&a.support)
                .then_with(|| a.template.cmp(&b.template))
        });
        patterns
    }

    /// Whether `url` matches a high-support page-number template. The URL's template must have at
    /// least [`PAGE_MIN_SUPPORT`] observations and be a page-number shape (a `{n}` segment
    /// preceded by a page segment such as `/page/`).
    pub fn is_paginated(&self, url: &str) -> bool {
        let template = templatize(url);
        let support = self.counts.get(&template).copied().unwrap_or(0);
        support >= PAGE_MIN_SUPPORT && is_page_template(&template)
    }

    /// Support observed for a URL's template.
    pub fn support(&self, url: &str) -> u64 {
        self.counts.get(&templatize(url)).copied().unwrap_or(0)
    }
}

/// A template is a page template when a `{n}` path segment is immediately preceded by a page
/// segment (`page`, `p`, `pages`, ...), or a `page={n}` / `page={n}&...` query is present.
fn is_page_template(template: &str) -> bool {
    if template.contains("page={n}") {
        return true;
    }
    let segments: Vec<&str> = template.split('/').collect();
    for pair in segments.windows(2) {
        if pair[1] == TOKEN_NUMBER {
            let prev = pair[0].split(['?', '#', '&']).next().unwrap_or(pair[0]);
            if PAGE_SEGMENTS.iter().any(|s| prev.eq_ignore_ascii_case(s)) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templatize_collapses_paginated_urls() {
        assert_eq!(
            templatize("https://example.com/page/3"),
            "https://example.com/page/{n}"
        );
        assert_eq!(
            templatize("https://example.com/page/3"),
            templatize("https://example.com/page/4"),
            "/page/3 and /page/4 must share one template"
        );
    }

    #[test]
    fn templatize_preserves_host_and_path_shape() {
        let t = templatize("https://api.example.com:8443/v2/items/42?q=7");
        assert!(t.starts_with("https://api.example.com:8443/"), "host+port preserved: {t}");
        // Digit runs in the path and query are tokenized; the host (and its port) are not.
        assert_eq!(t, "https://api.example.com:8443/v{n}/items/{n}?q={n}");
    }

    #[test]
    fn templatize_replaces_uuid_and_date_runs() {
        let url = "https://example.com/post/550e8400-e29b-41d4-a716-446655440000/2024-01-02";
        assert_eq!(
            templatize(url),
            "https://example.com/post/{uuid}/{date}"
        );
        assert_eq!(
            templatize("https://example.com/archive/2024/12/31.html"),
            "https://example.com/archive/{date}.html"
        );
    }

    #[test]
    fn learner_ranks_patterns_by_support() {
        let mut l = PatternLearner::new();
        for u in [
            "https://example.com/page/1",
            "https://example.com/page/2",
            "https://example.com/page/3",
            "https://example.com/about",
        ] {
            l.observe(u);
        }
        let patterns = l.patterns();
        assert_eq!(patterns[0].template, "https://example.com/page/{n}");
        assert_eq!(patterns[0].support, 3);
        assert_eq!(patterns[1].template, "https://example.com/about");
        assert_eq!(patterns[1].support, 1);
    }

    #[test]
    fn is_paginated_true_for_high_support_page_template() {
        let mut l = PatternLearner::new();
        for n in 1..=5 {
            l.observe(&format!("https://example.com/page/{n}"));
        }
        assert!(l.is_paginated("https://example.com/page/42"));
    }

    #[test]
    fn is_paginated_false_for_low_support_or_non_page() {
        let mut l = PatternLearner::new();
        l.observe("https://example.com/page/1"); // support 1 < PAGE_MIN_SUPPORT
        assert!(!l.is_paginated("https://example.com/page/2"));

        // A numeric template that is not a page pattern never counts as paginated.
        for n in 1..=5 {
            l.observe(&format!("https://example.com/article/{n}"));
        }
        assert!(!l.is_paginated("https://example.com/article/3"));
    }
}
