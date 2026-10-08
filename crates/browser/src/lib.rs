//! Browser escalation (plan Stage 1, crawler §21–22).
//!
//! A browser fetcher renders JS-heavy pages and reports the XHR/fetch endpoints it saw so
//! future fetches can hit the JSON directly. Default build ships a hermetic mock; the real
//! Chromium driver sits behind `feature = "chromium"`. Interfaces frozen in
//! `docs/STAGE1_SPEC.md`.

#![forbid(unsafe_code)]
