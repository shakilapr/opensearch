//! fetcher: HTTP, URL canonicalization, SSRF guard, robots, CrawlPolicy,
//! and streaming resource limits (spec §8-§9).

pub mod canon;
pub mod crawlpolic;
pub mod http;
pub mod robots;
pub mod ssrf;

pub use http::{FetchOutcome, Fetcher};
pub use robots::{RobotsPolicy, RobotsVerdict};
