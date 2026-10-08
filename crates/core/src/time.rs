//! Time helpers. All internal times are Unix seconds (UTC) unless suffixed _ms.
//!
//! Never substitute crawl time for an unknown publication time: callers must
//! represent "unknown" as `None`/`-1`, not as `now()`.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
