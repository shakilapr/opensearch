//! Local crawl policy, distinct from robots (spec §9). OWNER: fetcher agent.
//!
//! CrawlPolicy is local policy and MUST NOT be folded into RobotsPolicy. It
//! carries source-specific restrictions, accepted content types, rate limits,
//! auth boundaries, redistribution/index policy and browser permissions.
//! [`CrawlPolicy::allows`] ANDs local policy with a robots verdict.

use serde::{Deserialize, Serialize};

use crate::robots::RobotsVerdict;
use search_core::{AppError, AppResult};

/// Per-source local policy. MUST NOT be folded into RobotsPolicy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlPolicy {
    pub source_id: String,
    pub max_rate: u32,
    pub allowed_types: String,
    pub auth_boundary: Option<String>,
    pub browser_allowed: bool,
    pub index_allowed: bool,
    pub redistribution: String,
}

impl CrawlPolicy {
    /// The fetch gate: local policy AND robots. Returns `Ok(())` only when the
    /// robots verdict permits access *and* the response content type is in the
    /// source's allow-list. Rate limiting / auth / redistribution are enforced
    /// elsewhere, not here.
    pub fn allows(&self, content_type: &str, robots: RobotsVerdict) -> AppResult<()> {
        if robots == RobotsVerdict::Disallowed {
            return Err(AppError::RobotsDisallowed(format!(
                "source {} is disallowed by robots.txt",
                self.source_id
            )));
        }
        if !content_type_allowed(&self.allowed_types, content_type) {
            return Err(AppError::CrawlPolicyDenied(format!(
                "content-type {content_type:?} not allowed for source {}",
                self.source_id
            )));
        }
        Ok(())
    }
}

/// Match a response `content_type` against a comma-separated allow-list.
/// Supports `*/*` (all) and `type/*` (subtype wildcard). Parameters after `;`
/// are ignored. An empty allow-list means "accept everything".
pub fn content_type_allowed(allowed: &str, content_type: &str) -> bool {
    if allowed.trim().is_empty() {
        return true;
    }
    let ct = normalize_media_type(content_type);
    allowed.split(',').any(|entry| {
        let entry = normalize_media_type(entry);
        if entry == "*/*" || entry.is_empty() {
            return true;
        }
        if let Some(prefix) = entry.strip_suffix("/*") {
            return ct.starts_with(&format!("{prefix}/"));
        }
        entry == ct
    })
}

fn normalize_media_type(s: &str) -> String {
    s.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &str) -> CrawlPolicy {
        CrawlPolicy {
            source_id: "s".into(),
            max_rate: 1,
            allowed_types: allowed.into(),
            auth_boundary: None,
            browser_allowed: false,
            index_allowed: true,
            redistribution: "metadata_only".into(),
        }
    }

    #[test]
    fn robots_disallow_denies_even_when_type_ok() {
        let p = policy("text/html");
        let err = p.allows("text/html", RobotsVerdict::Disallowed).unwrap_err();
        assert!(matches!(err, AppError::RobotsDisallowed(_)));
    }

    #[test]
    fn type_gate_denies_non_allowed() {
        let p = policy("text/html");
        assert!(p.allows("text/html; charset=utf-8", RobotsVerdict::Allowed).is_ok());
        let err = p.allows("application/pdf", RobotsVerdict::Allowed).unwrap_err();
        assert!(matches!(err, AppError::CrawlPolicyDenied(_)));
    }

    #[test]
    fn wildcard_and_empty_lists() {
        assert!(content_type_allowed("*/*", "application/pdf"));
        assert!(content_type_allowed("text/*", "text/plain"));
        assert!(!content_type_allowed("text/*", "image/png"));
        assert!(content_type_allowed("", "anything/at-all"));
    }
}
