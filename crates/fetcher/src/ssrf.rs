//! SSRF guard (spec §8). OWNER: fetcher agent.
//!
//! Every URL and every redirect target is validated. Resolve DNS and re-check
//! the resolved IP, not just the literal host.

use search_core::{AppError, AppResult};

/// Returns Ok(()) if the URL is safe to fetch, Err otherwise.
pub fn validate_url(_raw: &str) -> AppResult<()> {
    Err(AppError::Internal("ssrf::validate_url not yet implemented".into()))
}

/// Validate a literal/resolved IP address against the block list.
pub fn is_blocked_ip(_ip: std::net::IpAddr) -> bool {
    true
}
