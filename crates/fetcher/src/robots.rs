//! Robots Exclusion Protocol (RFC 9309) (spec §9). OWNER: fetcher agent.
//!
//! Failure semantics are part of the contract and are tested explicitly.

/// The outcome of evaluating robots for a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotsVerdict {
    Allowed,
    Disallowed,
}

/// Cached robots state for one origin.
#[derive(Debug, Clone)]
pub struct RobotsPolicy {
    pub domain_id: String,
    pub status: RobotsStatus,
    pub fetched_at: i64,
    pub rules: Vec<RobotsRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RobotsStatus {
    /// Fetched successfully; rules apply.
    Ok,
    /// 4xx: MAY access any resource.
    Unavailable,
    /// 5xx or network error: MUST assume complete disallow.
    Unreachable,
}

#[derive(Debug, Clone)]
pub struct RobotsRule {
    pub agent: String,
    pub allow: bool,
    pub path: String,
}
