//! Freshness scheduler (plan Stage 3, crawler §39–40, engine §11–12, engine-arch §13).
//!
//! HOT/WARM/COLD classes, the learned change model `P(changed) = 1 - exp(-rate·age)`,
//! priority tiers, a charge-before-dispatch budget ledger, and an activation scheduler with
//! bounded per-origin and total live-fetch fan-out. Pure logic; the crawl loop is wired in
//! `agent0`. Interfaces are frozen in `docs/STAGE3_SPEC.md`.

#![forbid(unsafe_code)]
