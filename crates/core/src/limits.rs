//! Safety / resource bounds. These are Stage-0 acceptance criteria, not
//! later hardening. Every network or parse path consults them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Max bytes accepted from one HTTP response body.
    pub max_response_bytes: u64,
    /// Max bytes after decompression (zip/gzip/brotli bomb guard).
    pub max_decompressed_bytes: u64,
    /// Max HTTP redirects followed (each hop re-validated for SSRF).
    pub max_redirects: u32,
    /// Max wall-clock milliseconds for one parse.
    pub max_parse_ms: u64,
    /// Max expanded bytes for an archive.
    pub max_archive_expansion: u64,
    /// Max nested archive depth.
    pub max_archive_depth: u32,
    /// Max pages extracted from a PDF.
    pub max_pdf_pages: u32,
    /// Max wall-clock milliseconds for one browser render.
    pub max_browser_ms: u64,
    /// Max robots.txt bytes parsed. RFC 9309 requires >= 500 KiB.
    pub max_robots_bytes: u64,
    /// Lease duration for a frontier job, in seconds.
    pub lease_seconds: u64,
    /// Global in-flight fetch cap.
    pub max_inflight_fetches: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_response_bytes: 5 * 1024 * 1024,
            max_decompressed_bytes: 25 * 1024 * 1024,
            max_redirects: 5,
            max_parse_ms: 2000,
            max_archive_expansion: 100 * 1024 * 1024,
            max_archive_depth: 3,
            max_pdf_pages: 500,
            max_browser_ms: 15_000,
            max_robots_bytes: 512 * 1024, // >= 500 KiB per RFC 9309 §2.5
            lease_seconds: 120,
            max_inflight_fetches: 32,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_robots_bytes < 500 * 1024 {
            return Err("max_robots_bytes must be >= 500 KiB (RFC 9309)".into());
        }
        if self.max_decompressed_bytes < self.max_response_bytes {
            return Err("max_decompressed_bytes must be >= max_response_bytes".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        assert!(Limits::default().validate().is_ok());
    }
}
