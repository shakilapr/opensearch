//! Sitemap and RSS/Atom feed discovery (plan Stage 1, crawler §7–8).
//!
//! Turns sitemaps, sitemap indexes, and feeds into discovered-resource records so the
//! crawler learns about URLs without fetching every page. Pure parsing: no I/O.
//!
//! Interfaces are frozen in `docs/STAGE1_SPEC.md`. Module layout is up to the owner.

#![forbid(unsafe_code)]
