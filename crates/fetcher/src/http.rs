//! HTTP fetching with streaming limits (spec §8). OWNER: fetcher agent.

use search_core::{AppError, AppResult, Limits};

pub struct Fetcher;

/// The result of one fetch attempt. A fetch does NOT imply a new
/// DocumentVersion: `304` carries no new body.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    pub final_url: String,
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<String>,
}

impl Fetcher {
    pub fn new(_limits: Limits, _user_agent: String) -> Self {
        Self
    }

    pub async fn fetch(
        &self,
        _url: &str,
        _etag: Option<&str>,
        _last_modified: Option<&str>,
    ) -> AppResult<FetchOutcome> {
        Err(AppError::Internal("fetcher::fetch not yet implemented".into()))
    }
}
