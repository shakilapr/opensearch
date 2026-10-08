//! Local crawl policy, distinct from robots (spec §9). OWNER: fetcher agent.

use serde::{Deserialize, Serialize};

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
