//! fetcher: HTTP, URL canonicalization, SSRF guard, robots, CrawlPolicy,
//! and streaming resource limits (spec §8-§9).

pub mod canon;
pub mod crawlpolic;
pub mod http;
pub mod robots;
pub mod ssrf;
pub mod validators;

pub use crawlpolic::{content_type_allowed, CrawlPolicy};
pub use http::{read_body_capped, FetchOutcome, Fetched, Fetcher};
pub use robots::{parse, parse_capped, RobotsPolicy, RobotsRule, RobotsStatus, RobotsVerdict};
pub use validators::{MemoryValidators, ValidatorStore, Validators};
